//! Bounded folder creation and deletion for the fixed HTTPS body worker.
use crate::{
    admission::work::{Charge, Meter, Stop},
    clock::RuntimeClock,
    device_credentials::Presented,
    format::Sequence,
    ids::{AccountId, DeviceId, MailboxId, MailboxOperationId, StoreEpoch},
    jmap_request::dispatch::MailboxMutator,
    ports::{Clock, CommitFailure, Deadline, Error},
    postgres_input::Loaded,
    postgres_mailbox_set::{self as changes, Change},
    postgres_probe::{self, Mode},
    postgres_read::ReadSlots,
    postgres_runtime_profile::Binding,
    store_fs::{
        MailboxCreateCompletion, MailboxCreateRequest, MailboxCreateResult,
        MailboxDeleteCompletion, MailboxDeleteRequest, MailboxDeleteResult, UploadError,
    },
};
use std::{
    path::Path,
    sync::Mutex,
    time::{Duration, Instant},
};
/// One active or uncertain mutation. Retain this owner across HTTP requests.
/// Production startup/recovery wiring remains part of the backend cutover.
#[derive(Default)]
pub struct Pending {
    change: Mutex<Option<Change>>,
}
impl Pending {
    pub fn operation(&self) -> Result<Option<MailboxOperationId>, Error> {
        Ok(self
            .change
            .try_lock()
            .map_err(|_| Error::Busy)?
            .as_ref()
            .map(Change::operation))
    }
}

/// A confirmed historical receipt; settling never authorizes a fresh write.
#[derive(Debug)]
pub struct Settled {
    pub operation: MailboxOperationId,
    pub outcome: changes::Outcome,
}
impl Pending {
    /// Use a separate settlement connection allowance, never foreground slots.
    /// Absence or any error retains the exact operation and keeps writes fenced.
    pub fn settle(
        &self,
        loaded: &Loaded,
        executable: &Path,
        settlement_slots: &ReadSlots,
        clock: &RuntimeClock,
        until: Deadline,
    ) -> Result<Option<Settled>, Error> {
        if !executable.is_absolute() {
            return Err(Error::Invalid);
        }
        let mut pending = self.change.try_lock().map_err(|_| Error::Busy)?;
        let Some(change) = pending.as_ref() else {
            return Ok(None);
        };
        let now = Instant::now();
        let deadline = clock.deadline_instant(until)?.min(
            now.checked_add(Duration::from_secs(10))
                .ok_or(Error::Invalid)?,
        );
        if now >= deadline {
            return Err(Error::Deadline);
        }
        let _slot = settlement_slots.acquire().map_err(|_| Error::Busy)?;
        let dns_deadline = deadline.min(
            now.checked_add(Duration::from_secs(5))
                .ok_or(Error::Invalid)?,
        );
        let addresses =
            crate::postgres_resolver::resolve(executable, &loaded.bootstrap, dns_deadline)
                .map_err(|error| match error {
                    crate::postgres_resolver::Error::Deadline => Error::Deadline,
                    _ => Error::Dns,
                })?;
        for address in addresses {
            let mut connection = loaded.bootstrap.request(address);
            connection.mode = Mode::Runtime;
            match changes::inspect(&connection, &loaded.trust, deadline, change) {
                Ok(Some(outcome)) => {
                    let operation = change.operation();
                    *pending = None;
                    return Ok(Some(Settled { operation, outcome }));
                }
                Ok(None) => return Ok(None),
                Err(changes::Error::Connection(postgres_probe::Error::Transport)) => (),
                Err(error) => return Err(map_error(error)),
            }
        }
        Err(Error::Invalid)
    }
}

/// Borrows the request secret only during bounded synchronous publication.
pub struct Mailboxes<'a> {
    pub loaded: &'a Loaded,
    pub executable: &'a Path,
    pub binding: &'a Binding,
    pub slots: &'a ReadSlots,
    pub clock: &'a RuntimeClock,
    pub pending: &'a Pending,
    pub password: &'a [u8],
}
impl MailboxMutator for Mailboxes<'_> {
    fn create_mailboxes(
        &self,
        request: MailboxCreateRequest<'_>,
        meter: &mut Meter,
    ) -> Result<MailboxCreateCompletion, UploadError> {
        if request.names.len() > changes::MAX_CREATES {
            return Err(Error::Capacity.into());
        }
        let mut results = Vec::new();
        results
            .try_reserve_exact(request.names.len())
            .map_err(|_| Error::Capacity)?;
        let complete = self.apply(
            Request {
                account: request.account,
                device: request.device,
                expected: request.expected,
                deadline: request.deadline,
                names: Some(request.names),
                mailbox: None,
            },
            meter,
        )?;
        for result in complete.results {
            results.push(match result {
                changes::ResultEntry::Created(id) => MailboxCreateResult::Created(id),
                changes::ResultEntry::InvalidName => MailboxCreateResult::InvalidName,
                changes::ResultEntry::AlreadyExists => MailboxCreateResult::AlreadyExists,
                _ => return Err(Error::Corrupt.into()),
            });
        }
        Ok(MailboxCreateCompletion {
            before_epoch: complete.before_epoch,
            before_sequence: complete.before_sequence,
            outcome: complete.outcome,
            results,
            admission_stopped: complete.admission_stopped,
        })
    }
    fn delete_mailbox(
        &self,
        request: MailboxDeleteRequest,
        meter: &mut Meter,
    ) -> Result<MailboxDeleteCompletion, UploadError> {
        let complete = self.apply(
            Request {
                account: request.account,
                device: request.device,
                expected: request.expected,
                deadline: request.deadline,
                names: None,
                mailbox: request.mailbox,
            },
            meter,
        )?;
        let result = complete
            .results
            .first()
            .map(|result| match result {
                changes::ResultEntry::Destroyed(id) if Some(*id) == request.mailbox => {
                    Ok(MailboxDeleteResult::Destroyed)
                }
                changes::ResultEntry::NotFound => Ok(MailboxDeleteResult::NotFound),
                changes::ResultEntry::Forbidden => Ok(MailboxDeleteResult::Forbidden),
                changes::ResultEntry::HasChild => Ok(MailboxDeleteResult::HasChild),
                changes::ResultEntry::HasEmail => Ok(MailboxDeleteResult::HasEmail),
                _ => Err(Error::Corrupt),
            })
            .transpose()?;
        Ok(MailboxDeleteCompletion {
            before_epoch: complete.before_epoch,
            before_sequence: complete.before_sequence,
            outcome: complete.outcome,
            result,
            admission_stopped: complete.admission_stopped,
        })
    }
}
struct Request<'a> {
    account: AccountId,
    device: DeviceId,
    expected: Option<(StoreEpoch, Sequence)>,
    deadline: Deadline,
    names: Option<&'a [&'a str]>,
    mailbox: Option<MailboxId>,
}
struct Completion {
    before_epoch: StoreEpoch,
    before_sequence: Sequence,
    outcome: Result<Sequence, CommitFailure>,
    results: Vec<changes::ResultEntry>,
    admission_stopped: bool,
}
impl Mailboxes<'_> {
    fn apply(&self, request: Request<'_>, meter: &mut Meter) -> Result<Completion, Error> {
        if !self.executable.is_absolute()
            || !self.binding.roles().jmap()
            || self.loaded.bootstrap.profile() != self.binding.profile()
            || request.account != self.binding.account()
            || Presented::parse(self.password).is_none_or(|value| value.id() != request.device)
        {
            return Err(Error::Forbidden);
        }
        if request
            .expected
            .is_some_and(|(epoch, _)| epoch != self.binding.epoch())
        {
            return Err(Error::Conflict);
        }
        let count = request.names.map_or(1, <[&str]>::len);
        if count > changes::MAX_CREATES || meter.deadline() < request.deadline.into() {
            return Err(Error::Capacity);
        }
        let now = self.clock.sample()?.monotonic;
        let until = Deadline::after(now, 10_000)?.min(request.deadline);
        meter
            .charge(
                now,
                Charge {
                    records: count as u64
                        * (crate::postgres_mailboxes::MAX_MAILBOXES as u64
                            + changes::MAX_CREATES as u64
                            + 32)
                        + 32,
                    ..Default::default()
                },
            )
            .map_err(|error| match error {
                Stop::Deadline => Error::Deadline,
                _ => Error::Capacity,
            })?;
        let deadline = self.clock.deadline_instant(until)?;
        if Instant::now() >= deadline {
            return Err(Error::Deadline);
        }
        let mut pending = self.pending.change.try_lock().map_err(|_| Error::Busy)?;
        if pending.is_some() {
            return Err(Error::Busy);
        }
        let _slot = self.slots.acquire().map_err(|_| Error::Busy)?;
        let mut results = Vec::new();
        results
            .try_reserve_exact(count)
            .map_err(|_| Error::Capacity)?;
        let change = match request.names {
            Some(names) => changes::prepare_with_meter(
                self.binding,
                request.device,
                request.expected.map(|(_, sequence)| sequence.number()),
                names,
                deadline,
                meter,
                self.clock,
            ),
            None => changes::prepare_delete_with_meter(
                self.binding,
                request.device,
                request.expected.map(|(_, sequence)| sequence.number()),
                request.mailbox,
                deadline,
                meter,
                self.clock,
            ),
        }
        .map_err(map_error)?;
        let dns_deadline = deadline.min(
            Instant::now()
                .checked_add(Duration::from_secs(5))
                .ok_or(Error::Invalid)?,
        );
        let addresses = crate::postgres_resolver::resolve(
            self.executable,
            &self.loaded.bootstrap,
            dns_deadline,
        )
        .map_err(|error| match error {
            crate::postgres_resolver::Error::Deadline => Error::Deadline,
            _ => Error::Dns,
        })?;
        *pending = Some(change);
        let change = pending.as_ref().ok_or(Error::Invalid)?;
        let mut outcome = Err(changes::Error::Connection(postgres_probe::Error::Transport));
        for address in addresses {
            let mut connection = self.loaded.bootstrap.request(address);
            connection.mode = Mode::Runtime;
            outcome = changes::publish(
                &connection,
                &self.loaded.trust,
                deadline,
                change,
                self.password,
            );
            if !matches!(
                outcome,
                Err(changes::Error::Connection(postgres_probe::Error::Transport))
            ) {
                break;
            }
        }
        match outcome {
            Ok(outcome) => {
                if outcome.results.len() != count {
                    return Err(Error::Corrupt);
                }
                results.extend(outcome.results);
                *pending = None;
                Ok(Completion {
                    before_epoch: self.binding.epoch(),
                    before_sequence: Sequence::from_u64(outcome.old_sequence),
                    outcome: Ok(Sequence::from_u64(outcome.new_sequence)),
                    results,
                    admission_stopped: false,
                })
            }
            Err(changes::Error::Indeterminate) => Ok(Completion {
                before_epoch: self.binding.epoch(),
                before_sequence: Sequence::from_u64(0),
                outcome: Err(CommitFailure::Indeterminate(Error::WriterStopped)),
                results,
                admission_stopped: true,
            }),
            Err(error) => {
                *pending = None;
                Err(map_error(error))
            }
        }
    }
}
fn map_error(error: changes::Error) -> Error {
    match error {
        changes::Error::Connection(postgres_probe::Error::Deadline) => Error::Deadline,
        changes::Error::Unauthenticated | changes::Error::Policy => Error::Forbidden,
        changes::Error::State | changes::Error::Epoch => Error::Conflict,
        changes::Error::Capacity => Error::Capacity,
        changes::Error::Integrity => Error::Corrupt,
        changes::Error::Validation(error) => error,
        _ => Error::Invalid,
    }
}

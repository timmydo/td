//! Bounded prepared Email/set adapter for the fixed HTTPS body worker.
use crate::{
    admission::work::{Charge, Meter, Stop},
    clock::RuntimeClock,
    device_credentials::Presented,
    format::Sequence,
    ids::EmailOperationId,
    jmap_request::dispatch::EmailMutator,
    ports::{Clock, CommitFailure, Deadline, Error},
    postgres_email_set::{self as changes, Change},
    postgres_input::Loaded,
    postgres_probe::{self, Mode},
    postgres_read::ReadSlots,
    postgres_runtime_profile::Binding,
    store_fs::{EmailUpdateCompletion, EmailUpdateRequest, EmailUpdateResult, UploadError},
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
    pub fn operation(&self) -> Result<Option<EmailOperationId>, Error> {
        Ok(self
            .change
            .try_lock()
            .map_err(|_| Error::Busy)?
            .as_ref()
            .map(Change::operation))
    }
}

/// Borrows the request secret only for this synchronous call. No background SQL.
pub struct Updates<'a> {
    pub loaded: &'a Loaded,
    pub executable: &'a Path,
    pub binding: &'a Binding,
    pub slots: &'a ReadSlots,
    pub clock: &'a RuntimeClock,
    pub pending: &'a Pending,
    pub password: &'a [u8],
}
impl EmailMutator for Updates<'_> {
    fn update(
        &self,
        request: EmailUpdateRequest<'_>,
        meter: &mut Meter,
    ) -> Result<EmailUpdateCompletion, UploadError> {
        self.apply(request, meter).map_err(Into::into)
    }
}
impl Updates<'_> {
    fn apply(
        &self,
        request: EmailUpdateRequest<'_>,
        meter: &mut Meter,
    ) -> Result<EmailUpdateCompletion, Error> {
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
        if request.updates.len() > changes::MAX_UPDATES
            || meter.deadline() < request.deadline.into()
        {
            return Err(Error::Capacity);
        }
        // This milestone admits only td-mail's keyword assignments.
        if request
            .updates
            .iter()
            .any(|update| update.mailbox.is_some())
        {
            return Err(Error::Forbidden);
        }
        let now = self.clock.sample()?.monotonic;
        let until = Deadline::after(now, 10_000)?.min(request.deadline);
        meter
            .charge(
                now,
                Charge {
                    records: request.updates.len() as u64
                        * (crate::postgres_mailboxes::MAX_MAILBOXES as u64 + 16)
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
        let mut updates = Vec::new();
        updates
            .try_reserve_exact(request.updates.len())
            .map_err(|_| Error::Capacity)?;
        let mut results = Vec::new();
        results
            .try_reserve_exact(request.updates.len())
            .map_err(|_| Error::Capacity)?;
        for update in request.updates {
            updates.push(changes::Update {
                mailbox: None,
                email: update.email,
                seen: update.seen,
                flagged: update.flagged,
            });
        }
        let change = changes::prepare_batch(
            self.binding,
            request.device,
            &updates,
            request.expected.map(|(_, sequence)| sequence.number()),
        )
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
            outcome = changes::publish_batch(
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
                // Allocate before effects; preserve a known commit after deadline.
                if outcome.results.len() != request.updates.len() {
                    return Err(Error::Corrupt);
                }
                for result in outcome.results {
                    results.push(match result {
                        changes::UpdateResult::Updated => EmailUpdateResult::Updated,
                        changes::UpdateResult::NotFound => EmailUpdateResult::NotFound,
                        changes::UpdateResult::InvalidMailbox => EmailUpdateResult::InvalidMailbox,
                        changes::UpdateResult::Destroyed => return Err(Error::Corrupt),
                    });
                }
                *pending = None;
                Ok(EmailUpdateCompletion {
                    before_epoch: self.binding.epoch(),
                    before_sequence: Sequence::from_u64(outcome.state.old_sequence),
                    outcome: Ok(Sequence::from_u64(outcome.state.new_sequence)),
                    results,
                    admission_stopped: false,
                })
            }
            Err(changes::Error::Indeterminate) => Ok(EmailUpdateCompletion {
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
        _ => Error::Invalid,
    }
}

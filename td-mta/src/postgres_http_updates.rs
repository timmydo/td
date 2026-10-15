//! Bounded prepared Email/set adapter for the fixed HTTPS body worker.
use crate::{
    admission::work::{Charge, Meter, Stop},
    clock::RuntimeClock,
    device_credentials::Presented,
    format::Sequence,
    ids::{AccountId, DeviceId, EmailId, EmailOperationId, StoreEpoch},
    jmap_request::dispatch::EmailMutator,
    ports::{Clock, CommitFailure, Deadline, Error},
    postgres_email_set::{self as changes, deletion, Change},
    postgres_input::Loaded,
    postgres_probe::{self, Mode},
    postgres_read::ReadSlots,
    postgres_runtime_profile::Binding,
    store_fs::{
        EmailDestroyCompletion, EmailDestroyRequest, EmailDestroyResult, EmailUpdate,
        EmailUpdateCompletion, EmailUpdateRequest, EmailUpdateResult, UploadError,
    },
};
use std::{
    path::Path,
    sync::Mutex,
    time::{Duration, Instant},
};

enum Operation {
    Update(Change),
    Destroy(deletion::Deletion),
}
impl Operation {
    fn operation(&self) -> EmailOperationId {
        match self {
            Self::Update(value) => value.operation(),
            Self::Destroy(value) => value.operation(),
        }
    }
    fn inspect(
        &self,
        connection: &postgres_probe::Request<'_>,
        trust: &td_crypto::TrustStore,
        deadline: Instant,
    ) -> Result<Option<changes::BatchOutcome>, changes::Error> {
        match self {
            Self::Update(value) => changes::inspect_batch(connection, trust, deadline, value),
            Self::Destroy(value) => deletion::inspect(connection, trust, deadline, value),
        }
    }
    fn publish(
        &self,
        connection: &postgres_probe::Request<'_>,
        trust: &td_crypto::TrustStore,
        deadline: Instant,
        password: &[u8],
    ) -> Result<changes::BatchOutcome, changes::Error> {
        match self {
            Self::Update(value) => {
                changes::publish_batch(connection, trust, deadline, value, password)
            }
            Self::Destroy(value) => deletion::publish(connection, trust, deadline, value, password),
        }
    }
}
enum Input<'a> {
    Update(&'a [EmailUpdate]),
    Destroy(&'a [EmailId]),
}
impl Input<'_> {
    fn len(&self) -> usize {
        match self {
            Self::Update(values) => values.len(),
            Self::Destroy(values) => values.len(),
        }
    }
    fn valid_result(&self, result: changes::UpdateResult) -> bool {
        match self {
            Self::Update(_) => matches!(
                result,
                changes::UpdateResult::Updated
                    | changes::UpdateResult::NotFound
                    | changes::UpdateResult::InvalidMailbox
            ),
            Self::Destroy(_) => matches!(
                result,
                changes::UpdateResult::Destroyed | changes::UpdateResult::NotFound
            ),
        }
    }
    fn prepare(
        &self,
        binding: &Binding,
        device: DeviceId,
        expected: Option<u64>,
    ) -> Result<Operation, Error> {
        match self {
            Self::Destroy(values) => deletion::prepare(binding, device, values, expected)
                .map(Operation::Destroy)
                .map_err(map_error),
            Self::Update(values) => {
                let mut updates = Vec::new();
                updates
                    .try_reserve_exact(values.len())
                    .map_err(|_| Error::Capacity)?;
                for update in *values {
                    updates.push(changes::Update {
                        mailbox: update.mailbox,
                        email: update.email,
                        seen: update.seen,
                        flagged: update.flagged,
                    });
                }
                changes::prepare_batch(binding, device, &updates, expected)
                    .map(Operation::Update)
                    .map_err(map_error)
            }
        }
    }
}
struct MutationRequest<'a> {
    account: AccountId,
    device: DeviceId,
    expected: Option<(StoreEpoch, Sequence)>,
    deadline: Deadline,
    input: Input<'a>,
}
struct Completion {
    before_sequence: Sequence,
    outcome: Result<Sequence, CommitFailure>,
    results: Vec<changes::UpdateResult>,
    admission_stopped: bool,
}

/// One active or uncertain mutation. Retain this owner across HTTP requests.
/// Production startup/recovery wiring remains part of the backend cutover.
#[derive(Default)]
pub struct Pending {
    change: Mutex<Option<Operation>>,
    pub mailboxes: crate::postgres_http_mailboxes::Pending,
    pub drafts: crate::postgres_http_drafts::Pending,
}
impl Pending {
    pub fn operation(&self) -> Result<Option<EmailOperationId>, Error> {
        Ok(self
            .change
            .try_lock()
            .map_err(|_| Error::Busy)?
            .as_ref()
            .map(Operation::operation))
    }
}

/// A confirmed historical receipt; settling never authorizes a fresh write.
#[derive(Debug)]
pub struct Settled {
    pub operation: EmailOperationId,
    pub outcome: changes::BatchOutcome,
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
            match change.inspect(&connection, &loaded.trust, deadline) {
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
        if request.updates.len() > changes::MAX_UPDATES {
            return Err(Error::Capacity.into());
        }
        let mut results = Vec::new();
        results
            .try_reserve_exact(request.updates.len())
            .map_err(|_| Error::Capacity)?;
        let complete = self.apply(
            MutationRequest {
                account: request.account,
                device: request.device,
                expected: request.expected,
                deadline: request.deadline,
                input: Input::Update(request.updates),
            },
            meter,
        )?;
        for result in complete.results {
            results.push(match result {
                changes::UpdateResult::Updated => EmailUpdateResult::Updated,
                changes::UpdateResult::NotFound => EmailUpdateResult::NotFound,
                changes::UpdateResult::InvalidMailbox => EmailUpdateResult::InvalidMailbox,
                _ => return Err(Error::Corrupt.into()),
            });
        }
        Ok(EmailUpdateCompletion {
            before_epoch: self.binding.epoch(),
            before_sequence: complete.before_sequence,
            outcome: complete.outcome,
            results,
            admission_stopped: complete.admission_stopped,
        })
    }
    fn destroy(
        &self,
        request: EmailDestroyRequest<'_>,
        meter: &mut Meter,
    ) -> Result<EmailDestroyCompletion, UploadError> {
        if request.emails.len() > changes::MAX_UPDATES {
            return Err(Error::Capacity.into());
        }
        let mut results = Vec::new();
        results
            .try_reserve_exact(request.emails.len())
            .map_err(|_| Error::Capacity)?;
        let complete = self.apply(
            MutationRequest {
                account: request.account,
                device: request.device,
                expected: request.expected,
                deadline: request.deadline,
                input: Input::Destroy(request.emails),
            },
            meter,
        )?;
        for result in complete.results {
            results.push(match result {
                changes::UpdateResult::Destroyed => EmailDestroyResult::Destroyed,
                changes::UpdateResult::NotFound => EmailDestroyResult::NotFound,
                _ => return Err(Error::Corrupt.into()),
            });
        }
        Ok(EmailDestroyCompletion {
            before_epoch: self.binding.epoch(),
            before_sequence: complete.before_sequence,
            outcome: complete.outcome,
            results,
            admission_stopped: complete.admission_stopped,
        })
    }
}
impl Updates<'_> {
    fn apply(&self, request: MutationRequest<'_>, meter: &mut Meter) -> Result<Completion, Error> {
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
        if request.input.len() > changes::MAX_UPDATES || meter.deadline() < request.deadline.into()
        {
            return Err(Error::Capacity);
        }
        let now = self.clock.sample()?.monotonic;
        let until = Deadline::after(now, 10_000)?.min(request.deadline);
        meter
            .charge(
                now,
                Charge {
                    records: request.input.len() as u64
                        * (crate::postgres_mailboxes::MAX_MAILBOXES as u64
                            + if matches!(&request.input, Input::Destroy(_)) {
                                3 * 256 + 32
                            } else {
                                16
                            })
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
        let change = request.input.prepare(
            self.binding,
            request.device,
            request.expected.map(|(_, sequence)| sequence.number()),
        )?;
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
            outcome = change.publish(&connection, &self.loaded.trust, deadline, self.password);
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
                if outcome.results.len() != request.input.len() {
                    return Err(Error::Corrupt);
                }
                if outcome
                    .results
                    .iter()
                    .any(|result| !request.input.valid_result(*result))
                {
                    return Err(Error::Corrupt);
                }
                *pending = None;
                Ok(Completion {
                    before_sequence: Sequence::from_u64(outcome.state.old_sequence),
                    outcome: Ok(Sequence::from_u64(outcome.state.new_sequence)),
                    results: outcome.results,
                    admission_stopped: false,
                })
            }
            Err(changes::Error::Indeterminate) => Ok(Completion {
                before_sequence: Sequence::from_u64(0),
                outcome: Err(CommitFailure::Indeterminate(Error::WriterStopped)),
                results: Vec::new(),
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

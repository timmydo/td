//! Bounded authenticated cancellation owner for the fixed HTTPS worker.
use crate::{
    admission::work::{Charge, Meter, Stop},
    clock::RuntimeClock,
    device_credentials::Presented,
    ids::{AccountId, DeviceId, StoreEpoch, SubmissionCancelId, SubmissionId},
    ports::{self, Clock, Deadline},
    postgres_input::Loaded,
    postgres_probe::{self, Mode},
    postgres_read::ReadSlots,
    postgres_route_owners::Claim,
    postgres_runtime_profile::Binding,
    postgres_submission_cancel::{self as changes, Cancellation},
};
use std::{
    path::Path,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Admission(ports::Error),
    Publication(changes::Error),
}
impl From<ports::Error> for Error {
    fn from(error: ports::Error) -> Self {
        Self::Admission(error)
    }
}

/// Retain across requests. Unknown COMMIT blocks another cancellation until
/// historical settlement; no password or transport callback is retained.
#[derive(Default)]
pub struct Pending {
    change: Mutex<Option<Cancellation>>,
}
#[derive(Debug)]
pub struct Settled {
    pub operation: SubmissionCancelId,
    pub outcome: changes::Outcome,
}
impl Pending {
    pub fn operation(&self) -> Result<Option<SubmissionCancelId>, Error> {
        Ok(self
            .change
            .try_lock()
            .map_err(|_| ports::Error::Busy)?
            .as_ref()
            .map(Cancellation::operation))
    }
    /// Internal historical settlement using independent connection capacity.
    /// Absence or errors retain the exact operation. This is not client auth.
    pub fn settle(
        &self,
        loaded: &Loaded,
        executable: &Path,
        slots: &ReadSlots,
        clock: &RuntimeClock,
        until: Deadline,
    ) -> Result<Option<Settled>, Error> {
        if !executable.is_absolute() {
            return Err(ports::Error::Invalid.into());
        }
        let mut pending = self.change.try_lock().map_err(|_| ports::Error::Busy)?;
        let Some(change) = pending.as_ref() else {
            return Ok(None);
        };
        let deadline = phase(clock, until)?;
        let _slot = slots.acquire().map_err(|_| ports::Error::Busy)?;
        for address in addresses(loaded, executable, deadline)? {
            let mut connection = loaded.bootstrap.request(address);
            connection.mode = Mode::Runtime;
            match changes::inspect(&connection, &loaded.trust, deadline, change) {
                Ok(Some(outcome)) => {
                    let operation = change.operation();
                    *pending = None;
                    return Ok(Some(Settled { operation, outcome }));
                }
                Ok(None) => {
                    // Frontend inspect holds the account lock behind the unknown
                    // COMMIT: absence now proves rollback, not a live race.
                    if change.frontend() {
                        *pending = None;
                    }
                    return Ok(None);
                }
                Err(changes::Error::Connection(postgres_probe::Error::Transport)) => (),
                Err(error) => return Err(Error::Publication(error)),
            }
        }
        Err(Error::Publication(changes::Error::Connection(
            postgres_probe::Error::Transport,
        )))
    }
}
pub struct Input {
    pub account: AccountId,
    pub device: DeviceId,
    pub submission: SubmissionId,
    pub expected: Option<(StoreEpoch, u64)>,
    pub deadline: Deadline,
}
pub struct Cancellations<'a> {
    pub loaded: &'a Loaded,
    pub executable: &'a Path,
    pub binding: &'a Binding,
    pub slots: &'a ReadSlots,
    pub clock: &'a RuntimeClock,
    pub pending: &'a Pending,
    pub password: &'a [u8],
}
impl Cancellations<'_> {
    /// The trusted relay owner supplies its claim and an idempotent local abort.
    /// The callback must obey the publisher's no-SQL/no-SMTP-round-trip contract;
    /// a connection retry may invoke it again after an aborted transaction.
    pub fn cancel(
        &self,
        input: Input,
        claim: &Claim,
        generation: u64,
        meter: &mut Meter,
        close: impl Fn(Instant) -> Result<(), changes::Error> + Send + Sync + 'static,
    ) -> Result<changes::Outcome, Error> {
        self.publish(input, Some((claim, generation)), meter, close)
    }
    pub fn cancel_frontend(
        &self,
        input: Input,
        meter: &mut Meter,
    ) -> Result<changes::Outcome, Error> {
        if self.pending.operation()?.is_some() {
            charge(meter, self.clock)?;
            let original = Deadline::after(ports::Tick(0), meter.deadline().tick().0)?;
            self.pending.settle(
                self.loaded,
                self.executable,
                self.slots,
                self.clock,
                input.deadline.min(original),
            )?;
        }
        self.publish(input, None, meter, |_| Ok(()))
    }
    fn publish(
        &self,
        input: Input,
        owner: Option<(&Claim, u64)>,
        meter: &mut Meter,
        close: impl Fn(Instant) -> Result<(), changes::Error> + Send + Sync + 'static,
    ) -> Result<changes::Outcome, Error> {
        if !self.executable.is_absolute()
            || !self.binding.roles().jmap()
            || self.loaded.bootstrap.profile() != self.binding.profile()
            || input.account != self.binding.account()
            || Presented::parse(self.password).is_none_or(|value| value.id() != input.device)
        {
            return Err(ports::Error::Forbidden.into());
        }
        if input
            .expected
            .is_some_and(|(epoch, _)| epoch != self.binding.epoch())
        {
            return Err(ports::Error::Conflict.into());
        }
        let original = Deadline::after(ports::Tick(0), meter.deadline().tick().0)?;
        let deadline =
            phase(self.clock, input.deadline)?.min(self.clock.deadline_instant(original)?);
        let mut pending = self
            .pending
            .change
            .try_lock()
            .map_err(|_| ports::Error::Busy)?;
        if pending.is_some() {
            return Err(ports::Error::Busy.into());
        }
        charge(meter, self.clock)?;
        let _slot = self.slots.acquire().map_err(|_| ports::Error::Busy)?;
        let authorization = changes::ClientRequest {
            binding: self.binding,
            device: input.device,
            expected: input.expected,
        };
        let change = match owner {
            Some((claim, generation)) => {
                changes::prepare_for_device(claim, generation, input.submission, authorization)
            }
            None => changes::prepare_frontend(input.submission, authorization),
        }
        .map_err(Error::Publication)?;
        let addresses = addresses(self.loaded, self.executable, deadline)?;
        *pending = Some(change);
        let change = pending.as_ref().ok_or(ports::Error::Invalid)?;
        let close = Arc::new(close);
        let mut outcome = Err(changes::Error::Connection(postgres_probe::Error::Transport));
        for (index, address) in addresses.into_iter().enumerate() {
            if index != 0 {
                if let Err(error) = charge(meter, self.clock) {
                    *pending = None;
                    return Err(error.into());
                }
            }
            let mut connection = self.loaded.bootstrap.request(address);
            connection.mode = Mode::Runtime;
            let close = close.clone();
            outcome = changes::publish_authenticated(
                &connection,
                &self.loaded.trust,
                deadline,
                change,
                self.password,
                move |until| close(until),
            );
            if !matches!(
                outcome,
                Err(changes::Error::Connection(postgres_probe::Error::Transport))
            ) {
                break;
            }
        }
        if !matches!(outcome, Err(changes::Error::Indeterminate)) {
            *pending = None;
        }
        outcome.map_err(Error::Publication)
    }
}
fn charge(meter: &mut Meter, clock: &RuntimeClock) -> Result<(), ports::Error> {
    // Prepay both receipt inspection and publication connection ceilings,
    // including runtime schema frames, plus whole-group staging/validation.
    meter
        .charge(
            clock.sample()?.monotonic,
            Charge {
                io_bytes: 3 * 1024 * 1024 + 192 * 1024,
                records: 8192,
                ..Default::default()
            },
        )
        .map_err(|error| {
            if error == Stop::Deadline {
                ports::Error::Deadline
            } else {
                ports::Error::Capacity
            }
        })
}
fn phase(clock: &RuntimeClock, until: Deadline) -> Result<Instant, ports::Error> {
    let now = Instant::now();
    let deadline = clock.deadline_instant(until)?.min(
        now.checked_add(Duration::from_secs(10))
            .ok_or(ports::Error::Invalid)?,
    );
    if now >= deadline {
        return Err(ports::Error::Deadline);
    }
    Ok(deadline)
}
fn addresses(
    loaded: &Loaded,
    executable: &Path,
    deadline: Instant,
) -> Result<Vec<crate::postgres_probe::Endpoint>, ports::Error> {
    let deadline = deadline.min(
        Instant::now()
            .checked_add(Duration::from_secs(5))
            .ok_or(ports::Error::Invalid)?,
    );
    crate::postgres_resolver::resolve(executable, &loaded.bootstrap, deadline).map_err(|error| {
        match error {
            crate::postgres_resolver::Error::Deadline => ports::Error::Deadline,
            _ => ports::Error::Dns,
        }
    })
}

impl crate::jmap_request::dispatch::SubmissionCanceler for Cancellations<'_> {
    fn cancel(&self, input: Input, meter: &mut Meter) -> Result<changes::Outcome, Error> {
        self.cancel_frontend(input, meter)
    }
}

//! Bounded submission owner for the fixed HTTPS worker. No dispatch activation.
use crate::{
    admission::work::{Charge, Meter, Stop},
    clock::RuntimeClock,
    device_credentials::Presented,
    ids::{AccountId, DeviceId, EmailId, IdentityId, StoreEpoch, SubmissionOperationId},
    ports::{self, Clock, Deadline},
    postgres_input::Loaded,
    postgres_probe::Mode,
    postgres_read::ReadSlots,
    postgres_runtime_profile::Binding,
    postgres_settlement::{self as settlement, Retained},
    postgres_submission_input as inputs,
    postgres_submissions::{self as changes, Submission},
};
use std::{
    sync::Mutex,
    time::{Duration, Instant},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Admission(ports::Error),
    Input(inputs::Error),
    Publication(changes::Error),
}
impl From<ports::Error> for Error {
    fn from(error: ports::Error) -> Self {
        Self::Admission(error)
    }
}

/// One active or uncertain outgoing body and the endpoint it was published
/// to. Retain across requests; no password survives the call. Runtime
/// startup/recovery wiring belongs to cutover.
#[derive(Default)]
pub struct Pending {
    change: Mutex<Option<Retained<Submission>>>,
}

#[derive(Debug)]
pub struct Settled {
    pub operation: SubmissionOperationId,
    pub outcome: changes::Outcome,
}
impl Pending {
    pub fn operation(&self) -> Result<Option<SubmissionOperationId>, Error> {
        Ok(self
            .change
            .try_lock()
            .map_err(|_| ports::Error::Busy)?
            .as_ref()
            .map(|held| held.change.operation()))
    }
    /// Historical inspection only, using independent settlement capacity.
    /// A found receipt or proven absence releases the operation; errors
    /// leave it available for further inspection.
    pub fn settle(
        &self,
        loaded: &Loaded,
        slots: &ReadSlots,
        clock: &RuntimeClock,
        until: Deadline,
    ) -> Result<Option<Settled>, Error> {
        let mut pending = self.change.try_lock().map_err(|_| ports::Error::Busy)?;
        if pending.is_none() {
            return Ok(None);
        }
        let deadline = phase(clock, until)?;
        settle_held(&mut pending, loaded, slots, deadline)
    }
}
/// Inspect only at the retained socket. Absence proves rollback unless another
/// process swept the expired receipt (POSTGRES.md section 165).
fn settle_held(
    pending: &mut Option<Retained<Submission>>,
    loaded: &Loaded,
    slots: &ReadSlots,
    deadline: Instant,
) -> Result<Option<Settled>, Error> {
    let Some(held) = pending.as_ref() else {
        return Ok(None);
    };
    let _slot = slots.acquire().map_err(|_| ports::Error::Busy)?;
    let connection = settlement::request(loaded);
    let settled = changes::inspect(&connection, deadline, &held.change)
        .map_err(Error::Publication)?
        .map(|outcome| Settled {
            operation: held.change.operation(),
            outcome,
        });
    *pending = None;
    Ok(settled)
}

pub struct Input {
    pub account: AccountId,
    pub device: DeviceId,
    pub email: EmailId,
    pub identity: IdentityId,
    pub expected: Option<(StoreEpoch, u64)>,
    pub deadline: Deadline,
}
pub struct Submissions<'a> {
    pub loaded: &'a Loaded,
    pub binding: &'a Binding,
    pub slots: &'a ReadSlots,
    pub clock: &'a RuntimeClock,
    pub pending: &'a Pending,
    pub password: &'a [u8],
}
impl Submissions<'_> {
    /// Resolve and freeze one source, then publish exactly one operation. A
    /// transport retry never substitutes another profile's route or identity.
    pub fn create(&self, input: Input, meter: &mut Meter) -> Result<changes::Outcome, Error> {
        if !self.binding.roles().jmap()
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
        meter
            .charge(
                self.clock.sample()?.monotonic,
                Charge {
                    records: 1,
                    ..Default::default()
                },
            )
            .map_err(stop)?;
        let mut pending = self
            .pending
            .change
            .try_lock()
            .map_err(|_| ports::Error::Busy)?;
        if Retained::inspect_before_write(&mut pending, self.binding.account(), Instant::now()) {
            // Settle within this request's capped deadline, with a foreground
            // slot it releases before taking its own.
            settlement::charge(meter, self.clock, 1)?;
            let inline = settlement::inline_deadline(deadline)?;
            if settle_held(&mut pending, self.loaded, self.slots, inline).is_err()
                || pending.is_some()
            {
                return Err(ports::Error::Busy.into());
            }
        }
        // One outgoing body, capped at 32 MiB by input preparation, plus its
        // temporary verified source. No second request can prepare alongside it.
        let _slot = self.slots.acquire().map_err(|_| ports::Error::Busy)?;
        let mut connection = self.loaded.bootstrap.request();
        connection.mode = Mode::Runtime;
        let prepared = inputs::prepare_with_meter(
            &connection,
            deadline,
            inputs::Input {
                binding: self.binding,
                email: input.email,
                identity: input.identity,
                expected: input.expected.map(|(_, sequence)| sequence),
            },
            self.password,
            meter,
            self.clock,
        )
        .map_err(Error::Input)?;
        let length = prepared.bytes().len() as u64;
        let recipients = prepared.recipients().len() as u64;
        let change = changes::prepare_with_meter(prepared, deadline, meter, self.clock)
            .map_err(Error::Publication)?;
        meter
            .charge(
                self.clock.sample()?.monotonic,
                Charge {
                    io_bytes: length,
                    records: length.div_ceil(4096) + recipients + 1024,
                    ..Default::default()
                },
            )
            .map_err(stop)?;
        let connection = settlement::request(self.loaded);
        let outcome = changes::publish(&connection, deadline, &change, self.password);
        if let Err(changes::Error::Indeterminate) = &outcome {
            *pending = Some(Retained::new(change, self.binding.account()));
        }
        outcome.map_err(Error::Publication)
    }
}
fn stop(error: Stop) -> ports::Error {
    if error == Stop::Deadline {
        ports::Error::Deadline
    } else {
        ports::Error::Capacity
    }
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

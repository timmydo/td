//! Bounded submission owner for the fixed HTTPS worker. No dispatch activation.
use crate::{
    admission::work::{Charge, Meter, Stop},
    clock::RuntimeClock,
    device_credentials::Presented,
    ids::{AccountId, DeviceId, EmailId, IdentityId, StoreEpoch, SubmissionOperationId},
    ports::{self, Clock, Deadline},
    postgres_input::Loaded,
    postgres_probe::{self, Mode},
    postgres_read::{self, ReadSlots},
    postgres_runtime_profile::Binding,
    postgres_submission_input as inputs,
    postgres_submissions::{self as changes, Submission},
};
use std::{
    path::Path,
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

/// One active or uncertain outgoing body. Retain across requests; no password
/// survives the call. Runtime startup/recovery wiring belongs to cutover.
#[derive(Default)]
pub struct Pending {
    change: Mutex<Option<Submission>>,
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
            .map(Submission::operation))
    }
    /// Historical inspection only, using independent settlement capacity.
    /// Absence and errors leave the original operation fenced and retained.
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
                Ok(None) => return Ok(None),
                Err(changes::Error::Connection(postgres_probe::Error::Transport)) => (),
                Err(error) => return Err(Error::Publication(error)),
            }
        }
        Err(ports::Error::Invalid.into())
    }
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
    pub executable: &'a Path,
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
        if pending.is_some() {
            return Err(ports::Error::Busy.into());
        }
        // One outgoing body, capped at 32 MiB by input preparation, plus its
        // temporary verified source. No second request can prepare alongside it.
        let _slot = self.slots.acquire().map_err(|_| ports::Error::Busy)?;
        let addresses = addresses(self.loaded, self.executable, deadline)?;
        let mut prepared = None;
        for address in &addresses {
            let mut connection = self.loaded.bootstrap.request(*address);
            connection.mode = Mode::Runtime;
            match inputs::prepare_with_meter(
                &connection,
                &self.loaded.trust,
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
            ) {
                Ok(value) => {
                    prepared = Some(value);
                    break;
                }
                Err(inputs::Error::Read(postgres_read::Error::Connection(
                    postgres_probe::Error::Transport,
                ))) => (),
                Err(error) => return Err(Error::Input(error)),
            }
        }
        let prepared = prepared.ok_or(Error::Input(inputs::Error::Read(
            postgres_read::Error::Connection(postgres_probe::Error::Transport),
        )))?;
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
        *pending = Some(change);
        let change = pending.as_ref().ok_or(ports::Error::Invalid)?;
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
        if !matches!(outcome, Err(changes::Error::Indeterminate)) {
            *pending = None;
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

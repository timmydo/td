//! Bounded upload publication owner; HTTP ingress remains separate.
use crate::{
    admission::work::{Charge, Meter, Stop},
    clock::RuntimeClock,
    device_credentials::Presented,
    ids::{AccountId, DeviceId, UploadOperationId},
    ports::{self, Clock, Deadline},
    postgres_input::Loaded,
    postgres_probe::{self, Mode},
    postgres_read::ReadSlots,
    postgres_runtime_profile::Binding,
    postgres_uploads::{self as changes, Upload},
};
use std::{
    path::Path,
    sync::{Mutex, MutexGuard},
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

/// One active or uncertain upload body. Retain across requests; no password
/// survives the call. Runtime startup/recovery wiring belongs to cutover.
#[derive(Default)]
pub struct Pending {
    change: Mutex<Option<Upload>>,
    ingress: Mutex<()>,
}

pub(crate) struct Ingress<'a> {
    owner: &'a Pending,
    _guard: MutexGuard<'a, ()>,
}
impl Pending {
    pub(crate) fn admit_ingress(&self) -> Result<Ingress<'_>, ports::Error> {
        let guard = self.ingress.try_lock().map_err(|_| ports::Error::Busy)?;
        if self
            .change
            .try_lock()
            .map_err(|_| ports::Error::Busy)?
            .is_some()
        {
            return Err(ports::Error::Busy);
        }
        Ok(Ingress {
            owner: self,
            _guard: guard,
        })
    }
}

#[derive(Debug)]
pub struct Settled {
    pub operation: UploadOperationId,
    pub outcome: changes::Outcome,
}
impl Pending {
    pub fn operation(&self) -> Result<Option<UploadOperationId>, Error> {
        Ok(self
            .change
            .try_lock()
            .map_err(|_| ports::Error::Busy)?
            .as_ref()
            .map(Upload::operation))
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
        Err(Error::Publication(changes::Error::Connection(
            postgres_probe::Error::Transport,
        )))
    }
}

pub struct Input<'a> {
    pub account: AccountId,
    pub device: DeviceId,
    pub bytes: &'a [u8],
    pub deadline: Deadline,
}
pub struct Uploads<'a> {
    pub loaded: &'a Loaded,
    pub executable: &'a Path,
    pub binding: &'a Binding,
    pub slots: &'a ReadSlots,
    pub clock: &'a RuntimeClock,
    pub pending: &'a Pending,
    pub password: &'a [u8],
}
impl Uploads<'_> {
    /// Publish completed, caller-admitted ingress with one retained operation.
    /// The caller keeps its original body and response reservations until return.
    pub fn create(&self, input: Input<'_>, meter: &mut Meter) -> Result<changes::Outcome, Error> {
        let ingress = self.pending.admit_ingress()?;
        self.create_admitted(input, meter, &ingress)
    }
    pub(crate) fn create_admitted(
        &self,
        input: Input<'_>,
        meter: &mut Meter,
        ingress: &Ingress<'_>,
    ) -> Result<changes::Outcome, Error> {
        if !std::ptr::eq(ingress.owner, self.pending) {
            return Err(ports::Error::Invalid.into());
        }
        if !self.executable.is_absolute()
            || !self.binding.roles().jmap()
            || self.loaded.bootstrap.profile() != self.binding.profile()
            || input.account != self.binding.account()
            || Presented::parse(self.password).is_none_or(|value| value.id() != input.device)
        {
            return Err(ports::Error::Forbidden.into());
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
        // Caller-admitted ingress and one retained copy are each at most
        // 32 MiB. The owner prevents another preparation while either is live.
        let _slot = self.slots.acquire().map_err(|_| ports::Error::Busy)?;
        let addresses = addresses(self.loaded, self.executable, deadline)?;
        let change = changes::prepare_with_meter(
            self.binding,
            input.device,
            input.bytes,
            deadline,
            meter,
            self.clock,
        )
        .map_err(Error::Publication)?;
        let length = input.bytes.len() as u64;
        meter
            .charge(
                self.clock.sample()?.monotonic,
                Charge {
                    io_bytes: length,
                    records: length.div_ceil(4096) + 1024,
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

#[cfg(test)]
mod ingress_tests {
    use super::*;
    #[test]
    fn ingress_is_exclusive_and_released_without_publication() -> Result<(), ports::Error> {
        let pending = Pending::default();
        let permit = pending.admit_ingress()?;
        assert!(matches!(pending.admit_ingress(), Err(ports::Error::Busy)));
        assert_eq!(pending.operation(), Ok(None));
        drop(permit);
        assert!(pending.admit_ingress().is_ok());
        Ok(())
    }
}

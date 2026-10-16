//! Bounded attempt publication custody for a trusted relay worker.
use crate::{
    admission::work::{Charge, Meter, Stop},
    clock::RuntimeClock,
    ids::{AttemptId, SubmissionId},
    ports::{self, Clock, Deadline},
    postgres_attempts::{self as attempts, Dispatch},
    postgres_probe::{Mode, Request},
    postgres_read::ReadSlots,
    postgres_route_owners::Claim,
    queue_clock::Guard,
};
use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use td_crypto::TrustStore;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Admission(ports::Error),
    Publication(attempts::Error),
}
impl From<ports::Error> for Error {
    fn from(error: ports::Error) -> Self {
        Self::Admission(error)
    }
}
/// A trusted caller resolves and selects one primary before admission. It owns
/// primary-change notification and shares this runtime clock with its guard.
/// No address fallback, DNS work or credential retention occurs here.
pub struct Connection<'a, 'r> {
    pub request: &'a Request<'r>,
    pub trust: &'a TrustStore,
    pub slots: &'a ReadSlots,
    pub clock: Arc<RuntimeClock>,
}
impl Connection<'_, '_> {
    fn admit(&self, until: Deadline, meter: &mut Meter) -> Result<Instant, Error> {
        if self.request.mode != Mode::Runtime {
            return Err(ports::Error::Forbidden.into());
        }
        let original = Deadline::after(ports::Tick(0), meter.deadline().tick().0)?;
        let now = Instant::now();
        let limit = self
            .clock
            .deadline_instant(until)?
            .min(self.clock.deadline_instant(original)?)
            .min(
                now.checked_add(Duration::from_secs(10))
                    .ok_or(ports::Error::Invalid)?,
            );
        if now >= limit {
            return Err(ports::Error::Deadline.into());
        }
        // Receipt + publication connection ceilings, runtime admission frames,
        // and bounded selection/staging. Settlement conservatively prepays both.
        meter
            .charge(
                self.clock.sample()?.monotonic,
                Charge {
                    io_bytes: 2 * 1024 * 1024 + 192 * 1024,
                    records: 4096,
                    ..Default::default()
                },
            )
            .map_err(|error| {
                if error == Stop::Deadline {
                    ports::Error::Deadline
                } else {
                    ports::Error::Capacity
                }
            })?;
        Ok(limit)
    }
}
/// Retain for the relay worker's lifetime. An unresolved COMMIT blocks another
/// attempt; settlement only inspects its exact receipt, never republishes it.
#[derive(Default)]
pub struct Pending {
    dispatch: Mutex<Option<Dispatch>>,
}
impl Pending {
    pub fn attempt(&self) -> Result<Option<AttemptId>, Error> {
        Ok(self
            .dispatch
            .try_lock()
            .map_err(|_| ports::Error::Busy)?
            .as_ref()
            .map(Dispatch::attempt))
    }
    /// Use independently reserved settlement slots. Absence and every error
    /// retain custody. A receipt is historical and cannot authorize SMTP.
    pub fn settle(
        &self,
        connection: &Connection<'_, '_>,
        until: Deadline,
        meter: &mut Meter,
    ) -> Result<Option<attempts::Outcome>, Error> {
        let mut pending = self.dispatch.try_lock().map_err(|_| ports::Error::Busy)?;
        let Some(dispatch) = pending.as_ref() else {
            return Ok(None);
        };
        let limit = connection.admit(until, meter)?;
        let _slot = connection.slots.acquire().map_err(|_| ports::Error::Busy)?;
        let outcome = attempts::inspect(connection.request, connection.trust, limit, dispatch)
            .map_err(Error::Publication)?;
        if outcome.is_some() {
            *pending = None;
        }
        Ok(outcome)
    }
}
pub struct Input {
    pub submission: SubmissionId,
    pub retry: bool,
    pub deadline: Deadline,
}
/// Passive prepared-attempt identity and receipt. Current transport admission,
/// initial envelope/body reads and durable phase transitions remain mandatory.
pub struct Published {
    pub dispatch: Dispatch,
    pub outcome: attempts::Outcome,
}
pub struct Dispatches<'a, 'r> {
    pub connection: Connection<'a, 'r>,
    pub guard: Arc<Mutex<Guard>>,
    pub pending: &'a Pending,
}
impl Dispatches<'_, '_> {
    pub fn publish(
        &self,
        claim: &Claim,
        generation: u64,
        input: Input,
        meter: &mut Meter,
    ) -> Result<Published, Error> {
        let mut pending = self
            .pending
            .dispatch
            .try_lock()
            .map_err(|_| ports::Error::Busy)?;
        if pending.is_some() {
            return Err(ports::Error::Busy.into());
        }
        let limit = self.connection.admit(input.deadline, meter)?;
        let _slot = self
            .connection
            .slots
            .acquire()
            .map_err(|_| ports::Error::Busy)?;
        let dispatch = if input.retry {
            attempts::retry(claim, generation, input.submission)
        } else {
            attempts::prepare(claim, generation, input.submission)
        }
        .map_err(Error::Publication)?;
        *pending = Some(dispatch);
        let value = pending.as_ref().ok_or(ports::Error::Invalid)?;
        let result = attempts::publish(
            self.connection.request,
            self.connection.trust,
            limit,
            value,
            self.connection.clock.clone(),
            self.guard.clone(),
        );
        match result {
            Ok(outcome) => Ok(Published {
                dispatch: pending.take().ok_or(ports::Error::Invalid)?,
                outcome,
            }),
            Err(error) => {
                if error != attempts::Error::Indeterminate {
                    *pending = None;
                }
                Err(Error::Publication(error))
            }
        }
    }
}

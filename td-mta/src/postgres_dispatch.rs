//! Bounded attempt and phase publication custody for a trusted relay worker.
use crate::{
    admission::work::{Charge, Meter, Stop},
    clock::RuntimeClock,
    ids::{AttemptId, SubmissionId},
    ports::{self, Clock, Deadline},
    postgres_attempt_phases::{self as phases, Change},
    postgres_attempts::{self as attempts, Dispatch},
    postgres_probe::{Mode, Request},
    postgres_read::{ReadPermit, ReadSlots},
    postgres_route_owners::Claim,
    queue_clock::Guard,
};
use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use td_crypto::TrustStore;

const ATTEMPT_IO: u64 = 2 * 1024 * 1024 + 192 * 1024;
const PHASE_IO: u64 = 3 * 1024 * 1024 + 128 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Admission(ports::Error),
    Publication(attempts::Error),
    Phase(phases::Error),
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
    fn admit(&self, until: Deadline, meter: &mut Meter, io_bytes: u64) -> Result<Instant, Error> {
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
                    io_bytes,
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
/// attempt or phase; settlement only inspects the exact receipt, never republishes it.
#[derive(Default)]
pub struct Pending {
    operation: Mutex<Option<Operation>>,
}
enum Operation {
    Dispatch(Box<Dispatch>),
    Phase(Change),
}
impl Operation {
    fn attempt(&self) -> AttemptId {
        match self {
            Self::Dispatch(value) => value.attempt(),
            Self::Phase(value) => value.attempt(),
        }
    }
}
/// Historical evidence only: neither variant permits further SMTP output.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Settlement {
    Dispatch(attempts::Outcome),
    Phase(phases::Outcome),
}
impl Pending {
    pub fn attempt(&self) -> Result<Option<AttemptId>, Error> {
        Ok(self
            .operation
            .try_lock()
            .map_err(|_| ports::Error::Busy)?
            .as_ref()
            .map(Operation::attempt))
    }
    /// Use independently reserved settlement slots. Absence and every error
    /// retain custody. A receipt is historical and cannot authorize SMTP.
    pub fn settle(
        &self,
        connection: &Connection<'_, '_>,
        until: Deadline,
        meter: &mut Meter,
    ) -> Result<Option<Settlement>, Error> {
        let mut pending = self.operation.try_lock().map_err(|_| ports::Error::Busy)?;
        let Some(operation) = pending.as_ref() else {
            return Ok(None);
        };
        let io = match operation {
            Operation::Dispatch(_) => ATTEMPT_IO,
            Operation::Phase(_) => PHASE_IO,
        };
        let limit = connection.admit(until, meter, io)?;
        let _slot = connection.slots.acquire().map_err(|_| ports::Error::Busy)?;
        let outcome = match operation {
            Operation::Dispatch(value) => {
                attempts::inspect(connection.request, connection.trust, limit, value)
                    .map_err(Error::Publication)?
                    .map(Settlement::Dispatch)
            }
            Operation::Phase(value) => {
                phases::inspect(connection.request, connection.trust, limit, value)
                    .map_err(Error::Phase)?
                    .map(Settlement::Phase)
            }
        };
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
            .operation
            .try_lock()
            .map_err(|_| ports::Error::Busy)?;
        if pending.is_some() {
            return Err(ports::Error::Busy.into());
        }
        let limit = self.connection.admit(input.deadline, meter, ATTEMPT_IO)?;
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
        *pending = Some(Operation::Dispatch(Box::new(dispatch)));
        let Some(Operation::Dispatch(value)) = pending.as_ref() else {
            return Err(ports::Error::Invalid.into());
        };
        let result = attempts::publish(
            self.connection.request,
            self.connection.trust,
            limit,
            value,
            self.connection.clock.clone(),
            self.guard.clone(),
        );
        match result {
            Ok(outcome) => {
                let Some(Operation::Dispatch(dispatch)) = pending.take() else {
                    return Err(ports::Error::Invalid.into());
                };
                Ok(Published {
                    dispatch: *dispatch,
                    outcome,
                })
            }
            Err(error) => {
                if error != attempts::Error::Indeterminate {
                    *pending = None;
                }
                Err(Error::Publication(error))
            }
        }
    }

    /// Publish a retained bounded phase/result request. The caller supplies
    /// actual current-transaction SMTP evidence. Close transport on uncertain
    /// COMMIT; even success is a passive receipt, not continuing SMTP authority.
    /// Admission here is not the separate final-outcome reservation required
    /// before AcceptancePossible or terminator handoff.
    pub fn publish_phase(
        &self,
        value: Change,
        until: Deadline,
        meter: &mut Meter,
    ) -> Result<phases::Outcome, Error> {
        self.publish_phase_with(value, until, meter, false)
    }
    /// Publish only a newly committed AcceptancePossible transition. Refuse
    /// retained receipts; settlement never grants continuing SMTP authority.
    pub fn publish_new_possible(
        &self,
        value: Change,
        until: Deadline,
        meter: &mut Meter,
    ) -> Result<phases::Outcome, Error> {
        self.publish_phase_with(value, until, meter, true)
    }
    fn publish_phase_with(
        &self,
        value: Change,
        until: Deadline,
        meter: &mut Meter,
        new_possible: bool,
    ) -> Result<phases::Outcome, Error> {
        let mut pending = self
            .pending
            .operation
            .try_lock()
            .map_err(|_| ports::Error::Busy)?;
        if pending.is_some() {
            return Err(ports::Error::Busy.into());
        }
        let limit = self.connection.admit(until, meter, PHASE_IO)?;
        let _slot = self
            .connection
            .slots
            .acquire()
            .map_err(|_| ports::Error::Busy)?;
        publish_phase(&self.connection, &mut pending, value, limit, new_possible)
    }

    /// Bind local reply storage and publication work to one exact subset before
    /// attempting a new AcceptancePossible commit. This is not SMTP authority.
    pub fn reserve_final_result(
        &self,
        claim: &Claim,
        generation: u64,
        attempt: AttemptId,
        ordinals: &[u32],
        until: Deadline,
        meter: &mut Meter,
    ) -> Result<FinalPlan<'_, '_, '_>, Error> {
        let possible = phases::acceptance_possible(claim, generation, attempt, ordinals)
            .map_err(Error::Phase)?;
        let reply = phases::reserve_final_reply(claim, generation, attempt, ordinals)
            .map_err(Error::Phase)?;
        let work = self.reserve_final(attempt, until, meter)?;
        Ok(FinalPlan {
            possible,
            reply,
            work,
            until,
        })
    }

    /// Prepay one final DATA result's publication work and retain its connection
    /// slot until consumption or drop. This is capacity, not SMTP authority;
    /// reply storage, durable claims and live ownership remain separate.
    pub fn reserve_final(
        &self,
        attempt: AttemptId,
        until: Deadline,
        meter: &mut Meter,
    ) -> Result<FinalWork<'_, '_, '_>, Error> {
        let pending = self
            .pending
            .operation
            .try_lock()
            .map_err(|_| ports::Error::Busy)?;
        if pending.is_some() {
            return Err(ports::Error::Busy.into());
        }
        let limit = self.connection.admit(until, meter, PHASE_IO)?;
        let permit = self
            .connection
            .slots
            .acquire()
            .map_err(|_| ports::Error::Busy)?;
        Ok(FinalWork {
            worker: self,
            attempt,
            limit,
            _permit: permit,
        })
    }
}

/// Local final-result capacity bound to one exact phase request. Consuming a
/// plan requires a second ordinary connection slot for the new publication.
pub struct FinalPlan<'w, 'a, 'r> {
    possible: Change,
    reply: phases::FinalReply,
    work: FinalWork<'w, 'a, 'r>,
    until: Deadline,
}
impl<'w, 'a, 'r> FinalPlan<'w, 'a, 'r> {
    /// Retain final-result custody only after a known new commit. An uncertain
    /// COMMIT remains in Pending and cannot be converted into this owner.
    pub fn publish_possible(self, meter: &mut Meter) -> Result<FinalResult<'w, 'a, 'r>, Error> {
        let worker = self.work.worker;
        let mut pending = worker
            .pending
            .operation
            .try_lock()
            .map_err(|_| ports::Error::Busy)?;
        if pending.is_some() {
            return Err(ports::Error::Busy.into());
        }
        if Instant::now() >= self.work.limit {
            return Err(ports::Error::Deadline.into());
        }
        let limit = worker
            .connection
            .admit(self.until, meter, PHASE_IO)?
            .min(self.work.limit);
        let _slot = worker
            .connection
            .slots
            .acquire()
            .map_err(|_| ports::Error::Busy)?;
        let outcome = publish_phase(
            &worker.connection,
            &mut pending,
            self.possible.clone(),
            limit,
            true,
        )?;
        if Instant::now() >= self.work.limit {
            return Err(ports::Error::Deadline.into());
        }
        Ok(FinalResult {
            possible: self.possible,
            reply: self.reply,
            work: self.work,
            outcome,
        })
    }
}
/// Non-cloneable final-result custody after known new publication. Its receipt
/// is passive: live ownership and transport admission remain separate.
pub struct FinalResult<'w, 'a, 'r> {
    possible: Change,
    reply: phases::FinalReply,
    work: FinalWork<'w, 'a, 'r>,
    outcome: phases::Outcome,
}
impl FinalResult<'_, '_, '_> {
    pub fn attempt(&self) -> AttemptId {
        self.possible.attempt()
    }
    pub fn outcome(&self) -> &phases::Outcome {
        &self.outcome
    }
    /// Consume retained capacity for the actual normalized final DATA reply.
    /// No caller-supplied Change can replace the original attempt or subset.
    pub fn record(self, reply: &str) -> Result<phases::Outcome, Error> {
        let value = self.reply.finish(reply).map_err(Error::Phase)?;
        self.work.publish(value)
    }
}

/// One non-cloneable final-result work allowance. Holding it leaves less pool
/// capacity for ordinary publications; reserve settlement capacity separately.
/// The original deadline is fixed at reservation and cannot be renewed.
pub struct FinalWork<'w, 'a, 'r> {
    worker: &'w Dispatches<'a, 'r>,
    attempt: AttemptId,
    limit: Instant,
    _permit: ReadPermit<'w>,
}
impl FinalWork<'_, '_, '_> {
    /// Consume the allowance for this attempt's positive or negative DATA result.
    /// Unknown COMMIT transfers the request to the worker's existing Pending.
    /// No fresh connection slot or work charge is needed at consumption.
    pub fn publish(self, value: Change) -> Result<phases::Outcome, Error> {
        if !value.is_final_for(self.attempt) {
            return Err(ports::Error::Invalid.into());
        }
        if Instant::now() >= self.limit {
            return Err(ports::Error::Deadline.into());
        }
        let mut pending = self
            .worker
            .pending
            .operation
            .try_lock()
            .map_err(|_| ports::Error::Busy)?;
        if pending.is_some() {
            return Err(ports::Error::Busy.into());
        }
        publish_phase(
            &self.worker.connection,
            &mut pending,
            value,
            self.limit,
            false,
        )
    }
}
fn publish_phase(
    connection: &Connection<'_, '_>,
    pending: &mut Option<Operation>,
    value: Change,
    limit: Instant,
    new_possible: bool,
) -> Result<phases::Outcome, Error> {
    *pending = Some(Operation::Phase(value));
    let Some(Operation::Phase(value)) = pending.as_ref() else {
        return Err(ports::Error::Invalid.into());
    };
    let result = if new_possible {
        phases::publish_new_possible(connection.request, connection.trust, limit, value)
    } else {
        phases::publish(connection.request, connection.trust, limit, value)
    };
    match result {
        Ok(outcome) => {
            *pending = None;
            Ok(outcome)
        }
        Err(error) => {
            if error != phases::Error::Indeterminate {
                *pending = None;
            }
            Err(Error::Phase(error))
        }
    }
}

//! Passive ordered route-health publication and retirement of one Prepared job.
use super::{
    Connection, Dispatches, Error, Operation, Pending, PreparedResult, Settlement, PHASE_IO,
};
use crate::{
    admission::work::Meter,
    ids::AttemptId,
    ports::{self, Deadline},
    postgres_attempt_recovery as recovery,
    postgres_attempts::Dispatch,
    postgres_read::ReadSlots,
    postgres_route_health as health,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FailureOutcome {
    pub health: health::Outcome,
    pub recovery: recovery::Outcome,
}
enum Stage {
    Preparing(Dispatch, health::Failure),
    Health {
        change: Box<health::Change>,
        recovery: Option<recovery::Recovery>,
        unknown: bool,
    },
    Recovery {
        health: health::Outcome,
        value: recovery::Recovery,
        unknown: bool,
    },
    Complete(FailureOutcome),
}
/// Passive continuation after the caller has closed all failed transport.
/// Retain this owner across errors; it never returns SMTP authority.
#[must_use = "retain failure publication until route health and recovery settle"]
pub struct FailedPrepared<'w, 'a, 'r> {
    worker: &'w Dispatches<'a, 'r>,
    attempt: AttemptId,
    stage: Stage,
}
impl<'w, 'a, 'r> PreparedResult<'w, 'a, 'r> {
    /// Consume pre-MAIL authority after fencing transport. Classification must
    /// describe the remote failure; local capacity/cancellation is not Network.
    /// Passive settlement uses separately admitted work after the SMTP cap.
    pub fn into_failed(self, failure: health::Failure) -> FailedPrepared<'w, 'a, 'r> {
        FailedPrepared {
            worker: self.worker,
            attempt: self.attempt(),
            stage: Stage::Preparing(self.published.dispatch, failure),
        }
    }
}
impl FailedPrepared<'_, '_, '_> {
    pub fn attempt(&self) -> AttemptId {
        self.attempt
    }
    /// Perform at most one publication or exact receipt inspection. None means
    /// retain and step again; it never permits a new attempt or SMTP output.
    /// The settlement pool must be separately reserved from ordinary work.
    pub fn step(
        &mut self,
        until: Deadline,
        meter: &mut Meter,
        settlement_slots: &ReadSlots,
    ) -> Result<Option<FailureOutcome>, Error> {
        if std::ptr::eq(settlement_slots, self.worker.connection.slots) {
            return Err(ports::Error::Forbidden.into());
        }
        if let Stage::Preparing(dispatch, failure) = &self.stage {
            let change = dispatch
                .route_failure(*failure)
                .map_err(Error::RouteFailure)?;
            let recovery = dispatch.interrupted(&[]).map_err(Error::Recovery)?;
            self.stage = Stage::Health {
                change: Box::new(change),
                recovery: Some(recovery),
                unknown: false,
            };
        }
        match &mut self.stage {
            Stage::Preparing(_, _) => Err(ports::Error::Invalid.into()),
            Stage::Health {
                change,
                recovery,
                unknown,
            } => {
                let outcome = if *unknown {
                    match self.worker.inspect_failure(
                        Operation::RouteFailure(change.clone()),
                        until,
                        meter,
                        settlement_slots,
                    )? {
                        Some(Settlement::RouteFailure(value)) => Some(value),
                        None => None,
                        _ => return Err(ports::Error::Invalid.into()),
                    }
                } else {
                    match self
                        .worker
                        .publish_route_failure(change.as_ref().clone(), until, meter)
                    {
                        Ok(value) => Some(value),
                        Err(error) => {
                            if error == Error::RouteFailure(health::Error::Indeterminate) {
                                *unknown = true;
                            }
                            return Err(error);
                        }
                    }
                };
                if let Some(health) = outcome {
                    let value = recovery.take().ok_or(ports::Error::Invalid)?;
                    self.stage = Stage::Recovery {
                        health,
                        value,
                        unknown: false,
                    };
                }
                Ok(None)
            }
            Stage::Recovery {
                health,
                value,
                unknown,
            } => {
                let outcome = if *unknown {
                    match self.worker.inspect_failure(
                        Operation::Recovery(Box::new(value.clone())),
                        until,
                        meter,
                        settlement_slots,
                    )? {
                        Some(Settlement::Recovery(value)) => Some(value),
                        None => None,
                        _ => return Err(ports::Error::Invalid.into()),
                    }
                } else {
                    match self.worker.publish_recovery(value.clone(), until, meter) {
                        Ok(value) => Some(value),
                        Err(error) => {
                            if error == Error::Recovery(recovery::Error::Indeterminate) {
                                *unknown = true;
                            }
                            return Err(error);
                        }
                    }
                };
                if let Some(recovery) = outcome {
                    let result = FailureOutcome {
                        health: *health,
                        recovery,
                    };
                    self.stage = Stage::Complete(result);
                    Ok(Some(result))
                } else {
                    Ok(None)
                }
            }
            Stage::Complete(value) => Ok(Some(*value)),
        }
    }
}
impl Dispatches<'_, '_> {
    fn inspect_failure(
        &self,
        expected: Operation,
        until: Deadline,
        meter: &mut Meter,
        slots: &ReadSlots,
    ) -> Result<Option<Settlement>, Error> {
        let mut pending = self
            .pending
            .operation
            .try_lock()
            .map_err(|_| ports::Error::Busy)?;
        if let Some(current) = pending.as_ref() {
            let matches = match (current, &expected) {
                (Operation::RouteFailure(a), Operation::RouteFailure(b)) => a.same_request(b),
                (Operation::Recovery(a), Operation::Recovery(b)) => a.same_request(b),
                _ => false,
            };
            if !matches {
                return Err(ports::Error::Busy.into());
            }
        } else {
            // Generic settlement may already have cleared custody. Reinspect
            // this exact request; an empty slot never proves its completion.
            *pending = Some(expected);
        }
        let connection = Connection {
            request: self.connection.request,
            trust: self.connection.trust,
            slots,
            clock: self.connection.clock.clone(),
        };
        Pending::settle_locked(&mut pending, &connection, until, meter)
    }
    fn publish_route_failure(
        &self,
        value: health::Change,
        until: Deadline,
        meter: &mut Meter,
    ) -> Result<health::Outcome, Error> {
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
        *pending = Some(Operation::RouteFailure(Box::new(value)));
        let Some(Operation::RouteFailure(value)) = pending.as_ref() else {
            return Err(ports::Error::Invalid.into());
        };
        match health::publish(self.connection.request, self.connection.trust, limit, value) {
            Ok(outcome) => {
                *pending = None;
                Ok(outcome)
            }
            Err(error) => {
                if error != health::Error::Indeterminate {
                    *pending = None;
                }
                Err(Error::RouteFailure(error))
            }
        }
    }
}

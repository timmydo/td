//! One whole-submission cancellation with response capacity before effects.
use super::{submission_set::states, Error, SubmissionAccess, Worker};
use crate::{
    admission::work::Meter, bounded::TextBuffer, format::Sequence, ports,
    postgres_http_cancellations as canceling, postgres_submission_cancel as changes,
    submission_request, sync::DataState, wire,
};
use std::{ops::Range, sync::Arc, time::Instant};
use td_json::arena::Value;

/// Trusted adapter to the current relay owner and its local transport fence.
/// Unknown COMMIT must retain the operation and refuse further cancellation.
pub trait SubmissionCanceler {
    fn cancel(
        &self,
        input: canceling::Input,
        meter: &mut Meter,
    ) -> Result<changes::Outcome, canceling::Error>;
}
/// Bind the authenticated fixed worker to a trusted relay owner. The local
/// abort callback is idempotent and obeys the cancellation publisher contract.
pub struct RouteCancellation<'a> {
    pub worker: &'a canceling::Cancellations<'a>,
    pub claim: &'a crate::postgres_route_owners::Claim,
    pub generation: u64,
    pub close: Arc<dyn Fn(Instant) -> Result<(), changes::Error> + Send + Sync>,
}
impl SubmissionCanceler for RouteCancellation<'_> {
    fn cancel(
        &self,
        input: canceling::Input,
        meter: &mut Meter,
    ) -> Result<changes::Outcome, canceling::Error> {
        let close = self.close.clone();
        self.worker
            .cancel(input, self.claim, self.generation, meter, move |until| {
                close(until)
            })
    }
}
impl Worker<'_, '_, '_, '_, '_> {
    pub(super) fn submission_cancel(
        &mut self,
        access: SubmissionAccess<'_>,
        update: Value<'_>,
        expected: Option<(crate::ids::StoreEpoch, u64)>,
        maximum: usize,
    ) -> Result<Range<u64>, Error> {
        let canceler = access.canceler.ok_or(Error::InvalidArguments)?;
        if expected.is_some_and(|(epoch, sequence)| {
            epoch != access.state.epoch || sequence != access.state.sequence.number()
        }) {
            return Err(Error::StateMismatch);
        }
        let mut objects = update.as_object().ok_or(Error::InvalidArguments)?;
        if objects.len() > maximum {
            return Err(Error::RequestTooLarge);
        }
        if objects.len() != 1 {
            return Err(Error::InvalidArguments);
        }
        let (key, value) = objects.next_bytes().ok_or(Error::InvalidArguments)?;
        let key = std::str::from_utf8(key).map_err(|_| Error::InvalidArguments)?;
        let id = wire::Id::parse(key).map_err(|_| Error::InvalidArguments)?;
        let patch = self
            .retention
            .with_mutation_meter(self.execution, |meter| {
                Ok(submission_request::cancellation(
                    value,
                    access.clock,
                    self.execution,
                    meter,
                ))
            })
            .map_err(|error| super::email_set::failure(error, expected.is_some()))?;
        let refusal = match patch {
            Ok(()) => None,
            Err(submission_request::Error::Property(_)) => Some("invalidProperties"),
            Err(submission_request::Error::Capacity) => return Err(Error::StorageCapacity),
            Err(submission_request::Error::Deadline) => return Err(Error::ExecutionDeadline),
            Err(submission_request::Error::Clock) => return Err(Error::State),
        };
        let submission = id.local();
        let refusal = refusal.or(if submission.is_none() {
            Some("notFound")
        } else {
            None
        });
        let bound = 2048usize
            .checked_add(key.len())
            .ok_or(Error::ResponseCapacity)?;
        if bound as u64 > self.available()? {
            return Err(Error::ResponseCapacity);
        }
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(bound)
            .map_err(|_| Error::ResponseCapacity)?;
        bytes.resize(bound, 0);
        let reserved = self
            .retention
            .reserve_mutation_output(bound)
            .map_err(Error::Storage)?;
        let mut old = access.state;
        let mut new = old;
        let refusal = if let Some(reason) = refusal {
            Some(reason)
        } else {
            let submission = submission.ok_or(Error::State)?;
            let result = self
                .retention
                .with_mutation_meter(self.execution, |meter| {
                    Ok(canceler.cancel(
                        canceling::Input {
                            account: self.account,
                            device: access.device,
                            submission,
                            expected,
                            deadline: self.execution,
                        },
                        meter,
                    ))
                })
                .map_err(|error| super::email_set::failure(error, expected.is_some()))?;
            match result {
                Ok(outcome) => {
                    if outcome.submission != submission
                        || !(1..=1000).contains(&outcome.recipients)
                        || !(outcome.changed == 0 || outcome.changed == outcome.recipients)
                        || outcome.old_sequence == 0
                        || expected.is_some_and(|(_, sequence)| sequence != outcome.old_sequence)
                        || (if outcome.changed == 0 {
                            Some(outcome.old_sequence)
                        } else {
                            outcome.old_sequence.checked_add(1)
                        }) != Some(outcome.new_sequence)
                    {
                        return Err(Error::ServerPartialFail);
                    }
                    old = DataState {
                        sequence: Sequence::from_u64(outcome.old_sequence),
                        ..old
                    };
                    new = DataState {
                        sequence: Sequence::from_u64(outcome.new_sequence),
                        ..old
                    };
                    None
                }
                Err(error) => Some(classify(error, expected.is_some())?),
            }
        };
        let mut output = TextBuffer::new(&mut bytes);
        states(&mut output, old, new)?;
        output
            .append(",\"created\":null,\"notCreated\":null,")
            .map_err(|_| Error::ServerPartialFail)?;
        if let Some(kind) = refusal {
            output
                .format(format_args!(
                    "\"updated\":null,\"notUpdated\":{{\"{key}\":{{\"type\":\"{kind}\"}}}}"
                ))
                .map_err(|_| Error::ServerPartialFail)?;
        } else {
            output
                .format(format_args!(
                    "\"updated\":{{\"{key}\":null}},\"notUpdated\":null"
                ))
                .map_err(|_| Error::ServerPartialFail)?;
        }
        output
            .append(",\"destroyed\":null,\"notDestroyed\":null}")
            .map_err(|_| Error::ServerPartialFail)?;
        let length = output.len();
        self.retention
            .finish_mutation_output(
                reserved,
                bytes.get(..length).ok_or(Error::ServerPartialFail)?,
            )
            .map_err(Error::Storage)
    }
}
fn classify(error: canceling::Error, conditional: bool) -> Result<&'static str, Error> {
    use crate::postgres_probe::Error as Connection;
    match error {
        canceling::Error::Publication(changes::Error::Indeterminate) => {
            Err(Error::ServerPartialFail)
        }
        canceling::Error::Publication(changes::Error::StateMismatch)
        | canceling::Error::Admission(ports::Error::Conflict)
            if conditional =>
        {
            Err(Error::StateMismatch)
        }
        canceling::Error::Publication(changes::Error::Missing) => Ok("notFound"),
        canceling::Error::Publication(
            changes::Error::CannotUnsend | changes::Error::Capacity | changes::Error::Fence,
        ) => Ok("cannotUnsend"),
        canceling::Error::Publication(changes::Error::Unauthenticated | changes::Error::Policy)
        | canceling::Error::Admission(ports::Error::Forbidden) => Ok("forbidden"),
        canceling::Error::Publication(
            changes::Error::Deadline | changes::Error::Connection(Connection::Deadline),
        )
        | canceling::Error::Admission(ports::Error::Deadline) => Err(Error::ExecutionDeadline),
        canceling::Error::Admission(ports::Error::Capacity) => Err(Error::StorageCapacity),
        canceling::Error::Publication(changes::Error::Integrity) => {
            Err(Error::Storage(ports::Error::Corrupt))
        }
        _ => Err(Error::StorageBusy),
    }
}

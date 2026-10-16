//! Bounded attempt and phase publication custody for a trusted relay worker.
use crate::{
    admission::work::{Charge, Meter, Stop},
    clock::RuntimeClock,
    ids::{AttemptId, SubmissionId},
    ports::{self, Clock, Deadline},
    postgres_attempt_phases::{self as phases, Change},
    postgres_attempt_recovery as recovery,
    postgres_attempts::{self as attempts, Dispatch},
    postgres_probe::{Mode, Request, BODY_CHECK_TURNS},
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
const BODY_CHECK_IO: u64 = 1024 * 1024 + 128 * 1024;

#[derive(Clone, Copy)]
enum LivePhase {
    Body,
    Possible,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Admission(ports::Error),
    Publication(attempts::Error),
    Phase(phases::Error),
    Recovery(recovery::Error),
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
        self.admit_work(until, meter, io_bytes, 4096)
    }
    fn admit_work(
        &self,
        until: Deadline,
        meter: &mut Meter,
        io_bytes: u64,
        records: u64,
    ) -> Result<Instant, Error> {
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
                    records,
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
    Recovery(Box<recovery::Recovery>),
}
impl Operation {
    fn attempt(&self) -> AttemptId {
        match self {
            Self::Dispatch(value) => value.attempt(),
            Self::Phase(value) => value.attempt(),
            Self::Recovery(value) => value.attempt(),
        }
    }
}
/// Historical evidence only: no variant permits further SMTP output.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Settlement {
    Dispatch(attempts::Outcome),
    Phase(phases::Outcome),
    Recovery(recovery::Outcome),
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
            Operation::Phase(_) | Operation::Recovery(_) => PHASE_IO,
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
            Operation::Recovery(value) => {
                recovery::inspect(connection.request, connection.trust, limit, value)
                    .map_err(Error::Recovery)?
                    .map(Settlement::Recovery)
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
impl<'a, 'r> Dispatches<'a, 'r> {
    pub fn publish(
        &self,
        claim: &Claim,
        generation: u64,
        input: Input,
        meter: &mut Meter,
    ) -> Result<Published, Error> {
        self.publish_with(claim, generation, input, meter, false)
            .map(|(published, _)| published)
    }
    /// Retain an owner only for a known new Prepared commit within the original
    /// job limit. Uncertainty retains Pending and yields no owner.
    pub fn publish_prepared(
        &self,
        claim: &Claim,
        generation: u64,
        input: Input,
        meter: &mut Meter,
    ) -> Result<PreparedResult<'_, 'a, 'r>, Error> {
        let until = input.deadline;
        let (published, limit) = self.publish_with(claim, generation, input, meter, true)?;
        if Instant::now() >= limit {
            return Err(ports::Error::Deadline.into());
        }
        Ok(PreparedResult {
            worker: self,
            published,
            limit,
            until,
        })
    }
    fn publish_with(
        &self,
        claim: &Claim,
        generation: u64,
        input: Input,
        meter: &mut Meter,
        new_only: bool,
    ) -> Result<(Published, Instant), Error> {
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
        let publish = if new_only {
            attempts::publish_new
        } else {
            attempts::publish
        };
        let result = publish(
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
                Ok((
                    Published {
                        dispatch: *dispatch,
                        outcome,
                    },
                    limit,
                ))
            }
            Err(error) => {
                if error != attempts::Error::Indeterminate {
                    *pending = None;
                }
                Err(Error::Publication(error))
            }
        }
    }

    /// Settle an interrupted or abandoned attempt after transport is fenced.
    /// The caller retains a clone for known-failure retry; unknown COMMIT moves
    /// exact custody into Pending and can only be inspected there.
    pub fn publish_recovery(
        &self,
        value: recovery::Recovery,
        until: Deadline,
        meter: &mut Meter,
    ) -> Result<recovery::Outcome, Error> {
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
        *pending = Some(Operation::Recovery(Box::new(value)));
        let Some(Operation::Recovery(value)) = pending.as_ref() else {
            return Err(ports::Error::Invalid.into());
        };
        let result =
            recovery::publish(self.connection.request, self.connection.trust, limit, value);
        match result {
            Ok(outcome) => {
                *pending = None;
                Ok(outcome)
            }
            Err(error) => {
                if error != recovery::Error::Indeterminate {
                    *pending = None;
                }
                Err(Error::Recovery(error))
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
        self.publish_phase_with(value, until, meter, None, None)
            .map(|(outcome, _)| outcome)
    }
    /// Publish only a newly committed AcceptancePossible transition. Refuse
    /// retained receipts; settlement never grants continuing SMTP authority.
    pub fn publish_new_possible(
        &self,
        value: Change,
        until: Deadline,
        meter: &mut Meter,
    ) -> Result<phases::Outcome, Error> {
        self.publish_phase_with(value, until, meter, Some(LivePhase::Possible), None)
            .map(|(outcome, _)| outcome)
    }
    /// Retain the exact Body operation only after known new publication.
    /// The caller owns actual 354 and the matching immutable body source.
    pub fn publish_body(
        &self,
        claim: &Claim,
        generation: u64,
        attempt: AttemptId,
        accepted: &[(u32, &str)],
        until: Deadline,
        meter: &mut Meter,
    ) -> Result<BodyResult<'_, 'a, 'r>, Error> {
        let body = phases::body(claim, generation, attempt, accepted).map_err(Error::Phase)?;
        let (outcome, limit) =
            self.publish_phase_with(body.clone(), until, meter, Some(LivePhase::Body), None)?;
        if Instant::now() >= limit {
            return Err(ports::Error::Deadline.into());
        }
        Ok(BodyResult {
            worker: self,
            body,
            limit,
            until,
            outcome,
        })
    }
    fn publish_phase_with(
        &self,
        value: Change,
        until: Deadline,
        meter: &mut Meter,
        new_phase: Option<LivePhase>,
        original: Option<Instant>,
    ) -> Result<(phases::Outcome, Instant), Error> {
        let mut pending = self
            .pending
            .operation
            .try_lock()
            .map_err(|_| ports::Error::Busy)?;
        if pending.is_some() {
            return Err(ports::Error::Busy.into());
        }
        if original.is_some_and(|limit| Instant::now() >= limit) {
            return Err(ports::Error::Deadline.into());
        }
        let admitted = self.connection.admit(until, meter, PHASE_IO)?;
        let limit = original.map_or(admitted, |limit| admitted.min(limit));
        let _slot = self
            .connection
            .slots
            .acquire()
            .map_err(|_| ports::Error::Busy)?;
        let outcome = publish_phase(
            &self.connection,
            &self.guard,
            &mut pending,
            value,
            limit,
            new_phase,
        )?;
        Ok((outcome, limit))
    }

    fn check_current_phase(
        &self,
        value: &Change,
        original: Instant,
        meter: &mut Meter,
        phase: LivePhase,
    ) -> Result<Instant, Error> {
        let worker = self;
        let pending = worker
            .pending
            .operation
            .try_lock()
            .map_err(|_| ports::Error::Busy)?;
        if pending.is_some() {
            return Err(ports::Error::Busy.into());
        }
        if Instant::now() >= original {
            return Err(ports::Error::Deadline.into());
        }
        let until = Deadline::after(ports::Tick(0), meter.deadline().tick().0)?;
        let io_bytes = match phase {
            LivePhase::Body => BODY_CHECK_IO,
            LivePhase::Possible => PHASE_IO,
        };
        let limit = worker
            .connection
            .admit(until, meter, io_bytes)?
            .min(original);
        let _slot = worker
            .connection
            .slots
            .acquire()
            .map_err(|_| ports::Error::Busy)?;
        let check = match phase {
            LivePhase::Body => phases::check_current_body,
            LivePhase::Possible => phases::check_current_possible,
        };
        check(
            worker.connection.request,
            worker.connection.trust,
            limit,
            value,
            worker.connection.clock.clone(),
            worker.guard.clone(),
        )
        .map_err(Error::Phase)
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
    ) -> Result<FinalPlan<'_, 'a, 'r>, Error> {
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
    ) -> Result<FinalWork<'_, 'a, 'r>, Error> {
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

/// Non-cloneable custody of a known new Prepared publication. The receipt and
/// observed envelope are passive; transport orchestration remains separate.
pub struct PreparedResult<'w, 'a, 'r> {
    worker: &'w Dispatches<'a, 'r>,
    published: Published,
    limit: Instant,
    until: Deadline,
}
impl<'w, 'a, 'r> PreparedResult<'w, 'a, 'r> {
    /// Bind the first untouched envelope and its observed cap to this owner.
    pub fn admit_envelope(mut self, meter: &mut Meter) -> Result<EnvelopePlan<'w, 'a, 'r>, Error> {
        let (envelope, limit) = self.observe_envelope(meter)?;
        self.limit = self.limit.min(limit);
        Ok(EnvelopePlan {
            prepared: self,
            envelope,
        })
    }
    /// After actual 354, consume this exact Prepared identity into new Body
    /// custody. Return its passive request for the matching immutable read.
    /// The caller supplies actual accepted RCPT replies, never a replacement
    /// claim, generation, attempt or deadline.
    pub fn publish_body(
        self,
        accepted: &[(u32, &str)],
        meter: &mut Meter,
    ) -> Result<(BodyResult<'w, 'a, 'r>, Dispatch), Error> {
        let body = self
            .published
            .dispatch
            .body(accepted)
            .map_err(Error::Phase)?;
        self.publish_body_change(body, meter)
    }
    fn publish_body_change(
        self,
        body: Change,
        meter: &mut Meter,
    ) -> Result<(BodyResult<'w, 'a, 'r>, Dispatch), Error> {
        let (outcome, limit) = self.worker.publish_phase_with(
            body.clone(),
            self.until,
            meter,
            Some(LivePhase::Body),
            Some(self.limit),
        )?;
        if Instant::now() >= limit {
            return Err(ports::Error::Deadline.into());
        }
        Ok((
            BodyResult {
                worker: self.worker,
                body,
                limit,
                until: self.until,
                outcome,
            },
            self.published.dispatch,
        ))
    }
    pub fn attempt(&self) -> AttemptId {
        self.published.dispatch.attempt()
    }
    pub fn outcome(&self) -> &attempts::Outcome {
        &self.published.outcome
    }
    /// Charge a fresh untouched-envelope observation without extending the
    /// original deadline. No connection slot or database lock survives return.
    pub fn observe_envelope(
        &self,
        meter: &mut Meter,
    ) -> Result<(attempts::Envelope, Instant), Error> {
        let worker = self.worker;
        let pending = worker
            .pending
            .operation
            .try_lock()
            .map_err(|_| ports::Error::Busy)?;
        if pending.is_some() {
            return Err(ports::Error::Busy.into());
        }
        if Instant::now() >= self.limit {
            return Err(ports::Error::Deadline.into());
        }
        let until = Deadline::after(ports::Tick(0), meter.deadline().tick().0)?;
        let limit = worker
            .connection
            .admit(until, meter, ATTEMPT_IO)?
            .min(self.limit);
        let _slot = worker
            .connection
            .slots
            .acquire()
            .map_err(|_| ports::Error::Busy)?;
        attempts::observe_envelope(
            worker.connection.request,
            worker.connection.trust,
            limit,
            &self.published.dispatch,
            worker.connection.clock.clone(),
            worker.guard.clone(),
        )
        .map_err(Error::Publication)
    }
}

/// Exact admitted envelope paired with Prepared custody. Subsequent untouched
/// checks preserve its identity and only shorten the retained deadline.
pub struct EnvelopePlan<'w, 'a, 'r> {
    prepared: PreparedResult<'w, 'a, 'r>,
    envelope: attempts::Envelope,
}
impl<'w, 'a, 'r> EnvelopePlan<'w, 'a, 'r> {
    /// Reserve ordered reply storage for this exact admitted attempt batch.
    pub fn reserve_rcpt(self) -> Result<RcptPlan<'w, 'a, 'r>, Error> {
        let published = &self.prepared.published;
        let replies = published
            .dispatch
            .reserve_rcpt_replies(&published.outcome.ordinals)
            .map_err(Error::Phase)?;
        Ok(RcptPlan {
            envelope: Box::new(self),
            replies,
        })
    }

    /// Bind actual positive RCPT replies before issuing DATA. Both alternative
    /// result requests retain this owner's exact identity and admitted subset.
    pub fn reserve_data(self, accepted: &[(u32, &str)]) -> Result<DataPlan<'w, 'a, 'r>, Error> {
        let (body, reply) = self.data_requests(accepted)?;
        Ok(DataPlan {
            envelope: self,
            body,
            reply,
        })
    }
    fn data_requests(
        &self,
        accepted: &[(u32, &str)],
    ) -> Result<(Change, phases::CommandReply), Error> {
        if accepted.iter().any(|(ordinal, _)| {
            self.envelope
                .recipients
                .binary_search_by_key(ordinal, |recipient| recipient.ordinal)
                .is_err()
        }) {
            return Err(ports::Error::Invalid.into());
        }
        let dispatch = &self.prepared.published.dispatch;
        Ok((
            dispatch.body(accepted).map_err(Error::Phase)?,
            dispatch
                .reserve_data_reply(accepted)
                .map_err(Error::Phase)?,
        ))
    }

    /// Reserve negative MAIL evidence before sending this envelope's sender.
    /// Live transport checks and actual command sequencing remain required.
    pub fn reserve_mail(self) -> Result<MailPlan<'w, 'a, 'r>, Error> {
        let reply = self
            .prepared
            .published
            .dispatch
            .reserve_mail_reply(&self.prepared.published.outcome.ordinals)
            .map_err(Error::Phase)?;
        Ok(MailPlan {
            envelope: self,
            reply,
        })
    }

    pub fn attempt(&self) -> AttemptId {
        self.prepared.attempt()
    }
    pub fn outcome(&self) -> &attempts::Outcome {
        self.prepared.outcome()
    }
    pub fn envelope(&self) -> &attempts::Envelope {
        &self.envelope
    }
    /// Applies only while the complete batch remains untouched Prepared.
    pub fn check_current(&mut self, meter: &mut Meter) -> Result<Instant, Error> {
        let (current, limit) = self.prepared.observe_envelope(meter)?;
        if current != self.envelope {
            return Err(Error::Publication(attempts::Error::Integrity));
        }
        self.prepared.limit = self.prepared.limit.min(limit);
        Ok(self.prepared.limit)
    }
    /// The caller still owns actual 354, matching session and accepted replies.
    pub fn publish_body(
        self,
        accepted: &[(u32, &str)],
        meter: &mut Meter,
    ) -> Result<(BodyResult<'w, 'a, 'r>, Dispatch), Error> {
        self.prepared.publish_body(accepted, meter)
    }
}

/// Admitted envelope paired with preallocated negative MAIL evidence.
/// Actual SMTP command/result custody remains the trusted caller's duty.
pub struct MailPlan<'w, 'a, 'r> {
    envelope: EnvelopePlan<'w, 'a, 'r>,
    reply: phases::CommandReply,
}
impl<'w, 'a, 'r> MailPlan<'w, 'a, 'r> {
    pub(crate) fn runtime_clock(&self) -> Arc<dyn Clock> {
        self.envelope.prepared.worker.connection.clock.clone()
    }
    pub(crate) fn original_deadline(&self) -> Deadline {
        self.envelope.prepared.until
    }
    pub(crate) fn work_limit(&self) -> Instant {
        self.envelope.prepared.limit
    }
    pub fn attempt(&self) -> AttemptId {
        self.envelope.attempt()
    }
    pub fn envelope(&self) -> &attempts::Envelope {
        self.envelope.envelope()
    }
    pub fn check_current(&mut self, meter: &mut Meter) -> Result<Instant, Error> {
        self.envelope.check_current(meter)
    }
    /// Return the original owner only for an actual normalized positive MAIL
    /// reply. This receipt does not authorize another command without checking.
    pub fn accepted(self, reply: &str) -> Result<EnvelopePlan<'w, 'a, 'r>, Error> {
        if crate::format::row::smtp_reply_text(reply).is_err()
            || crate::recipient_sweep::reply_class(Some(reply)) != Some(2)
        {
            return Err(ports::Error::Invalid.into());
        }
        Ok(self.envelope)
    }
    /// Retain the genuine refusal before attempting database admission. Failure
    /// keeps that exact reply even when the original deadline has elapsed.
    pub fn record_refusal(
        self,
        reply: &str,
        meter: &mut Meter,
    ) -> Result<phases::Outcome, CommandFailure<'w, 'a, 'r>> {
        let value = self.reply.finish(reply).map_err(|error| CommandFailure {
            error: Error::Phase(error),
            retained: None,
        })?;
        let prepared = self.envelope.prepared;
        RetainedCommand {
            worker: prepared.worker,
            limit: prepared.limit,
            until: prepared.until,
            value,
        }
        .publish(meter)
    }
}
/// Exact admitted envelope and reserved ordered RCPT evidence. Actual command
/// sequencing, session matching and fresh pre-output checks remain required.
pub struct RcptPlan<'w, 'a, 'r> {
    envelope: Box<EnvelopePlan<'w, 'a, 'r>>,
    replies: phases::RcptReplies,
}
impl<'w, 'a, 'r> RcptPlan<'w, 'a, 'r> {
    pub(crate) fn runtime_clock(&self) -> Arc<dyn Clock> {
        self.envelope.prepared.worker.connection.clock.clone()
    }
    pub(crate) fn original_deadline(&self) -> Deadline {
        self.envelope.prepared.until
    }
    pub(crate) fn work_limit(&self) -> Instant {
        self.envelope.prepared.limit
    }
    pub fn attempt(&self) -> AttemptId {
        self.envelope.attempt()
    }
    pub fn check_current(&mut self, meter: &mut Meter) -> Result<Instant, Error> {
        self.envelope.check_current(meter)
    }
    pub fn next_recipient(&self) -> Option<&attempts::Recipient> {
        let ordinal = self.replies.next_ordinal()?;
        let recipients = &self.envelope.envelope.recipients;
        let index = recipients
            .binary_search_by_key(&ordinal, |recipient| recipient.ordinal)
            .ok()?;
        recipients.get(index)
    }
    pub fn record_reply(&mut self, ordinal: u32, reply: &str) -> Result<(), Error> {
        self.replies.record(ordinal, reply).map_err(Error::Phase)
    }
    pub fn finish(self) -> Result<RcptResult<'w, 'a, 'r>, Error> {
        Ok(RcptResult {
            envelope: self.envelope,
            evidence: self.replies.finish().map_err(Error::Phase)?,
        })
    }
}
/// Retained actual replies paired with the original envelope owner. Partial
/// results can only plan interruption; they cannot create DATA custody.
pub struct RcptResult<'w, 'a, 'r> {
    envelope: Box<EnvelopePlan<'w, 'a, 'r>>,
    evidence: phases::RcptEvidence,
}
/// Exact negative-RCPT request paired with DATA alternatives. Actual command
/// sequencing remains the caller's duty; negative publication retains both steps.
pub struct RcptDataPlan<'w, 'a, 'r> {
    data: Box<DataPlan<'w, 'a, 'r>>,
    refusal: Option<Change>,
}
impl<'w, 'a, 'r> RcptDataPlan<'w, 'a, 'r> {
    pub(crate) fn runtime_clock(&self) -> Arc<dyn Clock> {
        self.data.envelope.prepared.worker.connection.clock.clone()
    }
    pub(crate) fn original_deadline(&self) -> Deadline {
        self.data.envelope.prepared.until
    }
    pub(crate) fn work_limit(&self) -> Instant {
        self.data.envelope.prepared.limit
    }
    pub(crate) fn fenced(self, error: Error) -> RcptBodyFailure<'w, 'a, 'r> {
        RcptBodyFailure {
            error,
            retained: self,
        }
    }
    /// Publish preceding RCPT refusal and new observed Body only after actual
    /// 354. Any failure retains evidence and requires fencing the session;
    /// the failure grants no transport retry or historical output owner.
    pub fn publish_body(
        mut self,
        reply: &str,
        meter: &mut Meter,
    ) -> Result<(BodyResult<'w, 'a, 'r>, Dispatch), RcptBodyFailure<'w, 'a, 'r>> {
        let result = self.try_body(reply, meter);
        match result {
            Ok((outcome, limit)) if Instant::now() < limit => {
                let DataPlan { envelope, body, .. } = *self.data;
                let prepared = envelope.prepared;
                Ok((
                    BodyResult {
                        worker: prepared.worker,
                        body,
                        limit,
                        until: prepared.until,
                        outcome,
                    },
                    prepared.published.dispatch,
                ))
            }
            Ok(_) => Err(RcptBodyFailure {
                error: ports::Error::Deadline.into(),
                retained: self,
            }),
            Err(error) => Err(RcptBodyFailure {
                error,
                retained: self,
            }),
        }
    }
    fn try_body(
        &mut self,
        reply: &str,
        meter: &mut Meter,
    ) -> Result<(phases::Outcome, Instant), Error> {
        if !valid_data_reply(reply) {
            return Err(ports::Error::Invalid.into());
        }
        if let Some(refusal) = self.refusal.as_ref() {
            let prepared = &mut self.data.envelope.prepared;
            let mut retained = RetainedCommand {
                worker: prepared.worker,
                limit: prepared.limit,
                until: prepared.until,
                value: refusal.clone(),
            };
            let result = retained.try_publish(meter);
            prepared.limit = retained.limit;
            result?;
            self.refusal = None;
        }
        let prepared = &self.data.envelope.prepared;
        prepared.worker.publish_phase_with(
            self.data.body.clone(),
            prepared.until,
            meter,
            Some(LivePhase::Body),
            Some(prepared.limit),
        )
    }
    pub fn attempt(&self) -> AttemptId {
        self.data.attempt()
    }
    pub fn check_current(&mut self, meter: &mut Meter) -> Result<Instant, Error> {
        self.data.check_current(meter)
    }
    /// Split for explicit caller-managed ordering and evidence retention.
    pub fn into_parts(self) -> (DataPlan<'w, 'a, 'r>, Option<Change>) {
        (*self.data, self.refusal)
    }
    /// Capture actual negative DATA before admitting either database operation.
    /// The matching SMTP session must be closed/fenced before publication.
    pub fn record_refusal(
        self,
        reply: &str,
        meter: &mut Meter,
    ) -> Result<phases::Outcome, RcptCommandFailure<'w, 'a, 'r>> {
        let command = self
            .data
            .retain_refusal(reply)
            .map_err(|error| RcptCommandFailure {
                error,
                retained: None,
            })?;
        RcptCommand {
            command,
            refusal: self.refusal,
        }
        .publish(meter)
    }
}
/// Failed positive DATA handoff retains the original actual RCPT evidence.
/// Close/fence the session; only passive interruption planning remains.
pub struct RcptBodyFailure<'w, 'a, 'r> {
    pub error: Error,
    retained: RcptDataPlan<'w, 'a, 'r>,
}
impl std::fmt::Debug for RcptBodyFailure<'_, '_, '_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RcptBodyFailure")
            .field("error", &self.error)
            .finish_non_exhaustive()
    }
}
impl RcptBodyFailure<'_, '_, '_> {
    pub fn attempt(&self) -> AttemptId {
        self.retained.attempt()
    }
    /// Construct only after fencing transport. Pending must settle before
    /// ordered refusal/recovery publication; allocation failure keeps custody.
    pub fn into_interruption(mut self) -> Result<RcptInterruption, Self> {
        match self.retained.data.body.interrupted_body() {
            Ok(recovery) => Ok(RcptInterruption {
                refusal: self.retained.refusal,
                recovery: Some(recovery),
            }),
            Err(error) => {
                self.error = Error::Recovery(error);
                Err(self)
            }
        }
    }
}
/// Exact passive command settlement requests. Publish refusal first through
/// the same worker/Pending; unknown COMMIT must settle before the next request.
/// No transport owner, slot, deadline or permission survives this extraction.
pub struct CommandEvidence {
    pub refusal: Option<Change>,
    pub command: Change,
}
/// Failed negative DATA settlement retains both actual command evidence and
/// any preceding RCPT refusal. It never grants transport authority.
pub struct RcptCommandFailure<'w, 'a, 'r> {
    pub error: Error,
    retained: Option<RcptCommand<'w, 'a, 'r>>,
}
impl std::fmt::Debug for RcptCommandFailure<'_, '_, '_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RcptCommandFailure")
            .field("error", &self.error)
            .finish_non_exhaustive()
    }
}
struct RcptCommand<'w, 'a, 'r> {
    command: RetainedCommand<'w, 'a, 'r>,
    refusal: Option<Change>,
}
impl<'w, 'a, 'r> RcptCommand<'w, 'a, 'r> {
    fn publish(
        mut self,
        meter: &mut Meter,
    ) -> Result<phases::Outcome, RcptCommandFailure<'w, 'a, 'r>> {
        let result = (|| {
            if let Some(refusal) = self.refusal.as_ref() {
                self.command.try_publish_value(refusal.clone(), meter)?;
                self.refusal = None;
            }
            self.command.try_publish(meter)
        })();
        result.map_err(|error| RcptCommandFailure {
            error,
            retained: Some(self),
        })
    }
}
impl<'w, 'a, 'r> RcptCommandFailure<'w, 'a, 'r> {
    /// Consume only after fencing the matching session. Separate bounded
    /// settlement may admit fresh work; it cannot revive this SMTP attempt.
    pub fn into_evidence(self) -> Option<CommandEvidence> {
        self.retained.map(|retained| CommandEvidence {
            refusal: retained.refusal,
            command: retained.command.value,
        })
    }
    pub fn attempt(&self) -> Option<AttemptId> {
        self.retained
            .as_ref()
            .map(|retained| retained.command.value.attempt())
    }
    /// Unknown COMMIT must first settle through Pending. Retrying preserves
    /// original identity, evidence, ordering and the earliest admitted cap.
    pub fn retry(mut self, meter: &mut Meter) -> Result<phases::Outcome, Self> {
        match self.retained.take() {
            Some(retained) => retained.publish(meter),
            None => Err(self),
        }
    }
}

/// Passive exact requests for an already fenced session. Publish refusal first;
/// unknown COMMIT must settle before recovery. An all-refused complete batch
/// needs no recovery because refusal publication itself retires the attempt.
pub struct RcptInterruption {
    pub refusal: Option<Change>,
    pub recovery: Option<recovery::Recovery>,
}
/// Planning failure preserves the exact evidence and minimum envelope cap.
pub struct RcptFailure<'w, 'a, 'r> {
    pub error: Error,
    retained: RcptResult<'w, 'a, 'r>,
}
impl std::fmt::Debug for RcptFailure<'_, '_, '_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RcptFailure")
            .field("error", &self.error)
            .finish_non_exhaustive()
    }
}
impl<'w, 'a, 'r> RcptFailure<'w, 'a, 'r> {
    pub fn into_retained(self) -> RcptResult<'w, 'a, 'r> {
        self.retained
    }
}
impl<'w, 'a, 'r> RcptResult<'w, 'a, 'r> {
    pub fn attempt(&self) -> AttemptId {
        self.envelope.attempt()
    }
    pub fn evidence(&self) -> &phases::RcptEvidence {
        &self.evidence
    }
    pub fn reserve_data(self) -> Result<RcptDataPlan<'w, 'a, 'r>, RcptFailure<'w, 'a, 'r>> {
        let requests = (|| {
            if !self.evidence.is_complete() || self.evidence.accepted().is_empty() {
                return Err(ports::Error::Invalid.into());
            }
            let accepted = borrowed_rcpt(self.evidence.accepted())?;
            self.envelope.data_requests(&accepted)
        })();
        match requests {
            Ok((body, reply)) => Ok(RcptDataPlan {
                data: Box::new(DataPlan {
                    envelope: *self.envelope,
                    body,
                    reply,
                }),
                refusal: self.evidence.refusal().cloned(),
            }),
            Err(error) => Err(RcptFailure {
                error,
                retained: self,
            }),
        }
    }
    /// Consume only after closing/fencing the matching SMTP session. These
    /// passive requests preserve evidence but never renew transport deadlines.
    pub fn into_interruption(self) -> Result<RcptInterruption, RcptFailure<'w, 'a, 'r>> {
        let recovery = (|| {
            if self.evidence.is_complete() && self.evidence.accepted().is_empty() {
                Ok(None)
            } else {
                let accepted = borrowed_rcpt(self.evidence.accepted())?;
                self.envelope
                    .prepared
                    .published
                    .dispatch
                    .interrupted(&accepted)
                    .map(Some)
                    .map_err(Error::Recovery)
            }
        })();
        match recovery {
            Ok(recovery) => Ok(RcptInterruption {
                refusal: self.evidence.refusal().cloned(),
                recovery,
            }),
            Err(error) => Err(RcptFailure {
                error,
                retained: self,
            }),
        }
    }
}
fn valid_data_reply(reply: &str) -> bool {
    crate::format::row::smtp_reply_text(reply).is_ok()
        && crate::recipient_sweep::reply_class(Some(reply)) == Some(3)
        && reply.as_bytes().get(..3) == Some(b"354".as_slice())
}
fn borrowed_rcpt(replies: &[(u32, String)]) -> Result<Vec<(u32, &str)>, Error> {
    let mut accepted = Vec::new();
    accepted
        .try_reserve_exact(replies.len())
        .map_err(|_| ports::Error::Capacity)?;
    for (ordinal, reply) in replies {
        accepted.push((*ordinal, reply.as_str()));
    }
    Ok(accepted)
}

/// Exact accepted RCPT subset paired with both possible DATA-command results.
/// Actual DATA sequencing, fresh checks and session matching remain required.
pub struct DataPlan<'w, 'a, 'r> {
    envelope: EnvelopePlan<'w, 'a, 'r>,
    body: Change,
    reply: phases::CommandReply,
}
impl<'w, 'a, 'r> DataPlan<'w, 'a, 'r> {
    pub fn attempt(&self) -> AttemptId {
        self.envelope.attempt()
    }
    pub fn check_current(&mut self, meter: &mut Meter) -> Result<Instant, Error> {
        self.envelope.check_current(meter)
    }
    /// Only actual normalized 354 may consume the retained subset into Body.
    /// No replacement recipient list, reply evidence or deadline is accepted.
    pub fn publish_body(
        self,
        reply: &str,
        meter: &mut Meter,
    ) -> Result<(BodyResult<'w, 'a, 'r>, Dispatch), Error> {
        if !valid_data_reply(reply) {
            return Err(ports::Error::Invalid.into());
        }
        self.envelope.prepared.publish_body_change(self.body, meter)
    }
    /// Capture genuine negative DATA-command evidence before database admission.
    /// Failure uses the same bounded retained-command retry owner as MAIL.
    pub fn record_refusal(
        self,
        reply: &str,
        meter: &mut Meter,
    ) -> Result<phases::Outcome, CommandFailure<'w, 'a, 'r>> {
        self.retain_refusal(reply)
            .map_err(|error| CommandFailure {
                error,
                retained: None,
            })?
            .publish(meter)
    }
    fn retain_refusal(self, reply: &str) -> Result<RetainedCommand<'w, 'a, 'r>, Error> {
        let value = self.reply.finish(reply).map_err(Error::Phase)?;
        let prepared = self.envelope.prepared;
        Ok(RetainedCommand {
            worker: prepared.worker,
            limit: prepared.limit,
            until: prepared.until,
            value,
        })
    }
}

/// Failed publication of actual negative command evidence. Retrying cannot
/// replace its identity or reply and cannot extend its retained deadline.
pub struct CommandFailure<'w, 'a, 'r> {
    pub error: Error,
    retained: Option<RetainedCommand<'w, 'a, 'r>>,
}
impl std::fmt::Debug for CommandFailure<'_, '_, '_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CommandFailure")
            .field("error", &self.error)
            .finish_non_exhaustive()
    }
}
struct RetainedCommand<'w, 'a, 'r> {
    worker: &'w Dispatches<'a, 'r>,
    limit: Instant,
    until: Deadline,
    value: Change,
}
impl<'w, 'a, 'r> RetainedCommand<'w, 'a, 'r> {
    fn publish(mut self, meter: &mut Meter) -> Result<phases::Outcome, CommandFailure<'w, 'a, 'r>> {
        match self.try_publish(meter) {
            Ok(outcome) => Ok(outcome),
            Err(error) => Err(CommandFailure {
                error,
                retained: Some(self),
            }),
        }
    }
    fn try_publish(&mut self, meter: &mut Meter) -> Result<phases::Outcome, Error> {
        self.try_publish_value(self.value.clone(), meter)
    }
    fn try_publish_value(
        &mut self,
        value: Change,
        meter: &mut Meter,
    ) -> Result<phases::Outcome, Error> {
        let worker = self.worker;
        let mut pending = worker
            .pending
            .operation
            .try_lock()
            .map_err(|_| ports::Error::Busy)?;
        if pending.is_some() {
            return Err(ports::Error::Busy.into());
        }
        if Instant::now() >= self.limit {
            return Err(ports::Error::Deadline.into());
        }
        self.limit = self
            .limit
            .min(worker.connection.admit(self.until, meter, PHASE_IO)?);
        let _slot = worker
            .connection
            .slots
            .acquire()
            .map_err(|_| ports::Error::Busy)?;
        publish_phase(
            &worker.connection,
            &worker.guard,
            &mut pending,
            value,
            self.limit,
            None,
        )
    }
}
impl<'w, 'a, 'r> CommandFailure<'w, 'a, 'r> {
    /// Extract exact passive evidence after fencing the matching session.
    /// Fresh settlement admission cannot recreate transport authority, and
    /// unresolved COMMIT remains blocked by the same worker's Pending owner.
    pub fn into_evidence(self) -> Option<Change> {
        self.retained.map(|retained| retained.value)
    }
    pub fn attempt(&self) -> Option<AttemptId> {
        self.retained
            .as_ref()
            .map(|retained| retained.value.attempt())
    }
    /// Explicit fresh work/slot admission. Unknown COMMIT must first settle
    /// through Pending; this retry grants no continuing transport permission.
    pub fn retry(mut self, meter: &mut Meter) -> Result<phases::Outcome, Self> {
        match self.retained.take() {
            Some(retained) => retained.publish(meter),
            None => Err(self),
        }
    }
}

/// Non-cloneable custody of a known new Body publication. Its receipt is
/// passive; current checks and transport admission remain separate.
pub struct BodyResult<'w, 'a, 'r> {
    worker: &'w Dispatches<'a, 'r>,
    body: Change,
    limit: Instant,
    until: Deadline,
    outcome: phases::Outcome,
}
impl<'w, 'a, 'r> BodyResult<'w, 'a, 'r> {
    /// Bind final publication capacity to this exact Body subset. The input
    /// deadline and earliest retained limit cannot be replaced or extended.
    pub fn reserve_final(self, meter: &mut Meter) -> Result<BodyPlan<'w, 'a, 'r>, Error> {
        if Instant::now() >= self.limit {
            return Err(ports::Error::Deadline.into());
        }
        let mut work = self
            .worker
            .reserve_final(self.attempt(), self.until, meter)?;
        work.limit = work.limit.min(self.limit);
        if Instant::now() >= work.limit {
            return Err(ports::Error::Deadline.into());
        }
        let (possible, reply) = self.body.body_final().map_err(Error::Phase)?;
        let until = self.until;
        Ok(BodyPlan {
            body: self,
            final_plan: FinalPlan {
                possible,
                reply,
                work,
                until,
            },
        })
    }
    pub fn attempt(&self) -> AttemptId {
        self.body.attempt()
    }
    pub fn outcome(&self) -> &phases::Outcome {
        &self.outcome
    }
    /// Charge one fresh live check without extending the original job limit.
    /// No connection slot or database lock remains held after return.
    pub fn check_current(&self, meter: &mut Meter) -> Result<Instant, Error> {
        self.worker
            .check_current_phase(&self.body, self.limit, meter, LivePhase::Body)
    }
}

/// Exact Body custody paired with its own final-result capacity. The caller
/// still owns matching immutable source, actual 354 and body completion.
pub struct BodyPlan<'w, 'a, 'r> {
    body: BodyResult<'w, 'a, 'r>,
    final_plan: FinalPlan<'w, 'a, 'r>,
}
impl<'w, 'a, 'r> BodyPlan<'w, 'a, 'r> {
    pub(crate) fn reserve_checks(
        self,
        meter: &mut Meter,
    ) -> Result<BodyTransferPlan<'w, 'a, 'r>, Error> {
        let worker = self.body.worker;
        let pending = worker
            .pending
            .operation
            .try_lock()
            .map_err(|_| ports::Error::Busy)?;
        if pending.is_some() {
            return Err(ports::Error::Busy.into());
        }
        if Instant::now() >= self.work_limit() {
            return Err(ports::Error::Deadline.into());
        }
        let turns = u64::try_from(BODY_CHECK_TURNS).map_err(|_| ports::Error::Capacity)?;
        let limit = worker
            .connection
            .admit_work(self.body.until, meter, BODY_CHECK_IO * turns, 4096 * turns)?
            .min(self.work_limit());
        let permit = worker
            .connection
            .slots
            .acquire()
            .map_err(|_| ports::Error::Busy)?;
        let checks = phases::BodyChecks::transfer(
            worker.connection.request,
            worker.connection.trust,
            limit,
            &self.body.body,
            worker.connection.clock.clone(),
            worker.guard.clone(),
        )
        .map_err(Error::Phase)?;
        Ok(BodyTransferPlan {
            checks,
            permit,
            plan: self,
            remaining: BODY_CHECK_TURNS,
            limit,
        })
    }
    pub(crate) fn runtime_clock(&self) -> Arc<dyn Clock> {
        self.body.worker.connection.clock.clone()
    }
    pub(crate) fn original_deadline(&self) -> Deadline {
        self.body.until
    }
    pub(crate) fn work_limit(&self) -> Instant {
        self.final_plan.work.limit
    }

    pub fn attempt(&self) -> AttemptId {
        self.body.attempt()
    }
    pub fn check_current(&self, meter: &mut Meter) -> Result<Instant, Error> {
        self.body.worker.check_current_phase(
            &self.body.body,
            self.final_plan.work.limit,
            meter,
            LivePhase::Body,
        )
    }
    /// The trusted caller invokes this only after transferring the matching
    /// body. Unknown COMMIT retains Pending and supplies no final owner.
    pub fn publish_possible(self, meter: &mut Meter) -> Result<FinalResult<'w, 'a, 'r>, Error> {
        self.final_plan.publish_possible(meter)
    }
}

// Field order closes the connection before releasing its ordinary permit.
// Final-result capacity remains separate inside the exact Body plan.
pub(crate) struct BodyTransferPlan<'w, 'a, 'r> {
    checks: phases::BodyChecks,
    permit: ReadPermit<'w>,
    plan: BodyPlan<'w, 'a, 'r>,
    remaining: usize,
    limit: Instant,
}
impl<'w, 'a, 'r> BodyTransferPlan<'w, 'a, 'r> {
    pub(crate) fn runtime_clock(&self) -> Arc<dyn Clock> {
        self.plan.runtime_clock()
    }
    pub(crate) fn original_deadline(&self) -> Deadline {
        self.plan.original_deadline()
    }
    pub(crate) fn work_limit(&self) -> Instant {
        self.limit
    }
    pub(crate) fn check_current(&mut self, meter: &mut Meter) -> Result<Instant, Error> {
        let worker = self.plan.body.worker;
        let pending = worker
            .pending
            .operation
            .try_lock()
            .map_err(|_| ports::Error::Busy)?;
        if pending.is_some() {
            return Err(ports::Error::Busy.into());
        }
        if Instant::now() >= self.limit {
            return Err(ports::Error::Deadline.into());
        }
        self.remaining = self
            .remaining
            .checked_sub(1)
            .ok_or(ports::Error::Capacity)?;
        self.limit = self.limit.min(worker.connection.admit_work(
            self.plan.body.until,
            meter,
            0,
            0,
        )?);
        self.limit = self
            .limit
            .min(self.checks.check_until(self.limit).map_err(Error::Phase)?);
        if Instant::now() >= self.limit {
            return Err(ports::Error::Deadline.into());
        }
        Ok(self.limit)
    }
    pub(crate) fn publish_possible(
        self,
        meter: &mut Meter,
    ) -> Result<FinalResult<'w, 'a, 'r>, Error> {
        let Self {
            checks,
            permit,
            mut plan,
            limit,
            ..
        } = self;
        drop(checks);
        drop(permit);
        plan.final_plan.work.limit = plan.final_plan.work.limit.min(limit);
        plan.publish_possible(meter)
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
            &worker.guard,
            &mut pending,
            self.possible.clone(),
            limit,
            Some(LivePhase::Possible),
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
impl<'w, 'a, 'r> FinalResult<'w, 'a, 'r> {
    pub(crate) fn runtime_clock(&self) -> Arc<dyn Clock> {
        self.work.worker.connection.clock.clone()
    }
    pub(crate) fn work_limit(&self) -> Instant {
        self.work.limit
    }
    pub fn attempt(&self) -> AttemptId {
        self.possible.attempt()
    }
    pub fn outcome(&self) -> &phases::Outcome {
        &self.outcome
    }
    /// Recheck the exact live phase under current policy, owner and queue clock.
    /// The returned local cap is passive and never extends the reservation.
    pub fn check_current(&self, meter: &mut Meter) -> Result<Instant, Error> {
        self.work.worker.check_current_phase(
            &self.possible,
            self.work.limit,
            meter,
            LivePhase::Possible,
        )
    }
    /// Consume retained capacity for the actual normalized final DATA reply.
    /// No caller-supplied Change can replace the original attempt or subset.
    pub fn record(self, reply: &str) -> Result<phases::Outcome, FinalFailure<'w, 'a, 'r>> {
        let value = self.reply.finish(reply).map_err(|error| FinalFailure {
            error: Error::Phase(error),
            retained: None,
        })?;
        RetainedFinal {
            work: self.work,
            value,
        }
        .publish()
    }
}

/// A failed final publication retains genuine reply evidence and its reserved
/// slot. Dropping it abandons local custody, not the durable capacity claim.
pub struct FinalFailure<'w, 'a, 'r> {
    pub error: Error,
    retained: Option<RetainedFinal<'w, 'a, 'r>>,
}
impl std::fmt::Debug for FinalFailure<'_, '_, '_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FinalFailure")
            .field("error", &self.error)
            .finish_non_exhaustive()
    }
}
struct RetainedFinal<'w, 'a, 'r> {
    work: FinalWork<'w, 'a, 'r>,
    value: Change,
}
impl<'w, 'a, 'r> RetainedFinal<'w, 'a, 'r> {
    fn publish(self) -> Result<phases::Outcome, FinalFailure<'w, 'a, 'r>> {
        match self.work.publish_retained(self.value.clone()) {
            Ok(outcome) => Ok(outcome),
            Err(error) => Err(FinalFailure {
                error,
                retained: Some(self),
            }),
        }
    }
}
impl<'w, 'a, 'r> FinalFailure<'w, 'a, 'r> {
    /// After closing transport, move exact final evidence into fresh passive
    /// settlement. Release the local slot, not the durable final capacity claim;
    /// unresolved COMMIT remains in the same worker's Pending owner.
    pub fn into_evidence(self) -> Option<Change> {
        self.retained.map(|retained| retained.value)
    }
    pub fn attempt(&self) -> Option<AttemptId> {
        self.retained.as_ref().map(|value| value.value.attempt())
    }
    /// Explicitly prepay another publication attempt using the same held slot.
    /// Admission failures retain custody; the original deadline cannot renew.
    /// Unknown COMMIT must first settle through the worker's Pending owner.
    pub fn retry(mut self, meter: &mut Meter) -> Result<phases::Outcome, Self> {
        let Some(mut retained) = self.retained.take() else {
            return Err(self);
        };
        let admission = (|| {
            if Instant::now() >= retained.work.limit {
                return Err(ports::Error::Deadline.into());
            }
            let until = Deadline::after(ports::Tick(0), meter.deadline().tick().0)?;
            retained
                .work
                .worker
                .connection
                .admit(until, meter, PHASE_IO)
        })();
        match admission {
            Ok(limit) => retained.work.limit = retained.work.limit.min(limit),
            Err(error) => {
                self.error = error;
                self.retained = Some(retained);
                return Err(self);
            }
        }
        retained.publish()
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
        self.publish_retained(value)
    }
    fn publish_retained(&self, value: Change) -> Result<phases::Outcome, Error> {
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
            &self.worker.guard,
            &mut pending,
            value,
            self.limit,
            None,
        )
    }
}
fn publish_phase(
    connection: &Connection<'_, '_>,
    guard: &Arc<Mutex<Guard>>,
    pending: &mut Option<Operation>,
    value: Change,
    limit: Instant,
    new_phase: Option<LivePhase>,
) -> Result<phases::Outcome, Error> {
    *pending = Some(Operation::Phase(value));
    let Some(Operation::Phase(value)) = pending.as_ref() else {
        return Err(ports::Error::Invalid.into());
    };
    let result = match new_phase {
        Some(phase) => {
            let publish = match phase {
                LivePhase::Body => phases::publish_new_body_observed,
                LivePhase::Possible => phases::publish_new_possible_observed,
            };
            publish(
                connection.request,
                connection.trust,
                limit,
                value,
                connection.clock.clone(),
                guard.clone(),
            )
        }
        None => phases::publish(connection.request, connection.trust, limit, value),
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

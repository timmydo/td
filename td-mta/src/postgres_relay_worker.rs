//! One foreground route worker; SMTP custody never crosses scheduling turns.
use crate::{
    admission::work::{Charge, Meter},
    clock::RuntimeClock,
    ids::BootId,
    ports::{self, Clock, Deadline, Time},
    postgres_attempt_phases::Change,
    postgres_attempt_recovery as recovery,
    postgres_dispatch::{self as dispatch, Connection, Dispatches, Pending},
    postgres_probe::Request,
    postgres_queue_candidates::Kind,
    postgres_read::ReadSlots,
    postgres_relay_attempt as attempt, postgres_relay_connect as connect,
    postgres_relay_data::Retained,
    postgres_route_owners as owners,
    postgres_runtime_profile::PublishedProfile,
    queue_clock::Guard,
    smtp_receiving::Control,
    tls_admission::HandshakePool,
    tls_io::TLS_WIRE_BYTES,
};
use std::{
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};
use td_crypto::{Entropy, SystemEntropy, TrustStore};

mod multi;

type Buffers = (Box<[u8; TLS_WIRE_BYTES]>, Box<[u8; TLS_WIRE_BYTES]>);
#[derive(Debug)]
pub enum Error {
    Admission(ports::Error),
    Database,
    Custody,
    Worker,
}
impl From<ports::Error> for Error {
    fn from(e: ports::Error) -> Self {
        Self::Admission(e)
    }
}
fn database<T>(value: Result<T, impl std::fmt::Debug>) -> Result<T, Error> {
    value.map_err(|_| Error::Database)
}
fn until(clock: &RuntimeClock, ms: u64) -> Result<Deadline, Error> {
    Ok(Deadline::after(clock.sample()?.monotonic, ms)?)
}
fn meter(until: Deadline) -> Meter {
    Meter::new(
        until,
        Charge {
            io_bytes: 1024 * 1024 * 1024,
            output_bytes: 1024 * 1024 * 1024,
            records: 2 * 1024 * 1024,
            ..Default::default()
        },
    )
}
/// Between empty retention passes for one account.
const RETENTION_PAUSE: Duration = Duration::from_secs(600);
/// The observation's database UTC when retention is due and the queue clock
/// is stable; decided once, so the idle reader closes exactly when it runs.
fn retention_due(
    state: &multi::AccountState,
    status: crate::queue_clock::Status,
    observed_at: i64,
) -> Option<i64> {
    (status.retry && Instant::now() >= state.retention_at).then_some(observed_at)
}
fn pause(control: &Control, duration: Duration) {
    let end = Instant::now() + duration;
    while !control.stop_requested() && Instant::now() < end {
        std::thread::sleep(Duration::from_millis(50));
    }
}
/// The database address remains pinned for this generation. A reload or restart
/// selects a different primary, and starts a new five-minute retry clock guard.
#[derive(Default)]
pub struct QueueControl {
    confirm: AtomicBool,
    confirmed: AtomicBool,
    repair: AtomicU64,
}
impl QueueControl {
    pub fn request_confirmation(&self) {
        self.confirm.store(true, Ordering::Release);
    }
    pub fn request_repair(&self) {
        let mut current = self.repair.load(Ordering::Acquire);
        for _ in 0..32 {
            if !matches!(current & 3, 0 | 3) {
                return;
            }
            let Some(next) = (current & !3).checked_add(4).map(|next| next | 1) else {
                return;
            };
            match self.repair.compare_exchange_weak(
                current,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return,
                Err(actual) => current = actual,
            }
        }
    }
    fn repair_request(&self) -> Option<u64> {
        let current = self.repair.load(Ordering::Acquire);
        (current & 3 == 1).then_some(current)
    }
    fn finish_repair(&self, request: u64, success: bool) {
        let _ = self.repair.compare_exchange(
            request,
            (request & !3) | if success { 2 } else { 3 },
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }
    pub fn repair_status(&self) -> &'static str {
        match self.repair.load(Ordering::Acquire) & 3 {
            1 => "requested",
            2 => "repaired",
            3 => "refused",
            _ => "idle",
        }
    }
    pub fn expiry_enabled(&self) -> bool {
        self.confirmed.load(Ordering::Acquire)
    }
}
pub struct Worker<'a, 'r> {
    pub queue: &'a QueueControl,
    pub request: &'a Request<'r>,
    pub trust: &'a TrustStore,
    pub profile: &'a PublishedProfile,
    pub clock: Arc<RuntimeClock>,
    pub handshakes: &'a HandshakePool,
    pub maximum: u64,
}
impl Worker<'_, '_> {
    pub fn run(&self, control: &Control, ready: &AtomicBool) -> Result<(), Error> {
        multi::run(self, control, ready)
    }
    #[allow(clippy::too_many_arguments)]
    fn schedule_turn(
        &self,
        worker: &Dispatches<'_, '_>,
        settlement: &ReadSlots,
        source_slots: &ReadSlots,
        claim: &owners::Claim,
        generation: u64,
        control: &Control,
        state: &mut multi::AccountState,
        idle_reader: &mut crate::postgres_queue_candidates::IdleReader<'_, '_>,
        dns_cache: &mut crate::dns_owner::Cache,
        mut expiry_turn: Option<&mut multi::ExpiryTurn>,
        mut buffers: Buffers,
    ) -> Result<Buffers, Error> {
        state.empty = false;
        state.active = database(worker.pending.identity())?.is_some();
        if state.active || self.queue.repair_request().is_some() {
            idle_reader.close();
        }
        settle(worker, settlement, control)?;
        if !state.active && self.queue.repair_request().is_none() {
            let observed = match multi::idle_poll_result(
                idle_reader.observe(Instant::now() + Duration::from_secs(10), claim, generation),
                expiry_turn.as_deref_mut(),
            ) {
                Ok(multi::IdlePoll::Observed(observed)) => observed,
                Ok(multi::IdlePoll::Deferred) => {
                    control.log(crate::observability::Kind::RelayPollDeferred, None, None);
                    return Ok(buffers);
                }
                Err(_) => {
                    control.log(
                        crate::observability::Kind::RelayStepFailed {
                            stage: crate::observability::RelayStep::IdlePoll,
                        },
                        None,
                        None,
                    );
                    return Err(Error::Database);
                }
            };
            if let Some(observed_at) = observed {
                state.empty = true;
                let status = self.observe_queue_clock(worker, observed_at)?;
                // Quiet accounts reclaim too; the reader stays open otherwise.
                let due = retention_due(state, status, observed_at);
                if due.is_some() {
                    idle_reader.close();
                }
                self.retain(worker, claim, generation, control, state, due)?;
                if let Some(turn) = expiry_turn {
                    turn.advance();
                }
                state.retry = !state.retry;
                return Ok(buffers);
            }
        }
        // Active work must recover the foreground slot before acquiring custody.
        idle_reader.close();
        if Instant::now() >= state.notice_at {
            let _slot = database(worker.connection.slots.acquire())?;
            let notice = crate::postgres_failure_notice::publish_next(
                self.request,
                self.trust,
                Instant::now() + Duration::from_secs(5),
                claim,
                generation,
            );
            state.active |= notice.as_ref().is_ok_and(|published| *published);
            if let Err(error) = notice {
                state.active = true;
                control.log(
                    crate::observability::Kind::DeliveryNoticeDeferred { error },
                    None,
                    None,
                );
                // Serialize behind COMMIT or rollback before short-lock reads.
                database(owners::renew(
                    self.request,
                    self.trust,
                    Instant::now() + Duration::from_secs(10),
                    claim,
                    generation,
                ))?;
            }
            state.notice_at = Instant::now() + Duration::from_secs(5);
        }
        if let Some(request) = self.queue.repair_request().filter(|_| !state.repaired) {
            state.active = true;
            let needed = database(crate::postgres_route_repair::needed(
                self.request,
                self.trust,
                Instant::now() + Duration::from_secs(10),
                claim,
                generation,
            ))?;
            let value = database(crate::postgres_route_repair::prepare(claim, generation))?;
            let repaired = !needed
                || health(
                    control,
                    || {
                        crate::postgres_route_repair::publish(
                            self.request,
                            self.trust,
                            Instant::now() + Duration::from_secs(10),
                            &value,
                        )
                    },
                    || {
                        crate::postgres_route_repair::inspect(
                            self.request,
                            self.trust,
                            Instant::now() + Duration::from_secs(10),
                            &value,
                        )
                    },
                )?;
            if repaired {
                state.repaired = true;
            } else {
                self.queue.finish_repair(request, false);
            }
        }
        let cap = until(&self.clock, 10_000)?;
        let expiry = database(worker.candidates(
            claim,
            generation,
            Kind::Expiry,
            None,
            cap,
            &mut meter(cap),
        ))?;
        state.active |= !expiry.candidates.is_empty();
        let status = self.observe_queue_clock(worker, expiry.observed_at)?;
        let due = retention_due(state, status, expiry.observed_at);
        self.retain(worker, claim, generation, control, state, due)?;
        if let Some(turn) = expiry_turn {
            let mut limited = false;
            if let Some(candidate) = expiry.candidates.first().filter(|_| status.expiry) {
                let value = database(crate::postgres_queue_expiry::prepare(
                    claim,
                    generation,
                    candidate.submission,
                ))?;
                match worker.expire(value, cap, &mut meter(cap)) {
                    Ok(_) => (),
                    Err(dispatch::Error::Expiry(
                        crate::postgres_queue_expiry::Error::Clock(
                            crate::queue_clock::Error::RateLimited,
                        )
                        | crate::postgres_queue_expiry::Error::RateLimited,
                    )) => limited = true,
                    Err(dispatch::Error::Expiry(crate::postgres_queue_expiry::Error::Clock(_))) => {
                    }
                    Err(_) => settle(worker, settlement, control)?,
                }
            }
            if !limited {
                turn.advance();
            }
        }
        // Includes a Prepared commit whose response/deadline was lost locally.
        if let Some((id, prior)) = database(recovery::active(
            self.request,
            self.trust,
            Instant::now() + Duration::from_secs(10),
            claim,
            generation,
        ))? {
            state.active = true;
            let value = database(if prior == generation {
                recovery::interrupted(claim, generation, id)
            } else {
                recovery::prepare(claim, generation, id)
            })?;
            recover(worker, settlement, control, value)?;
            return Ok(buffers);
        }
        let cap = until(&self.clock, 10_000)?;
        let page = match worker.candidates(
            claim,
            generation,
            if state.retry {
                Kind::Retry
            } else {
                Kind::Fresh
            },
            None,
            cap,
            &mut meter(cap),
        ) {
            Ok(page) => page,
            Err(dispatch::Error::Candidates(
                crate::postgres_queue_candidates::Error::RouteHealth(_),
            )) => {
                return Ok(buffers);
            }
            Err(_) => return Err(Error::Database),
        };
        state.active |= !page.candidates.is_empty();
        let status = database(worker.guard.lock().map_err(|_| Error::Custody))?
            .observe(Time {
                utc_ms: page.observed_at,
                monotonic: self.clock.sample()?.monotonic,
            })
            .map_err(|_| Error::Custody)?;
        let allowed = if state.retry {
            status.retry
        } else {
            status.fresh_dispatch
        };
        if let Some(candidate) = page.candidates.first().filter(|_| allowed) {
            let cap = until(&self.clock, 120_000)?;
            let mut db = meter(cap);
            match worker.publish_prepared(
                claim,
                generation,
                dispatch::Input {
                    submission: candidate.submission,
                    retry: state.retry,
                    deadline: cap,
                },
                &mut db,
            ) {
                Ok(prepared) => {
                    buffers = self.deliver(
                        prepared,
                        worker,
                        settlement,
                        source_slots,
                        control,
                        buffers,
                        cap,
                        &mut db,
                        dns_cache,
                        claim,
                        generation,
                    )?;
                }
                Err(_) => {
                    settle(worker, settlement, control)?;
                }
            }
        }
        state.retry = !state.retry;
        Ok(buffers)
    }
    /// Retire settled submissions (POSTGRES.md section 216) when
    /// `retention_due` returned the stable observation. Failure is logged and
    /// retried later, as retention only frees capacity; the caller has
    /// closed its idle reader.
    fn retain(
        &self,
        worker: &Dispatches<'_, '_>,
        claim: &owners::Claim,
        generation: u64,
        control: &Control,
        state: &mut multi::AccountState,
        stable_at: Option<i64>,
    ) -> Result<(), Error> {
        let Some(observed_at) = stable_at else {
            return Ok(());
        };
        let retired = match worker.connection.slots.acquire() {
            Ok(_slot) => crate::postgres_submission_retention::retire(
                self.request,
                self.trust,
                Instant::now() + Duration::from_secs(10),
                claim,
                generation,
                observed_at,
            )
            .ok(),
            Err(_) => None,
        };
        let wait = match retired {
            // A full batch may leave more for the next turn.
            Some(count) if count > 0 => Duration::ZERO,
            Some(_) => RETENTION_PAUSE,
            None => {
                control.log(
                    crate::observability::Kind::RelayStepFailed {
                        stage: crate::observability::RelayStep::Retention,
                    },
                    None,
                    None,
                );
                // Serialize behind COMMIT or rollback before short-lock reads.
                database(owners::renew(
                    self.request,
                    self.trust,
                    Instant::now() + Duration::from_secs(10),
                    claim,
                    generation,
                ))?;
                RETENTION_PAUSE
            }
        };
        state.retention_at = Instant::now() + wait;
        Ok(())
    }
    fn observe_queue_clock(
        &self,
        worker: &Dispatches<'_, '_>,
        observed_at: i64,
    ) -> Result<crate::queue_clock::Status, Error> {
        let sample = Time {
            utc_ms: observed_at,
            monotonic: self.clock.sample()?.monotonic,
        };
        let status = {
            let mut guard = worker.guard.lock().map_err(|_| Error::Custody)?;
            if self.queue.confirm.swap(false, Ordering::AcqRel) {
                let _ = guard.confirm(sample);
            }
            guard.observe(sample).map_err(|_| Error::Custody)?
        };
        self.queue.confirmed.store(status.expiry, Ordering::Release);
        Ok(status)
    }
    fn success(
        &self,
        claim: &owners::Claim,
        generation: u64,
        id: crate::ids::AttemptId,
        control: &Control,
    ) -> Result<(), Error> {
        let value = database(crate::postgres_route_success::prepare(
            claim, generation, id,
        ))?;
        health(
            control,
            || {
                crate::postgres_route_success::publish(
                    self.request,
                    self.trust,
                    Instant::now() + Duration::from_secs(10),
                    &value,
                )
            },
            || {
                crate::postgres_route_success::inspect(
                    self.request,
                    self.trust,
                    Instant::now() + Duration::from_secs(10),
                    &value,
                )
            },
        )?;
        Ok(())
    }
    #[allow(clippy::too_many_arguments)]
    fn deliver(
        &self,
        prepared: dispatch::PreparedResult<'_, '_, '_>,
        worker: &Dispatches<'_, '_>,
        settlement: &ReadSlots,
        source_slots: &ReadSlots,
        control: &Control,
        buffers: Buffers,
        cap: Deadline,
        db: &mut Meter,
        dns_cache: &mut crate::dns_owner::Cache,
        claim: &owners::Claim,
        generation: u64,
    ) -> Result<Buffers, Error> {
        let id = prepared.attempt();
        let submission = prepared.outcome().submission;
        let mode = database(prepared.body_mode(self.maximum, db))?;
        let mut wire = meter(cap);
        let (mut input, mut first, mut second, mut third, mut command) =
            ([0; 512], [0; 512], [0; 512], [0; 512], [0; 512]);
        let mut plain = [0; crate::smtp_relay_auth::PLAIN_BYTES];
        let mut encoded = [0; crate::smtp_relay_auth::RESPONSE_BYTES];
        let mut dns = [0; crate::dns_transport::PACKET_BYTES];
        let connected = connect::connect(
            prepared,
            self.profile,
            self.handshakes,
            buffers.0,
            buffers.1,
            connect::Scratch {
                input: &mut input,
                first: &mut first,
                second: &mut second,
                third: &mut third,
                command: &mut command,
                plain: &mut plain,
                encoded: &mut encoded,
                dns: &mut dns,
                dns_cache,
            },
            database(SystemEntropy::try_new())?,
            &mut wire,
            db,
        );
        let connected = match connected {
            Ok(value) => value,
            Err(failure) => {
                control.log(
                    crate::observability::Kind::RelayConnectionFailed {
                        stage: failure.stage,
                        error: match failure.error {
                            connect::Error::Protocol(error) => Some(error),
                            connect::Error::Publication(_) => None,
                        },
                    },
                    Some(submission),
                    None,
                );
                let buffers = failure.buffers?;
                match failure.closed {
                    connect::Closed::Interrupted(closed) => recover(
                        worker,
                        settlement,
                        control,
                        closed.prepare().map_err(|_| Error::Custody)?,
                    )?,
                    connect::Closed::Auth(mut failed) => loop {
                        let cap = until(&self.clock, 10_000)?;
                        match failed.step(cap, &mut meter(cap), settlement) {
                            Ok(Some(_)) => break,
                            _ if control.stop_requested() => return Err(Error::Custody),
                            _ => pause(control, Duration::from_secs(1)),
                        }
                    },
                }
                return Ok(buffers);
            }
        };
        let (mut line, mut reply, mut body_input, mut body_output) =
            ([0; 512], [0; 4096], [0; 4096], [0; 16384]);
        let mut source = meter(cap);
        match attempt::drive(
            connected,
            mode,
            attempt::Scratch {
                input: &mut input,
                line: &mut line,
                reply: &mut reply,
                command: &mut command,
                body_input: &mut body_input,
                body_output: &mut body_output,
            },
            &mut wire,
            db,
            &mut source,
            source_slots,
            self.maximum,
        ) {
            Ok(done) => {
                if done.stage == attempt::Stage::Final {
                    self.success(claim, generation, id, control)?;
                }
                Ok(done.buffers?)
            }
            Err(stopped) => {
                use crate::observability::{Kind, RelayStep};
                let stage = match &stopped.evidence {
                    attempt::Evidence::Mail(_) => RelayStep::Mail,
                    attempt::Evidence::Rcpt { .. } => RelayStep::Rcpt,
                    attempt::Evidence::Data(_) => RelayStep::Data,
                    attempt::Evidence::Body(_) => RelayStep::Body,
                    attempt::Evidence::Source(_) => RelayStep::Source,
                    attempt::Evidence::Final(_) => RelayStep::Final,
                };
                control.log(Kind::RelayStepFailed { stage }, Some(submission), None);
                let fallback = stopped.interruption;
                let (buffers, changes, recovery) = match stopped.evidence {
                    attempt::Evidence::Mail(f) => (
                        f.buffers,
                        [None, f.retained.and_then(|f| f.into_evidence())],
                        None,
                    ),
                    attempt::Evidence::Final(f) => (
                        f.buffers,
                        [None, f.retained.and_then(|f| f.into_evidence())],
                        None,
                    ),
                    attempt::Evidence::Rcpt { closed, .. } => {
                        let evidence = closed
                            .evidence
                            .map_err(|_| Error::Custody)?
                            .into_interruption()
                            .map_err(|_| Error::Custody)?;
                        (
                            closed.buffers,
                            [None, evidence.refusal],
                            Some(evidence.recovery),
                        )
                    }
                    attempt::Evidence::Data(f) => match f.retained {
                        Retained::Command(value) => {
                            let evidence = value.into_evidence().ok_or(Error::Custody)?;
                            (
                                f.buffers,
                                [evidence.refusal, Some(evidence.command)],
                                Some(None),
                            )
                        }
                        Retained::Planning(value) => {
                            let evidence = value
                                .into_retained()
                                .into_interruption()
                                .map_err(|_| Error::Custody)?;
                            (f.buffers, [None, evidence.refusal], Some(evidence.recovery))
                        }
                        Retained::Body(value) => {
                            let evidence = value.into_interruption().map_err(|_| Error::Custody)?;
                            (f.buffers, [None, evidence.refusal], Some(evidence.recovery))
                        }
                    },
                    attempt::Evidence::Body(f) => (f.buffers, [None, None], None),
                    attempt::Evidence::Source(f) => (f.buffers, [None, None], None),
                };
                let terminal = changes.iter().any(Option::is_some) && recovery.is_none();
                for change in changes.into_iter().flatten() {
                    phase(worker, settlement, control, change)?;
                }
                settle(worker, settlement, control)?;
                let generic = recovery.is_none();
                if let Some(value) = recovery.flatten() {
                    recover(worker, settlement, control, value)?;
                } else if !terminal && generic {
                    recover(
                        worker,
                        settlement,
                        control,
                        fallback.prepare().map_err(|_| Error::Custody)?,
                    )?;
                }
                self.success(claim, generation, id, control)?;
                Ok(buffers?)
            }
        }
    }
}
fn settle(worker: &Dispatches<'_, '_>, slots: &ReadSlots, control: &Control) -> Result<(), Error> {
    let connection = Connection {
        slots,
        ..Connection {
            request: worker.connection.request,
            trust: worker.connection.trust,
            slots: worker.connection.slots,
            clock: worker.connection.clock.clone(),
        }
    };
    while database(worker.pending.identity())?.is_some() {
        let cap = until(&worker.connection.clock, 10_000)?;
        // A found receipt or proven absence both release custody.
        if worker
            .pending
            .settle(&connection, cap, &mut meter(cap))
            .is_ok()
        {
            continue;
        }
        if control.stop_requested() {
            return Err(Error::Custody);
        }
        pause(control, Duration::from_secs(1));
    }
    Ok(())
}
fn phase(
    worker: &Dispatches<'_, '_>,
    slots: &ReadSlots,
    control: &Control,
    value: Change,
) -> Result<(), Error> {
    loop {
        settle(worker, slots, control)?;
        let cap = until(&worker.connection.clock, 10_000)?;
        match worker.publish_phase(value.clone(), cap, &mut meter(cap)) {
            Ok(_) | Err(dispatch::Error::Phase(crate::postgres_attempt_phases::Error::Missing)) => {
                return Ok(())
            }
            _ => (),
        }
        if control.stop_requested() {
            return Err(Error::Custody);
        }
        pause(control, Duration::from_secs(1));
    }
}
fn recover(
    worker: &Dispatches<'_, '_>,
    slots: &ReadSlots,
    control: &Control,
    value: recovery::Recovery,
) -> Result<(), Error> {
    loop {
        settle(worker, slots, control)?;
        let cap = until(&worker.connection.clock, 10_000)?;
        match worker.publish_recovery(value.clone(), cap, &mut meter(cap)) {
            Ok(_) | Err(dispatch::Error::Recovery(recovery::Error::Missing)) => return Ok(()),
            _ if control.stop_requested() => return Err(Error::Custody),
            _ => pause(control, Duration::from_secs(1)),
        }
    }
}

fn health(
    control: &Control,
    mut publish: impl FnMut() -> Result<
        crate::postgres_route_health::Outcome,
        crate::postgres_route_health::Error,
    >,
    mut inspect: impl FnMut() -> Result<
        Option<crate::postgres_route_health::Outcome>,
        crate::postgres_route_health::Error,
    >,
) -> Result<bool, Error> {
    use crate::postgres_route_health::Error as E;
    let mut unknown = false;
    loop {
        let result = if unknown {
            inspect()
        } else {
            publish().map(Some)
        };
        match result {
            Ok(Some(_)) => return Ok(true),
            Err(E::Conflict | E::Missing | E::RepairRequired) if !unknown => return Ok(false),
            Err(E::Indeterminate) => unknown = true,
            // Proven absent under the account share lock (POSTGRES.md
            // section 165): republish the same change.
            Ok(None) => unknown = false,
            // A lock timeout, as while an uncertain COMMIT still runs, is
            // transient like a lost connection. The route owner row's lock
            // timeout surfaces as the owner's Database error, and a schema
            // check's statement failure as its schema Database error. A
            // permanent one also fails lease renewal, which stops the relay.
            Err(
                E::Connection(_)
                | E::Database
                | E::Owner(
                    crate::postgres_route_owners::Error::Busy
                    | crate::postgres_route_owners::Error::Database
                    | crate::postgres_route_owners::Error::Schema(
                        crate::postgres_schema::Error::Database,
                    ),
                ),
            ) => (),
            Err(_) => return Err(Error::Database),
        }
        if control.stop_requested() {
            return Err(Error::Custody);
        }
        pause(control, Duration::from_secs(1));
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    #[test]
    fn health_republishes_after_proven_absence() {
        use crate::postgres_route_health::{Error as E, Outcome, State};
        let control = Control::default();
        let outcome = Outcome {
            old_revision: 0,
            state: State {
                revision: 1,
                failure_count: 0,
                reason: 0,
                failed_at: None,
                next_at: None,
                repair_generation: None,
            },
        };
        let mut publishes = 0;
        let mut inspections = 0;
        let result = health(
            &control,
            || {
                publishes += 1;
                if publishes == 1 {
                    Err(E::Indeterminate)
                } else {
                    Ok(outcome)
                }
            },
            || {
                inspections += 1;
                // Under the old rule this would inspect forever; fail instead.
                if inspections > 2 {
                    control.stop();
                }
                Ok(None)
            },
        );
        assert!(matches!(result, Ok(true)));
        assert_eq!((publishes, inspections), (2, 1));
    }
    #[test]
    fn health_retries_a_lock_timeout_while_the_commit_runs() {
        use crate::postgres_route_health::{Error as E, Outcome, State};
        use crate::postgres_route_owners::Error as Owner;
        let control = Control::default();
        let outcome = Outcome {
            old_revision: 0,
            state: State {
                revision: 1,
                failure_count: 0,
                reason: 0,
                failed_at: None,
                next_at: None,
                repair_generation: None,
            },
        };
        let mut inspections = 0;
        let result = health(
            &control,
            || Err(E::Indeterminate),
            || {
                inspections += 1;
                if inspections == 1 {
                    Err(E::Owner(Owner::Busy))
                } else {
                    Ok(Some(outcome))
                }
            },
        );
        assert!(matches!(result, Ok(true)));
        assert_eq!(inspections, 2);
    }
    #[test]
    fn health_retries_a_publication_lock_timeout() {
        use crate::postgres_route_health::{Error as E, Outcome, State};
        use crate::postgres_route_owners::Error as Owner;
        let control = Control::default();
        let outcome = Outcome {
            old_revision: 0,
            state: State {
                revision: 1,
                failure_count: 0,
                reason: 0,
                failed_at: None,
                next_at: None,
                repair_generation: None,
            },
        };
        // The account row's lock timeout, the route owner row's (reported as
        // the owner's Database error) and a schema check's statement failure
        // each retry instead of stopping the relay.
        let transient = [
            Owner::Busy,
            Owner::Database,
            Owner::Schema(crate::postgres_schema::Error::Database),
        ];
        for error in transient {
            let mut publishes = 0;
            let mut inspections = 0;
            let result = health(
                &control,
                || {
                    publishes += 1;
                    if publishes == 1 {
                        Err(E::Owner(error))
                    } else {
                        Ok(outcome)
                    }
                },
                || {
                    inspections += 1;
                    Ok(None)
                },
            );
            assert!(matches!(result, Ok(true)), "{error:?}");
            // A lock timeout before any write is not uncertain.
            assert_eq!((publishes, inspections), (2, 0), "{error:?}");
        }
        // A refusal that cannot heal stops at once, without inspection.
        let (mut publishes, mut inspections) = (0, 0);
        let result = health(
            &control,
            || {
                publishes += 1;
                Err(E::Owner(Owner::Integrity))
            },
            || {
                inspections += 1;
                Ok(None)
            },
        );
        assert!(matches!(result, Err(Error::Database)));
        assert_eq!((publishes, inspections), (1, 0));
    }
    #[test]
    fn repair_completion_cannot_finish_a_newer_request() {
        let control = QueueControl::default();
        assert_eq!(control.repair_status(), "idle");
        control.request_repair();
        let old = control.repair_request().unwrap();
        control.finish_repair(old, false);
        assert_eq!(control.repair_status(), "refused");
        control.request_repair();
        let new = control.repair_request().unwrap();
        assert_ne!(old, new);
        control.finish_repair(old, true);
        assert_eq!(control.repair_status(), "requested");
        control.finish_repair(new, true);
        assert_eq!(control.repair_status(), "repaired");
    }
}

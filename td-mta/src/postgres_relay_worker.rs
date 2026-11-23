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
    postgres_submission_retention::Step as RetentionStep,
    queue_clock::{evidence::Journal, ClockControl, RetentionTime},
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
use td_crypto::{Entropy, SystemEntropy};

mod multi;

type Buffers = (Box<[u8; TLS_WIRE_BYTES]>, Box<[u8; TLS_WIRE_BYTES]>);
#[derive(Debug)]
pub enum Error {
    Admission(ports::Error),
    Body(dispatch::Error),
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
const CAPACITY_PAUSE: Duration = Duration::from_secs(5);
fn log_capacity(control: &Control, stage: crate::observability::RelayStep) {
    control.log(
        crate::observability::Kind::RelayStepFailed { stage },
        None,
        None,
    );
}
/// Between empty retention passes for one account.
const RETENTION_PAUSE: Duration = Duration::from_secs(600);
/// Leave a step for the next turn when little of the owner deadline remains.
const RETENTION_STEP_MINIMUM: Duration = Duration::from_secs(1);

struct RetentionCycle {
    next: RetentionStep,
    progress: bool,
}

impl RetentionCycle {
    fn new() -> Self {
        Self {
            next: RetentionStep::Retire,
            progress: false,
        }
    }

    /// Try each independent transaction once per cycle, across as many
    /// turns as the shared deadlines require. Failed steps also advance.
    fn run(
        &mut self,
        mut step: impl FnMut(RetentionStep) -> Option<u64>,
        mut has_time: impl FnMut() -> bool,
    ) -> (Duration, bool) {
        let mut failed = false;
        loop {
            let count = step(self.next);
            failed |= count.is_none();
            self.progress |= count.is_some_and(|count| count > 0);
            self.next = match self.next {
                RetentionStep::Retire => RetentionStep::UploadLeases,
                RetentionStep::UploadLeases => RetentionStep::AbandonedOwners,
                RetentionStep::AbandonedOwners => RetentionStep::IdleAttempts,
                RetentionStep::IdleAttempts => RetentionStep::Retire,
            };
            if self.next == RetentionStep::Retire {
                let wait = if std::mem::take(&mut self.progress) {
                    Duration::ZERO
                } else {
                    RETENTION_PAUSE
                };
                return (wait, failed);
            }
            if !has_time() {
                return (Duration::ZERO, true);
            }
        }
    }
}
/// The observation's database UTC when retention is due and the queue clock
/// is stable; decided once, so the idle reader closes exactly when it runs.
fn retention_due(
    state: &multi::AccountState,
    time: Option<RetentionTime>,
) -> Option<RetentionTime> {
    time.filter(|_| Instant::now() >= state.retention_at)
}
fn pause(control: &Control, duration: Duration) {
    let end = Instant::now() + duration;
    while !control.stop_requested() && Instant::now() < end {
        std::thread::sleep(Duration::from_millis(50));
    }
}
/// The database address remains pinned for this generation. A reload or restart
/// selects a different primary, and starts a new retry clock guard of the
/// worker's minimum retry interval.
#[derive(Default)]
pub struct QueueControl {
    clock: Arc<ClockControl>,
    repair: AtomicU64,
}
impl QueueControl {
    pub fn request_confirmation(&self) -> bool {
        self.clock.request()
    }
    pub fn clock_status(&self) -> &'static str {
        self.clock.status()
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
}
pub struct Worker<'a, 'r> {
    pub queue: &'a QueueControl,
    pub request: &'a Request<'r>,
    pub profile: &'a PublishedProfile,
    pub clock: Arc<RuntimeClock>,
    pub handshakes: &'a HandshakePool,
    pub maximum: u64,
    pub retry_floor: crate::queue_clock::RetryFloor,
    /// Private runtime directory locked by the serving control endpoint.
    pub runtime: &'a str,
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
        journal: &Journal<'_>,
        claim: &owners::Claim,
        generation: u64,
        control: &Control,
        state: &mut multi::AccountState,
        idle_reader: &mut crate::postgres_queue_candidates::IdleReader<'_, '_>,
        dns_cache: &mut crate::dns_owner::Cache,
        mut expiry_turn: Option<&mut multi::ExpiryTurn>,
        mut buffers: Buffers,
    ) -> Result<Buffers, Error> {
        self.queue.clock.start_sample();
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
                let (_, time) = self.observe_queue_clock(worker, journal, observed_at)?;
                // Quiet accounts reclaim too; the reader stays open otherwise.
                let due = retention_due(state, time);
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
                Instant::now() + Duration::from_secs(10),
                claim,
                generation,
            ))?;
            let value = database(crate::postgres_route_repair::prepare(claim, generation))?;
            let repaired = !needed
                || health(
                    control,
                    || match crate::postgres_route_repair::publish(
                        self.request,
                        Instant::now() + Duration::from_secs(10),
                        &value,
                    ) {
                        Err(crate::postgres_route_health::Error::Capacity) => {
                            log_capacity(control, crate::observability::RelayStep::HealthCapacity);
                            Err(crate::postgres_route_health::Error::RepairRequired)
                        }
                        other => other,
                    },
                    || {
                        crate::postgres_route_repair::inspect(
                            self.request,
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
        let (status, time) = self.observe_queue_clock(worker, journal, expiry.observed_at)?;
        let due = retention_due(state, time);
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
        if Instant::now() < state.dispatch_at {
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
                Err(dispatch::Error::Publication(
                    crate::postgres_attempts::Error::RouteHealth(
                        crate::postgres_route_health::Error::Capacity,
                    ),
                )) => {
                    state.dispatch_at = Instant::now() + CAPACITY_PAUSE;
                    log_capacity(control, crate::observability::RelayStep::HealthCapacity);
                }
                Err(_) => {
                    settle(worker, settlement, control)?;
                }
            }
        }
        state.retry = !state.retry;
        Ok(buffers)
    }
    /// Run account upkeep after the idle reader releases its connection.
    fn retain(
        &self,
        worker: &Dispatches<'_, '_>,
        claim: &owners::Claim,
        generation: u64,
        control: &Control,
        state: &mut multi::AccountState,
        stable_at: Option<RetentionTime>,
    ) -> Result<(), Error> {
        let Some(observed_at) = stable_at else {
            return Ok(());
        };
        let deadline = Instant::now() + Duration::from_secs(10);
        let (wait, failed) = match worker.connection.slots.acquire() {
            Ok(_slot) => state.retention.run(
                |step| {
                    step.run(self.request, deadline, claim, generation, observed_at)
                        .ok()
                },
                || deadline.saturating_duration_since(Instant::now()) >= RETENTION_STEP_MINIMUM,
            ),
            Err(_) => (RETENTION_PAUSE, true),
        };
        if failed {
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
                Instant::now() + Duration::from_secs(10),
                claim,
                generation,
            ))?;
        }
        state.retention_at = Instant::now() + wait;
        Ok(())
    }
    fn observe_queue_clock(
        &self,
        worker: &Dispatches<'_, '_>,
        journal: &Journal<'_>,
        observed_at: i64,
    ) -> Result<(crate::queue_clock::Status, Option<RetentionTime>), Error> {
        let sample = Time {
            utc_ms: observed_at,
            monotonic: self.clock.sample()?.monotonic,
        };
        crate::queue_clock::observe_request(&worker.guard, sample, |utc| journal.confirmed(utc))
            .map_err(|_| Error::Custody)
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
                    Instant::now() + Duration::from_secs(10),
                    &value,
                )
            },
            || {
                crate::postgres_route_success::inspect(
                    self.request,
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
        let Some(mode) = prepared.body_mode(self.maximum, db).map_err(Error::Body)? else {
            let value = database(recovery::interrupted(claim, generation, id))?;
            recover(worker, settlement, control, value)?;
            return Ok(buffers);
        };
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
                            // A reclaimed attempt was retired by a committed
                            // recovery; as in recover(), nothing remains.
                            Ok(Some(_))
                            | Err(dispatch::Error::Recovery(recovery::Error::Missing)) => break,
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
    fn early_confirmations_are_reported_and_do_not_release_later_holds() {
        let queue = QueueControl::default();
        let time = |utc_ms, ms| Time {
            utc_ms,
            monotonic: ports::Tick(ms),
        };
        let mut guard = crate::queue_clock::Guard::new(time(0, 0)).unwrap();
        guard.control = Some(Arc::clone(&queue.clock));
        let guard = Mutex::new(guard);
        assert!(!queue.request_confirmation());
        assert_eq!(queue.clock_status(), "refused");
        guard
            .lock()
            .unwrap()
            .observe(time(300_000, 300_000))
            .unwrap();
        assert_eq!(queue.clock_status(), "refused");
        assert!(queue.request_confirmation());
        queue.clock.start_sample();
        crate::queue_clock::observe_request(&guard, time(300_000, 300_000), |_| Ok(())).unwrap();
        assert_eq!(queue.clock_status(), "confirmed");
        assert!(queue.request_confirmation());
        // A separate dispatch observation sees the jump before the next poll.
        guard
            .lock()
            .unwrap()
            .observe(time(900_000, 300_001))
            .unwrap();
        assert!(!queue.request_confirmation());
        crate::queue_clock::observe_request(&guard, time(1_200_000, 600_001), |_| Ok(())).unwrap();
        assert_eq!(queue.clock_status(), "refused");
        assert!(queue.request_confirmation());
        queue.clock.start_sample();
        crate::queue_clock::observe_request(&guard, time(1_200_001, 600_002), |_| Ok(())).unwrap();
        assert_eq!(queue.clock_status(), "confirmed");
    }
    #[test]
    fn a_failed_or_slow_step_never_starves_the_rest_of_the_cycle() {
        use RetentionStep::{AbandonedOwners, IdleAttempts, Retire, UploadLeases};
        let mut cycle = RetentionCycle::new();
        let all = [Retire, UploadLeases, AbandonedOwners, IdleAttempts];
        for slow in all {
            for outcome in [None, Some(0), Some(1)] {
                let mut seen = Vec::new();
                let mut next_wait = Duration::ZERO;
                let mut turns = 0;
                while seen.len() < all.len() {
                    let last = std::cell::Cell::new(None);
                    let (wait, failed) = cycle.run(
                        |step| {
                            seen.push(step);
                            last.set(Some(step));
                            if step == slow {
                                outcome
                            } else {
                                Some(0)
                            }
                        },
                        || last.get() != Some(slow),
                    );
                    if seen.len() < all.len() {
                        assert!(failed);
                        assert!(wait.is_zero());
                    }
                    next_wait = wait;
                    turns += 1;
                }
                assert!(turns <= 2);
                assert_eq!(seen, all);
                assert_eq!(next_wait.is_zero(), outcome == Some(1));
            }
        }
        assert_eq!(
            cycle.run(|step| (step != UploadLeases).then_some(0), || true),
            (RETENTION_PAUSE, true)
        );
    }
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

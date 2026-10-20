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
        atomic::{AtomicBool, AtomicU8, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};
use td_crypto::{Entropy, SystemEntropy, TrustStore};

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
fn pause(control: &Control, duration: Duration) {
    let end = Instant::now() + duration;
    while !control.stop_requested() && Instant::now() < end {
        std::thread::sleep(Duration::from_millis(50));
    }
}
/// The database address remains pinned for this process. Restart to select a
/// different primary, which also starts a new five-minute retry clock guard.
#[derive(Default)]
pub struct QueueControl {
    confirm: AtomicBool,
    confirmed: AtomicBool,
    repair: AtomicU8,
}
impl QueueControl {
    pub fn request_confirmation(&self) {
        self.confirm.store(true, Ordering::Release);
    }
    pub fn request_repair(&self) {
        let _ = self
            .repair
            .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
            .or_else(|_| {
                self.repair
                    .compare_exchange(3, 1, Ordering::AcqRel, Ordering::Acquire)
            });
    }
    pub fn repair_status(&self) -> &'static str {
        match self.repair.load(Ordering::Acquire) {
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
        let mut bytes = [0; 16];
        database(SystemEntropy::try_new())?
            .fill(&mut bytes)
            .map_err(|_| Error::Custody)?;
        let claim = database(owners::prepare(
            self.profile.binding(),
            BootId::from_bytes(bytes),
        ))?;
        // Busy is an ordinary standby, including a previous process's lease.
        ready.store(true, Ordering::Release);
        let grant = loop {
            if control.stop_requested() {
                return Ok(());
            }
            match owners::acquire(
                self.request,
                self.trust,
                Instant::now() + Duration::from_secs(10),
                &claim,
            ) {
                Ok(grant) => break grant,
                Err(owners::Error::Busy | owners::Error::Connection(_)) => {
                    pause(control, Duration::from_secs(1))
                }
                Err(owners::Error::Indeterminate) => {
                    // Reinspection/reacquisition keeps this exact claim identity.
                    pause(control, Duration::from_secs(1));
                }
                Err(_) => return Err(Error::Database),
            }
        };
        let guard = Arc::new(Mutex::new(database(Guard::new(Time {
            utc_ms: grant.claimed_at,
            monotonic: self.clock.sample()?.monotonic,
        }))?));
        let slots = database(ReadSlots::new(2))?;
        let settlement = database(ReadSlots::new(1))?;
        let pending = Pending::default();
        let worker = Dispatches {
            connection: Connection {
                request: self.request,
                trust: self.trust,
                slots: &slots,
                clock: self.clock.clone(),
            },
            guard,
            pending: &pending,
        };
        let done = AtomicBool::new(false);
        let result = std::thread::scope(|scope| {
            let renew = std::thread::Builder::new()
                .name("relay-lease".into())
                .spawn_scoped(scope, || {
                    while !done.load(Ordering::Acquire) {
                        let next = Instant::now() + Duration::from_secs(5);
                        while !done.load(Ordering::Acquire) && Instant::now() < next {
                            std::thread::sleep(Duration::from_millis(50));
                        }
                        if done.load(Ordering::Acquire) {
                            break;
                        }
                        if owners::renew(
                            self.request,
                            self.trust,
                            Instant::now() + Duration::from_secs(10),
                            &claim,
                            grant.generation,
                        )
                        .is_err()
                        {
                            control.stop();
                            return Err(Error::Database);
                        }
                    }
                    Ok(())
                })
                .map_err(|_| Error::Worker)?;
            // Unwinding must also release the scoped renewal worker.
            struct Finish<'a>(&'a AtomicBool);
            impl Drop for Finish<'_> {
                fn drop(&mut self) {
                    self.0.store(true, Ordering::Release);
                }
            }
            let finish = Finish(&done);
            let result = self.schedule(&worker, &settlement, &claim, grant.generation, control);
            drop(finish);
            let renewed = renew.join().map_err(|_| Error::Worker)?;
            result.and(renewed)
        });
        // All transports and pending live owners are closed before release.
        let released = owners::release(
            self.request,
            self.trust,
            Instant::now() + Duration::from_secs(10),
            &claim,
            grant.generation,
        );
        result.and(database(released))
    }
    fn schedule(
        &self,
        worker: &Dispatches<'_, '_>,
        settlement: &ReadSlots,
        claim: &owners::Claim,
        generation: u64,
        control: &Control,
    ) -> Result<(), Error> {
        let mut buffers = (Box::new([0; TLS_WIRE_BYTES]), Box::new([0; TLS_WIRE_BYTES]));
        let source_slots = database(ReadSlots::new(2))?;
        let mut retry = false;
        let mut notice_at = Instant::now();
        while !control.stop_requested() {
            settle(worker, settlement, control)?;
            if Instant::now() >= notice_at {
                let _slot = database(worker.connection.slots.acquire())?;
                if let Err(error) = crate::postgres_failure_notice::publish_next(
                    self.request,
                    self.trust,
                    Instant::now() + Duration::from_secs(5),
                    claim,
                    generation,
                ) {
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
                notice_at = Instant::now() + Duration::from_secs(5);
            }
            if self.queue.repair.load(Ordering::Acquire) == 1 {
                let value = database(crate::postgres_route_repair::prepare(claim, generation))?;
                let repaired = health(
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
                self.queue
                    .repair
                    .store(if repaired { 2 } else { 3 }, Ordering::Release);
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
            let sample = Time {
                utc_ms: expiry.observed_at,
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
            if status.expiry {
                if let Some(candidate) = expiry.candidates.first() {
                    let value = database(crate::postgres_queue_expiry::prepare(
                        claim,
                        generation,
                        candidate.submission,
                    ))?;
                    match worker.expire(value, cap, &mut meter(cap)) {
                        Ok(_) => (),
                        Err(dispatch::Error::Expiry(
                            crate::postgres_queue_expiry::Error::Clock(_)
                            | crate::postgres_queue_expiry::Error::RateLimited,
                        )) => (),
                        Err(_) => settle(worker, settlement, control)?,
                    }
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
                let value = database(if prior == generation {
                    recovery::interrupted(claim, generation, id)
                } else {
                    recovery::prepare(claim, generation, id)
                })?;
                recover(worker, settlement, control, value)?;
                pause(control, Duration::from_secs(1));
                continue;
            }
            let cap = until(&self.clock, 10_000)?;
            let page = match worker.candidates(
                claim,
                generation,
                if retry { Kind::Retry } else { Kind::Fresh },
                None,
                cap,
                &mut meter(cap),
            ) {
                Ok(page) => page,
                Err(dispatch::Error::Candidates(
                    crate::postgres_queue_candidates::Error::RouteHealth(_),
                )) => {
                    pause(control, Duration::from_secs(1));
                    continue;
                }
                Err(_) => return Err(Error::Database),
            };
            let status = database(worker.guard.lock().map_err(|_| Error::Custody))?
                .observe(Time {
                    utc_ms: page.observed_at,
                    monotonic: self.clock.sample()?.monotonic,
                })
                .map_err(|_| Error::Custody)?;
            let allowed = if retry {
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
                        retry,
                        deadline: cap,
                    },
                    &mut db,
                ) {
                    Ok(prepared) => {
                        buffers = self.deliver(
                            prepared,
                            worker,
                            settlement,
                            &source_slots,
                            control,
                            buffers,
                            cap,
                            &mut db,
                            claim,
                            generation,
                        )?;
                    }
                    Err(_) => {
                        settle(worker, settlement, control)?;
                    }
                }
            }
            retry = !retry;
            pause(control, Duration::from_millis(250));
        }
        Ok(())
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
        if worker
            .pending
            .settle(&connection, cap, &mut meter(cap))
            .ok()
            .flatten()
            .is_some()
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
            Err(E::Connection(_) | E::Database) | Ok(None) => (),
            Err(_) => return Err(Error::Database),
        }
        if control.stop_requested() {
            return Err(Error::Custody);
        }
        pause(control, Duration::from_secs(1));
    }
}

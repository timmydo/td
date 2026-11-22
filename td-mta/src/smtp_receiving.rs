//! Fixed-worker direct SMTP receiving over a retained configuration snapshot.
use crate::transport::retry_accept;
use crate::{
    admission::timers::{self, TimeoutPlan},
    config::{
        listener::{Kind, Listener},
        routing::Routing,
    },
    format::row::ReceiptTls,
    generations::GenerationLease,
    limits::ResourcePlan,
    ports::{
        Clock, CommitFailure, Deadline, Error, PeerVerification, Tick, TlsPolicyId, TlsTransport,
        TlsVersion, Transport,
    },
    smtp_network::{Network, Progress},
    smtp_rate::PeerKey,
    smtp_session::{Pending, Settings},
    smtp_starttls::{self, Upgrade},
    smtp_storage::{self, Job as _, Storage},
    tls_admission::HandshakePool,
    tls_io::TLS_WIRE_BYTES,
    tls_policy::{PolicyRole, TlsConnection, TlsPolicies},
    transport::TcpTransport,
};
use std::{
    net::{IpAddr, TcpListener},
    sync::{
        atomic::{AtomicBool, AtomicU8, Ordering},
        Arc, OnceLock, TryLockError,
    },
    time::Duration,
};

type Wire = Box<[u8; TLS_WIRE_BYTES]>;
struct StorageClose<'a, B: smtp_storage::Storage>(&'a B, &'a Control);
impl<B: smtp_storage::Storage> Drop for StorageClose<'_, B> {
    fn drop(&mut self) {
        if self.0.close().is_err() {
            self.1.fail();
        }
    }
}

mod dkim_worker;
mod wait;
pub(crate) use dkim_worker::STACK_BYTES as DKIM_STACK_BYTES;
use wait::{Blocked, Cell, Owner};

const SCAN: Duration = Duration::from_millis(5);
const NETWORK_TURNS: usize = 16;

fn later(clock: &dyn Clock, millis: u64) -> Result<Tick, Error> {
    Ok(Tick(
        clock
            .sample()?
            .monotonic
            .0
            .checked_add(millis)
            .ok_or(Error::Invalid)?,
    ))
}
struct Finish<'a> {
    finished: &'a AtomicBool,
    storage: &'a std::thread::Thread,
    tls: &'a std::thread::Thread,
    dkim: Option<&'a std::thread::Thread>,
    control: &'a Control,
}
impl Drop for Finish<'_> {
    fn drop(&mut self) {
        if std::thread::panicking() {
            self.control.fail();
        }
        self.finished.store(true, Ordering::Release);
        self.storage.unpark();
        self.tls.unpark();
        if let Some(dkim) = self.dkim {
            dkim.unpark();
        }
    }
}

/// Observable lifecycle of this receiving listener, not whole-service health.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum State {
    Starting,
    Ready,
    Stopping,
    Stopped,
    Failed,
}

pub struct Control {
    pub(crate) https_wake: crate::network_wake::Notifications,
    smtp_wake: crate::network_wake::Notifications,
    smtp_drain: OnceLock<Deadline>,
    stop: AtomicBool,
    user_stopped: AtomicBool,
    state: AtomicU8,
    logging: Option<crate::application_log::Producer>,
}
impl Default for Control {
    fn default() -> Self {
        Self {
            stop: AtomicBool::new(false),
            user_stopped: AtomicBool::new(false),
            https_wake: Default::default(),
            smtp_wake: Default::default(),
            smtp_drain: OnceLock::new(),
            state: AtomicU8::new(0),
            logging: None,
        }
    }
}
impl Control {
    pub fn with_logging(logging: crate::application_log::Producer) -> Self {
        Self {
            logging: Some(logging),
            ..Self::default()
        }
    }
    pub fn log(
        &self,
        kind: crate::observability::Kind,
        submission: Option<crate::ids::SubmissionId>,
        transaction: Option<crate::format::Sequence>,
    ) {
        if let Some(logging) = self.logging.as_ref() {
            logging.emit(kind, submission, transaction);
        }
    }
    pub fn logging(&self) -> Option<&crate::application_log::Producer> {
        self.logging.as_ref()
    }
    pub fn logging_status(&self) -> Option<crate::application_log::Status> {
        self.logging.as_ref().map(|logging| logging.status())
    }

    fn can_claim_smtp_work(&self, clock: &dyn Clock) -> bool {
        if self.state() == State::Failed {
            return false;
        }
        let Some(deadline) = self.smtp_drain.get() else {
            return true;
        };
        match clock.monotonic() {
            Ok(now) => !deadline.expired(now),
            Err(_) => {
                self.fail();
                false
            }
        }
    }

    pub fn user_stop(&self) {
        self.user_stopped.store(true, Ordering::Release);
        self.stop();
    }
    pub fn user_stopped(&self) -> bool {
        self.user_stopped.load(Ordering::Acquire)
    }
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Release);
        self.https_wake.notify();
        self.smtp_wake.notify();
    }
    pub fn stop_requested(&self) -> bool {
        self.stop.load(Ordering::Acquire)
    }
    pub fn state(&self) -> State {
        match self.state.load(Ordering::Acquire) {
            0 | 5 => State::Starting,
            1 => State::Ready,
            2 => State::Stopping,
            3 => State::Stopped,
            _ => State::Failed,
        }
    }
    fn transition(&self, next: u8) {
        let _ = self
            .state
            .try_update(Ordering::AcqRel, Ordering::Acquire, |old| {
                (old != 4).then_some(next)
            });
    }
    /// Drive the shared lifecycle when HTTPS is the only network owner.
    pub fn wait_with_ready(&self, ready: impl FnOnce() -> Result<(), Error>) -> Result<(), Error> {
        if self.stop_requested() {
            return Ok(());
        }
        if let Err(error) = ready() {
            self.fail();
            return Err(error);
        }
        self.transition(1);
        while !self.stop_requested() {
            std::thread::sleep(SCAN);
        }
        self.transition(3);
        if self.state() == State::Failed {
            Err(Error::WriterStopped)
        } else {
            Ok(())
        }
    }

    pub fn fail(&self) {
        self.state.store(4, Ordering::Release);
        self.stop();
    }
}

/// Cold validation binds the real socket to its exact retained TLS policy.
pub struct BoundListener<'a> {
    socket: TcpListener,
    row: Listener<'a>,
    policy: TlsPolicyId,
    hostname: &'a str,
    trace_bytes: usize,
}
impl<'a> BoundListener<'a> {
    pub fn new(
        socket: TcpListener,
        row: Listener<'a>,
        policies: &GenerationLease<TlsPolicies>,
    ) -> Result<Self, Error> {
        let bind = row.bind.tcp().ok_or(Error::Invalid)?;
        if row.kind != Kind::DirectSmtp
            || (bind.is_ipv6() && !crate::transport::ipv6_only(&socket)?)
            || bind.port() == 0
            || socket.local_addr().map_err(Error::from)? != bind
        {
            return Err(Error::Invalid);
        }
        let policy = TlsPolicies::bound_listener(policies, row)?;
        if TlsPolicies::role(policies, policy)? != PolicyRole::DirectSmtp {
            return Err(Error::Forbidden);
        }
        let hostname = row.server_name.ok_or(Error::Invalid)?;
        let trace_bytes = crate::receiving_headers::auth_trace_allowance(hostname)?;
        socket.set_nonblocking(true).map_err(Error::from)?;
        Ok(Self {
            socket,
            row,
            policy,
            hostname,
            trace_bytes,
        })
    }
}

enum Connection<'a> {
    Plain(Network<'a>, TcpTransport),
    Tls(Network<'a>, TlsConnection<Wire>),
    Upgrade(Upgrade<'a, Wire>),
}
impl<'a> Connection<'a> {
    fn network(&self) -> Result<&Network<'a>, Error> {
        match self {
            Self::Plain(n, _) | Self::Tls(n, _) => Ok(n),
            _ => Err(Error::Conflict),
        }
    }
    fn network_mut(&mut self) -> Result<&mut Network<'a>, Error> {
        match self {
            Self::Plain(n, _) | Self::Tls(n, _) => Ok(n),
            _ => Err(Error::Conflict),
        }
    }
    fn advance(&mut self, clock: &dyn Clock, blocked: &mut Blocked) -> Result<Progress, Error> {
        match self {
            Self::Plain(n, t) => {
                let progress = n.advance(t, clock)?;
                if progress == Progress::Pending {
                    let (_, read) = n.blocked();
                    *blocked = Blocked { read, write: !read };
                }
                Ok(progress)
            }
            Self::Tls(n, t) => {
                let before = t.begin_turn();
                let progress = n.advance(t, clock)?;
                if progress == Progress::Pending {
                    let turn = t.wire_turn(before);
                    if turn.moved {
                        return Ok(Progress::Advanced);
                    }
                    *blocked = Blocked {
                        read: turn.read_blocked,
                        write: turn.write_blocked,
                    };
                }
                Ok(progress)
            }
            _ => Err(Error::Conflict),
        }
    }
    fn tls(&self) -> Result<ReceiptTls, Error> {
        match self {
            Self::Plain(..) => Ok(ReceiptTls::Plain),
            Self::Tls(_, t) => match t.info() {
                Some(info) if info.peer == PeerVerification::None => Ok(match info.version {
                    TlsVersion::V12 => ReceiptTls::Tls12,
                    TlsVersion::V13 => ReceiptTls::Tls13,
                }),
                _ => Err(Error::Forbidden),
            },
            _ => Err(Error::Conflict),
        }
    }
}

#[derive(Clone, Copy)]
enum Work {
    Recipient,
    Begin,
    Write,
    Commit { prepared: bool },
    Tls,
}
#[derive(Clone, Copy)]
enum Phase {
    Empty,
    Network,
    AwaitAuthentication {
        until: Tick,
    },
    Queued {
        work: Work,
        deadline: Deadline,
    },
    Done,
    Retry {
        work: Work,
        deadline: Deadline,
        at: Tick,
    },
    Retire,
}
struct Slot<'s, 'cfg, 'spf, B: Storage + 's> {
    phase: Phase,
    connection: Option<Connection<'cfg>>,
    delivery: Option<B::Job<'s>>,
    listener: usize,
    peer: IpAddr,
    final_reply: bool,
    blocked: Blocked,
    spf_ticket: Option<crate::smtp_spf::Ticket<'spf>>,
    spf_result: Option<crate::spf::Outcome>,
    authentication: Option<dkim_worker::MessageAuthentication>,
    data_timing: Option<crate::performance::Timer>,
    commit_wait_timing: Option<crate::performance::Timer>,
}
impl<'cfg, B: Storage> Slot<'_, 'cfg, '_, B> {
    fn connection(&mut self) -> Result<&mut Connection<'cfg>, Error> {
        self.connection.as_mut().ok_or(Error::Conflict)
    }
    fn freeze_spf(&mut self, result: Option<crate::spf::Outcome>) {
        self.spf_result = Some(result.unwrap_or(crate::spf::Outcome::Temperror));
        self.spf_ticket = None;
    }

    fn queue_retirement(&mut self) {
        self.spf_ticket = None;
        self.phase = Phase::Retire;
    }

    fn retire(&mut self) {
        self.data_timing.take();
        self.commit_wait_timing.take();
        self.delivery.take();
        self.connection.take();
        self.phase = Phase::Empty;
        self.final_reply = false;
        self.spf_ticket = None;
        self.spf_result = None;
        self.authentication = None;
    }
}

fn session_capacity(
    occupied: &[Option<(usize, PeerKey)>],
    candidate: (usize, PeerKey),
    listener_limit: usize,
    listener_peer_limit: usize,
    profile_peer_limit: usize,
) -> bool {
    let mut listener_sessions = 0;
    let mut listener_peer_sessions = 0;
    let mut profile_peer_sessions = 0;
    for &entry in occupied.iter().flatten() {
        listener_sessions += usize::from(entry.0 == candidate.0);
        listener_peer_sessions += usize::from(entry == candidate);
        profile_peer_sessions += usize::from(entry.1 == candidate.1);
    }
    listener_sessions < listener_limit
        && listener_peer_sessions < listener_peer_limit
        && profile_peer_sessions < profile_peer_limit
}

/// Charge the peer's last acceptance, then decide its next MAIL.
fn rate_turn(
    rates: &mut crate::smtp_rate::PeerRates,
    peer: IpAddr,
    network: &mut Network<'_>,
    now: Tick,
) {
    let key = PeerKey::from(peer);
    if let Some(charge) = network.take_accepted() {
        rates.charge(key, charge, now);
    }
    network.set_rate_limited(rates.limited(key, now));
}

/// Run with one immutable routing/TLS snapshot and a shared monotonic clock.
/// Startup storage checks precede readiness; no gateway is enabled, and a reload
/// replaces the whole snapshot with a new generation.
pub struct Receiving<'s, 'cfg, B: Storage> {
    pub storage: &'s B,
    pub routes: &'cfg Routing<'cfg>,
    pub resources: &'cfg ResourcePlan,
    pub admission: &'cfg crate::admission::Plan,
    pub timeouts: &'cfg TimeoutPlan,
    pub policies: &'cfg GenerationLease<TlsPolicies>,
    pub handshakes: &'cfg HandshakePool,
    pub listeners: &'cfg [BoundListener<'cfg>],
    pub clock: Arc<dyn Clock>,
    /// Accepted-mail rate per peer key; production passes `Policy::DEFAULT`.
    pub rates: crate::smtp_rate::Policy,
}

fn after(now: Tick, seconds: u64) -> Result<Deadline, Error> {
    timers::deadline(now, seconds).map_err(|_| Error::Invalid)
}
fn buffer() -> Result<Wire, Error> {
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(TLS_WIRE_BYTES)
        .map_err(|_| Error::Capacity)?;
    bytes.resize(TLS_WIRE_BYTES, 0);
    bytes
        .into_boxed_slice()
        .try_into()
        .map_err(|_| Error::Capacity)
}

impl<'s, 'cfg, B: Storage> Receiving<'s, 'cfg, B> {
    pub fn run(&self, control: &Control) -> Result<(), Error> {
        self.run_with_ready(control, || Ok(()))
    }

    /// Announce readiness after startup checks and both workers start, before
    /// accepting clients. A failed announcement stops and joins the workers.
    pub fn run_with_ready(
        &self,
        control: &Control,
        ready: impl FnOnce() -> Result<(), Error>,
    ) -> Result<(), Error> {
        self.run_with_resolvers(control, ready, &[])
    }

    /// Direct-MX DNS observations use only the configured recursive endpoints.
    /// With no resolver, accepted mail carries a local temporary error.
    pub fn run_with_resolvers(
        &self,
        control: &Control,
        ready: impl FnOnce() -> Result<(), Error>,
        resolvers: &[std::net::SocketAddr],
    ) -> Result<(), Error> {
        control
            .state
            .compare_exchange(0, 5, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| Error::Conflict)?;
        let result = crate::smtp_spf::with_worker(
            self.resources.limits().smtp_sessions,
            resolvers,
            &control.smtp_wake,
            |spf| self.run_inner(control, ready, spf, resolvers),
        );
        if result.is_err() {
            control.fail();
            return result;
        }
        control.transition(3);
        if control.state() == State::Failed {
            Err(Error::WriterStopped)
        } else {
            Ok(())
        }
    }

    fn run_inner(
        &self,
        control: &Control,
        ready: impl FnOnce() -> Result<(), Error>,
        spf: Option<&crate::smtp_spf::Service<'_>>,
        resolvers: &[std::net::SocketAddr],
    ) -> Result<(), Error> {
        let limits = self.resources.limits();
        if self.listeners.is_empty()
            || self.listeners.len() > crate::config::listener::MAX_LISTENERS
            || self.handshakes.capacity() != limits.tls_handshakes
        {
            return Err(Error::Invalid);
        }
        let mut total = 0usize;
        for (index, listener) in self.listeners.iter().enumerate() {
            if self
                .listeners
                .get(..index)
                .ok_or(Error::Invalid)?
                .iter()
                .any(|other| {
                    other.row.name == listener.row.name || other.row.bind == listener.row.bind
                })
            {
                return Err(Error::Conflict);
            }
            let sessions = listener.row.session_limit.ok_or(Error::Invalid)?;
            let peers = listener.row.per_peer_limit.ok_or(Error::Invalid)?;
            if sessions == 0
                || peers == 0
                || peers > sessions
                || peers > limits.smtp_per_peer
                || listener.policy != TlsPolicies::bound_listener(self.policies, listener.row)?
            {
                return Err(Error::Invalid);
            }
            if listener.trace_bytes > limits.header_bytes {
                return Err(Error::Invalid);
            }
            // Probe the same greeting/settings before advertising readiness.
            Network::new(
                self.routes,
                self.settings(listener),
                self.timeouts,
                self.clock.as_ref(),
            )?;
            total = total.checked_add(sessions).ok_or(Error::Capacity)?;
        }
        if total > limits.smtp_sessions || limits.message_bytes as u64 > self.storage.capacity() {
            return Err(Error::Capacity);
        }
        let startup = after(
            self.clock.monotonic()?,
            self.admission.work().admission_seconds,
        )?;
        loop {
            if startup.expired(self.clock.monotonic()?) {
                return Err(Error::Deadline);
            }
            match self.storage.ready(self.routes.account(), startup) {
                Ok(()) => break,
                Err(Error::Busy) => std::thread::park_timeout(SCAN),
                Err(error) => return Err(error),
            }
            if control.stop.load(Ordering::Acquire) {
                return Ok(());
            }
        }
        let receiver = control.smtp_wake.attach()?;
        let mut slots = Vec::new();
        slots
            .try_reserve_exact(total)
            .map_err(|_| Error::Capacity)?;
        let mut occupied = Vec::new();
        occupied
            .try_reserve_exact(total)
            .map_err(|_| Error::Capacity)?;
        for _ in 0..total {
            slots.push(Cell::new(Slot {
                phase: Phase::Empty,
                connection: None,
                delivery: None,
                listener: 0,
                peer: IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED),
                final_reply: false,
                blocked: Blocked::default(),
                spf_ticket: None,
                spf_result: None,
                authentication: None,
                data_timing: None,
                commit_wait_timing: None,
            }));
            occupied.push(None);
        }
        // Owned by this network loop alone, like `occupied`.
        let mut rates = crate::smtp_rate::PeerRates::new(self.rates);
        let finished = AtomicBool::new(false);
        let started = AtomicU8::new(0);
        let main = std::thread::current();
        std::thread::scope(|scope| {
            let storage = std::thread::Builder::new()
                .name("smtp-storage".into())
                .spawn_scoped(scope, || {
                    self.worker(&slots, false, &finished, &started, &main, control)
                })
                .map_err(Error::from)?;
            let tls = match std::thread::Builder::new()
                .name("smtp-tls".into())
                .spawn_scoped(scope, || {
                    self.worker(&slots, true, &finished, &started, &main, control)
                }) {
                Ok(handle) => handle,
                Err(error) => {
                    finished.store(true, Ordering::Release);
                    storage.thread().unpark();
                    let _ = storage.join();
                    return Err(Error::from(error));
                }
            };
            let dkim = td_alloc::string("smtp-dkim").ok().and_then(|name| {
                std::thread::Builder::new()
                    .name(name)
                    .stack_size(dkim_worker::STACK_BYTES)
                    .spawn_scoped(scope, || {
                        self.verify_message(&slots, &finished, control, resolvers)
                    })
                    .ok()
            });
            let dkim_thread = dkim.as_ref().map(|worker| worker.thread());
            let _finish = Finish {
                finished: &finished,
                storage: storage.thread(),
                tls: tls.thread(),
                dkim: dkim_thread,
                control,
            };
            while started.load(Ordering::Acquire) != 2 {
                if storage.is_finished() || tls.is_finished() {
                    break;
                }
                std::thread::park_timeout(SCAN);
            }
            let result = (|| {
                if started.load(Ordering::Acquire) == 2 && !control.stop.load(Ordering::Acquire) {
                    let utc_ms = self.clock.sample()?.utc_ms;
                    for listener in self.listeners {
                        TlsPolicies::check_receiving_certificate(
                            self.policies,
                            listener.policy,
                            utc_ms,
                        )?;
                    }
                    ready()?;
                    control.transition(1);
                }
                let mut cursor = 0;
                let mut stopping = None;
                let mut accept_after = Tick(0);
                let mut certificate_failed = false;
                loop {
                    receiver.consume()?;
                    if storage.is_finished() || tls.is_finished() {
                        control.fail();
                        break Err(Error::WriterStopped);
                    }
                    let now = match self.clock.sample() {
                        Ok(time) => {
                            if !certificate_failed
                                && self.listeners.iter().any(|listener| {
                                    TlsPolicies::check_receiving_certificate(
                                        self.policies,
                                        listener.policy,
                                        time.utc_ms,
                                    )
                                    .is_err()
                                })
                            {
                                // Fail closed: workers discard queued, uncommitted work.
                                certificate_failed = true;
                                control.fail();
                            }
                            time.monotonic
                        }
                        Err(error) => {
                            control.fail();
                            break Err(error);
                        }
                    };
                    if control.stop.load(Ordering::Acquire) && stopping.is_none() {
                        let deadline = after(now, 5)?;
                        control
                            .smtp_drain
                            .set(deadline)
                            .map_err(|_| Error::Conflict)?;
                        stopping = Some(deadline);
                        if control.state() != State::Failed {
                            control.transition(2);
                        }
                    }
                    let mut progress = false;
                    let mut wake_storage = false;
                    let mut wake_tls = false;
                    let mut wake_dkim = false;
                    for offset in 0..total {
                        let index = (cursor + offset) % total;
                        let Some(cell) = slots.get(index) else {
                            break;
                        };
                        if !cell.owned(Owner::Network) && !cell.owned(Owner::Dkim) {
                            continue;
                        }
                        let mut slot = match cell.slot.try_lock() {
                            Ok(slot) => slot,
                            Err(TryLockError::WouldBlock) => continue,
                            Err(TryLockError::Poisoned(_)) => {
                                control.fail();
                                continue;
                            }
                        };
                        if matches!(slot.phase, Phase::Empty) {
                            if let Some(entry) = occupied.get_mut(index) {
                                *entry = None;
                            }
                            continue;
                        }
                        let final_reply = slot.final_reply
                            && slot
                                .connection
                                .as_ref()
                                .and_then(|connection| connection.network().ok())
                                .is_some_and(|network| {
                                    matches!(network.session().pending(), Pending::Reply { .. })
                                });
                        slot.final_reply = final_reply;
                        if stopping.is_some_and(|deadline| deadline.expired(now)) && !final_reply {
                            slot.queue_retirement();
                        }
                        if let Phase::Done = slot.phase {
                            slot.phase = Phase::Network;
                        }
                        if let Phase::AwaitAuthentication { until } = slot.phase {
                            let result = slot.spf_result.or_else(|| match spf {
                                Some(service) => service.poll(index, slot.spf_ticket.as_ref()),
                                None => Some(crate::spf::Outcome::Temperror),
                            });
                            slot.spf_result = result;
                            if now < until && stopping.is_none() {
                                if slot.authentication.is_none() && dkim_thread.is_some() {
                                    cell.assign(Owner::Dkim);
                                    wake_dkim = true;
                                    continue;
                                }
                                if result.is_none() {
                                    continue;
                                }
                            }
                            slot.authentication.get_or_insert_with(
                                dkim_worker::MessageAuthentication::unavailable,
                            );
                            slot.freeze_spf(result);
                            slot.phase = Phase::Network;
                        }
                        if let Phase::Retry { work, deadline, at } = slot.phase {
                            if at <= now || deadline.expired(now) {
                                slot.phase = Phase::Queued { work, deadline };
                                progress = true;
                            }
                        }
                        for _ in 0..NETWORK_TURNS {
                            if matches!(slot.phase, Phase::Done) {
                                slot.phase = Phase::Network;
                            }
                            if !matches!(slot.phase, Phase::Network) {
                                break;
                            }
                            if stopping.is_some_and(|deadline| deadline.expired(now))
                                && !final_reply
                            {
                                slot.queue_retirement();
                            } else {
                                let turn = (|| {
                                    let peer = slot.peer;
                                    if let Some(envelope) =
                                        slot.connection()?.network_mut()?.take_spf_envelope()
                                    {
                                        slot.spf_ticket = spf.and_then(|service| {
                                            service.start(
                                                index,
                                                peer,
                                                &envelope.ehlo,
                                                &envelope.reverse_path,
                                            )
                                        });
                                        slot.spf_result = None;
                                        slot.authentication = None;
                                    }
                                    let connection = slot.connection()?;
                                    let network = connection.network_mut()?;
                                    rate_turn(&mut rates, peer, network, now);
                                    if let Some(reply) = network.take_rejection() {
                                        if let Some(logger) = control.logging() {
                                            logger.smtp_rejection(
                                                reply,
                                                peer,
                                                network.session().rejected_envelope(),
                                            );
                                        }
                                    }
                                    if stopping.is_some() {
                                        let _ = connection
                                            .network_mut()?
                                            .service_unavailable(self.clock.as_ref());
                                    }
                                    let mut blocked = Blocked::default();
                                    let progress =
                                        connection.advance(self.clock.as_ref(), &mut blocked)?;
                                    if connection.network()?.session().envelope().is_none() {
                                        slot.spf_ticket = None;
                                    }
                                    slot.blocked = blocked;
                                    Ok(progress)
                                })();
                                match turn {
                                    Ok(Progress::Pending) => break,
                                    Ok(Progress::Advanced) => progress = true,
                                    Ok(Progress::Closed) | Err(_) => {
                                        if slot.data_timing.is_some() {
                                            control.log(
                                                crate::observability::Kind::SmtpDataInterrupted {
                                                    peer: slot.peer,
                                                    error: turn.err(),
                                                },
                                                None,
                                                None,
                                            );
                                        }
                                        slot.queue_retirement();
                                        progress = true;
                                    }
                                    Ok(Progress::Work) => {
                                        let dispatched = self.dispatch(
                                            &mut slot,
                                            now,
                                            control,
                                            spf,
                                            index,
                                            dkim_thread.is_some(),
                                        );
                                        if dispatched.is_err() {
                                            slot.queue_retirement();
                                        }
                                        progress = true;
                                    }
                                }
                            }
                        }
                        let owner = wait::owner(
                            slot.phase,
                            dkim_thread.is_some() && slot.authentication.is_none(),
                        );
                        cell.assign(owner);
                        match owner {
                            Owner::Tls => wake_tls = true,
                            Owner::Storage => wake_storage = true,
                            Owner::Dkim => wake_dkim = true,
                            Owner::Network => (),
                        }
                    }
                    cursor = (cursor + 1) % total;
                    // Each worker can acquire the queued slot when it wakes.
                    if let Some(thread) = dkim_thread.filter(|_| wake_dkim) {
                        thread.unpark();
                    }
                    if wake_storage {
                        storage.thread().unpark();
                    }
                    if wake_tls {
                        tls.thread().unpark();
                    }
                    if stopping.is_some() && occupied.iter().all(Option::is_none) {
                        break if certificate_failed {
                            Err(Error::Tls)
                        } else if control.state() == State::Failed {
                            Err(Error::WriterStopped)
                        } else {
                            Ok(())
                        };
                    }
                    if stopping.is_none()
                        && !control.stop.load(Ordering::Acquire)
                        && now >= accept_after
                    {
                        for (index, listener) in self.listeners.iter().enumerate() {
                            if control.stop.load(Ordering::Acquire) {
                                break;
                            }
                            match listener.socket.accept() {
                                Ok((socket, _)) => {
                                    progress = true;
                                    let mut transport = match TcpTransport::from_stream(socket) {
                                        Ok(transport) => transport,
                                        Err(_) => continue,
                                    };
                                    if self
                                        .clock
                                        .sample()
                                        .and_then(|time| {
                                            TlsPolicies::check_receiving_certificate(
                                                self.policies,
                                                listener.policy,
                                                time.utc_ms,
                                            )
                                        })
                                        .is_err()
                                    {
                                        let _ =
                                            transport.write(b"421 4.3.2 Service unavailable\r\n");
                                        transport.abort();
                                        certificate_failed = true;
                                        control.fail();
                                        continue;
                                    }
                                    let peer = transport.peer_addr().ip();
                                    let candidate = (index, PeerKey::from(peer));
                                    if !session_capacity(
                                        &occupied,
                                        candidate,
                                        listener.row.session_limit.unwrap_or(0),
                                        listener.row.per_peer_limit.unwrap_or(0),
                                        limits.smtp_per_peer,
                                    ) {
                                        let _ =
                                            transport.write(b"421 4.3.2 Service unavailable\r\n");
                                        transport.abort();
                                        continue;
                                    }
                                    let free = occupied
                                        .iter()
                                        .enumerate()
                                        .filter(|(_, entry)| entry.is_none())
                                        .find_map(|(index, _)| {
                                            slots.get(index).and_then(|cell| {
                                                cell.slot.try_lock().ok().map(|slot| (index, slot))
                                            })
                                        });
                                    let Some((free, mut slot)) = free else {
                                        let _ =
                                            transport.write(b"421 4.3.2 Service unavailable\r\n");
                                        transport.abort();
                                        continue;
                                    };
                                    let network = match Network::new(
                                        self.routes,
                                        self.settings(listener),
                                        self.timeouts,
                                        self.clock.as_ref(),
                                    ) {
                                        Ok(network) => network,
                                        Err(_) => {
                                            let _ = transport
                                                .write(b"421 4.3.2 Service unavailable\r\n");
                                            transport.abort();
                                            continue;
                                        }
                                    };
                                    slot.connection = Some(Connection::Plain(network, transport));
                                    slot.listener = index;
                                    slot.peer = peer;
                                    slot.phase = Phase::Network;
                                    if let Some(entry) = occupied.get_mut(free) {
                                        *entry = Some(candidate);
                                    }
                                }
                                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => (),
                                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => (),
                                Err(error) if retry_accept(&error) => {
                                    accept_after = later(self.clock.as_ref(), 5)?;
                                }
                                Err(_) => control.fail(),
                            }
                        }
                    }
                    if !progress {
                        wait::wait(
                            &receiver,
                            &slots,
                            self.listeners,
                            accept_after,
                            stopping,
                            now,
                            self.clock.as_ref(),
                        )?;
                    }
                }
            })();
            finished.store(true, Ordering::Release);
            storage.thread().unpark();
            tls.thread().unpark();
            drop(_finish);
            let storage_result = storage.join();
            let tls_result = tls.join();
            let failed = storage_result.is_err() || tls_result.is_err();
            if failed {
                control.fail();
            }
            // Terminal teardown is outside the event loop; never resume a poisoned job.
            for cell in &slots {
                match cell.slot.lock() {
                    Ok(mut slot) => slot.retire(),
                    Err(poisoned) => poisoned.into_inner().retire(),
                }
            }
            if failed {
                Err(Error::WriterStopped)
            } else {
                result
            }
        })
    }

    fn settings(&self, listener: &BoundListener<'cfg>) -> Settings<'cfg> {
        Settings {
            hostname: listener.hostname,
            message_bytes: self.resources.limits().message_bytes,
            trace_bytes: listener.trace_bytes,
            recipients: self.resources.limits().smtp_recipients,
            starttls: true,
        }
    }

    fn dispatch(
        &self,
        slot: &mut Slot<'s, 'cfg, '_, B>,
        now: Tick,
        control: &Control,
        spf: Option<&crate::smtp_spf::Service<'_>>,
        index: usize,
        dkim_available: bool,
    ) -> Result<(), Error> {
        let network = slot.connection()?.network()?;
        let (work, seconds) = match network.session().pending() {
            Pending::Recipient { .. } => (Work::Recipient, self.admission.work().admission_seconds),
            Pending::BeginData { .. } => (Work::Begin, self.admission.work().admission_seconds),
            Pending::Data(_) => (Work::Write, self.timeouts.execution_seconds()),
            Pending::Commit => (
                Work::Commit { prepared: false },
                self.admission.work().commit_seconds,
            ),
            Pending::StartTls { .. } => (Work::Tls, self.timeouts.limits().handshake_seconds),
            _ => return Err(Error::Conflict),
        };
        let transport_deadline = network.deadline();
        if matches!(work, Work::Commit { .. }) && slot.spf_result.is_none() {
            let result = match spf {
                Some(service) => service.poll(index, slot.spf_ticket.as_ref()),
                None => Some(crate::spf::Outcome::Temperror),
            };
            // Keep the full original commit budget, including on nearly expired DATA.
            let until = crate::smtp_spf::wait_until(now, transport_deadline, seconds);
            if until > now && !control.stop_requested() && (dkim_available || result.is_none()) {
                slot.spf_result = result;
                slot.commit_wait_timing = Some(crate::performance::Timer::start(
                    crate::performance::Phase::SmtpAuthenticationWait,
                ));
                slot.phase = Phase::AwaitAuthentication { until };
                return Ok(());
            }
            slot.authentication = Some(dkim_worker::MessageAuthentication::unavailable());
            slot.freeze_spf(result);
        }
        let deadline = after(now, seconds)?.min(transport_deadline);
        if matches!(work, Work::Write) {
            return self.write_data(slot, deadline, control);
        }
        if matches!(work, Work::Commit { .. }) {
            if let Some(timing) = slot.data_timing.take() {
                timing.finish(crate::performance::Outcome::Success);
            }
            if let Some(authentication) = slot.commit_wait_timing.take() {
                authentication.finish(crate::performance::Outcome::Success);
            }
            slot.commit_wait_timing = Some(crate::performance::Timer::start(
                crate::performance::Phase::SmtpCommitWait,
            ));
        }
        slot.phase = Phase::Queued { work, deadline };
        Ok(())
    }

    fn worker(
        &self,
        slots: &[Cell<Slot<'s, 'cfg, '_, B>>],
        tls: bool,
        finished: &AtomicBool,
        started: &AtomicU8,
        main: &std::thread::Thread,
        control: &Control,
    ) {
        let owner = if tls { Owner::Tls } else { Owner::Storage };
        let _close = (!tls).then(|| StorageClose(self.storage, control));
        let mut random = if tls {
            None
        } else {
            Some(match td_crypto::SystemEntropy::try_new() {
                Ok(random) => random,
                Err(_) => {
                    control.fail();
                    main.unpark();
                    return;
                }
            })
        };
        started.fetch_add(1, Ordering::Release);
        main.unpark();
        while !finished.load(Ordering::Acquire) {
            if !tls {
                match self.storage.maintain() {
                    Ok(()) | Err(Error::Busy) => (),
                    Err(_) => {
                        control.fail();
                        main.unpark();
                    }
                }
            }
            let mut progress = false;
            for cell in slots {
                if !cell.owned(owner) {
                    continue;
                }
                let Ok(mut slot) = cell.slot.lock() else {
                    control.fail();
                    return;
                };
                match slot.phase {
                    Phase::Retire if !tls => {
                        slot.retire();
                        if self.storage.stopped() {
                            control.fail();
                        }
                        progress = true;
                    }
                    Phase::Queued { work, deadline } if matches!(work, Work::Tls) == tls => {
                        if !control.can_claim_smtp_work(self.clock.as_ref()) {
                            slot.queue_retirement();
                        } else {
                            let result = if tls {
                                self.tls(&mut slot, deadline)
                            } else {
                                match random.as_mut() {
                                    Some(random) => {
                                        self.storage(&mut slot, work, deadline, random, control)
                                    }
                                    None => Err(Error::Conflict),
                                }
                            };
                            if result.is_err() {
                                slot.retire();
                                if !tls && self.storage.stopped() {
                                    control.fail();
                                }
                            }
                        }
                        progress = true;
                    }
                    _ => (),
                }
                cell.assign(Owner::Network);
                drop(slot);
                control.smtp_wake.notify();
            }
            if !progress && !finished.load(Ordering::Acquire) {
                if tls {
                    std::thread::park();
                } else {
                    std::thread::park_timeout(Duration::from_secs(1));
                }
            }
        }
        if !tls {
            for cell in slots {
                if let Ok(mut slot) = cell.slot.lock() {
                    slot.retire();
                }
            }
            if self.storage.stopped() {
                control.fail();
            }
        }
    }

    fn storage(
        &self,
        slot: &mut Slot<'s, 'cfg, '_, B>,
        work: Work,
        cap: Deadline,
        random: &mut td_crypto::SystemEntropy,
        control: &Control,
    ) -> Result<(), Error> {
        let clock = self.clock.as_ref();
        let now = clock.monotonic()?;
        if cap.expired(now) {
            return self.storage_refused(slot, work, Error::Deadline, control);
        }
        match work {
            Work::Recipient => {
                let network = slot.connection()?.network()?;
                let Pending::Recipient {
                    account,
                    declared_bytes,
                } = network.session().pending()
                else {
                    return Err(Error::Conflict);
                };
                match self.storage.recipient(account, declared_bytes, cap) {
                    Ok(check) => {
                        if cap.expired(clock.monotonic()?) {
                            return self.storage_refused(slot, work, Error::Deadline, control);
                        }
                        slot.connection()?
                            .network_mut()?
                            .recipient_checked(Ok(check), clock)?;
                    }
                    Err(smtp_storage::Failure::Retry) => {
                        slot.phase = Phase::Retry {
                            work,
                            deadline: cap,
                            at: later(clock, 5)?,
                        };
                        return Ok(());
                    }
                    Err(smtp_storage::Failure::Refused(error)) => {
                        return self.storage_refused(slot, work, error, control)
                    }
                    Err(smtp_storage::Failure::HeaderLimit) => {
                        return self.storage_refused(slot, work, Error::Invalid, control)
                    }
                }
            }
            Work::Begin => {
                let timing =
                    crate::performance::Timer::start(crate::performance::Phase::SmtpAdmission);
                let connection = slot.connection.as_ref().ok_or(Error::Conflict)?;
                let network = connection.network()?;
                let result = self.storage.begin(
                    smtp_storage::Begin {
                        config_generation: self.policies.id().get(),
                        peer: slot.peer,
                        tls: connection.tls()?,
                        clock: self.clock.clone(),
                        session: network.session(),
                        header_bytes: self.resources.limits().header_bytes,
                        deadline: network.deadline(),
                    },
                    random,
                );
                timing.finish(match &result {
                    Ok(_) => crate::performance::Outcome::Success,
                    Err(smtp_storage::Failure::Retry) => crate::performance::Outcome::Busy,
                    Err(_) => crate::performance::Outcome::Failed,
                });
                match result {
                    Ok(job) => {
                        slot.delivery = Some(job);
                        if cap.expired(clock.monotonic()?) {
                            return self.storage_refused(slot, work, Error::Deadline, control);
                        }
                        slot.connection()?
                            .network_mut()?
                            .data_ready(Ok(()), clock)?;
                        slot.data_timing = Some(crate::performance::Timer::start(
                            crate::performance::Phase::SmtpData,
                        ));
                    }
                    Err(smtp_storage::Failure::Retry) => {
                        slot.phase = Phase::Retry {
                            work,
                            deadline: cap,
                            at: later(clock, 5)?,
                        };
                        return Ok(());
                    }
                    Err(error) => return self.delivery_refused(slot, work, error, control),
                }
            }
            Work::Write => return self.write_data(slot, cap, control),
            Work::Commit { prepared } => {
                if let Some(timing) = slot.commit_wait_timing.take() {
                    timing.finish(crate::performance::Outcome::Success);
                }
                let timing =
                    crate::performance::Timer::start(crate::performance::Phase::SmtpCommit);
                let job = slot.delivery.as_mut().ok_or(Error::Conflict)?;
                if !prepared {
                    let authentication = slot.authentication.take().ok_or(Error::Conflict)?;
                    let spf = slot.spf_result.ok_or(Error::Conflict)?;
                    if let Err(error) = job.prepare_with_dmarc(
                        cap,
                        Some(spf),
                        Some(authentication.dkim),
                        Some(authentication.dmarc.finish(spf)),
                    ) {
                        return self.delivery_refused(slot, work, error, control);
                    }
                }
                let completed = job.commit();
                timing.finish(match &completed {
                    Ok(completion) if completion.outcome.is_ok() => {
                        crate::performance::Outcome::Success
                    }
                    Err(smtp_storage::Failure::Retry) => crate::performance::Outcome::Busy,
                    _ => crate::performance::Outcome::Failed,
                });
                match completed {
                    Ok(completion) => {
                        // Preserve the native outcome even when accounting or cleanup failed.
                        let result = completion.outcome;
                        if let Ok(commit) = result {
                            control.log(
                                crate::observability::Kind::MetadataCommitted,
                                None,
                                commit
                                    .iter()
                                    .next()
                                    .filter(|_| commit.iter().count() == 1)
                                    .map(|entry| entry.sequence),
                            );
                        }
                        if completion.stop {
                            control.fail();
                            // A committed outcome the reply cannot describe
                            // closes without one.
                            if matches!(result, Err(CommitFailure::Indeterminate(_))) {
                                slot.delivery.take();
                                slot.queue_retirement();
                                return Ok(());
                            }
                        }
                        slot.delivery.take();
                        slot.connection()?.network_mut()?.committed(result, clock)?;
                        slot.final_reply = true;
                    }
                    Err(smtp_storage::Failure::Retry) => {
                        slot.phase = Phase::Retry {
                            work: Work::Commit { prepared: true },
                            deadline: cap,
                            at: later(clock, 5)?,
                        };
                        return Ok(());
                    }
                    Err(error) => return self.delivery_refused(slot, work, error, control),
                }
            }
            Work::Tls => return Err(Error::Conflict),
        }
        if self.storage.stopped() {
            control.fail();
        }
        slot.phase = Phase::Done;
        Ok(())
    }

    fn write_data(
        &self,
        slot: &mut Slot<'s, 'cfg, '_, B>,
        cap: Deadline,
        control: &Control,
    ) -> Result<(), Error> {
        if control.state() == State::Failed {
            slot.queue_retirement();
            return Ok(());
        }
        let work = Work::Write;
        let clock = self.clock.as_ref();
        if cap.expired(clock.monotonic()?) {
            return self.storage_refused(slot, work, Error::Deadline, control);
        }
        let bytes = match slot
            .connection
            .as_ref()
            .ok_or(Error::Conflict)?
            .network()?
            .session()
            .pending()
        {
            Pending::Data(bytes) => bytes,
            _ => return Err(Error::Conflict),
        };
        let result = slot.delivery.as_mut().ok_or(Error::Conflict)?.write(bytes);
        if let Err(error) = result {
            return self.delivery_refused(slot, work, error, control);
        }
        if cap.expired(clock.monotonic()?) {
            return self.storage_refused(slot, work, Error::Deadline, control);
        }
        slot.connection()?
            .network_mut()?
            .data_written(Ok(()), clock)?;
        if self.storage.stopped() {
            control.fail();
        }
        slot.phase = Phase::Done;
        Ok(())
    }

    fn delivery_refused(
        &self,
        slot: &mut Slot<'s, 'cfg, '_, B>,
        work: Work,
        error: smtp_storage::Failure,
        control: &Control,
    ) -> Result<(), Error> {
        slot.data_timing.take();
        slot.commit_wait_timing.take();
        if matches!(error, smtp_storage::Failure::HeaderLimit) {
            slot.spf_ticket = None;
            slot.delivery.take();
            slot.connection()?
                .network_mut()?
                .message_too_large(self.clock.as_ref())?;
            if self.storage.stopped() {
                control.fail();
            }
            slot.phase = Phase::Done;
            return Ok(());
        }
        let error = match error {
            smtp_storage::Failure::Refused(error) => error,
            _ => Error::Busy,
        };
        self.storage_refused(slot, work, error, control)
    }

    fn storage_refused(
        &self,
        slot: &mut Slot<'s, 'cfg, '_, B>,
        work: Work,
        error: Error,
        control: &Control,
    ) -> Result<(), Error> {
        // A refused RCPT check keeps the envelope and its SPF evaluation.
        if matches!(work, Work::Recipient) {
            slot.connection()?
                .network_mut()?
                .recipient_checked(Err(error), self.clock.as_ref())?;
            if self.storage.stopped() {
                control.fail();
            }
            slot.phase = Phase::Done;
            return Ok(());
        }
        slot.data_timing.take();
        slot.commit_wait_timing.take();
        slot.spf_ticket = None;
        slot.delivery.take();
        let network = slot.connection()?.network_mut()?;
        match work {
            Work::Begin => network.data_ready(Err(error), self.clock.as_ref())?,
            Work::Write => network.data_written(Err(error), self.clock.as_ref())?,
            Work::Commit { .. } => {
                network.committed(Err(CommitFailure::Rejected(error)), self.clock.as_ref())?
            }
            Work::Recipient | Work::Tls => return Err(Error::Conflict),
        }
        if self.storage.stopped() {
            control.fail();
        }
        slot.final_reply = matches!(work, Work::Commit { .. });
        slot.phase = Phase::Done;
        Ok(())
    }

    fn tls(&self, slot: &mut Slot<'s, 'cfg, '_, B>, cap: Deadline) -> Result<(), Error> {
        let now = self.clock.monotonic()?;
        if cap.expired(now) {
            return Err(Error::Deadline);
        }
        let owner = slot.connection.take().ok_or(Error::Conflict)?;
        let upgrade = match owner {
            Connection::Plain(network, plain) => {
                let listener = self.listeners.get(slot.listener).ok_or(Error::Conflict)?;
                let reserved = TlsPolicies::reserve_session(
                    self.policies.clone(),
                    listener.policy,
                    self.handshakes,
                    buffer()?,
                    buffer()?,
                );
                let prepared = match reserved {
                    Ok(reserved) => reserved.construct().map_err(|r| r.error())?,
                    Err(refusal) if refusal.error() == Error::Busy => {
                        slot.connection = Some(Connection::Plain(network, plain));
                        slot.phase = Phase::Retry {
                            work: Work::Tls,
                            deadline: cap,
                            at: later(self.clock.as_ref(), 10)?,
                        };
                        return Ok(());
                    }
                    Err(refusal) => return Err(refusal.error()),
                };
                Upgrade::new(network, plain, prepared, self.clock.clone(), cap)?
            }
            Connection::Upgrade(upgrade) => upgrade,
            Connection::Tls(..) => return Err(Error::Conflict),
        };
        match upgrade.advance()? {
            smtp_starttls::Progress::Pending(upgrade) => {
                slot.connection = Some(Connection::Upgrade(upgrade));
                slot.phase = Phase::Retry {
                    work: Work::Tls,
                    deadline: cap,
                    at: later(self.clock.as_ref(), 10)?,
                };
            }
            smtp_starttls::Progress::Established { network, transport } => {
                slot.connection = Some(Connection::Tls(network, transport));
                slot.phase = Phase::Done;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_peer_limits_apply_within_and_across_listeners() {
        let key = |prefix, host| {
            PeerKey::from(IpAddr::V6(std::net::Ipv6Addr::new(
                0x2001, 0xdb8, 0, prefix, 0, 0, 0, host,
            )))
        };
        let occupied = [Some((0, key(1, 1))), Some((1, key(1, 2))), None];
        let sibling = key(1, 3);
        assert!(!session_capacity(&occupied, (0, sibling), 8, 1, 3));
        assert!(session_capacity(&occupied, (2, sibling), 8, 1, 3));
        assert!(!session_capacity(&occupied, (2, sibling), 8, 1, 2));
        assert!(session_capacity(&occupied, (0, key(2, 1)), 8, 1, 2));
        assert!(!session_capacity(&occupied, (0, key(2, 1)), 1, 1, 2));
    }

    #[test]
    fn four_peer_keys_cannot_fill_the_default_session_pool() {
        // One listener takes the whole default pool, as SCHEMA.md's example does.
        let limits = crate::limits::Limits::default();
        let (sessions, per_peer) = (limits.smtp_sessions, limits.smtp_per_peer);
        let admit = |occupied: &[Option<(usize, PeerKey)>], address: IpAddr| {
            let key = (0, PeerKey::from(address));
            session_capacity(occupied, key, sessions, per_peer, per_peer).then_some(key)
        };
        let mut occupied = vec![None; sessions];
        let mut used = 0;
        // An IPv4 address in both spellings and three IPv6 /64s, each sending
        // from a new address within its key on every attempt.
        for prefix in 0..4u16 {
            for host in 1..=u16::try_from(sessions).unwrap_or(u16::MAX) {
                let address = match (prefix, host % 2) {
                    (0, 0) => IpAddr::V4(std::net::Ipv4Addr::new(192, 0, 2, 1)),
                    (0, _) => IpAddr::V6(std::net::Ipv4Addr::new(192, 0, 2, 1).to_ipv6_mapped()),
                    _ => IpAddr::V6(std::net::Ipv6Addr::new(
                        0x2001, 0xdb8, 0, prefix, 0, 0, 0, host,
                    )),
                };
                let Some(key) = admit(&occupied, address) else {
                    continue;
                };
                if let Some(slot) = occupied.get_mut(used) {
                    *slot = Some(key);
                    used += 1;
                }
            }
        }
        assert_eq!(used, 4 * per_peer);
        assert!(used < sessions);
        // Every other sender still finds room; filling the pool takes sixteen keys.
        let mut keys = 4;
        for prefix in 4..=u16::try_from(sessions).unwrap_or(u16::MAX) {
            let address = IpAddr::V6(std::net::Ipv6Addr::new(
                0x2001, 0xdb8, 0, prefix, 0, 0, 0, 1,
            ));
            while let Some(key) = admit(&occupied, address) {
                let Some(slot) = occupied.get_mut(used) else {
                    break;
                };
                *slot = Some(key);
                used += 1;
            }
            keys += 1;
            if used == sessions {
                break;
            }
        }
        assert_eq!(used, sessions);
        assert_eq!(keys, sessions.div_ceil(per_peer));
        assert_eq!(keys, 16);
    }

    #[test]
    fn pending_accept_errors_retry_but_broken_listener_fails() {
        for errno in [
            12, 23, 24, 64, 71, 92, 95, 100, 101, 103, 104, 105, 112, 113,
        ] {
            assert!(retry_accept(&std::io::Error::from_raw_os_error(errno)));
        }
        for errno in [9, 22, 88] {
            assert!(!retry_accept(&std::io::Error::from_raw_os_error(errno)));
        }
    }

    #[test]
    fn failed_status_cannot_be_replaced_by_lifecycle_transitions() {
        let control = Control::default();
        control.transition(1);
        control.fail();
        for state in [1, 2, 3] {
            control.transition(state);
            assert_eq!(control.state(), State::Failed);
        }
    }
}

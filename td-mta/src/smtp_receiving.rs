//! Fixed-worker direct SMTP receiving over a retained configuration snapshot.
use crate::{
    admission::timers::{self, TimeoutPlan},
    config::{
        listener::{Kind, Listener},
        routing::Routing,
    },
    format::row::ReceiptTls,
    generations::GenerationLease,
    ids::AccountId,
    limits::ResourcePlan,
    ports::{
        Access, Clock, CommitFailure, Deadline, Error, PeerVerification, Principal, Tick,
        TlsPolicyId, TlsTransport, TlsVersion, Transport,
    },
    smtp_network::{Network, Progress},
    smtp_session::{Pending, Settings},
    smtp_starttls::{self, Upgrade},
    store_fs::{
        smtp_trace_allowance, Delivery, DeliveryAuthorization, DeliveryError, DeliveryGuard,
        DeliveryPeer, DeliveryRequest, IngressSpool, StoreCoordinator, UploadError,
    },
    tls_admission::HandshakePool,
    tls_io::TLS_WIRE_BYTES,
    tls_policy::{PolicyRole, TlsConnection, TlsPolicies},
    transport::TcpTransport,
};
use std::{
    net::{IpAddr, TcpListener},
    sync::{
        atomic::{AtomicBool, AtomicU8, Ordering},
        Arc, Mutex, TryLockError,
    },
    time::Duration,
};

type Wire = Box<[u8; TLS_WIRE_BYTES]>;
const SCAN: Duration = Duration::from_millis(5);

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
fn retry_accept(error: &std::io::Error) -> bool {
    // Linux accept forwards pending connection errors. Resource pressure also
    // retries after a scan delay; invalid listening descriptors still fail.
    matches!(
        error.kind(),
        std::io::ErrorKind::ConnectionAborted | std::io::ErrorKind::ConnectionReset
    ) || matches!(
        error.raw_os_error(),
        Some(12 | 23 | 24 | 64 | 71 | 92 | 95 | 100 | 101 | 105 | 112 | 113)
    )
}
struct Finish<'a> {
    finished: &'a AtomicBool,
    storage: &'a std::thread::Thread,
    tls: &'a std::thread::Thread,
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
    stop: AtomicBool,
    state: AtomicU8,
}
impl Default for Control {
    fn default() -> Self {
        Self {
            stop: AtomicBool::new(false),
            state: AtomicU8::new(0),
        }
    }
}
impl Control {
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Release);
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
    fn fail(&self) {
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
        if row.kind != Kind::DirectSmtp
            || !row.bind.is_ipv4()
            || row.bind.port() == 0
            || socket.local_addr().map_err(Error::from)? != row.bind
        {
            return Err(Error::Invalid);
        }
        let policy = TlsPolicies::bound_listener(policies, row)?;
        if TlsPolicies::role(policies, policy)? != PolicyRole::DirectSmtp {
            return Err(Error::Forbidden);
        }
        let hostname = row.server_name.ok_or(Error::Invalid)?;
        let trace_bytes = smtp_trace_allowance(hostname)?;
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

struct Direct {
    access: Access,
    peer: IpAddr,
    tls: ReceiptTls,
    clock: Arc<dyn Clock>,
}
struct DirectGuard<'a>(&'a Direct);
impl DeliveryGuard for DirectGuard<'_> {
    fn access(&self) -> Access {
        self.0.access
    }
    fn peer(&self) -> DeliveryPeer<'_> {
        DeliveryPeer {
            peer: self.0.peer,
            tls: self.0.tls,
            gateway: None,
        }
    }
}
impl DeliveryAuthorization for Direct {
    type Guard<'a> = DirectGuard<'a>;
    fn authorize(&self, account: AccountId, deadline: Deadline) -> Result<DirectGuard<'_>, Error> {
        if account != self.access.account {
            return Err(Error::Forbidden);
        }
        if deadline.expired(self.clock.sample()?.monotonic) {
            return Err(Error::Deadline);
        }
        Ok(DirectGuard(self))
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
    fn advance(&mut self, clock: &dyn Clock) -> Result<Progress, Error> {
        match self {
            Self::Plain(n, t) => n.advance(t, clock),
            Self::Tls(n, t) => n.advance(t, clock),
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
    Begin,
    Write,
    Commit { prepared: bool },
    Tls,
}
#[derive(Clone, Copy)]
enum Phase {
    Empty,
    Network,
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
type Job<'s, 'r, 'l, P> = Delivery<'s, 's, 'r, 'l, td_crypto::Provider, P, Direct>;
struct Slot<'s, 'r, 'l, 'cfg, P> {
    phase: Phase,
    connection: Option<Connection<'cfg>>,
    delivery: Option<Job<'s, 'r, 'l, P>>,
    listener: usize,
    peer: IpAddr,
    final_reply: bool,
}
impl<'cfg, P> Slot<'_, '_, '_, 'cfg, P> {
    fn connection(&mut self) -> Result<&mut Connection<'cfg>, Error> {
        self.connection.as_mut().ok_or(Error::Conflict)
    }
    fn retire(&mut self) {
        self.delivery.take();
        self.connection.take();
        self.phase = Phase::Empty;
        self.final_reply = false;
    }
}

/// Run with one immutable routing/TLS snapshot and a shared monotonic clock.
/// Startup storage checks precede readiness; no gateway or reload is enabled.
pub struct Receiving<'s, 'r, 'l, 'cfg, P> {
    pub coordinator: &'s StoreCoordinator<'r, 'l, P>,
    pub spool: &'s IngressSpool<'r>,
    pub crypto: &'s td_crypto::Provider,
    pub routes: &'cfg Routing<'cfg>,
    pub resources: &'cfg ResourcePlan,
    pub admission: &'cfg crate::admission::Plan,
    pub timeouts: &'cfg TimeoutPlan,
    pub policies: &'cfg GenerationLease<TlsPolicies>,
    pub handshakes: &'cfg HandshakePool,
    pub listeners: &'cfg [BoundListener<'cfg>],
    pub clock: Arc<dyn Clock>,
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

impl<'s, 'r, 'l, 'cfg, P: Sync> Receiving<'s, 'r, 'l, 'cfg, P> {
    pub fn run(&self, control: &Control) -> Result<(), Error> {
        control
            .state
            .compare_exchange(0, 5, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| Error::Conflict)?;
        let result = self.run_inner(control);
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

    fn run_inner(&self, control: &Control) -> Result<(), Error> {
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
        if total > limits.smtp_sessions
            || limits.message_bytes as u64 > self.spool.capacity().bytes_each
        {
            return Err(Error::Capacity);
        }
        let startup = after(
            self.clock.sample()?.monotonic,
            self.admission.work().admission_seconds,
        )?;
        loop {
            if startup.expired(self.clock.sample()?.monotonic) {
                return Err(Error::Deadline);
            }
            match self
                .coordinator
                .receiving_ready(self.routes.account(), startup)
            {
                Ok(()) => break,
                Err(Error::Busy) => std::thread::park_timeout(SCAN),
                Err(error) => return Err(error),
            }
            if control.stop.load(Ordering::Acquire) {
                return Ok(());
            }
        }
        let mut slots = Vec::new();
        slots
            .try_reserve_exact(total)
            .map_err(|_| Error::Capacity)?;
        let mut occupied = Vec::new();
        occupied
            .try_reserve_exact(total)
            .map_err(|_| Error::Capacity)?;
        for _ in 0..total {
            slots.push(Mutex::new(Slot {
                phase: Phase::Empty,
                connection: None,
                delivery: None,
                listener: 0,
                peer: IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED),
                final_reply: false,
            }));
            occupied.push(None);
        }
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
            let _finish = Finish {
                finished: &finished,
                storage: storage.thread(),
                tls: tls.thread(),
                control,
            };
            while started.load(Ordering::Acquire) != 2 {
                if storage.is_finished() || tls.is_finished() {
                    break;
                }
                std::thread::park_timeout(SCAN);
            }
            if started.load(Ordering::Acquire) == 2 && !control.stop.load(Ordering::Acquire) {
                control.transition(1);
            }
            let mut cursor = 0;
            let mut stopping = None;
            let mut accept_after = Tick(0);
            let result = (|| loop {
                if storage.is_finished() || tls.is_finished() {
                    control.fail();
                    break Err(Error::WriterStopped);
                }
                let now = match self.clock.sample() {
                    Ok(time) => time.monotonic,
                    Err(error) => {
                        control.fail();
                        break Err(error);
                    }
                };
                if control.stop.load(Ordering::Acquire) && stopping.is_none() {
                    if control.state() != State::Failed {
                        control.transition(2);
                    }
                    stopping = Some(after(now, 5)?);
                }
                let mut progress = false;
                for offset in 0..total {
                    let index = (cursor + offset) % total;
                    let Some(cell) = slots.get(index) else {
                        break;
                    };
                    let mut slot = match cell.try_lock() {
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
                        slot.phase = Phase::Retire;
                    }
                    if let Phase::Done = slot.phase {
                        slot.phase = Phase::Network;
                    }
                    if let Phase::Retry { work, deadline, at } = slot.phase {
                        if at <= now || deadline.expired(now) {
                            slot.phase = Phase::Queued { work, deadline };
                            progress = true;
                        }
                    }
                    if matches!(slot.phase, Phase::Network) {
                        if stopping.is_some_and(|deadline| deadline.expired(now)) && !final_reply {
                            slot.phase = Phase::Retire;
                        } else {
                            let turn = (|| {
                                let connection = slot.connection()?;
                                if stopping.is_some() {
                                    let _ = connection
                                        .network_mut()?
                                        .service_unavailable(self.clock.as_ref());
                                }
                                connection.advance(self.clock.as_ref())
                            })();
                            match turn {
                                Ok(Progress::Pending) => (),
                                Ok(Progress::Advanced) => progress = true,
                                Ok(Progress::Closed) | Err(_) => {
                                    slot.phase = Phase::Retire;
                                    progress = true;
                                }
                                Ok(Progress::Work) => {
                                    let dispatched = self.dispatch(&mut slot, now);
                                    if dispatched.is_err() {
                                        slot.phase = Phase::Retire;
                                    }
                                    progress = true;
                                }
                            }
                        }
                    }
                }
                cursor = (cursor + 1) % total;
                storage.thread().unpark();
                tls.thread().unpark();
                if stopping.is_some() && occupied.iter().all(Option::is_none) {
                    break if control.state() == State::Failed {
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
                                let peer = transport.peer_addr().ip();
                                let count = occupied
                                    .iter()
                                    .flatten()
                                    .filter(|(l, _)| *l == index)
                                    .count();
                                let peer_count = occupied
                                    .iter()
                                    .flatten()
                                    .filter(|(_, p)| *p == peer)
                                    .count();
                                let local_peer = occupied
                                    .iter()
                                    .flatten()
                                    .filter(|(l, p)| *l == index && *p == peer)
                                    .count();
                                if count >= listener.row.session_limit.unwrap_or(0)
                                    || peer_count >= limits.smtp_per_peer
                                    || local_peer >= listener.row.per_peer_limit.unwrap_or(0)
                                {
                                    let _ = transport.write(b"421 4.3.2 Service unavailable\r\n");
                                    transport.abort();
                                    continue;
                                }
                                let free = occupied
                                    .iter()
                                    .enumerate()
                                    .filter(|(_, entry)| entry.is_none())
                                    .find_map(|(index, _)| {
                                        slots.get(index).and_then(|cell| {
                                            cell.try_lock().ok().map(|slot| (index, slot))
                                        })
                                    });
                                let Some((free, mut slot)) = free else {
                                    let _ = transport.write(b"421 4.3.2 Service unavailable\r\n");
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
                                        let _ =
                                            transport.write(b"421 4.3.2 Service unavailable\r\n");
                                        transport.abort();
                                        continue;
                                    }
                                };
                                slot.connection = Some(Connection::Plain(network, transport));
                                slot.listener = index;
                                slot.peer = peer;
                                slot.phase = Phase::Network;
                                if let Some(entry) = occupied.get_mut(free) {
                                    *entry = Some((index, peer));
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
                    std::thread::park_timeout(SCAN);
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
                match cell.lock() {
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

    fn dispatch(&self, slot: &mut Slot<'s, 'r, 'l, 'cfg, P>, now: Tick) -> Result<(), Error> {
        let network = slot.connection()?.network()?;
        let (work, seconds) = match network.session().pending() {
            Pending::BeginData { .. } => (Work::Begin, self.admission.work().admission_seconds),
            Pending::Data(_) => (Work::Write, self.timeouts.execution_seconds()),
            Pending::Commit => (
                Work::Commit { prepared: false },
                self.admission.work().commit_seconds,
            ),
            Pending::StartTls { .. } => (Work::Tls, self.timeouts.limits().handshake_seconds),
            _ => return Err(Error::Conflict),
        };
        slot.phase = Phase::Queued {
            work,
            deadline: after(now, seconds)?.min(network.deadline()),
        };
        Ok(())
    }

    fn worker(
        &self,
        slots: &[Mutex<Slot<'s, 'r, 'l, 'cfg, P>>],
        tls: bool,
        finished: &AtomicBool,
        started: &AtomicU8,
        main: &std::thread::Thread,
        control: &Control,
    ) {
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
            let mut progress = false;
            for cell in slots {
                let mut slot = match cell.try_lock() {
                    Ok(slot) => slot,
                    Err(_) => continue,
                };
                match slot.phase {
                    Phase::Retire if !tls => {
                        slot.retire();
                        if self.coordinator.admission_stopped() {
                            control.fail();
                        }
                        progress = true;
                    }
                    Phase::Queued { work, deadline } if matches!(work, Work::Tls) == tls => {
                        if control.state() == State::Failed {
                            slot.phase = Phase::Retire;
                            progress = true;
                            continue;
                        }
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
                            if !tls && self.coordinator.admission_stopped() {
                                control.fail();
                            }
                        }
                        progress = true;
                    }
                    _ => (),
                }
            }
            if progress {
                main.unpark();
            } else {
                std::thread::park_timeout(SCAN);
            }
        }
        if !tls {
            for cell in slots {
                if let Ok(mut slot) = cell.lock() {
                    slot.retire();
                }
            }
            if self.coordinator.admission_stopped() {
                control.fail();
            }
        }
    }

    fn storage(
        &self,
        slot: &mut Slot<'s, 'r, 'l, 'cfg, P>,
        work: Work,
        cap: Deadline,
        random: &mut td_crypto::SystemEntropy,
        control: &Control,
    ) -> Result<(), Error> {
        let clock = self.clock.as_ref();
        let now = clock.sample()?.monotonic;
        if cap.expired(now) {
            return self.storage_refused(slot, work, Error::Deadline, control);
        }
        match work {
            Work::Begin => {
                let connection = slot.connection.as_ref().ok_or(Error::Conflict)?;
                let network = connection.network()?;
                let authorization = Direct {
                    access: Access {
                        account: self.routes.account(),
                        principal: Principal::Smtp,
                        config_generation: self.policies.id().get(),
                    },
                    peer: slot.peer,
                    tls: connection.tls()?,
                    clock: self.clock.clone(),
                };
                let result = self.coordinator.reserve_delivery(
                    self.crypto,
                    random,
                    self.spool,
                    authorization,
                    network.session(),
                    DeliveryRequest {
                        header_bytes: self.resources.limits().header_bytes,
                        deadline: network.deadline(),
                    },
                );
                match result {
                    Ok(job) => {
                        slot.delivery = Some(job);
                        if cap.expired(clock.sample()?.monotonic) {
                            return self.storage_refused(slot, work, Error::Deadline, control);
                        }
                        slot.connection()?
                            .network_mut()?
                            .data_ready(Ok(()), clock)?;
                    }
                    Err(DeliveryError::Storage(UploadError::Store(Error::Busy)))
                    | Err(DeliveryError::Storage(UploadError::Ledger(
                        crate::admission::logical::Error::Slot(crate::ownership::Error::Contended),
                    ))) => {
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
            Work::Write => {
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
                if cap.expired(clock.sample()?.monotonic) {
                    return self.storage_refused(slot, work, Error::Deadline, control);
                }
                slot.connection()?
                    .network_mut()?
                    .data_written(Ok(()), clock)?;
            }
            Work::Commit { prepared } => {
                let job = slot.delivery.as_mut().ok_or(Error::Conflict)?;
                if !prepared {
                    if let Err(error) = job.prepare(cap) {
                        return self.delivery_refused(slot, work, error, control);
                    }
                }
                match job.commit() {
                    Ok(completion) => {
                        // Preserve the native outcome even when accounting or cleanup failed.
                        let result = completion.outcome;
                        if completion.admission_stopped
                            || completion.cleanup_error.is_some()
                            || matches!(result, Err(CommitFailure::Indeterminate(_)))
                        {
                            control.fail();
                        }
                        slot.delivery.take();
                        slot.connection()?.network_mut()?.committed(result, clock)?;
                        slot.final_reply = true;
                    }
                    Err(DeliveryError::CoordinationBusy) => {
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
        if self.coordinator.admission_stopped() {
            control.fail();
        }
        slot.phase = Phase::Done;
        Ok(())
    }

    fn delivery_refused(
        &self,
        slot: &mut Slot<'s, 'r, 'l, 'cfg, P>,
        work: Work,
        error: DeliveryError,
        control: &Control,
    ) -> Result<(), Error> {
        if matches!(error, DeliveryError::HeaderLimit) {
            slot.delivery.take();
            slot.connection()?
                .network_mut()?
                .message_too_large(self.clock.as_ref())?;
            if self.coordinator.admission_stopped() {
                control.fail();
            }
            slot.phase = Phase::Done;
            return Ok(());
        }
        let error = match error {
            DeliveryError::Storage(UploadError::Store(error)) => error,
            _ => Error::Busy,
        };
        self.storage_refused(slot, work, error, control)
    }

    fn storage_refused(
        &self,
        slot: &mut Slot<'s, 'r, 'l, 'cfg, P>,
        work: Work,
        error: Error,
        control: &Control,
    ) -> Result<(), Error> {
        slot.delivery.take();
        let network = slot.connection()?.network_mut()?;
        match work {
            Work::Begin => network.data_ready(Err(error), self.clock.as_ref())?,
            Work::Write => network.data_written(Err(error), self.clock.as_ref())?,
            Work::Commit { .. } => {
                network.committed(Err(CommitFailure::Rejected(error)), self.clock.as_ref())?
            }
            Work::Tls => return Err(Error::Conflict),
        }
        if self.coordinator.admission_stopped() {
            control.fail();
        }
        slot.final_reply = matches!(work, Work::Commit { .. });
        slot.phase = Phase::Done;
        Ok(())
    }

    fn tls(&self, slot: &mut Slot<'s, 'r, 'l, 'cfg, P>, cap: Deadline) -> Result<(), Error> {
        let now = self.clock.sample()?.monotonic;
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

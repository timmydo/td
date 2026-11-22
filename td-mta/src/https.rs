//! Bounded HTTPS admission and authenticated mailbox-access routing.
use crate::{
    admission::timers::{self, TimeoutPlan},
    config::{
        endpoint::Origin,
        listener::{Bind, Kind, Listener},
    },
    generations::GenerationLease,
    http_auth::{self, AdmissionPeer, Attempts, PeerAddress},
    http_request::{self, HeadReader, HEAD_BYTES, QUANTUM},
    https_upload,
    limits::ResourcePlan,
    ports::{
        Clock, Deadline, Error, FlushProgress, Handshake, IoProgress, Tick, TlsPolicyId,
        TlsTransport, Transport,
    },
    smtp_receiving::Control,
    tls_admission::HandshakePool,
    tls_io::TLS_WIRE_BYTES,
    tls_policy::{PolicyRole, TlsConnection, TlsPolicies},
    transport::TcpTransport,
};
use std::{
    net::{TcpListener, TcpStream},
    sync::{
        atomic::{AtomicBool, AtomicU8, Ordering},
        Arc, TryLockError,
    },
    time::{Duration, Instant},
};

mod wait;
use wait::{Owner, Slot, Wait};

type Wire = Box<[u8; TLS_WIRE_BYTES]>;
const SCAN: Duration = Duration::from_millis(5);
const MAX_PER_PEER: usize = 16;
const EVENTS_PREFIX: &str = "/jmap/events?";
const DRAIN_BYTES: usize = 64 * 1024;
const DRAIN_SECONDS: u64 = 5;

/// A network peer leaves at least one slot, and one ordinary admission, for
/// another peer; `ordinary` is the profile's `https_connections`.
fn peer_limit(peer: PeerAddress, slots: usize, ordinary: usize) -> usize {
    match peer {
        PeerAddress::Network(_) => slots
            .saturating_sub(1)
            .min(ordinary.saturating_sub(1))
            .clamp(1, MAX_PER_PEER),
        PeerAddress::Local => slots.min(MAX_PER_PEER),
    }
}

fn has_peer_capacity(
    peer: PeerAddress,
    occupied: &[Option<AdmissionPeer>],
    ordinary: usize,
) -> bool {
    let key = peer.admission_peer();
    occupied.iter().filter(|entry| **entry == Some(key)).count()
        < peer_limit(peer, occupied.len(), ordinary)
}

pub struct BoundListener {
    socket: ListenerSocket,
    policy: Option<TlsPolicyId>,
}
// Sixteen cold listener slots retain their bounded path and lock inline.
#[allow(clippy::large_enum_variant)]
enum ListenerSocket {
    Tcp(TcpListener),
    Unix(crate::unix_listener::PrivateListener),
}
enum Accepted {
    Tcp(TcpStream),
    Unix(std::os::unix::net::UnixStream),
}
impl ListenerSocket {
    fn accept(&self) -> std::io::Result<(Accepted, PeerAddress)> {
        match self {
            Self::Tcp(listener) => listener.accept().map(|(stream, address)| {
                (Accepted::Tcp(stream), PeerAddress::Network(address.ip()))
            }),
            Self::Unix(listener) => listener
                .accept()
                .map(|stream| (Accepted::Unix(stream), PeerAddress::Local)),
        }
    }
}
impl BoundListener {
    pub fn new(
        socket: TcpListener,
        row: Listener<'_>,
        policies: &GenerationLease<TlsPolicies>,
    ) -> Result<Self, Error> {
        let bind = row.bind.tcp().ok_or(Error::Invalid)?;
        if row.kind != Kind::Https
            || (bind.is_ipv6() && !crate::transport::ipv6_only(&socket)?)
            || bind.port() == 0
            || socket.local_addr()? != bind
        {
            return Err(Error::Invalid);
        }
        let policy = TlsPolicies::bound_listener(policies, row)?;
        if TlsPolicies::role(policies, policy)? != PolicyRole::Https {
            return Err(Error::Forbidden);
        }
        socket.set_nonblocking(true)?;
        Ok(Self {
            socket: ListenerSocket::Tcp(socket),
            policy: Some(policy),
        })
    }
    pub fn local(row: Listener<'_>) -> Result<Self, Error> {
        if row.kind != Kind::LocalJmap || row.certificate.is_some() {
            return Err(Error::Invalid);
        }
        let Bind::Unix(path) = row.bind else {
            return Err(Error::Invalid);
        };
        let path = td_fs::SocketPath::new(std::path::Path::new(path))?;
        Ok(Self {
            socket: ListenerSocket::Unix(crate::unix_listener::PrivateListener::bind(path)?),
            policy: None,
        })
    }
    fn check_certificate(
        &self,
        policies: &GenerationLease<TlsPolicies>,
        utc_ms: i64,
    ) -> Result<(), Error> {
        if let Some(policy) = self.policy {
            TlsPolicies::check_receiving_certificate(policies, policy, utc_ms)?;
        }
        Ok(())
    }
}
#[derive(Clone, Copy, PartialEq)]
enum Phase {
    Hello,
    Tls,
    Head,
    Authorize,
    Authenticate,
    Upload,
    Download,
    Api,
    EventRead,
    Events,
    Response,
    Complete,
    Close,
    Drain,
}
struct Client {
    wait: Wait,
    timing: Option<crate::performance::Timer>,
    response_outcome: crate::performance::Outcome,
    keep_alive: bool,
    requests: usize,
    origin: usize,
    peer: PeerAddress,
    phase: Phase,
    pending: Option<TcpStream>,
    wire: Option<(Wire, Wire)>,
    policy: Option<TlsPolicyId>,
    connection: Option<Connection>,
    head: HeadReader<Box<[u8]>>,
    input: Box<[u8]>,
    input_used: usize,
    drain_left: usize,
    response: Vec<u8>,
    sent: usize,
    attempt: Option<http_auth::Attempt>,
    principal: Option<http_auth::Principal>,
    credentials: Option<http_auth::BasicCredentials>,
    authentication: Option<Result<Option<http_auth::Principal>, Error>>,
    body_slot: Option<BodyPermit>,
    request_slot: Option<CountPermit>,
    events: Option<EventState>,
    stop_after_response: bool,
    absolute: Deadline,
    phase_deadline: Deadline,
    idle: Deadline,
}
// Preserve the existing inline TLS reservation without another allocation.
#[allow(clippy::large_enum_variant)]
enum Connection {
    Tls(TlsConnection<Wire>),
    Local(crate::unix_transport::UnixTransport),
}
impl Connection {
    fn wait_handshake_input(&mut self, until: Instant) -> Result<bool, Error> {
        match self {
            Self::Tls(connection) => connection.wait_handshake_input(until),
            Self::Local(_) => Err(Error::Invalid),
        }
    }
    fn handshake(&mut self, deadline: Deadline) -> Result<Handshake, Error> {
        match self {
            Self::Tls(connection) => connection.handshake(deadline),
            Self::Local(_) => Err(Error::Invalid),
        }
    }
}
impl Transport for Connection {
    fn read(&mut self, bytes: &mut [u8]) -> Result<IoProgress, Error> {
        match self {
            Self::Tls(c) => c.read(bytes),
            Self::Local(c) => c.read(bytes),
        }
    }
    fn write(&mut self, bytes: &[u8]) -> Result<IoProgress, Error> {
        match self {
            Self::Tls(c) => c.write(bytes),
            Self::Local(c) => c.write(bytes),
        }
    }
    fn flush(&mut self) -> Result<FlushProgress, Error> {
        match self {
            Self::Tls(c) => c.flush(),
            Self::Local(c) => c.flush(),
        }
    }
    fn close(&mut self) -> Result<FlushProgress, Error> {
        match self {
            Self::Tls(c) => c.close(),
            Self::Local(c) => c.close(),
        }
    }
    fn abort(&mut self) {
        match self {
            Self::Tls(c) => c.abort(),
            Self::Local(c) => c.abort(),
        }
    }
}
const EVENT_SAMPLE_MS: u64 = 5_000;

struct EventSample {
    observed: Tick,
    stamp: [u8; 32],
    snapshot: crate::jmap_events::Snapshot,
    expires: Deadline,
}
struct EventState {
    options: crate::jmap_events::Options,
    stream: Option<crate::jmap_events::Stream>,
    sample: Option<Result<EventSample, Error>>,
    headers: bool,
    pending: bool,
    last: Tick,
}
struct ApiWorker<'a> {
    account: crate::ids::AccountId,
    origin: usize,
    worker: Option<Box<dyn crate::https_api::Worker + 'a>>,
}
fn clear_head(client: &mut Client) {
    let previous = std::mem::replace(&mut client.head, HeadReader::new(Box::<[u8]>::default()));
    client.head = HeadReader::new(previous.into_storage());
}
impl Client {
    fn deadline(&self) -> Deadline {
        self.absolute.min(self.phase_deadline).min(self.idle)
    }
}
impl Drop for Client {
    fn drop(&mut self) {
        self.input.fill(0);
        std::hint::black_box(&mut self.input);
    }
}
#[derive(Clone, Copy, PartialEq)]
enum BodyKind {
    Upload,
    Download,
    Api,
}
/// Every pool counter, in one shared allocation; permits name their counter.
#[derive(Default)]
struct Counters {
    active: AtomicU8,
    transfers: AtomicU8,
    /// Connections serving ordinary requests; push streams have their own slots.
    requests: AtomicU8,
    uploading: AtomicBool,
}
#[derive(Clone, Copy)]
enum Counter {
    Active,
    Transfers,
    Requests,
}
impl Counters {
    fn count(&self, counter: Counter) -> &AtomicU8 {
        match counter {
            Counter::Active => &self.active,
            Counter::Transfers => &self.transfers,
            Counter::Requests => &self.requests,
        }
    }
}
struct BodyPool {
    counters: Arc<Counters>,
    maximum: u8,
    request_maximum: u8,
}
impl BodyPool {
    /// Held from a connection's first ordinary request until it closes or
    /// becomes an event stream, so streams cannot take ordinary capacity.
    fn admit_request(&self, slot: &mut Option<CountPermit>) -> bool {
        if slot.is_none() {
            *slot = CountPermit::acquire(&self.counters, Counter::Requests, self.request_maximum);
        }
        slot.is_some()
    }
    fn admit(&self, slot: &mut Option<BodyPermit>, kind: BodyKind) -> bool {
        if slot.is_some() {
            return false;
        }
        let upload = if kind == BodyKind::Upload {
            if self
                .counters
                .uploading
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
            {
                return false;
            }
            Some(UploadPermit(self.counters.clone()))
        } else {
            None
        };
        let transfer = if kind != BodyKind::Api {
            let maximum = self.maximum.saturating_sub(1).max(1);
            let Some(permit) = CountPermit::acquire(&self.counters, Counter::Transfers, maximum)
            else {
                return false;
            };
            Some(permit)
        } else {
            None
        };
        let Some(active) = CountPermit::acquire(&self.counters, Counter::Active, self.maximum)
        else {
            return false;
        };
        *slot = Some(BodyPermit {
            _active: active,
            _transfer: transfer,
            _upload: upload,
        });
        true
    }
}
struct CountPermit(Arc<Counters>, Counter);
impl CountPermit {
    fn acquire(counters: &Arc<Counters>, which: Counter, maximum: u8) -> Option<Self> {
        let counter = counters.count(which);
        let mut active = counter.load(Ordering::Acquire);
        for _ in 0..4 {
            if active >= maximum {
                return None;
            }
            match counter.compare_exchange(active, active + 1, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => return Some(Self(counters.clone(), which)),
                Err(current) => active = current,
            }
        }
        None
    }
}
impl Drop for CountPermit {
    fn drop(&mut self) {
        self.0.count(self.1).fetch_sub(1, Ordering::AcqRel);
    }
}
struct UploadPermit(Arc<Counters>);
impl Drop for UploadPermit {
    fn drop(&mut self) {
        self.0.uploading.store(false, Ordering::Release);
    }
}
struct BodyPermit {
    _active: CountPermit,
    _transfer: Option<CountPermit>,
    _upload: Option<UploadPermit>,
}
struct BodyFinish<'a, 'scope> {
    done: &'a AtomicBool,
    workers: &'a [std::thread::ScopedJoinHandle<'scope, ()>],
}
impl Drop for BodyFinish<'_, '_> {
    fn drop(&mut self) {
        self.done.store(true, Ordering::Release);
        for worker in self.workers {
            worker.thread().unpark();
        }
    }
}

struct Finish<'a> {
    done: &'a AtomicBool,
    worker: &'a std::thread::Thread,
}
impl Drop for Finish<'_> {
    fn drop(&mut self) {
        self.done.store(true, Ordering::Release);
        self.worker.unpark();
    }
}

struct WorkerExit<'a>(&'a Control);
impl Drop for WorkerExit<'_> {
    fn drop(&mut self) {
        if std::thread::panicking() {
            self.0.fail();
        }
    }
}
struct AuthenticationFinish<'a> {
    authentication: &'a dyn http_auth::Authenticator,
    control: &'a Control,
}
impl Drop for AuthenticationFinish<'_> {
    fn drop(&mut self) {
        if self.authentication.close().is_err() {
            self.control.fail();
        }
    }
}

pub struct Https<'a> {
    pub listeners: &'a [BoundListener],
    pub policies: &'a GenerationLease<TlsPolicies>,
    pub handshakes: &'a HandshakePool,
    pub authentication: &'a dyn http_auth::Authenticator,
    pub origins: &'a [Origin<'a>],
    pub resources: &'a ResourcePlan,
    pub timeouts: &'a TimeoutPlan,
    pub clock: Arc<dyn Clock>,
    pub backends: &'a [&'a dyn https_upload::Backend],
    pub ready: &'a AtomicBool,
}
impl Https<'_> {
    fn backend_origin<'b>(
        &'b self,
        backend: &'b dyn https_upload::Backend,
    ) -> Result<Origin<'b>, Error> {
        match backend.session() {
            Some(session) => session.origin(),
            None => self.origins.first().copied().ok_or(Error::Invalid),
        }
    }
    fn backend(
        &self,
        account: crate::ids::AccountId,
        selected: usize,
    ) -> Option<&dyn https_upload::Backend> {
        let origin = self.origins.get(selected)?;
        self.backends.iter().copied().find(|backend| {
            backend.account() == account
                && self
                    .backend_origin(*backend)
                    .is_ok_and(|value| value.same_origin(*origin))
        })
    }
    fn request_origin(&self, head: &http_request::Head<'_>) -> Result<usize, u16> {
        let (authority, scheme) = if let Some(target) = head.target_origin {
            let (scheme, authority) = target.split_once("://").ok_or(421u16)?;
            (authority, Some(scheme))
        } else {
            (head.host, None)
        };
        let index = self
            .origins
            .iter()
            .position(|origin| http_auth::same_authority(*origin, authority))
            .ok_or(421u16)?;
        let origin = self.origins.get(index).ok_or(421u16)?;
        if scheme.is_some_and(|scheme| {
            !scheme.eq_ignore_ascii_case(if origin.is_local() { "http" } else { "https" })
        }) {
            return Err(421);
        }
        if head.origin.is_some_and(|value| {
            !Origin::parse_jmap(value).is_ok_and(|value| origin.same_origin(value))
        }) {
            return Err(403);
        }
        Ok(index)
    }
    pub fn run(&self, control: &Control) -> Result<(), Error> {
        if self.listeners.is_empty()
            || self.listeners.len() > crate::config::listener::MAX_LISTENERS
            || self.handshakes.capacity() != self.resources.limits().tls_handshakes
        {
            return Err(Error::Invalid);
        }
        if self.origins.is_empty()
            || self.origins.len() > crate::config::endpoint::MAX_JMAP_ORIGINS
            || self.origins.iter().enumerate().any(|(index, origin)| {
                self.origins
                    .iter()
                    .take(index)
                    .any(|prior| prior.same_origin(*origin))
            })
            || self.backends.len() > crate::config::identities::MAX_ACCOUNTS * self.origins.len()
        {
            return Err(Error::Invalid);
        }
        if self.listeners.iter().any(|listener| {
            self.origins
                .iter()
                .any(|origin| origin.is_local() != listener.policy.is_none())
        }) || (self.origins.iter().any(|origin| origin.is_local()) && self.origins.len() != 1)
        {
            return Err(Error::Invalid);
        }
        let mut accounts = 0;
        for (index, backend) in self.backends.iter().copied().enumerate() {
            let origin = self.backend_origin(backend)?;
            if !self
                .origins
                .iter()
                .any(|allowed| allowed.same_origin(origin))
            {
                return Err(Error::Invalid);
            }
            let mut seen_account = false;
            for prior in self.backends.iter().copied().take(index) {
                if prior.account() == backend.account() {
                    seen_account = true;
                    if self.backend_origin(prior)?.same_origin(origin) {
                        return Err(Error::Invalid);
                    }
                }
            }
            accounts += usize::from(!seen_account);
        }
        if accounts > crate::config::identities::MAX_ACCOUNTS {
            return Err(Error::Invalid);
        }
        for session in self.backends.iter().filter_map(|backend| backend.session()) {
            if self.resources.limits().event_streams == 0
                || session.concurrency().requests != self.resources.https_body_workers()
                || session.concurrency().uploads != 1
            {
                return Err(Error::Invalid);
            }
            crate::jmap_request::Request::validate_resources(self.resources)
                .map_err(|_| Error::Invalid)?;
        }
        let limits = self.resources.limits();
        let count = limits
            .https_connections
            .checked_add(limits.event_streams)
            .filter(|count| *count <= crate::limits::MAX_HTTPS_SLOTS)
            .ok_or(Error::Invalid)?;
        let request_maximum = u8::try_from(limits.https_connections).map_err(|_| Error::Invalid)?;
        let mut slots: Vec<Slot> = Vec::new();
        slots
            .try_reserve_exact(count)
            .map_err(|_| Error::Capacity)?;
        slots.extend((0..count).map(|_| Slot::empty()));
        let done = AtomicBool::new(false);
        let body_count =
            u8::try_from(self.resources.https_body_workers()).map_err(|_| Error::Invalid)?;
        if body_count == 0 {
            return Err(Error::Capacity);
        }
        let body_active = BodyPool {
            counters: Arc::new(Counters::default()),
            maximum: body_count,
            request_maximum,
        };
        let body_warm = AtomicU8::new(0);
        std::thread::scope(|scope| {
            let mut tls_workers = Vec::new();
            tls_workers
                .try_reserve_exact(self.handshakes.capacity())
                .map_err(|_| Error::Capacity)?;
            for index in 0..self.handshakes.capacity() {
                match std::thread::Builder::new()
                    .name(format!("https-tls-{index}"))
                    .spawn_scoped(scope, || {
                        let _exit = WorkerExit(control);
                        self.tls_worker(&slots, &done, control)
                    }) {
                    Ok(worker) => tls_workers.push(worker),
                    Err(error) => {
                        done.store(true, Ordering::Release);
                        for worker in tls_workers {
                            worker.thread().unpark();
                            let _ = worker.join();
                        }
                        return Err(error.into());
                    }
                }
            }
            let finish = BodyFinish {
                done: &done,
                workers: &tls_workers,
            };
            let mut uploaders: Vec<std::thread::ScopedJoinHandle<'_, ()>> = Vec::new();
            uploaders
                .try_reserve_exact(usize::from(body_count))
                .map_err(|_| Error::Capacity)?;
            for index in 0..body_count {
                match std::thread::Builder::new()
                    .name(format!("https-body-{index}"))
                    .spawn_scoped(scope, || {
                        let _exit = WorkerExit(control);
                        self.body_worker(&slots, &done, &body_warm, control)
                    }) {
                    Ok(worker) => uploaders.push(worker),
                    Err(error) => {
                        drop(finish);
                        for uploader in uploaders {
                            uploader.thread().unpark();
                            let _ = uploader.join();
                        }
                        for worker in tls_workers {
                            let _ = worker.join();
                        }
                        return Err(error.into());
                    }
                }
            }
            let upload_finish = BodyFinish {
                done: &done,
                workers: &uploaders,
            };
            let verifier = match std::thread::Builder::new()
                .name("https-auth".into())
                .spawn_scoped(scope, || {
                    let _exit = WorkerExit(control);
                    let _finish = AuthenticationFinish {
                        authentication: self.authentication,
                        control,
                    };
                    let mut maintained = Instant::now();
                    let mut event_cursor = 0;
                    while !done.load(Ordering::Acquire) {
                        if control.stop_requested() {
                            std::thread::park_timeout(SCAN);
                            continue;
                        }
                        if maintained.elapsed() >= Duration::from_secs(1) {
                            match self.authentication.maintain() {
                                Ok(()) | Err(Error::Busy) => (),
                                Err(_) => {
                                    control.fail();
                                    return;
                                }
                            }
                            maintained = Instant::now();
                        }
                        let mut next_event = None;
                        let mut retry = false;
                        for (index, cell) in slots
                            .iter()
                            .enumerate()
                            .cycle()
                            .skip(event_cursor)
                            .take(slots.len())
                        {
                            if cell.owned(Owner::Event) {
                                if next_event.is_none() {
                                    next_event = Some((index, cell));
                                } else {
                                    retry = true;
                                }
                                continue;
                            }
                            if !cell.owned(Owner::Auth) {
                                continue;
                            }
                            let mut slot = match cell.client.try_lock() {
                                Ok(slot) => slot,
                                Err(TryLockError::WouldBlock) => {
                                    retry = true;
                                    continue;
                                }
                                Err(_) => {
                                    control.fail();
                                    return;
                                }
                            };
                            let Some(client) =
                                slot.as_mut().filter(|c| c.phase == Phase::Authenticate)
                            else {
                                continue;
                            };
                            if let Err(error) = self.authenticate(client) {
                                if error == Error::WriterStopped {
                                    control.fail();
                                    return;
                                }
                                *slot = None;
                            }
                            if slot
                                .as_ref()
                                .is_some_and(|c| c.phase == Phase::Authenticate)
                            {
                                retry = true;
                            } else {
                                cell.leave_worker(slot, control);
                            }
                        }
                        // Check all waiting sign-ins between any two stream samples.
                        if let Some((index, cell)) = next_event
                            .filter(|_| !control.stop_requested() && !done.load(Ordering::Acquire))
                        {
                            event_cursor = (index + 1) % slots.len();
                            if let Ok(mut slot) = cell.client.try_lock() {
                                if let Some(client) =
                                    slot.as_mut().filter(|c| c.phase == Phase::EventRead)
                                {
                                    let sample = self.event_sample(client);
                                    if let Some(event) = &mut client.events {
                                        event.sample = Some(sample);
                                    }
                                    client.phase = Phase::Events;
                                    cell.leave_worker(slot, control);
                                }
                            } else {
                                retry = true;
                            }
                        }
                        std::thread::park_timeout(if retry {
                            SCAN
                        } else {
                            Duration::from_secs(1).saturating_sub(maintained.elapsed())
                        });
                    }
                }) {
                Ok(worker) => worker,
                Err(error) => {
                    drop(upload_finish);
                    drop(finish);
                    for uploader in uploaders {
                        let _ = uploader.join();
                    }
                    for worker in tls_workers {
                        let _ = worker.join();
                    }
                    return Err(error.into());
                }
            };
            let auth_finish = Finish {
                done: &done,
                worker: verifier.thread(),
            };
            while body_warm.load(Ordering::Acquire) < body_count
                && uploaders.iter().all(|worker| !worker.is_finished())
                && !control.stop_requested()
            {
                std::thread::park_timeout(SCAN);
            }
            let result = if control.stop_requested() {
                Ok(())
            } else if body_warm.load(Ordering::Acquire) == body_count {
                self.network(
                    &slots,
                    &tls_workers,
                    &uploaders,
                    &verifier,
                    &body_active,
                    control,
                )
            } else {
                Err(Error::WriterStopped)
            };
            if result.is_err() {
                control.fail();
            }
            drop(auth_finish);
            drop(upload_finish);
            drop(finish);
            if verifier.join().is_err() {
                control.fail();
                return Err(Error::WriterStopped);
            }
            for uploader in uploaders {
                if uploader.join().is_err() {
                    control.fail();
                    return Err(Error::WriterStopped);
                }
            }
            for worker in tls_workers {
                if worker.join().is_err() {
                    control.fail();
                    return Err(Error::WriterStopped);
                }
            }
            let retired = slots.iter().any(|slot| {
                slot.client.lock().map_or(true, |slot| {
                    slot.as_ref()
                        .is_some_and(|client| client.stop_after_response)
                })
            });
            if retired || control.state() == crate::smtp_receiving::State::Failed {
                control.fail();
                return Err(Error::WriterStopped);
            }
            result
        })
    }
    fn body_worker(&self, slots: &[Slot], done: &AtomicBool, warm: &AtomicU8, control: &Control) {
        if td_crypto::SystemEntropy::try_new().is_err() {
            control.fail();
            return;
        }
        warm.fetch_add(1, Ordering::Release);
        let mut api_worker: Option<ApiWorker<'_>> = None;
        let mut maintained = Instant::now();
        while !done.load(Ordering::Acquire) {
            if maintained.elapsed() >= Duration::from_secs(1) {
                if let Some(worker) = api_worker.as_mut().and_then(|slot| slot.worker.as_mut()) {
                    if worker.maintain().is_err() {
                        api_worker = None;
                    }
                }
                maintained = Instant::now();
            }
            for cell in slots {
                if !cell.owned(Owner::Body) {
                    continue;
                }
                let mut slot = match cell.client.try_lock() {
                    Ok(slot) => slot,
                    Err(TryLockError::WouldBlock) => continue,
                    Err(_) => {
                        control.fail();
                        return;
                    }
                };
                let Some(client) = slot.as_mut().filter(|client| {
                    matches!(client.phase, Phase::Upload | Phase::Download | Phase::Api)
                }) else {
                    continue;
                };
                let result = if client.body_slot.is_none() {
                    Err(Error::Invalid)
                } else if client.phase == Phase::Download {
                    self.download(client, control, &mut api_worker)
                } else if client.phase == Phase::Api {
                    self.api(client, control, &mut api_worker)
                } else {
                    api_worker = None;
                    self.upload(client, control)
                };
                if result.is_err() {
                    if client.stop_after_response {
                        control.fail();
                    }
                    *slot = None;
                }
                cell.leave_worker(slot, control);
            }
            std::thread::park_timeout(Duration::from_secs(1).saturating_sub(maintained.elapsed()));
        }
    }
    fn authenticate(&self, client: &mut Client) -> Result<(), Error> {
        let mut timing = crate::performance::Timer::start(crate::performance::Phase::HttpAuth);
        let before = self.clock.sample()?.monotonic;
        let deadline = client.absolute.min(client.phase_deadline).min(client.idle);
        if deadline.expired(before) {
            return Err(Error::Deadline);
        }
        if client.credentials.is_none() {
            let head = client.head.head().map_err(|_| Error::Invalid)?;
            client.credentials = http_auth::parse_basic(head.authorization);
        }
        let result = match client.credentials.as_ref() {
            Some(credentials) => self.authentication.authenticate(
                credentials.username(),
                credentials.password(),
                deadline,
            ),
            None => Ok(None),
        };
        let after = self.clock.sample()?.monotonic;
        if after < before {
            return Err(Error::Invalid);
        }
        if deadline.expired(after) {
            return Err(Error::Deadline);
        }
        match result {
            Err(Error::Busy) => {
                timing.finish(crate::performance::Outcome::Busy);
                return Ok(());
            }
            Err(Error::WriterStopped) => return Err(Error::WriterStopped),
            _ => (),
        }
        if matches!(result, Ok(Some(_))) {
            timing.succeeded();
        }
        client.authentication = Some(result);
        client.phase = Phase::Authorize;
        Ok(())
    }
    fn tls_worker(&self, slots: &[Slot], done: &AtomicBool, control: &Control) {
        // Network owns shutdown: an early worker exit would look like failure.
        while !done.load(Ordering::Acquire) {
            let mut retry = false;
            for cell in slots {
                if !cell.owned(Owner::Tls) {
                    continue;
                }
                let mut slot = match cell.client.try_lock() {
                    Ok(slot) => slot,
                    Err(TryLockError::WouldBlock) => {
                        retry = true;
                        continue;
                    }
                    Err(_) => {
                        control.fail();
                        return;
                    }
                };
                let Some(client) = slot.as_mut().filter(|client| client.phase == Phase::Tls) else {
                    continue;
                };
                let result = (|| {
                    loop {
                        if done.load(Ordering::Acquire) || control.stop_requested() {
                            return Err(Error::WriterStopped);
                        }
                        self.tls(client)?;
                        if client.phase != Phase::Tls {
                            return Ok(());
                        }
                        let Some(connection) = client.connection.as_mut() else {
                            return Ok(());
                        };
                        let now = self.clock.monotonic()?;
                        let remaining = client
                            .phase_deadline
                            .min(client.absolute)
                            .tick()
                            .0
                            .checked_sub(now.0)
                            .filter(|n| *n != 0)
                            .ok_or(Error::Deadline)?;
                        let until = Instant::now() + Duration::from_millis(remaining.min(50));
                        if !connection.wait_handshake_input(until)? {
                            // Pending TLS output gets another bounded turn; avoid spinning
                            // when a peer is not draining its receive window.
                            // The outer worker park owns its unpark token.
                            std::thread::sleep(Duration::from_millis(1));
                        }
                    }
                })();
                if result.is_err() {
                    *slot = None;
                }
                if slot.as_ref().is_some_and(|c| c.phase == Phase::Tls) {
                    retry = true;
                } else {
                    cell.leave_worker(slot, control);
                }
            }
            if done.load(Ordering::Acquire) {
                break;
            }
            if retry {
                std::thread::park_timeout(SCAN);
            } else {
                std::thread::park();
            }
        }
    }
    fn tls(&self, client: &mut Client) -> Result<(), Error> {
        let now = self.clock.sample()?.monotonic;
        if client.phase_deadline.expired(now) {
            return Err(Error::Deadline);
        }
        if client.pending.is_some() {
            if client.timing.is_none() {
                client.timing = Some(crate::performance::Timer::start(
                    crate::performance::Phase::HttpTls,
                ));
            }
            let (input, output) = client.wire.take().ok_or(Error::Invalid)?;
            let prepared = match TlsPolicies::reserve_session(
                self.policies.clone(),
                client.policy.ok_or(Error::Invalid)?,
                self.handshakes,
                input,
                output,
            ) {
                Ok(prepared) => prepared,
                Err(refusal) => {
                    let error = refusal.error();
                    client.wire = Some(refusal.into_buffers());
                    return if error == Error::Busy {
                        Ok(())
                    } else {
                        Err(error)
                    };
                }
            };
            let plain = TcpTransport::from_stream(client.pending.take().ok_or(Error::Invalid)?)?;
            let reserved = prepared.construct().map_err(|refusal| refusal.error())?;
            client.connection = Some(Connection::Tls(
                reserved
                    .handoff(
                        plain,
                        &[],
                        self.clock.clone(),
                        client.absolute,
                        client.phase_deadline,
                    )
                    .map_err(|refusal| refusal.error())?,
            ));
        }
        let connection = client.connection.as_mut().ok_or(Error::Invalid)?;
        let result = connection.handshake(client.phase_deadline)?;
        let now = self.clock.sample()?.monotonic;
        if matches!(result, Handshake::Complete(_)) {
            if let Some(timing) = client.timing.take() {
                timing.finish(crate::performance::Outcome::Success);
            }
            client.phase = Phase::Head;
            client.phase_deadline =
                deadline(now, self.timeouts.limits().header_seconds)?.min(client.absolute);
            client.idle = deadline(now, self.timeouts.limits().header_idle_seconds)?
                .min(client.phase_deadline);
        }
        Ok(())
    }
    fn network(
        &self,
        slots: &[Slot],
        tls_workers: &[std::thread::ScopedJoinHandle<'_, ()>],
        uploaders: &[std::thread::ScopedJoinHandle<'_, ()>],
        verifier: &std::thread::ScopedJoinHandle<'_, ()>,
        body_active: &BodyPool,
        control: &Control,
    ) -> Result<(), Error> {
        let receiver = control.https_wake.attach()?;
        for listener in self.listeners {
            listener.check_certificate(self.policies, self.clock.sample()?.utc_ms)?;
        }
        self.ready.store(true, Ordering::Release);
        let mut attempts = Attempts::default();
        let mut peers = [None; crate::limits::MAX_HTTPS_SLOTS];
        let peers = peers.get_mut(..slots.len()).ok_or(Error::Invalid)?;
        let mut listener_cursor = 0;
        let mut cursor = 0;
        let mut accept_after = Tick(0);
        let mut event_count = 0usize;
        while !control.stop_requested() {
            receiver.consume()?;
            if tls_workers.iter().any(|worker| worker.is_finished())
                || uploaders.iter().any(|worker| worker.is_finished())
                || verifier.is_finished()
            {
                control.fail();
                return Err(Error::WriterStopped);
            }
            let time = self.clock.sample()?;
            for listener in self.listeners {
                listener.check_certificate(self.policies, time.utc_ms)?;
            }
            for offset in 0..slots.len() {
                let index = (cursor + offset) % slots.len();
                let Some(cell) = slots.get(index) else {
                    return Err(Error::Invalid);
                };
                let guard = if cell.owned(Owner::Event) {
                    cell.try_client()?
                } else {
                    cell.network_client()?
                };
                let Some(mut slot) = guard else {
                    continue;
                };
                let Some(client) = slot.as_mut() else {
                    *peers.get_mut(index).ok_or(Error::Invalid)? = None;
                    continue;
                };
                if matches!(
                    client.phase,
                    Phase::Tls | Phase::Authenticate | Phase::Upload | Phase::Download | Phase::Api
                ) || (client.phase == Phase::EventRead
                    && !client.deadline().expired(time.monotonic))
                {
                    continue;
                }
                client.wait = Wait::Again;
                let before = match client.connection.as_mut() {
                    Some(Connection::Tls(c)) => Some(c.begin_turn()),
                    _ => None,
                };
                let mut wake_body = false;
                let mut wake_auth = false;
                match self.step(
                    client,
                    &mut attempts,
                    body_active,
                    &mut event_count,
                    control,
                ) {
                    Ok(true) => {
                        if let (Wait::Socket { .. }, Some(Connection::Tls(c)), Some(before)) =
                            (client.wait, client.connection.as_ref(), before)
                        {
                            let turn = c.wire_turn(before);
                            client.wait = if turn.moved {
                                Wait::Again
                            } else {
                                Wait::Socket {
                                    read: turn.read_blocked,
                                    write: turn.write_blocked,
                                }
                            };
                        }
                        cell.assign(Owner::phase(client.phase));
                        wake_auth = matches!(client.phase, Phase::Authenticate | Phase::EventRead);
                        if matches!(client.phase, Phase::Upload | Phase::Download | Phase::Api) {
                            wake_body = true;
                        }
                        if matches!(client.phase, Phase::Close | Phase::Response)
                            && client.events.take().is_some()
                        {
                            event_count = event_count.checked_sub(1).ok_or(Error::Invalid)?;
                        }
                        if client.phase == Phase::Close
                            && client.body_slot.is_some()
                            && !client.stop_after_response
                        {
                            client.body_slot.take();
                        }
                    }
                    Err(Error::WriterStopped) => return Err(Error::WriterStopped),
                    Ok(false) | Err(_) => {
                        if client.stop_after_response {
                            return Err(Error::WriterStopped);
                        }
                        if client.events.take().is_some() {
                            event_count = event_count.checked_sub(1).ok_or(Error::Invalid)?;
                        }
                        *slot = None;
                        cell.assign(Owner::Network);
                    }
                }
                let wake_tls = cell.owned(Owner::Tls);
                drop(slot);
                if wake_tls {
                    for worker in tls_workers {
                        worker.thread().unpark();
                    }
                }
                if wake_body {
                    for uploader in uploaders {
                        uploader.thread().unpark();
                    }
                }
                if wake_auth {
                    verifier.thread().unpark();
                }
            }
            cursor = (cursor + 1) % slots.len();
            for listener in self
                .listeners
                .iter()
                .cycle()
                .skip(listener_cursor)
                .take(self.listeners.len())
            {
                if time.monotonic < accept_after {
                    break;
                }
                let free = slots.iter().enumerate().find_map(|(index, cell)| {
                    if !cell.owned(Owner::Network) {
                        return None;
                    }
                    let slot = cell.client.try_lock().ok()?;
                    slot.is_none().then_some((index, slot))
                });
                let Some((index, mut slot)) = free else {
                    break;
                };
                let (socket, peer) = match listener.socket.accept() {
                    Ok(pair) => pair,
                    Err(error)
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::WouldBlock
                                | std::io::ErrorKind::Interrupted
                                | std::io::ErrorKind::ConnectionAborted
                                | std::io::ErrorKind::ConnectionReset
                        ) || matches!(
                            error.raw_os_error(),
                            Some(64 | 71 | 92 | 95 | 100 | 101 | 112 | 113)
                        ) =>
                    {
                        continue
                    }
                    Err(error) if matches!(error.raw_os_error(), Some(12 | 23 | 24 | 105)) => {
                        accept_after =
                            Tick(time.monotonic.0.checked_add(5000).ok_or(Error::Invalid)?);
                        break;
                    }
                    Err(error) => return Err(error.into()),
                };
                *peers.get_mut(index).ok_or(Error::Invalid)? = None;
                if !has_peer_capacity(peer, peers, self.resources.limits().https_connections) {
                    control.log(
                        crate::observability::Kind::AdmissionRefused {
                            reason: crate::observability::Refusal::Capacity,
                        },
                        None,
                        None,
                    );
                    continue;
                }
                let allocated = (|| {
                    let wire = if matches!(socket, Accepted::Tcp(_)) {
                        Some((buffer::<TLS_WIRE_BYTES>()?, buffer::<TLS_WIRE_BYTES>()?))
                    } else {
                        None
                    };
                    let head: Box<[u8]> = buffer::<HEAD_BYTES>()?;
                    let input_plain: Box<[u8]> = buffer::<QUANTUM>()?;
                    let mut response = Vec::new();
                    response
                        .try_reserve_exact(512)
                        .map_err(|_| Error::Capacity)?;
                    Ok::<_, Error>((wire, head, input_plain, response))
                })();
                let (wire, head, input_plain, response) = match allocated {
                    Ok(buffers) => buffers,
                    Err(_) => {
                        accept_after =
                            Tick(time.monotonic.0.checked_add(5000).ok_or(Error::Invalid)?);
                        continue;
                    }
                };
                let now = self.clock.sample()?.monotonic;
                let absolute = deadline(now, timers::SESSION_SECONDS)?;
                let (phase, pending, connection, seconds) = match socket {
                    Accepted::Tcp(socket) => {
                        if socket.set_nonblocking(true).is_err() {
                            continue;
                        }
                        (
                            Phase::Hello,
                            Some(socket),
                            None,
                            self.timeouts.limits().handshake_seconds,
                        )
                    }
                    Accepted::Unix(socket) => {
                        let connection =
                            match crate::unix_transport::UnixTransport::from_stream(socket) {
                                Ok(connection) => connection,
                                Err(_) => continue,
                            };
                        (
                            Phase::Head,
                            None,
                            Some(Connection::Local(connection)),
                            self.timeouts.limits().header_seconds,
                        )
                    }
                };
                let phase_deadline = deadline(now, seconds)?.min(absolute);
                let idle = if phase == Phase::Head {
                    deadline(now, self.timeouts.limits().header_idle_seconds)?.min(phase_deadline)
                } else {
                    phase_deadline
                };
                let client = Client {
                    wait: Wait::Again,
                    timing: None,
                    response_outcome: crate::performance::Outcome::Failed,
                    keep_alive: false,
                    requests: 0,
                    origin: 0,
                    peer,
                    phase,
                    pending,
                    wire,
                    policy: listener.policy,
                    connection,
                    head: HeadReader::new(head),
                    input: input_plain,
                    input_used: 0,
                    drain_left: DRAIN_BYTES,
                    response,
                    sent: 0,
                    attempt: None,
                    principal: None,
                    credentials: None,
                    authentication: None,
                    body_slot: None,
                    request_slot: None,
                    events: None,
                    stop_after_response: false,
                    absolute,
                    phase_deadline,
                    idle,
                };
                *peers.get_mut(index).ok_or(Error::Invalid)? = Some(peer.admission_peer());
                *slot = Some(client);
            }
            listener_cursor = (listener_cursor + 1) % self.listeners.len();
            if !control.stop_requested() {
                wait::wait(
                    &receiver,
                    slots,
                    self.listeners,
                    accept_after,
                    self.clock.as_ref(),
                )?;
            }
        }
        Ok(())
    }
    fn step(
        &self,
        client: &mut Client,
        attempts: &mut Attempts,
        body_active: &BodyPool,
        event_count: &mut usize,
        control: &Control,
    ) -> Result<bool, Error> {
        let time = self.clock.sample()?;
        if client.deadline().expired(time.monotonic) {
            return Ok(false);
        }
        match client.phase {
            Phase::Hello => {
                match client
                    .pending
                    .as_ref()
                    .ok_or(Error::Invalid)?
                    .peek(&mut [0])
                {
                    Ok(0) => return Ok(false),
                    Ok(_) => client.phase = Phase::Tls,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        client.wait = Wait::READ
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => (),
                    Err(e) => return Err(e.into()),
                }
            }
            Phase::Head => {
                let progress = if client.input_used != 0 {
                    IoProgress::Bytes(client.input_used)
                } else {
                    client
                        .connection
                        .as_mut()
                        .ok_or(Error::Invalid)?
                        .read(&mut client.input)?
                };
                match progress {
                    IoProgress::Pending => client.wait = Wait::READ,
                    IoProgress::Closed => return Ok(false),
                    IoProgress::Bytes(n) => {
                        let result = client
                            .head
                            .feed(client.input.get(..n).ok_or(Error::Invalid)?);
                        client.input_used = match result {
                            Ok(progress) => {
                                let tail = retain_tail(&mut client.input, n, progress.consumed)?;
                                if !progress.complete && tail != 0 {
                                    return Err(Error::Invalid);
                                }
                                tail
                            }
                            Err(_) => {
                                client.drain_left = client.drain_left.saturating_sub(n);
                                client.input.fill(0);
                                0
                            }
                        };
                        client.idle =
                            deadline(time.monotonic, self.timeouts.limits().header_idle_seconds)?
                                .min(client.phase_deadline);
                        match result {
                            Ok(progress) if progress.complete => {
                                client.requests =
                                    client.requests.checked_add(1).ok_or(Error::Capacity)?;
                                let head = client.head.head().map_err(|_| Error::Invalid)?;
                                client.keep_alive = !head.close && client.requests < 32;
                                let target = head.target;
                                client.timing =
                                    Some(crate::performance::Timer::start(request_phase(target)));
                                client.phase = Phase::Authorize;
                                if !target.starts_with(EVENTS_PREFIX)
                                    && !body_active.admit_request(&mut client.request_slot)
                                {
                                    control.log(
                                        crate::observability::Kind::AdmissionRefused {
                                            reason: crate::observability::Refusal::Capacity,
                                        },
                                        None,
                                        None,
                                    );
                                    self.respond(client, 429, time.utc_ms)?;
                                }
                            }
                            Ok(_) => (),
                            Err(http_request::Error::Limit) => {
                                self.respond(client, 431, time.utc_ms)?
                            }
                            Err(_) => self.respond(client, 400, time.utc_ms)?,
                        }
                    }
                }
            }
            Phase::Authorize => {
                let head = client.head.head().map_err(|_| Error::Invalid)?;
                let selection = self.request_origin(&head);
                if let Ok(index) = selection {
                    client.origin = index;
                }
                if selection.is_ok() && client.attempt.is_none() {
                    client.attempt = attempts.reserve(client.peer, time.monotonic.0);
                }
                let status = if let Err(status) = selection {
                    status
                } else if client.attempt.is_none() {
                    429
                } else {
                    let Some(authentication) = client.authentication.take() else {
                        client.phase = Phase::Authenticate;
                        return Ok(true);
                    };
                    match authentication {
                        Ok(Some(principal)) => {
                            if let Some(attempt) = client.attempt.take() {
                                attempts.authenticated(attempt);
                            }
                            if let Some(backend) = self.backend(principal.account, client.origin) {
                                if let Some(account) = head.target.strip_prefix("/jmap/upload/") {
                                    if head.method != "POST" {
                                        405
                                    } else if crate::wire::Id::parse(account)
                                        .ok()
                                        .and_then(|id| id.local())
                                        != Some(backend.account())
                                    {
                                        404
                                    } else if backend
                                        .session()
                                        .is_some_and(|session| session.is_read_only())
                                    {
                                        403
                                    } else if !body_active
                                        .admit(&mut client.body_slot, BodyKind::Upload)
                                    {
                                        body_busy()
                                    } else {
                                        client.principal = Some(principal);
                                        client.keep_alive = false;
                                        client.phase = Phase::Upload;
                                        return Ok(true);
                                    }
                                } else if head.target.starts_with("/jmap/download/") {
                                    if head.method != "GET" {
                                        405
                                    } else if head.framing != http_request::Framing::Length(0)
                                        || head.expect_continue
                                    {
                                        400
                                    } else if !body_active
                                        .admit(&mut client.body_slot, BodyKind::Download)
                                    {
                                        body_busy()
                                    } else {
                                        client.principal = Some(principal);
                                        client.phase = Phase::Download;
                                        return Ok(true);
                                    }
                                } else if head.target == "/jmap/api" {
                                    if head.method != "POST" {
                                        405
                                    } else if !head.content_type.is_some_and(|media| {
                                        media.split(';').next().is_some_and(|kind| {
                                            kind.trim().eq_ignore_ascii_case("application/json")
                                        })
                                    }) {
                                        415
                                    } else if !body_active
                                        .admit(&mut client.body_slot, BodyKind::Api)
                                    {
                                        let status = body_busy();
                                        control.log(
                                            crate::observability::Kind::AdmissionRefused {
                                                reason: crate::observability::Refusal::Capacity,
                                            },
                                            None,
                                            None,
                                        );
                                        return self.respond_body(client, status, time.utc_ms,
                                            br#"{"type":"urn:ietf:params:jmap:error:limit","status":429,"limit":"maxConcurrentRequests"}"#).map(|()| true);
                                    } else {
                                        client.principal = Some(principal);
                                        client.phase = Phase::Api;
                                        return Ok(true);
                                    }
                                } else if head.target == "/.well-known/jmap" {
                                    if head.method != "GET" {
                                        405
                                    } else if head.framing != http_request::Framing::Length(0)
                                        || head.expect_continue
                                    {
                                        400
                                    } else if let Some(session) = backend.session() {
                                        return self
                                            .respond_body(client, 200, time.utc_ms, session.bytes())
                                            .map(|()| true);
                                    } else {
                                        503
                                    }
                                } else if let Some(query) = head.target.strip_prefix(EVENTS_PREFIX)
                                {
                                    if backend.session().is_none() {
                                        404
                                    } else if head.method != "GET" {
                                        405
                                    } else if head.framing != http_request::Framing::Length(0)
                                        || head.expect_continue
                                    {
                                        400
                                    } else if *event_count >= self.resources.limits().event_streams
                                    {
                                        429
                                    } else {
                                        let options =
                                            match crate::jmap_events::Options::parse(query) {
                                                Ok(options) => options,
                                                Err(_) => {
                                                    return self
                                                        .respond(client, 400, time.utc_ms)
                                                        .map(|()| true);
                                                }
                                            };
                                        client.events = Some(EventState {
                                            options,
                                            stream: None,
                                            sample: None,
                                            headers: true,
                                            pending: false,
                                            last: time.monotonic,
                                        });
                                        client.principal = Some(principal);
                                        client.keep_alive = false;
                                        // The stream slot replaces any ordinary charge
                                        // and reserves no earlier response body.
                                        client.request_slot = None;
                                        client.response = Vec::new();
                                        client.phase = Phase::EventRead;
                                        *event_count =
                                            event_count.checked_add(1).ok_or(Error::Capacity)?;
                                        return Ok(true);
                                    }
                                } else {
                                    404
                                }
                            } else if head.target == "/.well-known/jmap" {
                                503
                            } else {
                                404
                            }
                        }
                        Ok(None) => 401,
                        Err(_) => 503,
                    }
                };
                if status == 401 {
                    control.log(crate::observability::Kind::AuthenticationFailed, None, None);
                }
                if status == 429 {
                    control.log(
                        crate::observability::Kind::AdmissionRefused {
                            reason: crate::observability::Refusal::Capacity,
                        },
                        None,
                        None,
                    );
                }
                self.respond(client, status, time.utc_ms)?;
            }
            Phase::Events => return self.events(client, time.monotonic, time.utc_ms),
            Phase::Response => {
                let connection = client.connection.as_mut().ok_or(Error::Invalid)?;
                if client.sent < client.response.len() {
                    match connection
                        .write(client.response.get(client.sent..).ok_or(Error::Invalid)?)?
                    {
                        IoProgress::Bytes(n) => {
                            client.sent += n;
                            client.idle = deadline(
                                time.monotonic,
                                self.timeouts.limits().response_idle_seconds,
                            )?
                            .min(client.phase_deadline);
                        }
                        IoProgress::Pending => client.wait = Wait::WRITE,
                        IoProgress::Closed => return Ok(false),
                    }
                } else if connection.flush()? == FlushProgress::Complete {
                    if let Some(timing) = client.timing.take() {
                        timing.finish(client.response_outcome);
                    }
                    client.phase = if client.keep_alive {
                        Phase::Complete
                    } else {
                        Phase::Close
                    };
                    let now = self.clock.sample()?.monotonic;
                    client.phase_deadline = deadline(now, DRAIN_SECONDS)?.min(client.absolute);
                    client.idle = client.phase_deadline;
                } else {
                    client.wait = Wait::WRITE;
                }
            }
            Phase::Complete => {
                if !client.keep_alive || client.stop_after_response {
                    return Err(Error::Invalid);
                }
                clear_head(client);
                client.credentials = None;
                client.principal = None;
                client.authentication = None;
                client.attempt = None;
                client.body_slot.take();
                client.origin = 0;
                client.keep_alive = false;
                client.response.clear();
                client.sent = 0;
                client.drain_left = DRAIN_BYTES;
                client.response_outcome = crate::performance::Outcome::Failed;
                client.phase = Phase::Head;
                client.phase_deadline =
                    deadline(time.monotonic, self.timeouts.limits().header_seconds)?
                        .min(client.absolute);
                client.idle = deadline(
                    time.monotonic,
                    self.timeouts.limits().header_idle_seconds.min(5),
                )?
                .min(client.phase_deadline);
            }
            Phase::Close => {
                if client.connection.as_mut().ok_or(Error::Invalid)?.close()?
                    == FlushProgress::Complete
                {
                    if client.stop_after_response {
                        return Err(Error::WriterStopped);
                    }
                    client.phase = Phase::Drain;
                } else {
                    client.wait = Wait::WRITE;
                }
            }
            Phase::Drain => {
                let before = client.drain_left;
                let live = drain(
                    client.connection.as_mut().ok_or(Error::Invalid)?,
                    &mut client.input,
                    &mut client.drain_left,
                )?;
                if live && before == client.drain_left {
                    client.wait = Wait::READ;
                }
                return Ok(live);
            }
            Phase::Tls
            | Phase::Authenticate
            | Phase::EventRead
            | Phase::Upload
            | Phase::Download
            | Phase::Api => return Err(Error::Invalid),
        }
        Ok(true)
    }
    fn download<'a>(
        &'a self,
        client: &mut Client,
        control: &Control,
        worker: &mut Option<ApiWorker<'a>>,
    ) -> Result<(), Error> {
        use crate::https_download::{Failure, Request, Target};
        let principal = client.principal.ok_or(Error::Invalid)?;
        let backend = self
            .backend(principal.account, client.origin)
            .ok_or(Error::Invalid)?;
        let target = match Target::parse(client.head.head().map_err(|_| Error::Invalid)?.target) {
            Ok(target) if target.account() == backend.account() => target,
            Ok(_) | Err(Error::NotFound) => {
                return self.respond(client, 404, self.clock.sample()?.utc_ms)
            }
            Err(_) => return self.respond(client, 400, self.clock.sample()?.utc_ms),
        };
        let maximum =
            u64::try_from(self.resources.limits().message_bytes).map_err(|_| Error::Capacity)?;
        let now = self.clock.sample()?.monotonic;
        let operation = deadline(
            now,
            self.timeouts
                .http_exchange_seconds(0, maximum)
                .map_err(|_| Error::Invalid)?,
        )?
        .min(client.absolute);
        clear_head(client);
        client.drain_left = 0;
        if worker
            .as_ref()
            .is_none_or(|slot| slot.account != principal.account || slot.origin != client.origin)
        {
            *worker = None;
            *worker = Some(ApiWorker {
                account: principal.account,
                origin: client.origin,
                worker: backend.api_worker(),
            });
        }
        let request = Request {
            keep_alive: client.keep_alive,
            target: &target,
            device: principal.device,
            credentials: client.credentials.as_ref().ok_or(Error::Invalid)?,
            deadline: operation,
        };
        let transport = client.connection.as_mut().ok_or(Error::Invalid)?;
        let result = worker
            .as_mut()
            .and_then(|slot| slot.worker.as_mut())
            .and_then(|worker| worker.download(&request, transport, control))
            .unwrap_or_else(|| backend.download(request, transport, control));
        client.credentials = None;
        match result {
            Ok(completion) => {
                client.keep_alive &= completion.keep_alive;
                if let Some(timing) = client.timing.take() {
                    timing.finish(crate::performance::Outcome::Success);
                }
                client.phase = if client.keep_alive {
                    Phase::Complete
                } else {
                    Phase::Close
                };
                client.phase_deadline =
                    deadline(self.clock.sample()?.monotonic, DRAIN_SECONDS)?.min(operation);
                client.idle = client.phase_deadline;
                Ok(())
            }
            Err(Failure::Before(Error::WriterStopped) | Failure::Started(Error::WriterStopped)) => {
                control.fail();
                Err(Error::WriterStopped)
            }
            Err(Failure::Started(error)) => Err(error),
            Err(Failure::Before(error)) => {
                let status = match error {
                    Error::NotFound | Error::Forbidden => 404,
                    _ => 503,
                };
                self.respond(client, status, self.clock.sample()?.utc_ms)
            }
        }
    }
    fn api<'a>(
        &'a self,
        client: &mut Client,
        control: &Control,
        worker: &mut Option<ApiWorker<'a>>,
    ) -> Result<(), Error> {
        use crate::https_download::Failure;
        let principal = client.principal.ok_or(Error::Invalid)?;
        let backend = self
            .backend(principal.account, client.origin)
            .ok_or(Error::Invalid)?;
        let head = client.head.head().map_err(|_| Error::Invalid)?;
        let framing = head.framing;
        let expect_continue = head.expect_continue;
        clear_head(client);
        if worker
            .as_ref()
            .is_none_or(|slot| slot.account != principal.account || slot.origin != client.origin)
        {
            *worker = None;
            *worker = Some(ApiWorker {
                account: principal.account,
                origin: client.origin,
                worker: backend.api_worker(),
            });
        }
        let request = crate::https_api::Request {
            keep_alive: client.keep_alive,
            device: principal.device,
            credentials: client.credentials.as_ref().ok_or(Error::Invalid)?,
            framing,
            expect_continue,
            prefix: client
                .input
                .get(..client.input_used)
                .ok_or(Error::Invalid)?,
            deadline: client.absolute,
        };
        let transport = client.connection.as_mut().ok_or(Error::Invalid)?;
        let result = match worker.as_mut().and_then(|slot| slot.worker.as_mut()) {
            Some(worker) => worker.api(request, transport, control),
            None => backend.api(request, transport, control),
        };
        client.credentials = None;
        client.input.fill(0);
        client.input_used = 0;
        client.drain_left = 0;
        match result {
            Ok(completion) => {
                client.keep_alive &= completion.keep_alive;
                if client.keep_alive {
                    let count = completion.tail.len();
                    client
                        .input
                        .get_mut(..count)
                        .ok_or(Error::Capacity)?
                        .copy_from_slice(&completion.tail);
                    client.input_used = count;
                }
                if let Some(timing) = client.timing.take() {
                    timing.finish(crate::performance::Outcome::Success);
                }
                client.phase = if client.keep_alive {
                    Phase::Complete
                } else {
                    Phase::Close
                };
                client.phase_deadline =
                    deadline(self.clock.sample()?.monotonic, DRAIN_SECONDS)?.min(client.absolute);
                client.idle = client.phase_deadline;
                Ok(())
            }
            Err(Failure::Before(Error::WriterStopped) | Failure::Started(Error::WriterStopped)) => {
                control.fail();
                Err(Error::WriterStopped)
            }
            Err(Failure::Started(error)) => Err(error),
            Err(Failure::Before(error)) => {
                let status = match error {
                    Error::NotFound => 404,
                    Error::Forbidden => 403,
                    Error::Invalid => 400,
                    _ => 503,
                };
                self.respond(client, status, self.clock.sample()?.utc_ms)
            }
        }
    }
    fn upload(&self, client: &mut Client, control: &Control) -> Result<(), Error> {
        let principal = client.principal.ok_or(Error::Invalid)?;
        let backend = self
            .backend(principal.account, client.origin)
            .ok_or(Error::Invalid)?;
        let head = client.head.head().map_err(|_| Error::Invalid)?;
        let now = self.clock.sample()?.monotonic;
        let configured_max =
            u64::try_from(self.resources.limits().message_bytes).map_err(|_| Error::Capacity)?;
        let maximum = match head.framing {
            http_request::Framing::Length(n) => n.min(configured_max),
            http_request::Framing::Chunked => configured_max,
        };
        let transfer = deadline(
            now,
            self.timeouts
                .http_transfer_seconds(maximum)
                .map_err(|_| Error::Invalid)?,
        )?
        .min(client.absolute);
        let operation = deadline(
            now,
            self.timeouts
                .http_exchange_seconds(maximum, 4096)
                .map_err(|_| Error::Invalid)?,
        )?
        .min(client.absolute);
        // This connection serves one exchange; tighten its cap through flush.
        client.absolute = operation;
        let framing = head.framing;
        let expect_continue = head.expect_continue;
        let media = head.content_type.unwrap_or("application/octet-stream");
        let mut media_bytes = [0; 1024];
        let media_len = media.len();
        let Some(media_out) = media_bytes.get_mut(..media_len) else {
            return self.respond(client, 431, self.clock.sample()?.utc_ms);
        };
        media_out.copy_from_slice(media.as_bytes());
        clear_head(client);
        let media = std::str::from_utf8(media_bytes.get(..media_len).ok_or(Error::Invalid)?)
            .map_err(|_| Error::Invalid)?;
        let result = backend.receive(
            https_upload::Request {
                account: backend.account(),
                device: principal.device,
                credentials: client.credentials.as_ref().ok_or(Error::Invalid)?,
                framing,
                content_type: media,
                prefix: client
                    .input
                    .get(..client.input_used)
                    .ok_or(Error::Invalid)?,
                deadline: operation,
                body_deadline: transfer,
                expect_continue,
            },
            client.connection.as_mut().ok_or(Error::Invalid)?,
            control,
        );
        client.credentials = None;
        client.drain_left = if result.is_ok() {
            0
        } else {
            match framing {
                http_request::Framing::Length(n) => n.min(DRAIN_BYTES as u64) as usize,
                http_request::Framing::Chunked => DRAIN_BYTES,
            }
        }
        .saturating_sub(client.input_used);
        client.input_used = 0;
        client.input.fill(0);
        if let Ok(response) = &result {
            client.stop_after_response = response.stop;
        } else if matches!(result, Err(Error::WriterStopped)) {
            control.fail();
            return Err(Error::WriterStopped);
        }
        let time = self.clock.sample()?;
        match result {
            // Retire only this exchange; no success or retryable failure can
            // describe an unresolved publication. The backend retains its fence.
            Ok(response) if response.is_uncertain() => Err(Error::Conflict),
            Ok(response) => self.respond_body(client, 200, time.utc_ms, response.bytes()?),
            Err(error) => {
                let status = match error {
                    Error::Forbidden => 403,
                    Error::Capacity => 413,
                    Error::Quota => 507,
                    Error::Invalid => 400,
                    _ => 503,
                };
                self.respond(client, status, time.utc_ms)
            }
        }
    }
    fn event_sample(&self, client: &Client) -> Result<EventSample, Error> {
        let now = self.clock.sample()?.monotonic;
        let expires = Deadline::after(now, EVENT_SAMPLE_MS)?;
        let deadline = expires.min(client.deadline());
        if deadline.expired(now) {
            return Err(Error::Deadline);
        }
        let principal = client.principal.ok_or(Error::Forbidden)?;
        let event = client.events.as_ref().ok_or(Error::Invalid)?;
        if now < event.last {
            return Err(Error::Deadline);
        }
        let proof = match event.sample.as_ref() {
            Some(Ok(sample)) => http_auth::EventCredential::Continued(sample.stamp),
            Some(Err(error)) => return Err(*error),
            None => http_auth::EventCredential::Password(
                client
                    .credentials
                    .as_ref()
                    .ok_or(Error::Forbidden)?
                    .password(),
            ),
        };
        let observation = self
            .authentication
            .observe_events(principal, proof, deadline)?;
        let backend = self
            .backend(principal.account, client.origin)
            .ok_or(Error::Forbidden)?;
        let snapshot = backend.event_snapshot(observation.identity)?;
        let finished = self.clock.sample()?.monotonic;
        if finished < now || deadline.expired(finished) {
            return Err(Error::Deadline);
        }
        Ok(EventSample {
            observed: finished,
            stamp: observation.credential_stamp,
            snapshot,
            expires,
        })
    }
    fn begin_events(
        &self,
        client: &mut Client,
        stream: crate::jmap_events::Stream,
        utc_ms: i64,
        now: Tick,
    ) -> Result<(), Error> {
        use std::io::Write;
        let idle = deadline(now, self.timeouts.limits().event_stall_seconds)?.min(client.absolute);
        client.input.fill(0);
        client.input_used = 0;
        client.drain_left = 0;
        client.credentials = None;
        clear_head(client);
        client.response.clear();
        client
            .response
            .try_reserve(512)
            .map_err(|_| Error::Capacity)?;
        write!(&mut client.response,
            "HTTP/1.1 200 OK\r\nDate: {}\r\nConnection: close\r\nContent-Type: text/event-stream; charset=utf-8\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\n\r\n",
            http_date(utc_ms)?)?;
        client.events.as_mut().ok_or(Error::Invalid)?.stream = Some(stream);
        client.sent = 0;
        if let Some(timing) = client.timing.take() {
            timing.finish(crate::performance::Outcome::Success);
        }
        client.phase = Phase::Events;
        client.phase_deadline = client.absolute;
        client.idle = idle;
        Ok(())
    }
    fn events(&self, client: &mut Client, now: Tick, utc_ms: i64) -> Result<bool, Error> {
        let event = client.events.as_mut().ok_or(Error::Invalid)?;
        if now < event.last {
            return Err(Error::WriterStopped);
        }
        event.last = now;
        let sample = match event.sample.as_ref().ok_or(Error::Invalid)? {
            Ok(sample) => sample,
            Err(error) if event.headers && client.sent == 0 => {
                client.response.clear();
                let status = if *error == Error::Forbidden { 401 } else { 503 };
                self.respond(client, status, utc_ms)?;
                return Ok(true);
            }
            Err(_) => return Ok(false),
        };
        if now < sample.observed {
            return Ok(false);
        }
        if sample.expires.expired(now) {
            client.phase = Phase::EventRead;
            return Ok(true);
        }
        let snapshot = sample.snapshot;
        let Some(stream) = event.stream.as_mut() else {
            let head = client.head.head().map_err(|_| Error::Invalid)?;
            let stream =
                crate::jmap_events::Stream::new(event.options, snapshot, head.last_event_id, now)?;
            self.begin_events(client, stream, utc_ms, now)?;
            return Ok(true);
        };
        let connection = client.connection.as_mut().ok_or(Error::Invalid)?;
        if event.headers {
            if client.sent < client.response.len() {
                match connection.write(client.response.get(client.sent..).ok_or(Error::Invalid)?)? {
                    IoProgress::Bytes(n) if n > 0 && n <= client.response.len() - client.sent => {
                        client.sent += n
                    }
                    IoProgress::Pending => client.wait = Wait::WRITE,
                    _ => return Ok(false),
                }
            } else if connection.flush()? == FlushProgress::Complete {
                event.headers = false;
                client.sent = 0;
                client.response.clear();
                client.idle = client.absolute;
            } else {
                client.wait = Wait::WRITE;
            }
            return Ok(true);
        }
        let frame = stream.prepare(snapshot, now)?;
        if frame.is_empty() {
            // A silent stream still detects EOF; no pipelined requests are accepted.
            return match connection.read(&mut client.input)? {
                IoProgress::Pending => {
                    client.wait = Wait::READ;
                    Ok(true)
                }
                _ => Ok(false),
            };
        }
        if !event.pending {
            event.pending = true;
            client.idle =
                deadline(now, self.timeouts.limits().event_stall_seconds)?.min(client.absolute);
        }
        if client.sent < frame.len() {
            match connection.write(frame.get(client.sent..).ok_or(Error::Invalid)?)? {
                IoProgress::Bytes(n) if n > 0 && n <= frame.len() - client.sent => client.sent += n,
                IoProgress::Pending => client.wait = Wait::WRITE,
                _ => return Ok(false),
            }
        } else if connection.flush()? == FlushProgress::Complete {
            stream.acknowledge(self.clock.sample()?.monotonic)?;
            event.pending = false;
            client.sent = 0;
            client.idle = client.absolute;
            if stream.closed() {
                client.phase = Phase::Close;
                client.phase_deadline = deadline(now, DRAIN_SECONDS)?.min(client.absolute);
                client.idle = client.phase_deadline;
            }
        } else {
            client.wait = Wait::WRITE;
        }
        Ok(true)
    }
    fn respond(&self, client: &mut Client, status: u16, utc_ms: i64) -> Result<(), Error> {
        self.respond_body(client, status, utc_ms, &[])
    }
    fn respond_body(
        &self,
        client: &mut Client,
        status: u16,
        utc_ms: i64,
        body: &[u8],
    ) -> Result<(), Error> {
        use std::io::Write;
        client.response_outcome = match status {
            200..300 => crate::performance::Outcome::Success,
            429 => crate::performance::Outcome::Busy,
            503 => crate::performance::Outcome::Unavailable,
            _ => crate::performance::Outcome::Failed,
        };
        client.keep_alive &= status == 200
            && client
                .head
                .head()
                .is_ok_and(|head| head.target == "/.well-known/jmap");
        let get = client.head.head().is_ok_and(|head| {
            head.target.starts_with("/jmap/download/")
                || head.target.starts_with("/jmap/events?")
                || head.target == "/.well-known/jmap"
        });
        // Known framing and already-read bytes bound unread-body discard.
        if let Ok(head) = client.head.head() {
            if let http_request::Framing::Length(bytes) = head.framing {
                client.drain_left = client
                    .drain_left
                    .min(bytes.min(DRAIN_BYTES as u64) as usize);
            }
        }
        client.drain_left = client.drain_left.saturating_sub(client.input_used);
        if !client.keep_alive {
            client.input.fill(0);
            client.input_used = 0;
        }
        // Retire credentials before allocating or waiting on Session output.
        client.credentials = None;
        clear_head(client);
        client
            .response
            .try_reserve(512usize.checked_add(body.len()).ok_or(Error::Capacity)?)
            .map_err(|_| Error::Capacity)?;
        let reason = match status {
            200 => "OK",
            400 => "Bad Request",
            401 => "Unauthorized",
            403 => "Forbidden",
            404 => "Not Found",
            405 => "Method Not Allowed",
            413 => "Content Too Large",
            507 => "Insufficient Storage",
            421 => "Misdirected Request",
            429 => "Too Many Requests",
            431 => "Request Header Fields Too Large",
            415 => "Unsupported Media Type",
            _ => "Service Unavailable",
        };
        write!(&mut client.response, "HTTP/1.1 {status} {reason}\r\nDate: {}\r\nConnection: {}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\n", http_date(utc_ms)?, if client.keep_alive { "keep-alive" } else { "close" }, body.len())?;
        if status == 401 {
            client.response.extend_from_slice(
                b"WWW-Authenticate: Basic realm=\"td-mta\", charset=\"UTF-8\"\r\n",
            );
        }
        if status == 429 {
            client.response.extend_from_slice(b"Retry-After: 1\r\n");
        }
        if !body.is_empty() {
            client.response.extend_from_slice(if status == 429 {
                b"Content-Type: application/problem+json\r\n"
            } else {
                b"Content-Type: application/json\r\n"
            });
        }
        if status == 405 {
            client.response.extend_from_slice(if get {
                b"Allow: GET\r\n"
            } else {
                b"Allow: POST\r\n"
            });
        }
        client.response.extend_from_slice(b"\r\n");
        client.response.extend_from_slice(body);
        client.phase = Phase::Response;
        let now = self.clock.sample()?.monotonic;
        client.phase_deadline = deadline(
            now,
            self.timeouts
                .http_transfer_seconds(client.response.len() as u64)
                .map_err(|_| Error::Invalid)?,
        )?
        .min(client.absolute);
        client.idle =
            deadline(now, self.timeouts.limits().response_idle_seconds)?.min(client.phase_deadline);
        Ok(())
    }
}
fn body_busy() -> u16 {
    crate::performance::record(
        crate::performance::Phase::HttpBodyBusy,
        Duration::ZERO,
        crate::performance::Outcome::Busy,
    );
    429
}

fn request_phase(target: &str) -> crate::performance::Phase {
    use crate::performance::Phase;
    match target {
        "/jmap/api" => Phase::HttpApi,
        "/.well-known/jmap" => Phase::HttpSession,
        value if value.starts_with("/jmap/download/") => Phase::HttpDownload,
        value if value.starts_with("/jmap/upload/") => Phase::HttpUpload,
        _ => Phase::HttpOther,
    }
}

/// No framing or deadline renewal: these bytes can never become another request.
fn drain(
    connection: &mut impl Transport,
    input: &mut [u8],
    left: &mut usize,
) -> Result<bool, Error> {
    if *left == 0 {
        return Ok(false);
    }
    let count = (*left).min(input.len()).min(QUANTUM);
    let progress = connection.read(input.get_mut(..count).ok_or(Error::Invalid)?);
    match progress {
        Ok(IoProgress::Pending) => return Ok(true),
        Ok(IoProgress::Bytes(n)) if n <= count => {
            input.get_mut(..n).ok_or(Error::Invalid)?.fill(0);
        }
        _ => input.fill(0),
    }
    match progress? {
        IoProgress::Bytes(n) if n > 0 && n <= count => {
            *left = left.checked_sub(n).ok_or(Error::Invalid)?;
            Ok(*left != 0)
        }
        IoProgress::Bytes(_) => Err(Error::Invalid),
        IoProgress::Pending => Ok(true),
        IoProgress::Closed => Ok(false),
    }
}
/// Preserve only the bytes beyond the parsed head, in the existing input buffer.
fn retain_tail(input: &mut [u8], used: usize, consumed: usize) -> Result<usize, Error> {
    let tail = used.checked_sub(consumed).ok_or(Error::Invalid)?;
    if used > input.len() {
        return Err(Error::Invalid);
    }
    input.copy_within(consumed..used, 0);
    input.get_mut(tail..).ok_or(Error::Invalid)?.fill(0);
    Ok(tail)
}
fn buffer<const N: usize>() -> Result<Box<[u8; N]>, Error> {
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(N).map_err(|_| Error::Capacity)?;
    bytes.resize(N, 0);
    bytes
        .into_boxed_slice()
        .try_into()
        .map_err(|_| Error::Invalid)
}
fn deadline(now: Tick, seconds: u64) -> Result<Deadline, Error> {
    timers::deadline(now, seconds).map_err(|_| Error::Invalid)
}
pub(crate) use crate::http_date::http_date;

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn connection_limits_leave_room_for_another_network_peer() {
        let network = PeerAddress::Network(std::net::Ipv4Addr::LOCALHOST.into());
        for (slots, ordinary, network_limit, local_limit) in [
            (8, 8, 7, 8),
            (1, 1, 1, 1),
            (2, 2, 1, 2),
            (16, 16, 15, 16),
            (17, 17, 16, 16),
            (32, 32, 16, 16),
            // Stream slots never let one peer hold every ordinary admission.
            (24, 16, 15, 16),
            (8, 5, 4, 8),
            (3, 1, 1, 3),
            (
                crate::limits::MAX_HTTPS_SLOTS,
                crate::limits::MAX_HTTPS_CONNECTIONS,
                16,
                16,
            ),
        ] {
            assert_eq!(peer_limit(network, slots, ordinary), network_limit);
            assert_eq!(peer_limit(PeerAddress::Local, slots, ordinary), local_limit);
        }
    }

    #[test]
    #[allow(clippy::unwrap_used)]
    fn connection_capacity_groups_network_addresses_like_smtp() {
        let address = |ip: &str| PeerAddress::from(ip.parse::<std::net::IpAddr>().unwrap());
        let peer = address("2001:db8:1::1");
        let mut slots = [None; 8];
        for slot in slots.iter_mut().take(7) {
            *slot = Some(peer.admission_peer());
        }
        assert!(!has_peer_capacity(address("2001:db8:1::abcd"), &slots, 8));
        assert!(has_peer_capacity(address("2001:db8:1:1::1"), &slots, 8));
        let slots = [Some(address("::ffff:192.0.2.1").admission_peer()), None];
        assert!(!has_peer_capacity(address("192.0.2.1"), &slots, 2));
        assert!(has_peer_capacity(address("192.0.2.2"), &slots, 2));
    }

    #[test]
    fn timing_labels_are_closed_and_never_retain_target_ids() {
        use crate::performance::Phase as Metric;
        for (target, phase) in [
            ("/.well-known/jmap", Metric::HttpSession),
            ("/jmap/api", Metric::HttpApi),
            (
                "/jmap/download/private-account/private-blob",
                Metric::HttpDownload,
            ),
            ("/jmap/upload/private-account", Metric::HttpUpload),
            ("/unknown/private-string", Metric::HttpOther),
        ] {
            assert_eq!(request_phase(target), phase);
            assert!(!phase.code().contains("private"));
        }
    }

    #[test]
    #[allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
    fn discarded_data_is_bounded_cleared_and_never_written() {
        struct Source {
            calls: usize,
            maximum: usize,
            result: Option<Result<IoProgress, Error>>,
        }
        impl Transport for Source {
            fn read(&mut self, output: &mut [u8]) -> Result<IoProgress, Error> {
                self.calls += 1;
                self.maximum = self.maximum.max(output.len());
                let result = self
                    .result
                    .take()
                    .unwrap_or(Ok(IoProgress::Bytes(output.len())));
                if let Ok(IoProgress::Bytes(n)) = result {
                    output[..n].fill(0xaa);
                }
                result
            }
            fn write(&mut self, _: &[u8]) -> Result<IoProgress, Error> {
                panic!("drain wrote")
            }
            fn flush(&mut self) -> Result<FlushProgress, Error> {
                panic!("drain flushed")
            }
            fn close(&mut self) -> Result<FlushProgress, Error> {
                panic!("drain closed")
            }
            fn abort(&mut self) {
                panic!("drain aborted")
            }
        }
        let mut source = Source {
            calls: 0,
            maximum: 0,
            result: None,
        };
        let mut input = [0; QUANTUM + 1];
        let mut left = DRAIN_BYTES - 7;
        for expected in [true, true, true, false] {
            assert_eq!(drain(&mut source, &mut input, &mut left).unwrap(), expected);
            assert!(input.iter().all(|byte| *byte == 0));
        }
        assert_eq!(source.calls, 4);
        assert_eq!(source.maximum, QUANTUM);
        assert_eq!(left, 0);
        assert!(!drain(&mut source, &mut input, &mut left).unwrap());
        assert_eq!(source.calls, 4);
        for result in [
            Ok(IoProgress::Pending),
            Ok(IoProgress::Closed),
            Err(Error::Tls),
        ] {
            source.result = Some(result);
            left = 10;
            let observed = drain(&mut source, &mut input, &mut left);
            assert_eq!(observed, result.map(|p| p == IoProgress::Pending));
            assert_eq!(left, 10);
            assert!(input.iter().all(|byte| *byte == 0));
        }
    }
    #[test]
    #[allow(clippy::unwrap_used, clippy::indexing_slicing)]
    fn coalesced_body_survives_header_consumption_and_consumed_bytes_are_cleared() {
        let head = b"POST /jmap HTTP/1.1\r\nHost: localhost\r\nContent-Length: 4\r\n\r\n";
        let mut input = [0xaa; QUANTUM];
        input[..head.len()].copy_from_slice(head);
        input[head.len()..head.len() + 4].copy_from_slice(b"body");
        let mut storage = [0; HEAD_BYTES];
        let mut reader = HeadReader::new(&mut storage[..]);
        let used = head.len() + 4;
        let progress = reader.feed(&input[..used]).unwrap();
        assert!(progress.complete);
        let remaining = retain_tail(&mut input, used, progress.consumed).unwrap();
        assert_eq!(remaining, 4);
        assert_eq!(&input[..remaining], b"body");
        assert!(input[remaining..].iter().all(|byte| *byte == 0));
        assert!(retain_tail(&mut input, 1, 2).is_err());
        assert!(retain_tail(&mut input, QUANTUM + 1, 0).is_err());
    }
}

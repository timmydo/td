//! Bounded HTTPS admission and authenticated mailbox-access routing.
use crate::{
    admission::timers::{self, TimeoutPlan},
    config::{
        endpoint::Origin,
        listener::{Kind, Listener},
    },
    generations::GenerationLease,
    http_auth::{self, Attempts},
    http_request::{self, HeadReader, HEAD_BYTES, QUANTUM},
    https_upload,
    ids::DeviceId,
    limits::ResourcePlan,
    ports::{
        Clock, Deadline, Error, FlushProgress, Handshake, IoProgress, Tick, TlsPolicyId,
        TlsTransport, Transport,
    },
    smtp_receiving::Control,
    store_fs::DeviceStore,
    tls_admission::HandshakePool,
    tls_io::TLS_WIRE_BYTES,
    tls_policy::{PolicyRole, TlsConnection, TlsPolicies},
    transport::TcpTransport,
};
use std::{
    net::{IpAddr, TcpListener, TcpStream},
    sync::{
        atomic::{AtomicBool, AtomicU8, Ordering},
        Arc, Mutex, TryLockError,
    },
    time::Duration,
};

type Wire = Box<[u8; TLS_WIRE_BYTES]>;
const SCAN: Duration = Duration::from_millis(5);
const PER_PEER: usize = 2;
const DRAIN_BYTES: usize = 64 * 1024;
const DRAIN_SECONDS: u64 = 5;

pub struct BoundListener {
    socket: TcpListener,
    policy: TlsPolicyId,
}
impl BoundListener {
    pub fn new(
        socket: TcpListener,
        row: Listener<'_>,
        policies: &GenerationLease<TlsPolicies>,
    ) -> Result<Self, Error> {
        if row.kind != Kind::Https
            || !row.bind.is_ipv4()
            || row.bind.port() == 0
            || socket.local_addr()? != row.bind
        {
            return Err(Error::Invalid);
        }
        let policy = TlsPolicies::bound_listener(policies, row)?;
        if TlsPolicies::role(policies, policy)? != PolicyRole::Https {
            return Err(Error::Forbidden);
        }
        socket.set_nonblocking(true)?;
        Ok(Self { socket, policy })
    }
}
#[derive(Clone, Copy, PartialEq)]
enum Phase {
    Tls,
    Head,
    Authorize,
    Upload,
    Download,
    Api,
    Events,
    Response,
    Close,
    Drain,
}
struct Client {
    peer: IpAddr,
    phase: Phase,
    pending: Option<TcpStream>,
    wire: Option<(Wire, Wire)>,
    policy: TlsPolicyId,
    connection: Option<TlsConnection<Wire>>,
    head: HeadReader<Box<[u8]>>,
    input: Box<[u8]>,
    input_used: usize,
    drain_left: usize,
    response: Vec<u8>,
    sent: usize,
    attempt_admitted: bool,
    device: Option<DeviceId>,
    body_slot: bool,
    events: Option<EventState>,
    stop_after_response: bool,
    absolute: Deadline,
    phase_deadline: Deadline,
    idle: Deadline,
    tls_at: Tick,
}
struct EventState {
    stream: crate::jmap_events::Stream,
    headers: bool,
    pending: bool,
    last: Tick,
}
impl Drop for Client {
    fn drop(&mut self) {
        self.input.fill(0);
        std::hint::black_box(&mut self.input);
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

pub struct Https<'a, 'r> {
    pub listeners: &'a [BoundListener],
    pub policies: &'a GenerationLease<TlsPolicies>,
    pub handshakes: &'a HandshakePool,
    pub devices: &'a Mutex<DeviceStore<'r>>,
    pub origin: Origin<'a>,
    pub username: &'a str,
    pub resources: &'a ResourcePlan,
    pub timeouts: &'a TimeoutPlan,
    pub clock: Arc<dyn Clock>,
    pub uploads: Option<&'a dyn https_upload::Backend>,
    pub ready: &'a AtomicBool,
}
impl Https<'_, '_> {
    pub fn run(&self, control: &Control) -> Result<(), Error> {
        if self.listeners.is_empty() {
            return Err(Error::Invalid);
        }
        if let Some(session) = self.uploads.and_then(|backend| backend.session()) {
            if self.resources.limits().event_streams == 0
                || session.concurrency().requests != 1
                || session.concurrency().uploads != 1
            {
                return Err(Error::Invalid);
            }
            crate::jmap_request::Request::validate_resources(self.resources)
                .map_err(|_| Error::Invalid)?;
        }
        let count = self.resources.limits().https_connections;
        let mut slots: Vec<Mutex<Option<Client>>> = Vec::new();
        slots
            .try_reserve_exact(count)
            .map_err(|_| Error::Capacity)?;
        slots.extend((0..count).map(|_| Mutex::new(None)));
        let done = AtomicBool::new(false);
        let body_active = AtomicU8::new(0);
        let body_warm = AtomicBool::new(false);
        std::thread::scope(|scope| {
            let worker = std::thread::Builder::new()
                .name("https-tls".into())
                .spawn_scoped(scope, || {
                    while !done.load(Ordering::Acquire) {
                        for cell in &slots {
                            let mut slot = match cell.try_lock() {
                                Ok(slot) => slot,
                                Err(TryLockError::WouldBlock) => continue,
                                Err(_) => {
                                    control.fail();
                                    return;
                                }
                            };
                            let Some(client) =
                                slot.as_mut().filter(|client| client.phase == Phase::Tls)
                            else {
                                continue;
                            };
                            let result = self.tls(client);
                            if result.is_err() {
                                *slot = None;
                            }
                        }
                        std::thread::park_timeout(SCAN);
                    }
                })?;
            let finish = Finish {
                done: &done,
                worker: worker.thread(),
            };
            let uploader = match std::thread::Builder::new()
                .name("https-body".into())
                .spawn_scoped(scope, || {
                    if td_crypto::SystemEntropy::try_new().is_err() {
                        control.fail();
                        return;
                    }
                    body_warm.store(true, Ordering::Release);
                    while !done.load(Ordering::Acquire) {
                        for cell in &slots {
                            let mut slot = match cell.try_lock() {
                                Ok(slot) => slot,
                                Err(TryLockError::WouldBlock) => continue,
                                Err(_) => {
                                    control.fail();
                                    return;
                                }
                            };
                            let Some(client) = slot.as_mut().filter(|c| {
                                matches!(c.phase, Phase::Upload | Phase::Download | Phase::Api)
                            }) else {
                                continue;
                            };
                            let result = if client.phase == Phase::Download {
                                self.download(client, control)
                            } else if client.phase == Phase::Api {
                                self.api(client, control)
                            } else {
                                self.upload(client, control)
                            };
                            if result.is_err() {
                                if client.stop_after_response {
                                    control.fail();
                                }
                                *slot = None;
                                body_active.store(0, Ordering::Release);
                            }
                        }
                        std::thread::park_timeout(SCAN);
                    }
                }) {
                Ok(worker) => worker,
                Err(error) => {
                    drop(finish);
                    let _ = worker.join();
                    return Err(error.into());
                }
            };
            let upload_finish = Finish {
                done: &done,
                worker: uploader.thread(),
            };
            while !body_warm.load(Ordering::Acquire)
                && !uploader.is_finished()
                && !control.stop_requested()
            {
                std::thread::park_timeout(SCAN);
            }
            let result = if control.stop_requested() {
                Ok(())
            } else if body_warm.load(Ordering::Acquire) {
                self.network(&slots, &worker, &uploader, &body_active, control)
            } else {
                Err(Error::WriterStopped)
            };
            if result.is_err() {
                control.fail();
            }
            drop(upload_finish);
            drop(finish);
            if uploader.join().is_err() {
                control.fail();
                return Err(Error::WriterStopped);
            }
            if worker.join().is_err() {
                control.fail();
                return Err(Error::WriterStopped);
            }
            let retired = slots.iter().any(|slot| {
                slot.lock().map_or(true, |slot| {
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
    fn tls(&self, client: &mut Client) -> Result<(), Error> {
        let now = self.clock.sample()?.monotonic;
        if now < client.tls_at {
            return Ok(());
        }
        if client.phase_deadline.expired(now) {
            return Err(Error::Deadline);
        }
        client.tls_at = Tick(now.0.checked_add(10).ok_or(Error::Invalid)?);
        if let Some(socket) = client.pending.as_ref() {
            let mut first = [0; 1];
            match socket.peek(&mut first) {
                Ok(0) => return Err(Error::Tls),
                Ok(_) => (),
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                    ) =>
                {
                    return Ok(())
                }
                Err(error) => return Err(error.into()),
            }
            let (input, output) = client.wire.take().ok_or(Error::Invalid)?;
            let prepared = match TlsPolicies::reserve_session(
                self.policies.clone(),
                client.policy,
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
            client.connection = Some(
                reserved
                    .handoff(
                        plain,
                        &[],
                        self.clock.clone(),
                        client.absolute,
                        client.phase_deadline,
                    )
                    .map_err(|refusal| refusal.error())?,
            );
        }
        let connection = client.connection.as_mut().ok_or(Error::Invalid)?;
        let result = connection.handshake(client.phase_deadline)?;
        let now = self.clock.sample()?.monotonic;
        client.tls_at = Tick(now.0.checked_add(10).ok_or(Error::Invalid)?);
        if matches!(result, Handshake::Complete(_)) {
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
        slots: &[Mutex<Option<Client>>],
        worker: &std::thread::ScopedJoinHandle<'_, ()>,
        uploader: &std::thread::ScopedJoinHandle<'_, ()>,
        body_active: &AtomicU8,
        control: &Control,
    ) -> Result<(), Error> {
        for listener in self.listeners {
            TlsPolicies::check_receiving_certificate(
                self.policies,
                listener.policy,
                self.clock.sample()?.utc_ms,
            )?;
        }
        self.ready.store(true, Ordering::Release);
        let mut attempts = Attempts::default();
        let mut peers = [None; 32];
        let mut cursor = 0;
        let mut accept_after = Tick(0);
        let mut event_count = 0usize;
        while !control.stop_requested() {
            if worker.is_finished() || uploader.is_finished() {
                control.fail();
                return Err(Error::WriterStopped);
            }
            let time = self.clock.sample()?;
            for listener in self.listeners {
                TlsPolicies::check_receiving_certificate(
                    self.policies,
                    listener.policy,
                    time.utc_ms,
                )?;
            }
            for offset in 0..slots.len() {
                let index = (cursor + offset) % slots.len();
                let Some(cell) = slots.get(index) else {
                    return Err(Error::Invalid);
                };
                let mut slot = match cell.try_lock() {
                    Ok(slot) => slot,
                    Err(TryLockError::WouldBlock) => continue,
                    Err(_) => return Err(Error::WriterStopped),
                };
                let Some(client) = slot.as_mut() else {
                    *peers.get_mut(index).ok_or(Error::Invalid)? = None;
                    continue;
                };
                if matches!(
                    client.phase,
                    Phase::Tls | Phase::Upload | Phase::Download | Phase::Api
                ) {
                    continue;
                }
                let mut wake_body = false;
                match self.step(client, &mut attempts, body_active, &mut event_count) {
                    Ok(true) => {
                        if matches!(client.phase, Phase::Upload | Phase::Download | Phase::Api) {
                            wake_body = true;
                        }
                        if client.phase == Phase::Close && client.events.take().is_some() {
                            event_count = event_count.checked_sub(1).ok_or(Error::Invalid)?;
                        }
                        if client.phase == Phase::Close
                            && client.body_slot
                            && !client.stop_after_response
                        {
                            client.body_slot = false;
                            body_active.store(0, Ordering::Release);
                        }
                    }
                    Err(Error::WriterStopped) => return Err(Error::WriterStopped),
                    Ok(false) | Err(_) => {
                        if client.stop_after_response {
                            return Err(Error::WriterStopped);
                        }
                        if client.body_slot {
                            body_active.store(0, Ordering::Release);
                        }
                        if client.events.take().is_some() {
                            event_count = event_count.checked_sub(1).ok_or(Error::Invalid)?;
                        }
                        *slot = None;
                    }
                }
                drop(slot);
                if wake_body {
                    uploader.thread().unpark();
                }
            }
            cursor = (cursor + 1) % slots.len();
            for listener in self.listeners {
                if time.monotonic < accept_after {
                    break;
                }
                let free = slots.iter().enumerate().find_map(|(index, cell)| {
                    let slot = cell.try_lock().ok()?;
                    slot.is_none().then_some((index, slot))
                });
                let Some((index, mut slot)) = free else {
                    break;
                };
                let (socket, address) = match listener.socket.accept() {
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
                if peers
                    .iter()
                    .filter(|peer| **peer == Some(address.ip()))
                    .count()
                    >= PER_PEER
                {
                    continue;
                }
                if socket.set_nonblocking(true).is_err() {
                    continue;
                }
                let allocated = (|| {
                    let input = buffer::<TLS_WIRE_BYTES>()?;
                    let output = buffer::<TLS_WIRE_BYTES>()?;
                    let head: Box<[u8]> = buffer::<HEAD_BYTES>()?;
                    let input_plain: Box<[u8]> = buffer::<QUANTUM>()?;
                    let mut response = Vec::new();
                    response
                        .try_reserve_exact(512)
                        .map_err(|_| Error::Capacity)?;
                    Ok::<_, Error>((input, output, head, input_plain, response))
                })();
                let (input, output, head, input_plain, response) = match allocated {
                    Ok(buffers) => buffers,
                    Err(_) => {
                        accept_after =
                            Tick(time.monotonic.0.checked_add(5000).ok_or(Error::Invalid)?);
                        continue;
                    }
                };
                let now = self.clock.sample()?.monotonic;
                let absolute = deadline(now, timers::SESSION_SECONDS)?;
                let phase_deadline =
                    deadline(now, self.timeouts.limits().handshake_seconds)?.min(absolute);
                let client = Client {
                    peer: address.ip(),
                    phase: Phase::Tls,
                    pending: Some(socket),
                    wire: Some((input, output)),
                    policy: listener.policy,
                    connection: None,
                    head: HeadReader::new(head),
                    input: input_plain,
                    input_used: 0,
                    drain_left: DRAIN_BYTES,
                    response,
                    sent: 0,
                    attempt_admitted: false,
                    device: None,
                    body_slot: false,
                    events: None,
                    stop_after_response: false,
                    absolute,
                    phase_deadline,
                    idle: phase_deadline,
                    tls_at: now,
                };
                *peers.get_mut(index).ok_or(Error::Invalid)? = Some(address.ip());
                *slot = Some(client);
                worker.thread().unpark();
            }
            std::thread::sleep(SCAN);
        }
        Ok(())
    }
    fn step(
        &self,
        client: &mut Client,
        attempts: &mut Attempts,
        body_active: &AtomicU8,
        event_count: &mut usize,
    ) -> Result<bool, Error> {
        let time = self.clock.sample()?;
        if client.absolute.expired(time.monotonic)
            || client.phase_deadline.expired(time.monotonic)
            || client.idle.expired(time.monotonic)
        {
            return Ok(false);
        }
        match client.phase {
            Phase::Head => {
                let progress = client
                    .connection
                    .as_mut()
                    .ok_or(Error::Invalid)?
                    .read(&mut client.input)?;
                match progress {
                    IoProgress::Pending => (),
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
                            Ok(progress) if progress.complete => client.phase = Phase::Authorize,
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
                let status = if !head.target_origin.map_or_else(
                    || http_auth::same_authority(self.origin, head.host),
                    |value| {
                        value.split_once("://").is_some_and(|(scheme, authority)| {
                            scheme.eq_ignore_ascii_case("https")
                                && http_auth::same_authority(self.origin, authority)
                        })
                    },
                ) {
                    421
                } else if head.origin.is_some_and(|value| {
                    !Origin::parse(value).is_ok_and(|origin| self.origin.same_origin(origin))
                }) {
                    403
                } else if !client.attempt_admitted && !attempts.admit(client.peer, time.monotonic.0)
                {
                    429
                } else {
                    client.attempt_admitted = true;
                    let devices = match self.devices.try_lock() {
                        Ok(devices) => devices,
                        Err(TryLockError::WouldBlock) => return Ok(true),
                        Err(_) => return Err(Error::WriterStopped),
                    };
                    match http_auth::basic(&devices, self.username, head.authorization) {
                        Ok(Some(device)) => {
                            if let Some(backend) = self.uploads {
                                if let Some(account) = head.target.strip_prefix("/jmap/upload/") {
                                    if head.method != "POST" {
                                        405
                                    } else if crate::wire::Id::parse(account)
                                        .ok()
                                        .and_then(|id| id.local())
                                        != Some(backend.account())
                                    {
                                        404
                                    } else if let Err(active) = body_active.compare_exchange(
                                        0,
                                        1,
                                        Ordering::AcqRel,
                                        Ordering::Acquire,
                                    ) {
                                        if active == 2 {
                                            503
                                        } else {
                                            429
                                        }
                                    } else {
                                        client.body_slot = true;
                                        client.device = Some(device);
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
                                    } else if let Err(active) = body_active.compare_exchange(
                                        0,
                                        1,
                                        Ordering::AcqRel,
                                        Ordering::Acquire,
                                    ) {
                                        if active == 2 {
                                            503
                                        } else {
                                            429
                                        }
                                    } else {
                                        client.body_slot = true;
                                        client.device = Some(device);
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
                                    } else if let Err(active) = body_active.compare_exchange(
                                        0,
                                        2,
                                        Ordering::AcqRel,
                                        Ordering::Acquire,
                                    ) {
                                        drop(devices);
                                        if active == 2 {
                                            return self.respond_body(client, 429, time.utc_ms,
                                                br#"{"type":"urn:ietf:params:jmap:error:limit","status":429,"limit":"maxConcurrentRequests"}"#).map(|()| true);
                                        }
                                        503
                                    } else {
                                        client.body_slot = true;
                                        client.device = Some(device);
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
                                        drop(devices);
                                        return self
                                            .respond_body(client, 200, time.utc_ms, session.bytes())
                                            .map(|()| true);
                                    } else {
                                        503
                                    }
                                } else if let Some(query) =
                                    head.target.strip_prefix("/jmap/events?")
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
                                                    drop(devices);
                                                    return self
                                                        .respond(client, 400, time.utc_ms)
                                                        .map(|()| true);
                                                }
                                            };
                                        drop(devices);
                                        let snapshot =
                                            match backend.event_snapshot(device, client.absolute) {
                                                Ok(snapshot) => snapshot,
                                                Err(Error::Busy) => return Ok(true),
                                                Err(Error::Forbidden) => {
                                                    return self
                                                        .respond(client, 401, time.utc_ms)
                                                        .map(|()| true);
                                                }
                                                Err(error) => return Err(error),
                                            };
                                        let stream = crate::jmap_events::Stream::new(
                                            options,
                                            snapshot,
                                            head.last_event_id,
                                            time.monotonic,
                                        )?;
                                        client.device = Some(device);
                                        self.begin_events(
                                            client,
                                            stream,
                                            time.utc_ms,
                                            time.monotonic,
                                        )?;
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
                self.respond(client, status, time.utc_ms)?;
            }
            Phase::Events => return self.events(client, time.monotonic),
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
                        IoProgress::Pending => (),
                        IoProgress::Closed => return Ok(false),
                    }
                } else if connection.flush()? == FlushProgress::Complete {
                    client.phase = Phase::Close;
                    let now = self.clock.sample()?.monotonic;
                    client.phase_deadline = deadline(now, DRAIN_SECONDS)?.min(client.absolute);
                    client.idle = client.phase_deadline;
                }
            }
            Phase::Close => {
                if client.connection.as_mut().ok_or(Error::Invalid)?.close()?
                    == FlushProgress::Complete
                {
                    if client.stop_after_response {
                        return Err(Error::WriterStopped);
                    }
                    client.phase = Phase::Drain;
                }
            }
            Phase::Drain => {
                return drain(
                    client.connection.as_mut().ok_or(Error::Invalid)?,
                    &mut client.input,
                    &mut client.drain_left,
                );
            }
            Phase::Tls | Phase::Upload | Phase::Download | Phase::Api => {
                return Err(Error::Invalid)
            }
        }
        Ok(true)
    }
    fn download(&self, client: &mut Client, control: &Control) -> Result<(), Error> {
        use crate::https_download::{Failure, Request, Target};
        let backend = self.uploads.ok_or(Error::Invalid)?;
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
        client.absolute = operation;
        client.head = HeadReader::new(Vec::new().into_boxed_slice());
        client.input.fill(0);
        client.input_used = 0;
        client.drain_left = 0;
        let result = backend.download(
            Request {
                target: &target,
                device: client.device.ok_or(Error::Invalid)?,
                deadline: operation,
            },
            client.connection.as_mut().ok_or(Error::Invalid)?,
            control,
        );
        match result {
            Ok(()) => {
                client.phase = Phase::Close;
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
    fn api(&self, client: &mut Client, control: &Control) -> Result<(), Error> {
        use crate::https_download::Failure;
        let backend = self.uploads.ok_or(Error::Invalid)?;
        let head = client.head.head().map_err(|_| Error::Invalid)?;
        let framing = head.framing;
        let expect_continue = head.expect_continue;
        client.head = HeadReader::new(Vec::new().into_boxed_slice());
        let result = backend.api(
            crate::https_api::Request {
                device: client.device.ok_or(Error::Invalid)?,
                framing,
                expect_continue,
                prefix: client
                    .input
                    .get(..client.input_used)
                    .ok_or(Error::Invalid)?,
                deadline: client.absolute,
            },
            client.connection.as_mut().ok_or(Error::Invalid)?,
            control,
        );
        client.input.fill(0);
        client.input_used = 0;
        client.drain_left = 0;
        match result {
            Ok(()) => {
                client.phase = Phase::Close;
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
        let backend = self.uploads.ok_or(Error::Invalid)?;
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
        client.head = HeadReader::new(Vec::new().into_boxed_slice());
        let media = std::str::from_utf8(media_bytes.get(..media_len).ok_or(Error::Invalid)?)
            .map_err(|_| Error::Invalid)?;
        let result = backend.receive(
            https_upload::Request {
                account: backend.account(),
                device: client.device.ok_or(Error::Invalid)?,
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
        client.head = HeadReader::new(Vec::new().into_boxed_slice());
        client.response.clear();
        client
            .response
            .try_reserve(512)
            .map_err(|_| Error::Capacity)?;
        write!(&mut client.response,
            "HTTP/1.1 200 OK\r\nDate: {}\r\nConnection: close\r\nContent-Type: text/event-stream; charset=utf-8\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\n\r\n",
            http_date(utc_ms)?)?;
        client.events = Some(EventState {
            stream,
            headers: true,
            pending: false,
            last: now,
        });
        client.sent = 0;
        client.phase = Phase::Events;
        client.phase_deadline = client.absolute;
        client.idle = idle;
        Ok(())
    }
    fn events(&self, client: &mut Client, now: Tick) -> Result<bool, Error> {
        let state = client.events.as_mut().ok_or(Error::Invalid)?;
        if now < state.last {
            return Err(Error::WriterStopped);
        }
        state.last = now;
        let backend = self.uploads.ok_or(Error::Invalid)?;
        let snapshot =
            match backend.event_snapshot(client.device.ok_or(Error::Invalid)?, client.absolute) {
                Ok(snapshot) => snapshot,
                Err(Error::Busy) => return Ok(true),
                Err(error) => return Err(error),
            };
        let event = client.events.as_mut().ok_or(Error::Invalid)?;
        let connection = client.connection.as_mut().ok_or(Error::Invalid)?;
        if event.headers {
            if client.sent < client.response.len() {
                match connection.write(client.response.get(client.sent..).ok_or(Error::Invalid)?)? {
                    IoProgress::Bytes(n) if n > 0 && n <= client.response.len() - client.sent => {
                        client.sent += n
                    }
                    IoProgress::Pending => (),
                    _ => return Ok(false),
                }
            } else if connection.flush()? == FlushProgress::Complete {
                event.headers = false;
                client.sent = 0;
                client.response.clear();
                client.idle = client.absolute;
            }
            return Ok(true);
        }
        let frame = event.stream.prepare(snapshot, now)?;
        if frame.is_empty() {
            // A silent stream still detects EOF; no pipelined requests are accepted.
            return match connection.read(&mut client.input)? {
                IoProgress::Pending => Ok(true),
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
                IoProgress::Pending => (),
                _ => return Ok(false),
            }
        } else if connection.flush()? == FlushProgress::Complete {
            event.stream.acknowledge(self.clock.sample()?.monotonic)?;
            event.pending = false;
            client.sent = 0;
            client.idle = client.absolute;
            if event.stream.closed() {
                client.phase = Phase::Close;
                client.phase_deadline = deadline(now, DRAIN_SECONDS)?.min(client.absolute);
                client.idle = client.phase_deadline;
            }
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
        client.input.fill(0);
        client.input_used = 0;
        // Retire credentials before allocating or waiting on Session output.
        client.head = HeadReader::new(Vec::new().into_boxed_slice());
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
        write!(&mut client.response, "HTTP/1.1 {status} {reason}\r\nDate: {}\r\nConnection: close\r\nContent-Length: {}\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\n", http_date(utc_ms)?, body.len())?;
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
pub(crate) fn http_date(utc_ms: i64) -> Result<String, Error> {
    let seconds = u64::try_from(utc_ms).map_err(|_| Error::Invalid)? / 1000;
    if seconds > 253402300799 {
        return Err(Error::Invalid);
    }
    let day = seconds / 86400;
    let weekday = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"]
        .get((day % 7) as usize)
        .ok_or(Error::Invalid)?;
    let mut remaining = day;
    let mut year = 1970;
    let leap = |year| year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    loop {
        let days = if leap(year) { 366 } else { 365 };
        if remaining < days {
            break;
        }
        remaining -= days;
        year += 1;
    }
    let months = [
        ("Jan", 31),
        ("Feb", if leap(year) { 29 } else { 28 }),
        ("Mar", 31),
        ("Apr", 30),
        ("May", 31),
        ("Jun", 30),
        ("Jul", 31),
        ("Aug", 31),
        ("Sep", 30),
        ("Oct", 31),
        ("Nov", 30),
        ("Dec", 31),
    ];
    for (month, days) in months {
        if remaining < days {
            return Ok(format!(
                "{weekday}, {:02} {month} {year:04} {:02}:{:02}:{:02} GMT",
                remaining + 1,
                seconds / 3600 % 24,
                seconds / 60 % 60,
                seconds % 60
            ));
        }
        remaining -= days;
    }
    Err(Error::Invalid)
}

#[cfg(test)]
mod tests {
    use super::*;
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
    #[test]
    fn date_uses_imf_fixdate_across_leap_and_century_boundaries() {
        for (millis, expected) in [
            (0, "Thu, 01 Jan 1970 00:00:00 GMT"),
            (951782400000, "Tue, 29 Feb 2000 00:00:00 GMT"),
            (4107542400000, "Mon, 01 Mar 2100 00:00:00 GMT"),
            (253402300799000, "Fri, 31 Dec 9999 23:59:59 GMT"),
        ] {
            assert_eq!(http_date(millis).ok().as_deref(), Some(expected));
        }
        assert!(http_date(-1).is_err());
        assert!(http_date(i64::MAX).is_err());
    }
}

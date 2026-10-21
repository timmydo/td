use super::*;
use crate::{
    admission::{timers::NetworkLimits, DiskLimits, ViewMode, WorkLimits},
    config::{endpoint::Origin, listener::Kind},
    http_auth,
    https::{BoundListener, Https},
    ids::{AccountId, DeviceId},
    limits::Limits,
    ports::{Deadline, FlushProgress, Handshake, IoProgress, TlsTransport, Transport},
    smtp_receiving::Control,
    transport::TcpTransport,
};
use std::{
    net::{TcpListener, TcpStream},
    sync::{atomic::AtomicU64, Mutex},
    time::{Duration, Instant},
};
struct Now(Instant, AtomicU64);
impl Clock for Now {
    fn sample(&self) -> Result<Time, Error> {
        Ok(Time {
            utc_ms: 1_800_000_000_000,
            monotonic: Tick(self.0.elapsed().as_millis() as u64 + self.1.load(Ordering::Relaxed)),
        })
    }
}
struct Stop<'a>(&'a Control);
impl Drop for Stop<'_> {
    fn drop(&mut self) {
        self.0.stop();
    }
}
fn wait(mut test: impl FnMut() -> bool) {
    let until = Instant::now() + Duration::from_secs(5);
    while !test() {
        assert!(Instant::now() < until, "HTTPS progress timed out");
        std::thread::sleep(Duration::from_millis(1));
    }
}
fn client(
    address: std::net::SocketAddr,
    lease: &GenerationLease<TlsPolicies>,
    clock: Arc<dyn Clock>,
) -> TlsConnection<Box<[u8; TLS_WIRE_BYTES]>> {
    let pool = HandshakePool::new(1).unwrap();
    let (input, output) = buffers();
    let cap = Deadline::after(clock.sample().unwrap().monotonic, 20_000).unwrap();
    let absolute = Deadline::after(clock.sample().unwrap().monotonic, 3_600_000).unwrap();
    let reserved = TlsPolicies::reserve_session(
        lease.clone(),
        TlsPolicies::relay(lease).unwrap(),
        &pool,
        input,
        output,
    )
    .unwrap()
    .construct()
    .unwrap();
    let mut client = reserved
        .handoff(
            TcpTransport::from_stream(TcpStream::connect(address).unwrap()).unwrap(),
            &[],
            clock,
            absolute,
            cap,
        )
        .unwrap();
    wait(|| matches!(client.handshake(cap).unwrap(), Handshake::Complete(_)));
    client
}
fn send(client: &mut impl Transport, request: &[u8]) {
    let mut sent = 0;
    wait(|| {
        if sent < request.len() {
            match client.write(&request[sent..]).unwrap() {
                IoProgress::Bytes(n) => sent += n,
                IoProgress::Pending => (),
                IoProgress::Closed => panic!("HTTPS closed before request"),
            }
            false
        } else {
            client.flush().unwrap() == FlushProgress::Complete
        }
    });
}
fn request(client: &mut impl Transport, request: &[u8]) -> String {
    send(client, request);
    let mut reply = Vec::new();
    let mut scratch = [0; 1024];
    wait(|| {
        match client.read(&mut scratch).unwrap() {
            IoProgress::Bytes(n) => reply.extend_from_slice(&scratch[..n]),
            IoProgress::Pending => (),
            IoProgress::Closed => panic!("HTTPS closed before response"),
        }
        reply.ends_with(b"\r\n\r\n")
    });
    String::from_utf8(reply).unwrap()
}
fn base64(bytes: &[u8]) -> String {
    let alphabet = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for group in bytes.chunks(3) {
        let bits = (u32::from(group[0]) << 16)
            | (u32::from(group.get(1).copied().unwrap_or(0)) << 8)
            | u32::from(group.get(2).copied().unwrap_or(0));
        out.push(char::from(alphabet[(bits >> 18) as usize]));
        out.push(char::from(alphabet[((bits >> 12) & 63) as usize]));
        out.push(if group.len() > 1 {
            char::from(alphabet[((bits >> 6) & 63) as usize])
        } else {
            '='
        });
        out.push(if group.len() > 2 {
            char::from(alphabet[(bits & 63) as usize])
        } else {
            '='
        });
    }
    out
}
#[test]
fn https_enforces_auth_origin_framing_revocation_and_idle_deadlines() {
    let _serial = serial();
    let mut devices = TestCredentials::new(AccountId::from_bytes([0xaa; 16]));
    let (id, password) = devices
        .create(
            "client",
            1,
            &mut td_crypto::SystemEntropy::try_new().unwrap(),
        )
        .unwrap();
    let authorization = base64(&[b"private-user:".as_slice(), password.expose()].concat());
    let mut incorrect = password.expose().to_vec();
    let last = incorrect.last_mut().unwrap();
    *last = if *last == b'0' { b'1' } else { b'0' };
    let incorrect = base64(&[b"private-user:".as_slice(), &incorrect].concat());
    let devices = Mutex::new(devices);
    struct Delayed<'a> {
        inner: &'a Mutex<TestCredentials>,
        armed: AtomicBool,
        entered: AtomicBool,
        released: AtomicBool,
    }
    impl http_auth::Authenticator for Delayed<'_> {
        fn authenticate(
            &self,
            username: &str,
            password: &[u8],
            deadline: Deadline,
        ) -> Result<Option<http_auth::Principal>, Error> {
            assert_eq!(std::thread::current().name(), Some("https-auth"));
            if self.armed.swap(false, Ordering::AcqRel) {
                self.entered.store(true, Ordering::Release);
                let until = Instant::now() + Duration::from_secs(5);
                while !self.released.load(Ordering::Acquire) {
                    if Instant::now() >= until {
                        return Err(Error::Deadline);
                    }
                    std::thread::sleep(Duration::from_millis(1));
                }
            }
            http_auth::Authenticator::authenticate(self.inner, username, password, deadline)
        }
    }
    let authentication = Delayed {
        inner: &devices,
        armed: AtomicBool::new(false),
        entered: AtomicBool::new(false),
        released: AtomicBool::new(false),
    };
    let material = Material::new();
    let socket = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = socket.local_addr().unwrap();
    let origin_text = format!("https://localhost:{}", address.port());
    let source = source(false)
        .replace("https://localhost", &origin_text)
        .replace("127.0.0.1:443", &address.to_string());
    let lease = material.published(&source);
    let resolved = resolved(&source);
    let resource = Limits::default().plan().unwrap();
    let admission = DiskLimits::default()
        .plan(&resource, WorkLimits::default(), ViewMode::OnlineBackground)
        .unwrap();
    let timeouts = NetworkLimits::default().plan(&admission).unwrap();
    let clock = Arc::new(Now(Instant::now(), AtomicU64::new(0)));
    let handshakes = HandshakePool::new(2).unwrap();
    let ready = AtomicBool::new(false);
    let control = Control::default();
    resolved
        .candidate()
        .with_graph(|records, graph| {
            let rows = records.view(graph).unwrap().listeners().unwrap();
            let row = (0..rows.len())
                .filter_map(|index| rows.listener(index).unwrap())
                .find(|row| row.kind == Kind::Https)
                .unwrap();
            let listeners = [BoundListener::new(socket, row, &lease).unwrap()];
            let server = Https {
                backends: &[],
                listeners: &listeners,
                policies: &lease,
                handshakes: &handshakes,
                authentication: &authentication,
                origin: Origin::parse(&origin_text).unwrap(),
                resources: &resource,
                timeouts: &timeouts,
                clock: clock.clone(),
                ready: &ready,
            };
            std::thread::scope(|scope| {
                use std::io::{Read, Write};
                let worker = scope.spawn(|| server.run(&control));
                let stop = Stop(&control);
                wait(|| {
                    assert!(
                        !worker.is_finished(),
                        "HTTPS worker stopped before readiness"
                    );
                    ready.load(Ordering::Acquire)
                });
                // Idle TCP peers consume connection capacity but no handshake permit.
                let idle_a = TcpStream::connect(address).unwrap();
                let idle_b = TcpStream::connect(address).unwrap();
                std::thread::sleep(Duration::from_millis(50));
                assert_eq!(handshakes.available(), 2);
                let mut excess = TcpStream::connect(address).unwrap();
                excess.set_nonblocking(true).unwrap();
                wait(|| match excess.read(&mut [0; 1]) {
                    Ok(0) => true,
                    Err(error) => error.kind() == io::ErrorKind::ConnectionReset,
                    Ok(_) => false,
                });
                drop((idle_a, idle_b, excess));
                std::thread::sleep(Duration::from_millis(50));
                // A full shared pool delays ClientHello admission instead of closing TCP.
                let first = handshakes.reserve().unwrap();
                let second = handshakes.reserve().unwrap();
                let release = scope.spawn(move || {
                    std::thread::sleep(Duration::from_millis(150));
                    drop((first, second));
                });
                let mut queued = client(address, &lease, clock.clone());
                queued.abort();
                release.join().unwrap();
                std::thread::sleep(Duration::from_millis(50));
                {
                    let mut plain = TcpStream::connect(address).unwrap();
                    plain
                        .set_read_timeout(Some(Duration::from_secs(2)))
                        .unwrap();
                    plain
                        .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n")
                        .unwrap();
                    let mut bytes = [0; 512];
                    match plain.read(&mut bytes) {
                        Ok(n) => assert!(!bytes[..n].starts_with(b"HTTP/")),
                        Err(error) => assert!(matches!(
                            error.kind(),
                            io::ErrorKind::WouldBlock
                                | io::ErrorKind::TimedOut
                                | io::ErrorKind::ConnectionReset
                        )),
                    }
                    clock.1.store(11_000, Ordering::Relaxed);
                    plain.set_nonblocking(true).unwrap();
                    wait(|| match plain.read(&mut bytes) {
                        Ok(0) => true,
                        Ok(n) => {
                            assert!(!bytes[..n].starts_with(b"HTTP/"));
                            false
                        }
                        Err(error) => error.kind() != io::ErrorKind::WouldBlock,
                    });
                }
                let host = format!("localhost:{}", address.port());
                // Refuse misdirected traffic even while credential publication holds the lock.
                let registry = devices.lock().unwrap();
                let mut wire = client(address, &lease, clock.clone());
                let head = "GET / HTTP/1.1\r\nHost: evil.test\r\n\r\n";
                assert!(request(&mut wire, head.as_bytes()).starts_with("HTTP/1.1 421 "));
                wire.abort();
                drop(registry);
                std::thread::sleep(Duration::from_millis(20));
                // Retrying a busy registry charges this request only once.
                clock.1.fetch_add(2_000, Ordering::Relaxed);
                let registry = devices.lock().unwrap();
                authentication.armed.store(true, Ordering::Release);
                let reply = std::thread::scope(|waiting_scope| {
                    let waiting = waiting_scope.spawn(|| {
                        let mut wire = client(address, &lease, clock.clone());
                        let head = format!("GET / HTTP/1.1\r\nHost: {host}\r\nAuthorization: Basic {authorization}\r\n\r\n");
                        request(&mut wire, head.as_bytes())
                    });
                    wait(|| authentication.entered.load(Ordering::Acquire));
                    // A blocked credential backend cannot stall handshakes or
                    // the reactor's origin refusal for another client.
                    let mut other = client(address, &lease, clock.clone());
                    assert!(request(&mut other, b"GET / HTTP/1.1\r\nHost: evil.test\r\n\r\n").starts_with("HTTP/1.1 421 "));
                    other.abort();
                    authentication.released.store(true, Ordering::Release);
                    std::thread::sleep(Duration::from_millis(250));
                    drop(registry);
                    waiting.join().unwrap()
                });
                assert!(reply.starts_with("HTTP/1.1 404 "));
                std::thread::sleep(Duration::from_millis(20));
                // The late body arrives after the server has sent its refusal and close_notify.
                clock.1.fetch_add(2_000, Ordering::Relaxed);
                let mut post = client(address, &lease, clock.clone());
                let body = "x".repeat(32 * 1024);
                let head = format!("POST /jmap HTTP/1.1\r\nHost: {host}\r\nAuthorization: Basic {authorization}\r\nContent-Length: {}\r\n\r\n", body.len());
                assert!(request(&mut post, head.as_bytes()).starts_with("HTTP/1.1 404 "));
                std::thread::sleep(Duration::from_millis(100));
                send(&mut post, body.as_bytes());
                wait(|| post.close().unwrap() == FlushProgress::Complete);
                wait(|| post.read(&mut [0; 32]).unwrap() == IoProgress::Closed);
                std::thread::sleep(Duration::from_millis(20));
                // Two unfinished bodies occupy the per-peer slots. A trickle must not
                // renew the shared close/drain deadline when the fake clock advances.
                clock.1.fetch_add(2_000, Ordering::Relaxed);
                let mut silent = client(address, &lease, clock.clone());
                let mut trickle = client(address, &lease, clock.clone());
                let head = format!("POST /jmap HTTP/1.1\r\nHost: {host}\r\nAuthorization: Basic {authorization}\r\nContent-Length: 65536\r\n\r\n");
                assert!(request(&mut silent, head.as_bytes()).starts_with("HTTP/1.1 404 "));
                assert!(request(&mut trickle, head.as_bytes()).starts_with("HTTP/1.1 404 "));
                std::thread::sleep(Duration::from_millis(100));
                let mut refused = TcpStream::connect(address).unwrap();
                refused.set_nonblocking(true).unwrap();
                wait(|| match refused.read(&mut [0; 1]) {
                    Ok(0) => true,
                    Err(error) => error.kind() == io::ErrorKind::ConnectionReset,
                    Ok(_) => false,
                });
                clock.1.fetch_add(4_000, Ordering::Relaxed);
                send(&mut trickle, b"x");
                std::thread::sleep(Duration::from_millis(50));
                clock.1.fetch_add(1_001, Ordering::Relaxed);
                std::thread::sleep(Duration::from_millis(50));
                let mut released = client(address, &lease, clock.clone());
                released.abort();
                drop((silent, trickle, refused, post));
                std::thread::sleep(Duration::from_millis(20));
                let oversized = "x".repeat(8192);
                let wrong_user = base64(b"wrong:password");
                let wrong_password = base64(b"private-user:bad-password");
                let cases = [
                    (
                        "/",
                        host.as_str(),
                        format!("Authorization: Basic {incorrect}\r\n"),
                        401,
                    ),
                    ("/", host.as_str(), String::new(), 401),
                    (
                        "/",
                        &host,
                        format!("Authorization: Basic {wrong_user}\r\n"),
                        401,
                    ),
                    (
                        "/",
                        &host,
                        format!("Authorization: Basic {wrong_password}\r\n"),
                        401,
                    ),
                    ("/", &host, format!("X: {oversized}\r\n"), 431),
                    ("/", "evil.test", String::new(), 421),
                    ("/", "localhost", String::new(), 421),
                    ("/", &host, "Origin: https://evil.test\r\n".into(), 403),
                    (
                        "/",
                        &host,
                        format!("Authorization: Basic {authorization}\r\n"),
                        404,
                    ),
                    (
                        "/",
                        &host,
                        format!(
                            "Origin: {origin_text}\r\nAuthorization: Basic {authorization}\r\n"
                        ),
                        404,
                    ),
                    (
                        "/.well-known/jmap",
                        &host,
                        format!("Authorization: Basic {authorization}\r\n"),
                        503,
                    ),
                    (
                        "/",
                        &host,
                        "Content-Length: 0\r\nTransfer-Encoding: chunked\r\n".into(),
                        400,
                    ),
                ];
                for (target, authority, fields, status) in cases {
                    clock.1.fetch_add(2_000, Ordering::Relaxed);
                    let head =
                        format!("GET {target} HTTP/1.1\r\nHost: {authority}\r\n{fields}\r\n");
                    let mut wire = client(address, &lease, clock.clone());
                    let reply = request(&mut wire, head.as_bytes());
                    assert!(reply.starts_with(&format!("HTTP/1.1 {status} ")), "{reply}");
                    assert!(reply.contains("Connection: close\r\n"));
                    assert!(!reply.contains(&authorization));
                    if status == 401 {
                        assert!(reply.contains("WWW-Authenticate: Basic realm=\"td-mta\""));
                    }
                    wire.abort();
                    std::thread::sleep(Duration::from_millis(20));
                }
                devices.lock().unwrap().revoke(id, 2).unwrap();
                let mut wire = client(address, &lease, clock.clone());
                let head = format!(
                "GET / HTTP/1.1\r\nHost: {host}\r\nAuthorization: Basic {authorization}\r\n\r\n"
            );
                assert!(request(&mut wire, head.as_bytes()).starts_with("HTTP/1.1 401 "));
                wire.abort();
                std::thread::sleep(Duration::from_millis(20));
                let mut idle = client(address, &lease, clock.clone());
                let mut active = client(address, &lease, clock.clone());
                let head = format!("GET / HTTP/1.1\r\nHost: {host}\r\n\r\n");
                assert!(request(&mut active, head.as_bytes()).starts_with("HTTP/1.1 401 "));
                clock.1.fetch_add(31_000, Ordering::Relaxed);
                let mut scratch = [0; 1];
                wait(|| matches!(idle.read(&mut scratch), Err(_) | Ok(IoProgress::Closed)));
                drop(stop);
                assert!(worker.join().unwrap().is_ok());
                assert_eq!(handshakes.available(), handshakes.capacity());
            });
        })
        .unwrap();
}

#[test]
fn stopped_upload_receipt_retires_after_expiry_or_shutdown() {
    for late in [false, true] {
        stopped_upload_receipt(Some(late));
    }
}
#[test]
fn uncertain_upload_closes_only_its_exchange_without_a_receipt() {
    stopped_upload_receipt(None);
}
fn stopped_upload_receipt(late: Option<bool>) {
    struct Late<'a>(&'a Now, Option<bool>);
    impl crate::https_upload::Backend for Late<'_> {
        fn account(&self) -> AccountId {
            AccountId::from_bytes([0xaa; 16])
        }
        fn receive(
            &self,
            request: crate::https_upload::Request<'_>,
            _: &mut dyn Transport,
            control: &Control,
        ) -> Result<crate::https_upload::Response, Error> {
            let Some(late) = self.1 else {
                return Ok(crate::https_upload::Response::uncertain());
            };
            if late {
                self.0
                     .1
                    .store(request.deadline.tick().0 + 1, Ordering::Relaxed);
            } else {
                control.stop();
            }
            Ok(crate::https_upload::Response::stopped_for_test())
        }
    }
    let _serial = serial();
    let account = AccountId::from_bytes([0xaa; 16]);
    let mut devices = TestCredentials::new(account);
    let (_, password) = devices
        .create("late", 1, &mut td_crypto::SystemEntropy::try_new().unwrap())
        .unwrap();
    let authorization = base64(&[b"private-user:".as_slice(), password.expose()].concat());
    let devices = Mutex::new(devices);
    let material = Material::new();
    let socket = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = socket.local_addr().unwrap();
    let origin_text = format!("https://localhost:{}", address.port());
    let source = source(false)
        .replace("https://localhost", &origin_text)
        .replace("127.0.0.1:443", &address.to_string());
    let lease = material.published(&source);
    let resolved = resolved(&source);
    let resource = Limits::default().plan().unwrap();
    let admission = DiskLimits::default()
        .plan(&resource, WorkLimits::default(), ViewMode::OnlineBackground)
        .unwrap();
    let timeouts = NetworkLimits::default().plan(&admission).unwrap();
    let clock = Arc::new(Now(Instant::now(), AtomicU64::new(0)));
    let late = Late(&clock, late);
    let ready = AtomicBool::new(false);
    let control = Control::default();
    let handshakes = HandshakePool::new(2).unwrap();
    resolved
        .candidate()
        .with_graph(|records, graph| {
            let rows = records.view(graph).unwrap().listeners().unwrap();
            let row = (0..rows.len())
                .filter_map(|i| rows.listener(i).unwrap())
                .find(|r| r.kind == Kind::Https)
                .unwrap();
            let listeners = [BoundListener::new(socket, row, &lease).unwrap()];
            let server = Https {
                backends: &[&late],
                listeners: &listeners,
                policies: &lease,
                handshakes: &handshakes,
                authentication: &devices,
                origin: Origin::parse(&origin_text).unwrap(),
                resources: &resource,
                timeouts: &timeouts,
                clock: clock.clone(),
                ready: &ready,
            };
            std::thread::scope(|scope| {
                let worker = scope.spawn(|| server.run(&control));
                let _stop = Stop(&control);
                wait(|| ready.load(Ordering::Acquire));
                let mut wire = client(address, &lease, clock.clone());
                let mut account_bytes = [0; crate::wire::IdKind::WIRE_BYTES];
                let account_text = crate::wire::encode_id(account, &mut account_bytes).unwrap();
                let head = format!(
                    "POST /jmap/upload/{account_text} HTTP/1.1\r\n\
                Host: localhost:{}\r\n\
                Authorization: Basic {authorization}\r\n\
                Content-Length: 0\r\n\
                \r\n\
                ",
                    address.port()
                );
                send(&mut wire, head.as_bytes());
                if late.1.is_some() {
                    wait(|| worker.is_finished());
                }
                let mut bytes = [0; 4096];
                wait(|| match wire.read(&mut bytes) {
                    Ok(IoProgress::Pending) => false,
                    Ok(IoProgress::Bytes(_)) => panic!("expired exchange wrote a receipt"),
                    Ok(IoProgress::Closed) | Err(_) => true,
                });
                if late.1.is_none() {
                    assert!(!control.stop_requested());
                    assert!(!worker.is_finished());
                    let mut next = client(address, &lease, clock.clone());
                    send(&mut next, head.as_bytes());
                    wait(|| match next.read(&mut bytes) {
                        Ok(IoProgress::Pending) => false,
                        Ok(IoProgress::Bytes(_)) => panic!("uncertain upload wrote a receipt"),
                        Ok(IoProgress::Closed) | Err(_) => true,
                    });
                    assert!(!control.stop_requested());
                    control.stop();
                    worker.join().unwrap().unwrap();
                } else {
                    assert_eq!(worker.join().unwrap(), Err(Error::WriterStopped));
                }
            });
        })
        .unwrap();
}

#[test]
fn download_failures_release_shared_slot_and_shutdown_joins_streaming() {
    use crate::https_download::{Failure, Request};
    struct Script {
        mode: AtomicU64,
        entered: AtomicBool,
        release: AtomicBool,
    }
    impl crate::https_upload::Backend for Script {
        fn account(&self) -> AccountId {
            AccountId::from_bytes([0xaa; 16])
        }
        fn receive(
            &self,
            _: crate::https_upload::Request<'_>,
            _: &mut dyn Transport,
            _: &Control,
        ) -> Result<crate::https_upload::Response, Error> {
            Err(Error::Invalid)
        }
        fn download(
            &self,
            _: Request<'_>,
            transport: &mut dyn Transport,
            control: &Control,
        ) -> Result<(), Failure> {
            match self.mode.load(Ordering::Acquire) {
                0 => Err(Failure::Before(Error::Corrupt)),
                1 => {
                    let bytes = b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\npartial";
                    let mut offset = 0;
                    wait(|| {
                        if offset < bytes.len() {
                            match transport.write(&bytes[offset..]).unwrap() {
                                IoProgress::Bytes(n) => offset += n,
                                IoProgress::Pending => (),
                                IoProgress::Closed => panic!("closed before scripted failure"),
                            }
                            false
                        } else {
                            transport.flush().unwrap() == FlushProgress::Complete
                        }
                    });
                    Err(Failure::Started(Error::Corrupt))
                }
                _ => {
                    self.entered.store(true, Ordering::Release);
                    wait(|| control.stop_requested() || self.release.load(Ordering::Acquire));
                    Err(Failure::Started(Error::Deadline))
                }
            }
        }
    }
    let _serial = serial();
    let account = AccountId::from_bytes([0xaa; 16]);
    let mut devices = TestCredentials::new(account);
    let (_, password) = devices
        .create("late", 1, &mut td_crypto::SystemEntropy::try_new().unwrap())
        .unwrap();
    let authorization = base64(&[b"private-user:".as_slice(), password.expose()].concat());
    let devices = Mutex::new(devices);
    let material = Material::new();
    let socket = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = socket.local_addr().unwrap();
    let origin_text = format!("https://localhost:{}", address.port());
    let source = source(false)
        .replace("https://localhost", &origin_text)
        .replace("127.0.0.1:443", &address.to_string());
    let lease = material.published(&source);
    let resolved = resolved(&source);
    let resource = Limits::default().plan().unwrap();
    let admission = DiskLimits::default()
        .plan(&resource, WorkLimits::default(), ViewMode::OnlineBackground)
        .unwrap();
    let timeouts = NetworkLimits::default().plan(&admission).unwrap();
    let clock = Arc::new(Now(Instant::now(), AtomicU64::new(0)));
    let late = Script {
        mode: AtomicU64::new(0),
        entered: AtomicBool::new(false),
        release: AtomicBool::new(false),
    };
    let ready = AtomicBool::new(false);
    let control = Control::default();
    let handshakes = HandshakePool::new(2).unwrap();
    resolved
        .candidate()
        .with_graph(|records, graph| {
            let rows = records.view(graph).unwrap().listeners().unwrap();
            let row = (0..rows.len())
                .filter_map(|i| rows.listener(i).unwrap())
                .find(|r| r.kind == Kind::Https)
                .unwrap();
            let listeners = [BoundListener::new(socket, row, &lease).unwrap()];
            let server = Https {
                backends: &[&late],
                listeners: &listeners,
                policies: &lease,
                handshakes: &handshakes,
                authentication: &devices,
                origin: Origin::parse(&origin_text).unwrap(),
                resources: &resource,
                timeouts: &timeouts,
                clock: clock.clone(),
                ready: &ready,
            };

            std::thread::scope(|scope| {
                let worker = scope.spawn(|| server.run(&control));
                let stop = Stop(&control);
                wait(|| ready.load(Ordering::Acquire));
                let account = format!("a{}", "aa".repeat(16));
                let get = format!(
                    "GET /jmap/download/{account}/b{}/a?type=text%2Fplain HTTP/1.1\r\n\
                     Host: localhost:{}\r\nAuthorization: Basic {authorization}\r\n\r\n",
                    "11".repeat(16),
                    address.port()
                );
                let mut first = client(address, &lease, clock.clone());
                send(&mut first, get.as_bytes());
                assert_eq!(upload_reply(&mut first).0, 503);
                first.abort();
                std::thread::sleep(Duration::from_millis(30));
                late.mode.store(1, Ordering::Release);
                let mut broken = client(address, &lease, clock.clone());
                send(&mut broken, get.as_bytes());
                let mut received = Vec::new();
                let mut bytes = [0; 1024];
                wait(|| match broken.read(&mut bytes) {
                    Ok(IoProgress::Bytes(n)) => {
                        received.extend_from_slice(&bytes[..n]);
                        false
                    }
                    Ok(IoProgress::Pending) => false,
                    Ok(IoProgress::Closed) | Err(_) => true,
                });
                assert_eq!(
                    received,
                    b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\npartial"
                );
                std::thread::sleep(Duration::from_millis(30));
                late.mode.store(2, Ordering::Release);
                let mut blocked = client(address, &lease, clock.clone());
                send(&mut blocked, get.as_bytes());
                wait(|| late.entered.load(Ordering::Acquire));
                let mut upload = client(address, &lease, clock.clone());
                let post = format!(
                    "POST /jmap/upload/{account} HTTP/1.1\r\nHost: localhost:{}\r\n\
                     Authorization: Basic {authorization}\r\nContent-Length: 0\r\n\r\n",
                    address.port()
                );
                send(&mut upload, post.as_bytes());
                assert_eq!(upload_reply(&mut upload).0, 429);
                upload.abort();
                late.release.store(true, Ordering::Release);
                wait(|| matches!(blocked.read(&mut bytes), Ok(IoProgress::Closed) | Err(_)));
                std::thread::sleep(Duration::from_millis(30));
                late.mode.store(0, Ordering::Release);
                let mut again = client(address, &lease, clock.clone());
                send(&mut again, get.as_bytes());
                assert_eq!(upload_reply(&mut again).0, 503);
                again.abort();
                std::thread::sleep(Duration::from_millis(30));
                late.entered.store(false, Ordering::Release);
                late.release.store(false, Ordering::Release);
                late.mode.store(2, Ordering::Release);
                let mut shutdown = client(address, &lease, clock.clone());
                send(&mut shutdown, get.as_bytes());
                wait(|| late.entered.load(Ordering::Acquire));
                drop(stop);
                wait(|| worker.is_finished());
                assert!(worker.join().unwrap().is_ok());
                assert_eq!(handshakes.available(), handshakes.capacity());
            });
        })
        .unwrap();
}

struct TestCredentials {
    account: AccountId,
    credential: Option<crate::device_credentials::Credential>,
}
impl TestCredentials {
    fn new(account: AccountId) -> Self {
        Self {
            account,
            credential: None,
        }
    }
    fn create(
        &mut self,
        _: &str,
        _: u64,
        entropy: &mut impl td_crypto::Entropy,
    ) -> Result<(DeviceId, crate::device_credentials::DevicePassword), Error> {
        let (credential, password) = crate::device_credentials::generate(self.account, entropy)
            .map_err(|_| Error::Invalid)?;
        let id = credential.id;
        self.credential = Some(credential);
        Ok((id, password))
    }
    fn revoke(&mut self, id: DeviceId, _: u64) -> Result<(), Error> {
        if self.credential.as_ref().map(|c| c.id) != Some(id) {
            return Err(Error::NotFound);
        }
        self.credential = None;
        Ok(())
    }
}
impl http_auth::Authenticator for Mutex<TestCredentials> {
    fn authenticate(
        &self,
        username: &str,
        password: &[u8],
        _: Deadline,
    ) -> Result<Option<http_auth::Principal>, Error> {
        if username != "private-user" {
            return Ok(None);
        }
        let registry = self.try_lock().map_err(|_| Error::Busy)?;
        let Some(credential) = &registry.credential else {
            return Ok(None);
        };
        let Some(presented) = crate::device_credentials::Presented::parse(password) else {
            return Ok(None);
        };
        Ok((presented.id() == credential.id
            && presented
                .matches(registry.account, &credential.salt, &credential.verifier)
                .map_err(|_| Error::Invalid)?)
        .then_some(http_auth::Principal {
            account: registry.account,
            device: credential.id,
        }))
    }
}
fn upload_reply(client: &mut impl Transport) -> (u16, String) {
    let (status, _, body) = full_reply(client);
    (status, body)
}
fn full_reply(client: &mut impl Transport) -> (u16, String, String) {
    let mut bytes = Vec::new();
    let mut scratch = [0; 1024];
    wait(|| {
        match client.read(&mut scratch).unwrap() {
            IoProgress::Bytes(n) => bytes.extend_from_slice(&scratch[..n]),
            IoProgress::Pending => (),
            IoProgress::Closed => panic!("upload closed before complete response"),
        }
        let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") else {
            return false;
        };
        let head = std::str::from_utf8(&bytes[..end]).unwrap();
        let length: usize = head
            .lines()
            .find_map(|line| line.strip_prefix("Content-Length: "))
            .unwrap()
            .parse()
            .unwrap();
        bytes.len() == end + 4 + length
    });
    let response = String::from_utf8(bytes).unwrap();
    let (head, body) = response.split_once("\r\n\r\n").unwrap();
    (
        head.split_whitespace().nth(1).unwrap().parse().unwrap(),
        head.to_owned(),
        body.to_owned(),
    )
}

#[test]
fn authenticated_account_selects_backend_even_when_device_ids_match() {
    struct Accounts;
    impl http_auth::Authenticator for Accounts {
        fn authenticate(
            &self,
            username: &str,
            password: &[u8],
            _: Deadline,
        ) -> Result<Option<http_auth::Principal>, Error> {
            let account = match (username, password) {
                ("first", b"first-password") => AccountId::from_bytes([0xaa; 16]),
                ("second", b"second-password") => AccountId::from_bytes([0xbb; 16]),
                _ => return Ok(None),
            };
            Ok(Some(http_auth::Principal {
                account,
                device: DeviceId::from_bytes([7; 16]),
            }))
        }
    }
    struct Backend {
        account: AccountId,
        session: crate::jmap_session::Session,
        calls: AtomicU64,
    }
    impl crate::https_upload::Backend for Backend {
        fn account(&self) -> AccountId {
            self.account
        }
        fn session(&self) -> Option<&crate::jmap_session::Session> {
            Some(&self.session)
        }
        fn receive(
            &self,
            request: crate::https_upload::Request<'_>,
            _: &mut dyn Transport,
            _: &Control,
        ) -> Result<crate::https_upload::Response, Error> {
            assert_eq!(request.account, self.account);
            assert_eq!(request.device, DeviceId::from_bytes([7; 16]));
            assert_eq!(
                request.credentials.username(),
                if self.account == AccountId::from_bytes([0xaa; 16]) {
                    "first"
                } else {
                    "second"
                }
            );
            self.calls.fetch_add(1, Ordering::Relaxed);
            crate::https_upload::Response::created(
                self.account,
                crate::ids::BlobId::from_bytes([8; 16]),
                0,
                "text/plain",
                false,
            )
        }
    }
    let _serial = serial();
    let material = Material::new();
    let socket = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = socket.local_addr().unwrap();
    let origin_text = format!("https://localhost:{}", address.port());
    let source = source(false)
        .replace("https://localhost", &origin_text)
        .replace("127.0.0.1:443", &address.to_string());
    let lease = material.published(&source);
    let resolved = resolved(&source);
    let resource = Limits::default().plan().unwrap();
    let admission = DiskLimits::default()
        .plan(&resource, WorkLimits::default(), ViewMode::OnlineBackground)
        .unwrap();
    let timeouts = NetworkLimits::default().plan(&admission).unwrap();
    let clock = Arc::new(Now(Instant::now(), AtomicU64::new(0)));
    let ready = AtomicBool::new(false);
    let control = Control::default();
    let handshakes = HandshakePool::new(2).unwrap();
    let make = |id, username| Backend {
        account: id,
        session: crate::jmap_session::Session::mail_submission(
            Origin::parse(&origin_text).unwrap(),
            crate::config::identities::AccountView {
                id,
                username,
                name: username,
            },
            &resource,
            crate::jmap_session::Concurrency {
                uploads: 1,
                requests: 1,
            },
        )
        .unwrap(),
        calls: AtomicU64::new(0),
    };
    let first = make(AccountId::from_bytes([0xaa; 16]), "first");
    let second = make(AccountId::from_bytes([0xbb; 16]), "second");
    resolved.candidate().with_graph(|records, graph| {
        let rows = records.view(graph).unwrap().listeners().unwrap();
        let row = (0..rows.len()).filter_map(|i| rows.listener(i).unwrap()).find(|r| r.kind == Kind::Https).unwrap();
        let listeners = [BoundListener::new(socket, row, &lease).unwrap()];
        let server = Https { backends: &[&first, &second], listeners: &listeners, policies: &lease,
            handshakes: &handshakes, authentication: &Accounts, origin: Origin::parse(&origin_text).unwrap(),
            resources: &resource, timeouts: &timeouts, clock: clock.clone(), ready: &ready };
        std::thread::scope(|scope| {
            let worker = scope.spawn(|| server.run(&control));
            let stop = Stop(&control);
            wait(|| ready.load(Ordering::Acquire));
            for (username, password, own, other) in [
                ("first", "first-password", "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", "abbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"),
                ("second", "second-password", "abbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb", "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
            ] {
                let authorization = base64(format!("{username}:{password}").as_bytes());
                let mut peer = client(address, &lease, clock.clone());
                send(&mut peer, format!("GET /.well-known/jmap HTTP/1.1\r\nHost: localhost:{}\r\nAuthorization: Basic {authorization}\r\n\r\n", address.port()).as_bytes());
                let (status, _, body) = full_reply(&mut peer);
                assert_eq!(status, 200);
                assert!(body.contains(own), "{body}");
                assert!(!body.contains(other), "{body}");
                drop(peer);
                for (target, expected) in [(other, 404), (own, 200)] {
                    let mut peer = client(address, &lease, clock.clone());
                    send(&mut peer, format!("POST /jmap/upload/{target} HTTP/1.1\r\nHost: localhost:{}\r\nAuthorization: Basic {authorization}\r\nContent-Type: text/plain\r\nContent-Length: 0\r\n\r\n", address.port()).as_bytes());
                    assert_eq!(full_reply(&mut peer).0, expected);
                }
            }
            assert_eq!(first.calls.load(Ordering::Relaxed), 1);
            assert_eq!(second.calls.load(Ordering::Relaxed), 1);
            drop(stop);
            worker.join().unwrap().unwrap();
        });
    }).unwrap();
}

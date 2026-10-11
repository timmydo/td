use super::*;
use crate::{
    admission::{timers::NetworkLimits, DiskLimits, ViewMode, WorkLimits},
    config::{endpoint::Origin, listener::Kind},
    https::{BoundListener, Https},
    ids::AccountId,
    limits::Limits,
    ports::{Deadline, FlushProgress, Handshake, IoProgress, TlsTransport, Transport},
    smtp_receiving::Control,
    store_fs::{tests::Fixture, DeviceStore},
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
    let fixture = Fixture::new();
    let mut root = fixture.locked();
    let mut devices =
        DeviceStore::open_or_create(&mut root, AccountId::from_bytes([0xaa; 16])).unwrap();
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
                listeners: &listeners,
                policies: &lease,
                handshakes: &handshakes,
                devices: &devices,
                origin: Origin::parse(&origin_text).unwrap(),
                username: "private-user",
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
                let reply = std::thread::scope(|waiting_scope| {
                    let waiting = waiting_scope.spawn(|| {
                        let mut wire = client(address, &lease, clock.clone());
                        let head = format!("GET / HTTP/1.1\r\nHost: {host}\r\nAuthorization: Basic {authorization}\r\n\r\n");
                        request(&mut wire, head.as_bytes())
                    });
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

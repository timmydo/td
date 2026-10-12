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
                uploads: None,
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

#[test]
fn https_uploads_commit_real_bodies_and_recheck_revocation() {
    use crate::{
        admission::logical::Cell,
        format::{key::Key, row::Row},
        http_auth::DevicePolicy,
        ids::{BlobId, StoreEpoch},
        ownership::SlotState,
        ports::ReadView,
        store_fs::{AuxiliaryUsage, IndexStore, IngressSpool, StoreCoordinator},
        wire,
    };
    let _serial = serial();
    let account = AccountId::from_bytes([0xaa; 16]);
    let device_fixture = Fixture::new();
    let mut device_root = device_fixture.locked();
    let mut registry = DeviceStore::open_or_create(&mut device_root, account).unwrap();
    let (device, password) = registry
        .create(
            "upload",
            1,
            &mut td_crypto::SystemEntropy::try_new().unwrap(),
        )
        .unwrap();
    let authorization = base64(&[b"private-user:".as_slice(), password.expose()].concat());
    let devices = Mutex::new(registry);
    let material = Material::new();
    let socket = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = socket.local_addr().unwrap();
    let origin_text = format!("https://localhost:{}", address.port());
    let source = source(false)
        .replace("https://localhost", &origin_text)
        .replace("127.0.0.1:443", &address.to_string());
    let lease = material.published(&source);
    let resolved = resolved(&source);
    let resource = Limits {
        message_bytes: 32,
        header_bytes: 32,
        ..Limits::default()
    }
    .plan()
    .unwrap();
    let admission = DiskLimits::default()
        .plan(&resource, WorkLimits::default(), ViewMode::OnlineBackground)
        .unwrap();
    let timeouts = NetworkLimits::default().plan(&admission).unwrap();
    let clock = Arc::new(Now(Instant::now(), AtomicU64::new(0)));
    let until = Deadline::after(clock.sample().unwrap().monotonic, 120_000).unwrap();
    let data_fixture = Fixture::new();
    let spool_fixture = Fixture::new();
    let mut root = data_fixture.locked();
    let mut spool_root = spool_fixture.locked();
    let store = IndexStore::create(
        &mut root,
        StoreEpoch::from_bytes([3; 16]),
        clock.clone(),
        2,
        until,
    )
    .unwrap();
    store.create_account(account, until).unwrap();
    let spool = IngressSpool::open(&mut spool_root, &resource, clock.clone(), until).unwrap();
    let mut states = [const { SlotState::EMPTY }; 4];
    let mut cells = [const { Cell::EMPTY }; 4];
    let coordinator = StoreCoordinator::new(
        store,
        &admission,
        AuxiliaryUsage {
            sort_bytes: 0,
            response_bytes: 0,
            cache_bytes: 0,
            log_bytes: 0,
            cold_bytes: 0,
        },
        &mut states,
        &mut cells,
        DevicePolicy::new(&devices, clock.as_ref(), 1),
        until,
    )
    .unwrap();
    let uploads = crate::https_upload::Service {
        coordinator: &coordinator,
        spool: &spool,
        account,
        maximum: 32,
        clock: clock.as_ref(),
        timeouts: &timeouts,
    };
    let handshakes = HandshakePool::new(2).unwrap();
    let ready = AtomicBool::new(false);
    let control = Control::default();
    let mut published = Vec::new();
    let mut account_wire = [0; wire::IdKind::WIRE_BYTES];
    let account_wire = wire::encode_id(account, &mut account_wire).unwrap();
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
                uploads: Some(&uploads),
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
                let worker = scope.spawn(|| server.run(&control));
                let stop = Stop(&control);
                wait(|| ready.load(Ordering::Acquire));
                for (framing, body, expected) in [
                    ("Content-Length: 4", "body", "body"),
                    ("Content-Length: 0", "", ""),
                    (
                        "Transfer-Encoding: chunked",
                        "2\r\nab\r\n2\r\ncd\r\n0\r\n\r\n",
                        "abcd",
                    ),
                ] {
                    let mut wire = client(address, &lease, clock.clone());
                    let request = format!(
                        "POST /jmap/upload/{account_wire} HTTP/1.1\r\n\
                Host: localhost:{}\r\n\
                Authorization: Basic {authorization}\r\n\
                Content-Type: text/plain; charset=\"utf-8\"\r\n\
                {framing}\r\n\
                \r\n\
                {body}",
                        address.port()
                    );
                    let request = if body.is_empty() {
                        request.replace("Content-Type: text/plain; charset=\"utf-8\"\r\n", "")
                    } else {
                        request
                    };
                    if body.is_empty() {
                        send(&mut wire, request.as_bytes());
                    } else {
                        let split = request.len() - body.len() + 1;
                        send(&mut wire, &request.as_bytes()[..split]);
                        for byte in request.as_bytes()[split..].chunks(1) {
                            std::thread::sleep(Duration::from_millis(10));
                            send(&mut wire, byte);
                        }
                    }
                    let (status, body) = upload_reply(&mut wire);
                    assert_eq!(status, 200, "{body}");
                    let json = td_json::parse(&body).unwrap();
                    assert_eq!(json["accountId"].as_str(), Some(account_wire));
                    assert_eq!(
                        json["type"].as_str(),
                        Some(if expected.is_empty() {
                            "application/octet-stream"
                        } else {
                            "text/plain; charset=\"utf-8\""
                        })
                    );
                    assert_eq!(json["size"].as_u64(), Some(expected.len() as u64));
                    let id = wire::Id::parse(json["blobId"].as_str().unwrap())
                        .unwrap()
                        .local::<BlobId>()
                        .unwrap();
                    published.push((id, expected.as_bytes().to_vec()));
                    wire.abort();
                    std::thread::sleep(Duration::from_millis(20));
                }
                for (method, target, length, status) in [
                    ("POST", format!("/jmap/upload/{account_wire}"), 33, 413),
                    ("POST", "/jmap/upload/absent".into(), 0, 404),
                    ("GET", format!("/jmap/upload/{account_wire}"), 0, 405),
                ] {
                    let mut wire = client(address, &lease, clock.clone());
                    let head = format!(
                        "{method} {target} HTTP/1.1\r\n\
                Host: localhost:{}\r\n\
                Authorization: Basic {authorization}\r\n\
                Content-Length: {length}\r\n\
                Expect: 100-continue\r\n\
                \r\n\
                ",
                        address.port()
                    );
                    send(&mut wire, head.as_bytes());
                    assert_eq!(upload_reply(&mut wire).0, status);
                    wire.abort();
                    std::thread::sleep(Duration::from_millis(20));
                }
                for (media, length, sent_body, status) in [
                    ("x".repeat(1025), 0, String::new(), 431),
                    (String::new(), 0, String::new(), 400),
                    ("application/octet-stream".into(), 33, "x".repeat(33), 413),
                ] {
                    clock.1.fetch_add(1000, Ordering::Relaxed);
                    let mut wire = client(address, &lease, clock.clone());
                    let head = format!(
                        "POST /jmap/upload/{account_wire} HTTP/1.1\r\n\
                         Host: localhost:{}\r\n\
                         Authorization: Basic {authorization}\r\n\
                         Content-Type: {media}\r\n\
                         Content-Length: {length}\r\n\r\n{sent_body}",
                        address.port()
                    );
                    send(&mut wire, head.as_bytes());
                    assert_eq!(upload_reply(&mut wire).0, status);
                    wire.abort();
                    std::thread::sleep(Duration::from_millis(20));
                }
                clock.1.fetch_add(1000, Ordering::Relaxed);
                let mut wire = client(address, &lease, clock.clone());
                let head = format!(
                    "POST /jmap/upload/{account_wire} HTTP/1.1\r\n\
                Host: localhost:{}\r\n\
                Authorization: Basic {authorization}\r\n\
                Content-Length: 4\r\n\
                Expect: 100-continue\r\n\
                \r\n\
                ",
                    address.port()
                );
                assert!(request(&mut wire, head.as_bytes()).starts_with("HTTP/1.1 100 Continue"));
                let mut concurrent = client(address, &lease, clock.clone());
                send(&mut concurrent, head.as_bytes());
                assert_eq!(upload_reply(&mut concurrent).0, 429);
                concurrent.abort();
                devices.lock().unwrap().revoke(device, 2).unwrap();
                send(&mut wire, b"lost");
                assert_eq!(upload_reply(&mut wire).0, 403);
                wire.abort();
                std::thread::sleep(Duration::from_millis(20));
                clock.1.fetch_add(1000, Ordering::Relaxed);
                let (_, second) = devices
                    .lock()
                    .unwrap()
                    .create(
                        "cancel",
                        3,
                        &mut td_crypto::SystemEntropy::try_new().unwrap(),
                    )
                    .unwrap();
                let second = base64(&[b"private-user:".as_slice(), second.expose()].concat());
                let mut cancelled = client(address, &lease, clock.clone());
                let cancel_head = head.replace(&authorization, &second);
                assert!(request(&mut cancelled, cancel_head.as_bytes())
                    .starts_with("HTTP/1.1 100 Continue"));
                drop(stop);
                worker.join().unwrap().unwrap();
                cancelled.abort();
            });
        })
        .unwrap();
    assert_eq!(spool.status().unwrap().occupied_slots, 0);
    assert_eq!(
        coordinator
            .used(crate::admission::quota::Kind::BodyBytes)
            .unwrap(),
        8
    );
    assert_eq!(
        coordinator
            .used(crate::admission::quota::Kind::UploadBytes)
            .unwrap(),
        8
    );
    drop(coordinator);
    let store = IndexStore::open(&mut root, clock, 2, until).unwrap();
    store.validate_integrity(until).unwrap();
    let mut view = store.view(account, until).unwrap();
    let mut after: Option<BlobId> = None;
    let mut count = 0;
    loop {
        let mut key = [0; 16];
        let mut row = [0; 128];
        let Some(record) = view
            .next(
                crate::format::Table::Blobs,
                after.as_ref().map(|id| id.as_bytes().as_slice()),
                &mut key,
                &mut row,
            )
            .unwrap()
        else {
            break;
        };
        let Key::Blob(id) = record.key else {
            panic!("unexpected blob key");
        };
        after = Some(id);
        count += 1;
    }
    assert_eq!(count, published.len());
    for (id, expected) in published {
        let mut row = [0; 128];
        assert!(
            matches!(view.get(Key::Lease(id), &mut row).unwrap().unwrap().0, Row::Lease(lease) if lease.device == device)
        );
        let mut input = view.open_blob_input(&td_crypto::Provider, id, 32).unwrap();
        let mut bytes = [0; 32];
        let n = input.read(&mut bytes).unwrap();
        assert_eq!(&bytes[..n], expected);
        input.finish().unwrap();
    }
}

fn upload_reply(client: &mut impl Transport) -> (u16, String) {
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
        body.to_owned(),
    )
}

#[test]
fn stopped_upload_receipt_retires_after_expiry_or_shutdown() {
    for late in [false, true] {
        stopped_upload_receipt(late);
    }
}
fn stopped_upload_receipt(late: bool) {
    struct Late<'a>(&'a Now, bool);
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
            if self.1 {
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
    let fixture = Fixture::new();
    let mut root = fixture.locked();
    let account = AccountId::from_bytes([0xaa; 16]);
    let mut devices = DeviceStore::open_or_create(&mut root, account).unwrap();
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
                uploads: Some(&late),
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
                wait(|| worker.is_finished());
                assert_eq!(worker.join().unwrap(), Err(Error::WriterStopped));
                let mut bytes = [0; 4096];
                wait(|| match wire.read(&mut bytes) {
                    Ok(IoProgress::Pending) => false,
                    Ok(IoProgress::Bytes(_)) => panic!("expired exchange wrote a receipt"),
                    Ok(IoProgress::Closed) | Err(_) => true,
                });
            });
        })
        .unwrap();
}

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
        message_bytes: 131072,
        event_streams: 1,
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
    let until = Deadline::after(clock.sample().unwrap().monotonic, 600_000).unwrap();
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
        maximum: 131072,
        clock: clock.as_ref(),
        timeouts: &timeouts,
    };
    let response_fixture = Fixture::new();
    let mut response_root = response_fixture.locked();
    let request_spool =
        crate::store_fs::RequestSpool::open(&mut response_root, clock.clone(), until).unwrap();
    let session = crate::jmap_session::Session::core(
        Origin::parse(&origin_text).unwrap(),
        crate::config::identities::AccountView {
            id: account,
            username: "private-user",
            name: "Private",
        },
        &resource,
        crate::jmap_session::Concurrency {
            uploads: 1,
            requests: 1,
        },
    )
    .unwrap();
    let api = crate::https_api::Service {
        transfers: &uploads,
        coordinator: &coordinator,
        spool: &request_spool,
        policy: DevicePolicy::new(&devices, clock.as_ref(), 1),
        resources: &resource,
        session: &session,
        response_maximum: admission.disk().response_bytes,
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
            let mut server = Https {
                uploads: Some(&api),
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
            let zero_events = Limits { event_streams: 0, ..*resource.limits() }.plan().unwrap();
            server.resources = &zero_events;
            assert_eq!(server.run(&Control::default()), Err(Error::Invalid));
            server.resources = &resource;
            std::thread::scope(|scope| {
                let worker = scope.spawn(|| server.run(&control));
                let stop = Stop(&control);
                wait(|| ready.load(Ordering::Acquire));
                let discovered = discovery_cases(address, &lease, clock.clone(), &authorization, session.bytes(), &devices);
                let api_url = discovered["apiUrl"].as_str().unwrap();
                let upload_url = discovered["uploadUrl"].as_str().unwrap().replace("{accountId}", account_wire);
                let download_template = discovered["downloadUrl"].as_str().unwrap();
                api_cases(address, &lease, clock.clone(), &authorization, account_wire, session.state().unwrap(), &resource, &devices, api_url);
                assert_eq!(request_spool.status().occupied_slots, 0);
                assert_eq!(coordinator.used(crate::admission::quota::Kind::ResponseBytes).unwrap(), 0);
                let large = "x".repeat(65537);
                for (framing, body, expected) in [
                    ("Content-Length: 65537", large.as_str(), large.as_str()),
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
                        "POST {upload_url} HTTP/1.1\r\n\
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
                    if body.is_empty() || body.len() > 16 {
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
                    clock.1.fetch_add(2000, Ordering::Relaxed);
                    let blob = json["blobId"].as_str().unwrap();
                    let mut download = client(address, &lease, clock.clone());
                    let target = download_template.replace("{accountId}", account_wire).replace("{blobId}", blob).replace("{name}", "report%20%E2%98%83.txt").replace("{type}", "text%2Fplain%3B%20charset%3Dutf-8");
                    let head = format!("GET {target} HTTP/1.1\r\nHost: localhost:{}\r\nAuthorization: Basic {authorization}\r\n\r\n", address.port());
                    send(&mut download, head.as_bytes());
                    std::thread::sleep(Duration::from_millis(50));
                    let (status, headers, bytes) = full_reply(&mut download);
                    assert_eq!(status, 200);
                    assert_eq!(bytes, expected);
                    assert!(headers.contains("Content-Type: text/plain; charset=utf-8\r\n"));
                    assert!(headers.contains("filename*=UTF-8''%72%65%70%6F%72%74%20%E2%98%83%2E%74%78%74"));
                    assert!(!headers.contains(&authorization));
                    download.abort();
                    std::thread::sleep(Duration::from_millis(20));
                }
                for (method, target, length, status) in [
                    ("POST", format!("/jmap/upload/{account_wire}"), 131073, 413),
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
                    ("application/octet-stream".into(), 131073, "x".repeat(33), 413),
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

                for (method, target, fields, status) in [
                    ("GET", format!("/jmap/download/{account_wire}/absent/a?type=text%2Fplain"), "", 404),
                    ("GET", "/jmap/download/absent/absent/a?type=text%2Fplain".into(), "", 404),
                    ("GET", format!("/jmap/download/a{}/b{}/a?type=text%2Fplain", "bb".repeat(16), "11".repeat(16)), "", 404),
                    ("POST", format!("/jmap/download/{account_wire}/absent/a?type=text%2Fplain"), "", 405),
                    ("GET", format!("/jmap/download/{account_wire}/absent/a?type=text%2Fplain"), "Content-Length: 1\r\n", 400),
                ] {
                    clock.1.fetch_add(2000, Ordering::Relaxed);
                    let mut rejected = client(address, &lease, clock.clone());
                    send(&mut rejected, format!("{method} {target} HTTP/1.1\r\nHost: localhost:{}\r\nAuthorization: Basic {authorization}\r\n{fields}\r\n", address.port()).as_bytes());
                    let (actual, headers, _) = full_reply(&mut rejected);
                    assert_eq!(actual, status);
                    if status == 405 {
                        assert!(headers.contains("Allow: GET"));
                    }
                    rejected.abort();
                    std::thread::sleep(Duration::from_millis(20));
                }
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
                std::thread::sleep(Duration::from_millis(20));
                let mut blocked = client(address, &lease, clock.clone());
                send(&mut blocked, format!("GET /jmap/download/{account_wire}/absent/a?type=text%2Fplain HTTP/1.1\r\nHost: localhost:{}\r\nAuthorization: Basic {authorization}\r\n\r\n", address.port()).as_bytes());
                assert_eq!(upload_reply(&mut blocked).0, 429);
                blocked.abort();
                std::thread::sleep(Duration::from_millis(30));
                let mut api_blocked = client(address, &lease, clock.clone());
                send(&mut api_blocked, format!(
                    "POST /jmap/api HTTP/1.1\r\nHost: localhost:{}\r\n\
                     Authorization: Basic {authorization}\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n{{}}",
                    address.port()
                ).as_bytes());
                assert_eq!(upload_reply(&mut api_blocked).0, 503);
                api_blocked.abort();
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
                let mut blob_wire = [0; wire::IdKind::WIRE_BYTES];
                let blob_wire = wire::encode_id(published[0].0, &mut blob_wire).unwrap();
                for (credential, status) in [(&authorization, 401), (&second, 404)] {
                    clock.1.fetch_add(2000, Ordering::Relaxed);
                    let mut denied = client(address, &lease, clock.clone());
                    send(&mut denied, format!("GET /jmap/download/{account_wire}/{blob_wire}/a?type=text%2Fplain HTTP/1.1\r\nHost: localhost:{}\r\nAuthorization: Basic {credential}\r\n\r\n", address.port()).as_bytes());
                    assert_eq!(upload_reply(&mut denied).0, status);
                    denied.abort();
                    std::thread::sleep(Duration::from_millis(20));
                }
                let mut cancelled = client(address, &lease, clock.clone());
                let cancel_head = head.replace(&authorization, &second);
                assert!(request(&mut cancelled, cancel_head.as_bytes())
                    .starts_with("HTTP/1.1 100 Continue"));
                let mut event_shutdown = client(address, &lease, clock.clone());
                let event_head = format!("GET /jmap/events?types=*&closeafter=no&ping=0 HTTP/1.1\r\nHost: localhost:{}\r\nAuthorization: Basic {second}\r\n\r\n", address.port());
                assert!(request(&mut event_shutdown, event_head.as_bytes()).starts_with("HTTP/1.1 200 OK"));
                drop(stop);
                worker.join().unwrap().unwrap();
                cancelled.abort();
                finish_event(&mut event_shutdown);
            });
        })
        .unwrap();
    assert_eq!(spool.status().unwrap().occupied_slots, 0);
    assert_eq!(
        coordinator
            .used(crate::admission::quota::Kind::BodyBytes)
            .unwrap(),
        65545
    );
    assert_eq!(
        coordinator
            .used(crate::admission::quota::Kind::UploadBytes)
            .unwrap(),
        65545
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
        let mut input = view
            .open_blob_input(&td_crypto::Provider, id, 131072)
            .unwrap();
        let mut bytes = vec![0; expected.len()];
        let mut read = 0;
        while read < bytes.len() {
            let n = input.read(&mut bytes[read..]).unwrap();
            assert!(n > 0);
            read += n;
        }
        assert_eq!(bytes, expected);
        input.finish().unwrap();
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

#[allow(clippy::too_many_arguments)]
fn api_cases(
    address: std::net::SocketAddr,
    lease: &GenerationLease<TlsPolicies>,
    clock: Arc<Now>,
    authorization: &str,
    account: &str,
    state: &str,
    resources: &crate::limits::ResourcePlan,
    devices: &Mutex<DeviceStore<'_>>,
    api_url: &str,
) {
    let calls = format!(
        r##"{{"using":["urn:ietf:params:jmap:core"],"createdIds":{{"seed":"seed-id"}},"methodCalls":[
        ["Core/echo",{{"x":"hello"}},"a"],
        ["Core/echo",{{"#x":{{"resultOf":"a","name":"Core/echo","path":"/x"}}}},"b"],
        ["PushSubscription/get",{{}},"c"],
        ["PushSubscription/set",{{"create":{{"new":{{}}}}}},"d"],
        ["Blob/copy",{{"fromAccountId":"{account}","accountId":"{account}","blobIds":[]}},"e"],
        ["No/method",{{}},"f"]]}}"##
    );
    let long = format!(
        r#"{{"using":["urn:ietf:params:jmap:core"],"methodCalls":[["Core/echo",{{"long":"{}"}},"a"]]}}"#,
        "x".repeat(65537)
    );
    let too_many = format!(
        r#"{{"using":["urn:ietf:params:jmap:core"],"methodCalls":[{}]}}"#,
        vec![r#"["Core/echo",{},"a"]"#; resources.limits().json_methods + 1].join(",")
    );
    for (body, chunked, status, problem_type) in [
        (calls.as_str(), false, 200, ""),
        (calls.as_str(), true, 200, ""),
        (long.as_str(), false, 200, ""),
        ("{", false, 400, "notJSON"),
        ("[]", false, 400, "notRequest"),
        (
            r#"{"using":["urn:unknown"],"methodCalls":[]}"#,
            false,
            400,
            "unknownCapability",
        ),
        (too_many.as_str(), false, 400, "limit"),
    ] {
        clock.1.fetch_add(2000, Ordering::Relaxed);
        let mut wire = client(address, lease, clock.clone());
        let framing = if chunked {
            "Transfer-Encoding: chunked".to_owned()
        } else {
            format!("Content-Length: {}", body.len())
        };
        let interim = if chunked {
            "Expect: 100-continue\r\n"
        } else {
            ""
        };
        let head = format!("POST {api_url} HTTP/1.1\r\nHost: localhost:{}\r\nAuthorization: Basic {authorization}\r\nContent-Type: application/json; charset=utf-8\r\n{framing}\r\n{interim}\r\n", address.port());
        let body_wire = if chunked {
            format!("{:x}\r\n{body}\r\n0\r\n\r\n", body.len())
        } else {
            body.to_owned()
        };
        if chunked {
            assert!(request(&mut wire, head.as_bytes()).starts_with("HTTP/1.1 100 Continue"));
            send(&mut wire, body_wire.as_bytes());
        } else {
            send(&mut wire, format!("{head}{body_wire}").as_bytes());
        }
        let (actual, headers, reply) = full_reply(&mut wire);
        assert_eq!(actual, status, "{reply}");
        assert!(headers.contains("X-Content-Type-Options: nosniff"));
        let json = td_json::parse(&reply).unwrap();
        if status == 200 {
            assert_eq!(json["sessionState"].as_str(), Some(state));
            if body == long {
                assert_eq!(
                    json["methodResponses"][0][1]["long"].as_str(),
                    Some("x".repeat(65537).as_str())
                );
            } else {
                assert_eq!(json["methodResponses"][1][1]["x"].as_str(), Some("hello"));
                assert_eq!(
                    json["methodResponses"][2][1]["list"]
                        .as_array()
                        .unwrap()
                        .len(),
                    0
                );
                assert_eq!(
                    json["methodResponses"][3][1]["notCreated"]["new"]["type"].as_str(),
                    Some("forbidden")
                );
                assert_eq!(
                    json["methodResponses"][4][1]["type"].as_str(),
                    Some("invalidArguments")
                );
                assert_eq!(
                    json["methodResponses"][5][1]["type"].as_str(),
                    Some("unknownMethod")
                );
                assert_eq!(json["createdIds"]["seed"].as_str(), Some("seed-id"));
            }
        } else {
            assert!(headers.contains("Content-Type: application/problem+json"));
            assert_eq!(
                json["type"].as_str(),
                Some(format!("urn:ietf:params:jmap:error:{problem_type}").as_str())
            );
            if problem_type == "limit" {
                assert_eq!(json["limit"].as_str(), Some("maxCallsInRequest"));
            }
        }
        finish_api(&mut wire);
    }
    for (method, media, length, status) in [
        ("GET", "application/json", 0, 405),
        ("POST", "text/plain", 0, 415),
        (
            "POST",
            "application/json",
            resources.limits().json_bytes + 1,
            400,
        ),
    ] {
        clock.1.fetch_add(2000, Ordering::Relaxed);
        let mut wire = client(address, lease, clock.clone());
        let head = format!("{method} {api_url} HTTP/1.1\r\nHost: localhost:{}\r\nAuthorization: Basic {authorization}\r\nContent-Type: {media}\r\nContent-Length: {length}\r\nExpect: 100-continue\r\n\r\n", address.port());
        send(&mut wire, head.as_bytes());
        let (actual, _, body) = full_reply(&mut wire);
        assert_eq!(actual, status);
        if status == 400 {
            assert_eq!(
                td_json::parse(&body).unwrap()["limit"].as_str(),
                Some("maxSizeRequest")
            );
        }
        finish_api(&mut wire);
    }
    clock.1.fetch_add(2000, Ordering::Relaxed);
    let head = format!(
        "POST {api_url} HTTP/1.1\r\nHost: localhost:{}\r\n\
         Authorization: Basic {authorization}\r\nContent-Type: application/json\r\n\
         Content-Length: 2\r\nExpect: 100-continue\r\n\r\n",
        address.port()
    );
    let mut held = client(address, lease, clock.clone());
    assert!(request(&mut held, head.as_bytes()).starts_with("HTTP/1.1 100 Continue"));
    let mut busy = client(address, lease, clock.clone());
    send(&mut busy, head.as_bytes());
    let (status, headers, body) = full_reply(&mut busy);
    assert_eq!(status, 429);
    assert!(headers.contains("application/problem+json"));
    assert_eq!(
        td_json::parse(&body).unwrap()["limit"].as_str(),
        Some("maxConcurrentRequests")
    );
    finish_api(&mut busy);
    let mut binary = client(address, lease, clock.clone());
    send(&mut binary, format!("POST /jmap/upload/{account} HTTP/1.1\r\nHost: localhost:{}\r\nAuthorization: Basic {authorization}\r\nContent-Type: text/plain\r\nContent-Length: 0\r\n\r\n", address.port()).as_bytes());
    assert_eq!(full_reply(&mut binary).0, 503);
    finish_api(&mut binary);
    send(&mut held, b"{}");
    assert_eq!(upload_reply(&mut held).0, 400);
    finish_api(&mut held);

    clock.1.fetch_add(2000, Ordering::Relaxed);
    let mut oversized = client(address, lease, clock.clone());
    assert!(request(
        &mut oversized,
        head.replace("Content-Length: 2", "Transfer-Encoding: chunked")
            .as_bytes()
    )
    .starts_with("HTTP/1.1 100 Continue"));
    send(
        &mut oversized,
        format!("{:x}\r\n", resources.limits().json_bytes + 1).as_bytes(),
    );
    let (status, _, body) = full_reply(&mut oversized);
    assert_eq!(status, 400);
    assert_eq!(
        td_json::parse(&body).unwrap()["limit"].as_str(),
        Some("maxSizeRequest")
    );
    finish_api(&mut oversized);

    clock.1.fetch_add(2000, Ordering::Relaxed);
    let (device, password) = devices
        .lock()
        .unwrap()
        .create(
            "api-revoke",
            1,
            &mut td_crypto::SystemEntropy::try_new().unwrap(),
        )
        .unwrap();
    let temporary = base64(&[b"private-user:".as_slice(), password.expose()].concat());
    let mut revoked = client(address, lease, clock.clone());
    assert!(request(
        &mut revoked,
        head.replace(authorization, &temporary).as_bytes()
    )
    .starts_with("HTTP/1.1 100 Continue"));
    devices.lock().unwrap().revoke(device, 2).unwrap();
    send(&mut revoked, b"{}");
    assert_eq!(upload_reply(&mut revoked).0, 403);
    finish_api(&mut revoked);

    clock.1.fetch_add(2000, Ordering::Relaxed);
    let mut idle = client(address, lease, clock.clone());
    assert!(request(&mut idle, head.as_bytes()).starts_with("HTTP/1.1 100 Continue"));
    clock.1.fetch_add(16000, Ordering::Relaxed);
    assert_eq!(upload_reply(&mut idle).0, 503);
    finish_api(&mut idle);
}

fn finish_api(wire: &mut impl Transport) {
    let mut bytes = [0; 1024];
    wait(|| match wire.read(&mut bytes) {
        Ok(IoProgress::Pending) => false,
        Ok(IoProgress::Closed) | Err(_) => true,
        other => panic!("unexpected bytes after complete API response: {other:?}"),
    });
    wire.abort();
}

fn discovery_cases(
    address: std::net::SocketAddr,
    lease: &GenerationLease<TlsPolicies>,
    clock: Arc<Now>,
    authorization: &str,
    session: &[u8],
    devices: &Mutex<DeviceStore<'_>>,
) -> td_json::Json {
    let mut discovered = None;
    for (target, host, status) in [
        (
            "/.well-known/jmap".to_owned(),
            format!("localhost:{}", address.port()),
            200,
        ),
        (
            format!("HTTPS://LOCALHOST:{}/.well-known/jmap", address.port()),
            "other.example".to_owned(),
            200,
        ),
        (
            format!("https://localhost:{}?x=1", address.port()),
            format!("localhost:{}", address.port()),
            404,
        ),
        (
            "/?x=1".to_owned(),
            format!("localhost:{}", address.port()),
            404,
        ),
        (
            "https://other.example/.well-known/jmap".to_owned(),
            format!("localhost:{}", address.port()),
            421,
        ),
        (
            format!("http://localhost:{}/.well-known/jmap", address.port()),
            format!("localhost:{}", address.port()),
            421,
        ),
    ] {
        clock.1.fetch_add(2000, Ordering::Relaxed);
        let mut wire = client(address, lease, clock.clone());
        send(&mut wire, format!("GET {target} HTTP/1.1\r\nHost: {host}\r\nAuthorization: Basic {authorization}\r\n\r\n").as_bytes());
        let (actual, headers, body) = full_reply(&mut wire);
        assert_eq!(actual, status);
        if status == 200 {
            assert_eq!(body.as_bytes(), session);
            assert!(headers
                .lines()
                .any(|line| line == "Content-Type: application/json"));
            let json = td_json::parse(&body).unwrap();
            assert!(json["capabilities"]["urn:ietf:params:jmap:core"]
                .as_obj()
                .is_some());
            assert_eq!(json["capabilities"].as_obj().unwrap().len(), 1);
            assert!(json["capabilities"]["urn:ietf:params:jmap:mail"].is_null());
            discovered = Some(json);
        }
        finish_event(&mut wire);
    }
    let discovered = discovered.unwrap();
    let event_template = discovered["eventSourceUrl"].as_str().unwrap();
    let api_url = discovered["apiUrl"].as_str().unwrap();
    let event_head = |auth: &str, ping: u64| {
        let target = event_template
            .replace("{types}", "%2A")
            .replace("{closeafter}", "state")
            .replace("{ping}", &ping.to_string());
        format!("GET {target} HTTP/1.1\r\nHost: localhost:{}\r\nAuthorization: Basic {auth}\r\nLast-Event-ID: unknown\r\n\r\n", address.port())
    };
    clock.1.fetch_add(2000, Ordering::Relaxed);
    let (device, password) = devices
        .lock()
        .unwrap()
        .create(
            "events",
            1,
            &mut td_crypto::SystemEntropy::try_new().unwrap(),
        )
        .unwrap();
    let temporary = base64(&[b"private-user:".as_slice(), password.expose()].concat());
    let mut stream = client(address, lease, clock.clone());
    let headers = request(&mut stream, event_head(&temporary, 1).as_bytes());
    assert!(headers.starts_with("HTTP/1.1 200 OK"));
    assert!(headers.contains("Content-Type: text/event-stream"));
    assert!(!headers.contains("Content-Length:"));
    let mut scratch = [0; 1024];
    assert!(matches!(
        stream.read(&mut scratch).unwrap(),
        IoProgress::Pending
    ));
    clock.1.fetch_add(2000, Ordering::Relaxed);
    let mut ping = Vec::new();
    wait(|| {
        match stream.read(&mut scratch).unwrap() {
            IoProgress::Bytes(n) => ping.extend_from_slice(&scratch[..n]),
            IoProgress::Pending => (),
            IoProgress::Closed => panic!("event stream closed before ping"),
        }
        ping.ends_with(b"\n\n")
    });
    assert_eq!(ping, b"event: ping\ndata: {\"interval\":1}\n\n");
    {
        // Registry contention must not block the reactor's origin refusals.
        let guard = devices.lock().unwrap();
        let mut probe = client(address, lease, clock.clone());
        send(
            &mut probe,
            b"GET /.well-known/jmap HTTP/1.1\r\nHost: foreign.example\r\n\r\n",
        );
        assert_eq!(full_reply(&mut probe).0, 421);
        finish_event(&mut probe);
        drop(guard);
    }
    let mut excess = client(address, lease, clock.clone());
    send(&mut excess, event_head(authorization, 0).as_bytes());
    assert_eq!(full_reply(&mut excess).0, 429);
    finish_event(&mut excess);
    let mut api = client(address, lease, clock.clone());
    let body = r#"{"using":["urn:ietf:params:jmap:core"],"methodCalls":[["Core/echo",{},"a"]]}"#;
    send(&mut api, format!("POST {api_url} HTTP/1.1\r\nHost: localhost:{}\r\nAuthorization: Basic {authorization}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}", address.port(), body.len()).as_bytes());
    assert_eq!(full_reply(&mut api).0, 200);
    finish_event(&mut api);
    devices.lock().unwrap().revoke(device, 2).unwrap();
    finish_event(&mut stream);
    clock.1.fetch_add(2000, Ordering::Relaxed);
    let mut silent = client(address, lease, clock.clone());
    assert!(
        request(&mut silent, event_head(authorization, 0).as_bytes())
            .starts_with("HTTP/1.1 200 OK")
    );
    // Complete another exchange so the reactor has acknowledged the SSE header
    // flush before moving the synthetic clock beyond its output-stall window.
    for advance in [0, 31000] {
        clock.1.fetch_add(advance, Ordering::Relaxed);
        let mut probe = client(address, lease, clock.clone());
        send(&mut probe, format!("GET /.well-known/jmap HTTP/1.1\r\nHost: localhost:{}\r\nAuthorization: Basic {authorization}\r\n\r\n", address.port()).as_bytes());
        assert_eq!(full_reply(&mut probe).0, 200);
        finish_event(&mut probe);
    }
    assert!(matches!(
        silent.read(&mut scratch).unwrap(),
        IoProgress::Pending
    ));
    silent.abort();
    // Discovery remains independently available while the reactor retires EOF.
    for (method, target, framing, status) in [
        ("POST", "/.well-known/jmap", "", 405),
        ("GET", "/.well-known/jmap", "Content-Length: 1\r\n", 400),
        (
            "GET",
            "/jmap/events?types=*&closeafter=no&ping=bad",
            "",
            400,
        ),
        ("POST", "/jmap/events?types=*&closeafter=no&ping=0", "", 405),
        (
            "GET",
            "/jmap/events?types=*&closeafter=no&ping=0",
            "Expect: 100-continue\r\n",
            400,
        ),
    ] {
        clock.1.fetch_add(2000, Ordering::Relaxed);
        let mut wire = client(address, lease, clock.clone());
        send(&mut wire, format!("{method} {target} HTTP/1.1\r\nHost: localhost:{}\r\nAuthorization: Basic {authorization}\r\n{framing}\r\n", address.port()).as_bytes());
        let (actual, headers, _) = full_reply(&mut wire);
        assert_eq!(actual, status);
        if status == 405 {
            assert!(headers.contains("Allow: GET"));
        }
        finish_event(&mut wire);
    }
    discovered
}
fn finish_event(wire: &mut impl Transport) {
    let mut scratch = [0; 1024];
    wait(|| match wire.read(&mut scratch) {
        Ok(IoProgress::Pending) => false,
        Ok(IoProgress::Closed) | Err(_) => true,
        _ => false,
    });
    wire.abort();
}

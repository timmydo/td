#![deny(unsafe_code)]
#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use std::sync::{Arc, Mutex};
use td_crypto::{Crypto, Provider};
use td_json::{Json, ObjectBuilder};
use td_mta::acme::{
    self,
    http::{Exchange, Response},
    order::{Persistence, Settings, State},
    Error,
};
#[path = "support/tls_certificate_fixture.rs"]
mod certificate;
const NOW: i64 = 1792483200000;
fn response(status: u16, value: Json) -> Response {
    Response {
        status,
        nonce: None,
        location: None,
        retry_after: None,
        content_type: Some("application/json".into()),
        body: value.to_vec(),
    }
}
fn decode64(text: &str) -> Vec<u8> {
    let mut out = Vec::new();
    let mut word = 0u32;
    let mut bits = 0;
    for b in text.bytes() {
        let value = match b {
            b'A'..=b'Z' => b - b'A',
            b'a'..=b'z' => b - b'a' + 26,
            b'0'..=b'9' => b - b'0' + 52,
            b'-' => 62,
            b'_' => 63,
            _ => panic!("invalid base64"),
        };
        word = (word << 6) | u32::from(value);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((word >> bits) as u8);
        }
    }
    out
}
#[derive(Default)]
struct Saved {
    bytes: Vec<u8>,
    pair: Option<(Vec<u8>, Vec<u8>)>,
    acknowledged: bool,
    finalized: bool,
    account_calls: usize,
    order_calls: usize,
}
struct Store(
    Arc<Mutex<Saved>>,
    Option<Arc<std::sync::RwLock<acme::challenge::Current>>>,
);
impl Persistence for Store {
    fn save(&mut self, state: &State, pair: Option<(&[u8], &[u8])>) -> Result<(), Error> {
        if let Some(current) = &self.1 {
            let mut current = current.write().unwrap();
            current.token = state.token.clone();
            current.authorization = state.key_authorization.clone();
        }
        let mut saved = self.0.lock().unwrap();
        saved.bytes = state.encode();
        if let Some((chain, key)) = pair {
            saved.pair = Some((chain.to_vec(), key.to_vec()));
        }
        Ok(())
    }
}
struct Ca {
    saved: Arc<Mutex<Saved>>,
    bad_nonce: bool,
    issued: Vec<u8>,
    origin: String,
    challenge_address: Option<std::net::SocketAddr>,
}
impl Ca {
    fn order(&self) -> Json {
        let saved = self.saved.lock().unwrap();
        ObjectBuilder::new()
            .set(
                "status",
                if saved.finalized {
                    "valid"
                } else if saved.acknowledged {
                    "ready"
                } else {
                    "pending"
                },
            )
            .set(
                "identifiers",
                Json::Arr(vec![ObjectBuilder::new()
                    .set("type", "dns")
                    .set("value", "localhost")
                    .build()]),
            )
            .set(
                "authorizations",
                Json::Arr(vec![Json::Str(format!("{}/auth", self.origin))]),
            )
            .set("finalize", format!("{}/finalize", self.origin))
            .set("certificate", format!("{}/cert", self.origin))
            .build()
    }
}
impl Exchange for Ca {
    fn exchange(&mut self, method: &str, url: &str, body: &[u8]) -> Result<Response, Error> {
        let path = url.strip_prefix(&self.origin).unwrap();
        if path == "/directory" {
            assert_eq!(method, "GET");
            return Ok(response(
                200,
                ObjectBuilder::new()
                    .set("newNonce", format!("{}/nonce", self.origin))
                    .set("newAccount", format!("{}/account", self.origin))
                    .set("newOrder", format!("{}/new-order", self.origin))
                    .build(),
            ));
        }
        if path == "/nonce" {
            assert_eq!(method, "HEAD");
            let mut r = response(200, Json::Null);
            r.body.clear();
            r.nonce = Some("nonce-fixture".into());
            return Ok(r);
        }
        assert_eq!(method, "POST");
        let jws = td_json::parse_slice(body).unwrap();
        let protected =
            td_json::parse_slice(&decode64(jws.get("protected").unwrap().as_str().unwrap()))
                .unwrap();
        assert_eq!(protected.get("url").and_then(Json::as_str), Some(url));
        assert_eq!(
            decode64(jws.get("signature").unwrap().as_str().unwrap()).len(),
            64
        );
        let payload = decode64(jws.get("payload").unwrap().as_str().unwrap());
        if self.bad_nonce {
            self.bad_nonce = false;
            return Ok(response(
                400,
                ObjectBuilder::new()
                    .set("type", "urn:ietf:params:acme:error:badNonce")
                    .build(),
            ));
        }
        match path {
            "/account" => {
                assert!(protected.get("jwk").is_some());
                self.saved.lock().unwrap().account_calls += 1;
                let mut r = response(201, ObjectBuilder::new().set("status", "valid").build());
                r.location = Some(url.into());
                Ok(r)
            }
            "/new-order" => {
                self.saved.lock().unwrap().order_calls += 1;
                let mut r = response(201, self.order());
                r.location = Some(format!("{}/order", self.origin));
                Ok(r)
            }
            "/order" => {
                assert!(payload.is_empty());
                Ok(response(200, self.order()))
            }
            "/auth" => Ok(response(
                200,
                ObjectBuilder::new()
                    .set("status", "pending")
                    .set(
                        "identifier",
                        ObjectBuilder::new()
                            .set("type", "dns")
                            .set("value", "localhost")
                            .build(),
                    )
                    .set(
                        "challenges",
                        Json::Arr(vec![ObjectBuilder::new()
                            .set("type", "http-01")
                            .set("status", "pending")
                            .set("token", "abcdefghijklmnopqrstuvwxyz123456")
                            .set("url", format!("{}/challenge", self.origin))
                            .build()]),
                    )
                    .build(),
            )),
            "/challenge" => {
                assert_eq!(payload, b"{}");
                let mut saved = self.saved.lock().unwrap();
                let state = State::decode(&saved.bytes).unwrap();
                assert!(!state.key_authorization.is_empty());
                if let Some(address) = self.challenge_address {
                    use std::io::{Read, Write};
                    let mut socket = std::net::TcpStream::connect_timeout(
                        &address,
                        std::time::Duration::from_secs(2),
                    )
                    .unwrap();
                    socket
                        .set_read_timeout(Some(std::time::Duration::from_secs(2)))
                        .unwrap();
                    write!(
                        socket,
                        "GET /.well-known/acme-challenge/{} HTTP/1.1\r\nHost: localhost\r\n\r\n",
                        state.token
                    )
                    .unwrap();
                    let mut response = String::new();
                    socket.take(4096).read_to_string(&mut response).unwrap();
                    assert!(response.starts_with("HTTP/1.1 200"));
                    assert!(response.ends_with(&state.key_authorization));
                }
                saved.acknowledged = true;
                Ok(response(
                    200,
                    ObjectBuilder::new().set("status", "processing").build(),
                ))
            }
            "/finalize" => {
                let payload = td_json::parse_slice(&payload).unwrap();
                let csr = decode64(payload.get("csr").unwrap().as_str().unwrap());
                assert_eq!(csr.first(), Some(&0x30));
                assert!(csr.windows(9).any(|w| w == b"localhost"));
                self.saved.lock().unwrap().finalized = true;
                Ok(response(200, self.order()))
            }
            "/cert" => {
                let mut r = response(200, Json::Null);
                r.content_type = Some("application/pem-certificate-chain".into());
                r.body = self.issued.clone();
                Ok(r)
            }
            _ => panic!("unexpected local fixture endpoint"),
        }
    }
}
fn setup() -> (Settings, State, Store, Ca) {
    let state = State::initial().unwrap();
    let key = Provider
        .load_p256_pem(state.certificate_key.as_bytes())
        .unwrap();
    let mut raw = [0; td_crypto::P256_PKCS8_CAPACITY];
    let n = Provider.generate_p256(&mut raw).unwrap();
    let root = Provider.load_p256(&raw[..n]).unwrap();
    let mut issued = certificate::pem(
        "CERTIFICATE",
        &certificate::certificate_names(&key, &root, false, 2, &["localhost"], false),
    );
    issued.extend(certificate::pem(
        "CERTIFICATE",
        &certificate::certificate_names(&root, &root, true, 1, &[], false),
    ));
    let saved = Arc::new(Mutex::new(Saved::default()));
    let mut store = Store(saved.clone(), None);
    store.save(&state, None).unwrap();
    (
        Settings {
            directory: "https://localhost/directory".into(),
            contact: "operator@example.test".into(),
            names: vec!["localhost".into()],
        },
        state,
        store,
        Ca {
            saved,
            bad_nonce: true,
            issued,
            origin: "https://localhost".into(),
            challenge_address: None,
        },
    )
}
#[test]
fn issuance_survives_restart_at_every_checkpoint_and_keeps_pair_together() {
    let (settings, mut state, mut store, mut ca) = setup();
    let mut now = NOW;
    for _ in 0..8 {
        acme::order::advance(&settings, &mut state, &mut ca, &mut store, now).unwrap();
        state = State::decode(&store.0.lock().unwrap().bytes).unwrap();
        now = now.max(state.next_ms);
        if store.0.lock().unwrap().pair.is_some() {
            break;
        }
    }
    let saved = store.0.lock().unwrap();
    let (chain, key) = saved.pair.as_ref().unwrap();
    td_crypto::ServerIdentity::from_pem(chain, key, &["localhost"], Some((NOW / 1000) as u64))
        .unwrap();
    assert!(state.order.is_empty());
    assert!(state.token.is_empty());
    assert!(state.next_ms > NOW + 86400000);
    assert_eq!(saved.account_calls, 1);
    assert_eq!(saved.order_calls, 1);
    assert_ne!(key.as_slice(), state.certificate_key.as_bytes());
}
struct FailPublication<'a> {
    store: &'a mut Store,
    committed: bool,
}
impl Persistence for FailPublication<'_> {
    fn save(&mut self, state: &State, pair: Option<(&[u8], &[u8])>) -> Result<(), Error> {
        if pair.is_some() {
            if self.committed {
                self.store.save(state, pair)?;
            }
            return Err(Error::Database);
        }
        self.store.save(state, pair)
    }
}
#[test]
fn publication_failure_preserves_pending_key_and_durable_outcome() {
    for committed in [false, true] {
        let (settings, mut state, mut store, mut ca) = setup();
        state.account = "https://localhost/account".into();
        state.order = "https://localhost/order".into();
        state.order_started_ms = NOW;
        store.save(&state, None).unwrap();
        store.0.lock().unwrap().finalized = true;
        let pending_key = state.certificate_key.clone();
        assert_eq!(
            acme::order::advance(
                &settings,
                &mut state,
                &mut ca,
                &mut FailPublication {
                    store: &mut store,
                    committed
                },
                NOW
            ),
            Err(Error::Database)
        );
        assert_eq!(state.certificate_key, pending_key);
        assert!(!state.order.is_empty());
        // The worker reclaims and reloads before writing any failure metadata.
        state = State::decode(&store.0.lock().unwrap().bytes).unwrap();
        if committed {
            assert!(state.order.is_empty());
            assert!(state.next_ms > NOW);
            assert!(store.0.lock().unwrap().pair.is_some());
        } else {
            state.failed(NOW);
            store.save(&state, None).unwrap();
            assert_eq!(state.certificate_key, pending_key);
            assert!(!state.order.is_empty());
            let next = state.next_ms;
            acme::order::advance(&settings, &mut state, &mut ca, &mut store, next).unwrap();
            assert!(store.0.lock().unwrap().pair.is_some());
        }
        assert_eq!(store.0.lock().unwrap().order_calls, 0);
    }
}
#[test]
fn delayed_completed_order_is_published_or_retired_when_expired() {
    for expired in [false, true] {
        let (settings, mut state, mut store, mut ca) = setup();
        let identity = td_crypto::ServerIdentity::from_pem(
            &ca.issued,
            state.certificate_key.as_bytes(),
            &["localhost"],
            Some((NOW / 1000) as u64),
        )
        .unwrap();
        let (start, end) = identity.validity();
        let now = if expired {
            (end + 1) * 1000
        } else {
            (start + (end - start) * 3 / 4) * 1000
        } as i64;
        state.account = "https://localhost/account".into();
        state.order = "https://localhost/order".into();
        store.0.lock().unwrap().finalized = true;
        acme::order::advance(&settings, &mut state, &mut ca, &mut store, now).unwrap();
        assert!(state.order.is_empty());
        assert_eq!(state.next_ms, now);
        assert_eq!(store.0.lock().unwrap().pair.is_some(), !expired);
    }
}
#[test]
fn multi_name_authorization_retains_answer_until_validation_finishes() {
    struct Delayed {
        ca: Ca,
        polls: usize,
        second: bool,
    }
    impl Exchange for Delayed {
        fn exchange(&mut self, method: &str, url: &str, body: &[u8]) -> Result<Response, Error> {
            if url.ends_with("/order") {
                return Ok(response(200, td_json::parse(r#"{"status":"pending","identifiers":[{"type":"dns","value":"localhost"},{"type":"dns","value":"second.test"}],"authorizations":["https://localhost/auth","https://localhost/auth2"]}"#).unwrap()));
            }
            if url.ends_with("/auth") {
                self.polls += 1;
                if self.polls > 1 {
                    let saved = self.ca.saved.lock().unwrap();
                    let state = State::decode(&saved.bytes).unwrap();
                    assert_eq!(state.token, "abcdefghijklmnopqrstuvwxyz123456");
                }
                if self.polls >= 4 {
                    return Ok(response(
                        200,
                        td_json::parse(
                            r#"{"status":"valid","identifier":{"type":"dns","value":"localhost"}}"#,
                        )
                        .unwrap(),
                    ));
                }
            }
            if url.ends_with("/auth2") {
                assert!(
                    self.polls >= 4,
                    "first challenge was replaced during validation"
                );
                self.second = true;
                return Ok(response(
                    200,
                    td_json::parse(
                        r#"{"status":"valid","identifier":{"type":"dns","value":"second.test"}}"#,
                    )
                    .unwrap(),
                ));
            }
            self.ca.exchange(method, url, body)
        }
    }
    let (mut settings, mut state, mut store, mut ca) = setup();
    ca.bad_nonce = false;
    settings.names.push("second.test".into());
    state.account = "https://localhost/account".into();
    state.order = "https://localhost/order".into();
    let mut ca = Delayed {
        ca,
        polls: 0,
        second: false,
    };
    let mut now = NOW;
    for _ in 0..5 {
        acme::order::advance(&settings, &mut state, &mut ca, &mut store, now).unwrap();
        state = State::decode(&store.0.lock().unwrap().bytes).unwrap();
        now = state.next_ms;
    }
    assert!(ca.second);
}
#[test]
fn ambiguous_new_order_is_not_immediately_reissued() {
    let (settings, mut state, mut store, mut ca) = setup();
    state.account = "https://localhost/account".into();
    state.order_started_ms = NOW;
    acme::order::advance(&settings, &mut state, &mut ca, &mut store, NOW + 1000).unwrap();
    assert_eq!(store.0.lock().unwrap().order_calls, 0);
    assert!(state.next_ms >= NOW + 86400000);
}
#[test]
fn foreign_order_url_is_refused_before_network() {
    let (settings, mut state, mut store, mut ca) = setup();
    state.order = "https://evil.test/order".into();
    assert_eq!(
        acme::order::advance(&settings, &mut state, &mut ca, &mut store, NOW),
        Err(Error::Protocol)
    );
    assert_eq!(store.0.lock().unwrap().account_calls, 0);
}

#[test]
fn issued_certificate_requires_exact_dns_identifiers() {
    let (_, state, _, _) = setup();
    let key = Provider
        .load_p256_pem(state.certificate_key.as_bytes())
        .unwrap();
    let mut raw = [0; td_crypto::P256_PKCS8_CAPACITY];
    let n = Provider.generate_p256(&mut raw).unwrap();
    let root = Provider.load_p256(&raw[..n]).unwrap();
    for names in [
        vec!["localhost"],
        vec!["localhost", "extra.example.test"],
        vec!["localhost", "localhost"],
    ] {
        let mut chain = certificate::pem(
            "CERTIFICATE",
            &certificate::certificate_names(&key, &root, false, 2, &names, false),
        );
        chain.extend(certificate::pem(
            "CERTIFICATE",
            &certificate::certificate_names(&root, &root, true, 1, &[], false),
        ));
        let identity = td_crypto::ServerIdentity::from_pem(
            &chain,
            state.certificate_key.as_bytes(),
            &["localhost"],
            Some((NOW / 1000) as u64),
        )
        .unwrap();
        assert_eq!(identity.check_exact_dns_names().is_ok(), names.len() == 1);
    }
}

#[test]
fn complete_issuance_over_verified_local_https() {
    use std::{
        net::TcpListener,
        path::Path,
        sync::atomic::{AtomicBool, Ordering},
        time::{Duration, Instant},
    };
    use td_crypto::{
        ClockHandle, IdentitySelection, ServerConfig, ServerIdentity, TlsProtocol, TlsSession,
        TrustStore,
    };
    use td_mta::{
        clock::{RuntimeClock, TlsClockSource},
        ports::{Clock, Deadline, FlushProgress, IoProgress, Transport},
        tls_io::{TlsIo, TLS_WIRE_BYTES},
        transport::TcpTransport,
    };
    let (mut settings, mut state, mut store, mut ca) = setup();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    ca.origin = format!(
        "https://localhost:{}",
        listener.local_addr().unwrap().port()
    );
    settings.directory = format!("{}/directory", ca.origin);
    let second = ca
        .issued
        .windows(b"-----BEGIN CERTIFICATE-----".len())
        .enumerate()
        .filter(|(_, b)| *b == b"-----BEGIN CERTIFICATE-----")
        .nth(1)
        .unwrap()
        .0;
    let trust = TrustStore::from_pem(&ca.issued[second..]).unwrap();
    let clock: Arc<dyn Clock> = Arc::new(RuntimeClock::new());
    let identity = ServerIdentity::from_pem(
        &ca.issued,
        state.certificate_key.as_bytes(),
        &["localhost"],
        Some((NOW / 1000) as u64),
    )
    .unwrap();
    let config = Arc::new(
        ServerConfig::new(
            &[Arc::new(identity)],
            TlsProtocol::Http1,
            IdentitySelection::RequiredName,
            None,
            Arc::new(ClockHandle::new(TlsClockSource::new(clock.clone()))),
        )
        .unwrap(),
    );
    let challenge_listener = TcpListener::bind("127.0.0.1:0").unwrap();
    ca.challenge_address = Some(challenge_listener.local_addr().unwrap());
    let current = Arc::new(std::sync::RwLock::new(acme::challenge::Current {
        names: vec!["localhost".into()],
        token: String::new(),
        authorization: String::new(),
        expires: Instant::now() + Duration::from_secs(60),
    }));
    store.1 = Some(current.clone());
    let stop = Arc::new(AtomicBool::new(false));
    let http_stop = stop.clone();
    let http_worker = std::thread::spawn(move || {
        acme::challenge::run(challenge_listener, &current, &http_stop).unwrap()
    });
    let child_stop = stop.clone();
    let worker = std::thread::spawn(move || {
        let limit = Instant::now() + Duration::from_secs(60);
        while !child_stop.load(Ordering::Acquire) && Instant::now() < limit {
            let socket = match listener.accept() {
                Ok((s, _)) => s,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(1));
                    continue;
                }
                Err(e) => panic!("{e}"),
            };
            let cap = Deadline::after(clock.sample().unwrap().monotonic, 5000).unwrap();
            let mut io = TlsIo::new(
                TcpTransport::from_stream(socket).unwrap(),
                TlsSession::server(config.clone()).unwrap(),
                clock.clone(),
                cap,
                cap,
                Box::new([0; TLS_WIRE_BYTES]),
                Box::new([0; TLS_WIRE_BYTES]),
            )
            .unwrap();
            while io.handshake().unwrap().is_none() {
                std::thread::sleep(Duration::from_millis(1));
            }
            let mut input = Vec::new();
            let mut buffer = [0; 4096];
            let (header_end, length) = loop {
                match io.read(&mut buffer).unwrap() {
                    IoProgress::Bytes(n) => input.extend_from_slice(&buffer[..n]),
                    IoProgress::Pending => std::thread::sleep(Duration::from_millis(1)),
                    _ => panic!("early EOF"),
                }
                assert!(input.len() < 65536);
                if let Some(end) = input.windows(4).position(|p| p == b"\r\n\r\n") {
                    let head = std::str::from_utf8(&input[..end]).unwrap();
                    let length = head
                        .split("\r\n")
                        .find_map(|line| line.strip_prefix("Content-Length: "))
                        .map(|s| s.parse::<usize>().unwrap())
                        .unwrap_or(0);
                    if input.len() >= end + 4 + length {
                        break (end, length);
                    }
                }
            };
            let head = std::str::from_utf8(&input[..header_end]).unwrap();
            let mut first = head.lines().next().unwrap().split(' ');
            let method = first.next().unwrap();
            let path = first.next().unwrap();
            let mut response = ca
                .exchange(
                    method,
                    &format!("{}{path}", ca.origin),
                    &input[header_end + 4..header_end + 4 + length],
                )
                .unwrap();
            if method == "HEAD" {
                response.body.clear();
            }
            let mut output = format!(
                "HTTP/1.1 {} Fixture\r\nContent-Length: {}\r\nConnection: close\r\n",
                response.status,
                response.body.len()
            )
            .into_bytes();
            for (key, value) in [
                ("Replay-Nonce", response.nonce),
                ("Location", response.location),
                ("Content-Type", response.content_type),
            ] {
                if let Some(value) = value {
                    output.extend_from_slice(format!("{key}: {value}\r\n").as_bytes());
                }
            }
            output.extend_from_slice(b"\r\n");
            output.extend(response.body);
            let mut sent = 0;
            while sent < output.len() {
                match io.write(&output[sent..]).unwrap() {
                    IoProgress::Bytes(n) => sent += n,
                    IoProgress::Pending => std::thread::sleep(Duration::from_millis(1)),
                    _ => panic!("write closed"),
                }
            }
            while io.flush().unwrap() != FlushProgress::Complete {
                std::thread::sleep(Duration::from_millis(1));
            }
        }
    });
    struct Stop(Arc<AtomicBool>, Vec<std::thread::JoinHandle<()>>);
    impl Drop for Stop {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Release);
            for worker in self.1.drain(..) {
                let _ = worker.join();
            }
        }
    }
    let guard = Stop(stop, vec![worker, http_worker]);
    let mut https = acme::http::Https {
        executable: Path::new(env!("CARGO_BIN_EXE_td-mta")),
        directory: &settings.directory,
        trust: &trust,
        deadline: Instant::now() + Duration::from_secs(55),
    };
    let mut now = NOW;
    for _ in 0..8 {
        acme::order::advance(&settings, &mut state, &mut https, &mut store, now).unwrap();
        state = State::decode(&store.0.lock().unwrap().bytes).unwrap();
        now = now.max(state.next_ms);
        if store.0.lock().unwrap().pair.is_some() {
            break;
        }
    }
    assert!(store.0.lock().unwrap().pair.is_some());
    drop(guard);
}

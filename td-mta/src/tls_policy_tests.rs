#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
#[path = "tls_policy_client_tests.rs"]
mod client_tests;
#[path = "tls_gateway_process_tests.rs"]
mod gateway_process_tests;
#[path = "tls_policy_io_tests.rs"]
mod io_tests;
use super::*;
use crate::{
    clock::TlsClockSource,
    config::{load, materialize, stanza, storage, stream, text},
    generations::GenerationSet,
    ports::{Clock, Tick, Time},
    tls_io::{
        tests::{certificate, pem},
        TLS_WIRE_BYTES,
    },
};
use std::{
    io::{self, Cursor},
    sync::atomic::{AtomicBool, Ordering},
};
use td_crypto::{Crypto, Provider, TlsPhase, P256_PKCS8_CAPACITY};

const PREFIX: &str = r#"version = 1
[server]
hostname = "localhost"
jmap_origin = "https://localhost"
[account "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"]
username = "private-user"
[domain "example.test"]
[alias "main@example.test"]
account = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
[identity "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"]
account = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
email = "main@example.test"
[resolver "primary"]
address = "127.0.0.1:53"
[relay]
host = "localhost"
port = 465
username = "private-relay"
password_file = "/password"
ca_file = "/relay-ca"
[certificate "public"]
mode = "files"
chain_file = "/chain"
key_file = "/key"
"#;
const SMTP: &str = r#"[listener "smtp"]
kind = "direct_smtp"
bind = "127.0.0.1:25"
server_name = "localhost"
certificate = "public"
session_limit = 1
per_peer_limit = 1
"#;
const HTTPS: &str = r#"[listener "https"]
kind = "https"
bind = "127.0.0.1:443"
certificate = "public"
"#;
fn source(gateway: bool) -> String {
    let mut source = format!("{PREFIX}{SMTP}{HTTPS}");
    if gateway {
        source += &format!(
            r#"[gateway "upstream"]
ca_file = "/gateway-ca"
client_cert_sha256 = "{}"
[gateway_peer "upstream"]
network = "192.0.2.0/24"
[listener "gateway"]
kind = "gateway_smtp"
bind = "127.0.0.1:2525"
server_name = "localhost"
certificate = "public"
gateway = "upstream"
session_limit = 1
per_peer_limit = 1
"#,
            "11".repeat(32)
        );
    }
    source
}
fn resolved(source: &str) -> ResolvedText {
    let _lock = text::TEST_CONSTRUCTION_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut scratch = vec![0; stream::SCRATCH_BYTES];
    let loaded = load::read(
        storage::Storage::try_new().unwrap(),
        &mut stanza::Pending::new(),
        &mut scratch,
        &mut Cursor::new(source),
    )
    .unwrap();
    materialize::read_text(loaded, &mut scratch, |_| {
        Ok::<_, Error>(Cursor::new(b"password\n"))
    })
    .unwrap()
}
struct SwitchClock(AtomicBool);
impl Clock for SwitchClock {
    fn sample(&self) -> Result<Time, Error> {
        if self.0.load(Ordering::Relaxed) {
            return Err(Error::Busy);
        }
        Ok(Time {
            utc_ms: 1_800_000_000_000,
            monotonic: Tick(0),
        })
    }
}
struct Material {
    chain: Vec<u8>,
    key: Vec<u8>,
    ca: Vec<u8>,
    clock: Arc<ClockHandle>,
    source: Arc<SwitchClock>,
}
impl Material {
    fn new() -> Self {
        Self::with_names(&["localhost"])
    }
    fn with_names(names: &[&str]) -> Self {
        let mut root = [0; P256_PKCS8_CAPACITY];
        let mut leaf = [0; P256_PKCS8_CAPACITY];
        let r = Provider.generate_p256(&mut root).unwrap();
        let l = Provider.generate_p256(&mut leaf).unwrap();
        let root_key = Provider.load_p256(&root[..r]).unwrap();
        let leaf_key = Provider.load_p256(&leaf[..l]).unwrap();
        let ca = pem("CERTIFICATE", &certificate(&root_key, &root_key, true));
        let chain = [
            pem(
                "CERTIFICATE",
                &crate::tls_io::tests::certificate_with_names(&leaf_key, &root_key, names),
            ),
            ca.clone(),
        ]
        .concat();
        let key = pem("PRIVATE KEY", &leaf[..l]);
        let source = Arc::new(SwitchClock(AtomicBool::new(false)));
        Self {
            chain,
            key,
            ca,
            clock: Arc::new(ClockHandle::new(TlsClockSource::new(source.clone()))),
            source,
        }
    }
    fn open(&self, request: MaterialRequest<'_>) -> Result<Cursor<&[u8]>, Error> {
        assert!(!format!("{request:?}").contains("/"));
        match request.kind() {
            MaterialKind::Chain => {
                assert!(request.profile().is_some());
                Ok(Cursor::new(&self.chain))
            }
            MaterialKind::Key => Ok(Cursor::new(&self.key)),
            _ => Ok(Cursor::new(&self.ca)),
        }
    }
    fn prepare(
        &self,
        set: &GenerationSet<TlsPolicies>,
        source: &str,
    ) -> Result<PreparedGeneration<TlsPolicies>, Error> {
        TlsPolicies::prepare(
            set.reserve()?,
            &resolved(source),
            self.clock.clone(),
            |request| self.open(request),
        )
    }
    fn published(&self, source: &str) -> GenerationLease<TlsPolicies> {
        let mut set = GenerationSet::at_startup();
        let p = self.prepare(&set, source).unwrap();
        drop(set.publish(p).unwrap());
        set.current().unwrap()
    }
}
fn serial() -> std::sync::MutexGuard<'static, ()> {
    crate::generations::TEST_CONSTRUCTION_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
fn buffers() -> (Box<[u8; TLS_WIRE_BYTES]>, Box<[u8; TLS_WIRE_BYTES]>) {
    (
        vec![0x31; TLS_WIRE_BYTES]
            .into_boxed_slice()
            .try_into()
            .unwrap(),
        vec![0x42; TLS_WIRE_BYTES]
            .into_boxed_slice()
            .try_into()
            .unwrap(),
    )
}

#[test]
fn compiled_policy_roles_bind_names_trust_and_generation_ids() {
    let _serial = serial();
    let material = Material::new();
    let source = source(true) + "[listener \"fixture\"]\nkind = \"loopback_smtp_fixture\"\nbind = \"127.0.0.1:2526\"\nsession_limit = 1\nper_peer_limit = 1\n";
    let lease = material.published(&source);
    assert_eq!(lease.value().len(), 4);
    assert!(!lease.value().is_empty());
    assert_eq!(
        TlsPolicies::listener(&lease, "fixture"),
        Err(Error::NotFound)
    );
    assert_eq!(TlsPolicies::acme(&lease), Err(Error::NotFound));
    let pool = HandshakePool::new(1).unwrap();
    for (name, role) in [
        ("smtp", PolicyRole::DirectSmtp),
        ("https", PolicyRole::Https),
        ("gateway", PolicyRole::GatewaySmtp),
    ] {
        let id = TlsPolicies::listener(&lease, name).unwrap();
        assert_eq!(TlsPolicies::role(&lease, id), Ok(role));
        assert!(TlsPolicies::destination(&lease, id).unwrap().is_none());
        let (input, output) = buffers();
        let input_address = input.as_ptr();
        let output_address = output.as_ptr();
        let pending =
            TlsPolicies::reserve_session(lease.clone(), id, &pool, input, output).unwrap();
        assert_eq!(pool.available(), 0);
        let worker = std::thread::spawn(move || pending.construct().unwrap());
        let reservation = worker.join().unwrap();
        assert_eq!(reservation.role(), Ok(role));
        assert_eq!(reservation.policy_id(), id);
        assert_eq!(reservation.status().phase, TlsPhase::Handshaking);
        assert!(reservation.status().handshake.is_none());
        let (input, output) = reservation.into_buffers();
        assert_eq!(input.as_ptr(), input_address);
        assert_eq!(output.as_ptr(), output_address);
        assert_eq!(pool.available(), 1);
    }
    let id = TlsPolicies::relay(&lease).unwrap();
    assert_eq!(
        TlsPolicies::destination(&lease, id).unwrap(),
        Some(("localhost", 465, outbound::Transport::ImplicitTls))
    );
    let other = material.published(&source);
    assert_eq!(TlsPolicies::role(&other, id), Err(Error::Conflict));
    assert_eq!(
        TlsPolicies::role(
            &lease,
            TlsPolicyId {
                generation: id.generation,
                index: u16::MAX
            }
        ),
        Err(Error::NotFound)
    );
    assert_eq!(
        format!("{:?}", lease.value()),
        "TlsPolicies { count: 4, .. }"
    );
}

#[test]
fn queued_and_native_reservations_retain_capacity_and_recover_exact_buffers() {
    let _serial = serial();
    let material = Material::new();
    let mut set = GenerationSet::at_startup();
    drop(
        set.publish(material.prepare(&set, &source(false)).unwrap())
            .unwrap(),
    );
    let lease = set.current().unwrap();
    let id = TlsPolicies::relay(&lease).unwrap();
    let pool = HandshakePool::new(1).unwrap();
    let (input, output) = buffers();
    let prepared = TlsPolicies::reserve_session(lease.clone(), id, &pool, input, output).unwrap();
    let (input, output) = buffers();
    let address = input.as_ptr();
    let refused =
        TlsPolicies::reserve_session(lease.clone(), id, &pool, input, output).unwrap_err();
    assert_eq!(refused.error(), Error::Busy);
    assert_eq!(refused.into_buffers().0.as_ptr(), address);
    drop(
        set.publish(material.prepare(&set, &source(false)).unwrap())
            .unwrap(),
    );
    drop(lease);
    assert_eq!(set.available(), 0);
    assert_eq!(set.reserve().unwrap_err(), Error::Busy);
    let native = prepared.construct().unwrap();
    assert_eq!(set.available(), 0);
    assert_eq!(pool.available(), 0);
    let (input, output) = native.into_buffers();
    assert_eq!(set.available(), 1);
    assert_eq!(pool.available(), 1);
    let lease = set.current().unwrap();
    let refused =
        TlsPolicies::reserve_session(lease.clone(), id, &pool, input, output).unwrap_err();
    assert_eq!(refused.error(), Error::Conflict);
    assert_eq!(pool.available(), 1);
    let (input, output) = refused.into_buffers();
    let id = TlsPolicies::relay(&lease).unwrap();
    let queued = TlsPolicies::reserve_session(lease, id, &pool, input, output).unwrap();
    drop(
        set.publish(material.prepare(&set, &source(false)).unwrap())
            .unwrap(),
    );
    let pointers = (queued.input.as_ptr(), queued.output.as_ptr());
    material.source.0.store(true, Ordering::Relaxed);
    let refused = queued.construct().unwrap_err();
    assert_eq!(refused.error(), Error::Tls);
    let (input, output) = refused.into_buffers();
    assert_eq!((input.as_ptr(), output.as_ptr()), pointers);
    assert_eq!(set.available(), 1);
    assert_eq!(pool.available(), 1);
    material.source.0.store(false, Ordering::Relaxed);
    let lease = set.current().unwrap();
    let id = TlsPolicies::relay(&lease).unwrap();
    let queued = TlsPolicies::reserve_session(lease, id, &pool, input, output).unwrap();
    drop(
        set.publish(material.prepare(&set, &source(false)).unwrap())
            .unwrap(),
    );
    assert_eq!(set.available(), 0);
    assert_eq!(pool.available(), 0);
    let (input, output) = queued.into_buffers();
    assert_eq!((input.as_ptr(), output.as_ptr()), pointers);
    assert_eq!(set.available(), 1);
    assert_eq!(pool.available(), 1);
}

#[test]
fn gateway_comparison_survives_reordering_but_rejects_policy_or_binding_changes() {
    let _serial = serial();
    let material = Material::new();
    let original = source(true);
    let old = material.published(&original);
    let id = TlsPolicies::listener(&old, "gateway").unwrap();
    let equivalent = original.replace(&format!("{SMTP}{HTTPS}"), &format!("{HTTPS}{SMTP}"));
    let renewed = Material::new();
    let config = resolved(&equivalent);
    let mut set = GenerationSet::at_startup();
    let p = TlsPolicies::prepare(
        set.reserve().unwrap(),
        &config,
        renewed.clock.clone(),
        |r| {
            if r.kind() == MaterialKind::GatewayCa {
                Ok(Cursor::new(material.ca.as_slice()))
            } else {
                renewed.open(r)
            }
        },
    )
    .unwrap();
    drop(set.publish(p).unwrap());
    let current = set.current().unwrap();
    assert_ne!(
        TlsPolicies::listener(&old, "https").unwrap().index,
        TlsPolicies::listener(&current, "https").unwrap().index
    );
    assert!(TlsPolicies::gateway_unchanged(&old, id, &current).unwrap());
    assert_eq!(
        TlsPolicies::gateway_fingerprint(&old, id),
        TlsPolicies::gateway_fingerprint(
            &current,
            TlsPolicies::listener(&current, "gateway").unwrap()
        )
    );
    for changed in [
        original.replace("192.0.2.0/24", "192.0.0.0/16"),
        original.replace("127.0.0.1:2525", "127.0.0.1:2527"),
        original.replace(&"11".repeat(32), &"22".repeat(32)),
        source(false),
    ] {
        assert!(!TlsPolicies::gateway_unchanged(&old, id, &material.published(&changed)).unwrap());
    }
    assert!(!TlsPolicies::gateway_unchanged(&old, id, &renewed.published(&original)).unwrap());
}

#[test]
fn material_errors_never_publish_and_acme_requests_are_explicit() {
    let _serial = serial();
    let material = Material::new();
    let mut set = GenerationSet::at_startup();
    drop(
        set.publish(material.prepare(&set, &source(false)).unwrap())
            .unwrap(),
    );
    let before = set.current().unwrap().id();
    for bad in [
        MaterialKind::Chain,
        MaterialKind::Key,
        MaterialKind::RelayCa,
        MaterialKind::GatewayCa,
    ] {
        let config = resolved(&source(true));
        assert_eq!(
            TlsPolicies::prepare(
                set.reserve().unwrap(),
                &config,
                material.clock.clone(),
                |r| {
                    if r.kind() == bad {
                        Ok(Cursor::new(b"invalid".as_slice()))
                    } else {
                        material.open(r)
                    }
                }
            )
            .unwrap_err(),
            Error::Tls
        );
        assert_eq!(set.current().unwrap().id(), before);
        assert_eq!(set.available(), 1);
    }
    let wrong_name = source(false).replace("localhost", "other.test");
    assert_eq!(material.prepare(&set, &wrong_name).unwrap_err(), Error::Tls);
    let acme = source(false).replace(
        "mode = \"files\"\nchain_file = \"/chain\"\nkey_file = \"/key\"",
        "mode = \"acme\"",
    ) + r#"[acme]
directory = "https://localhost/directory"
contact = "main@example.test"
terms_accepted = true
ca_file = "/acme-ca"
[listener "http01"]
kind = "http01"
bind = "127.0.0.1:80"
"#;
    let config = resolved(&acme);
    let mut requests = Vec::new();
    let p = TlsPolicies::prepare(
        set.reserve().unwrap(),
        &config,
        material.clock.clone(),
        |r| {
            if matches!(r.kind(), MaterialKind::Chain | MaterialKind::Key) {
                assert_eq!(r.path(), None);
            }
            requests.push(r.kind());
            material.open(r)
        },
    )
    .unwrap();
    drop(set.publish(p).unwrap());
    assert!(requests.contains(&MaterialKind::AcmeCa));
    let current = set.current().unwrap();
    let id = TlsPolicies::acme(&current).unwrap();
    assert_eq!(TlsPolicies::role(&current, id), Ok(PolicyRole::Acme));
    assert_eq!(
        TlsPolicies::destination(&current, id).unwrap(),
        Some(("localhost", 443, outbound::Transport::ImplicitTls))
    );
}

#[test]
fn material_reader_caps_bytes_eof_and_interrupted_work() {
    let request = || MaterialRequest {
        kind: MaterialKind::Key,
        profile: Some("private"),
        path: Some("/secret"),
    };
    assert_eq!(
        read(request(), 4, &mut |_| Ok(Cursor::new(b"1234")))
            .unwrap()
            .0,
        b"1234"
    );
    assert_eq!(
        read(request(), 4, &mut |_| Ok(Cursor::new(b"12345"))).err(),
        Some(Error::Capacity)
    );
    struct Interrupted;
    impl Read for Interrupted {
        fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
            Err(io::ErrorKind::Interrupted.into())
        }
    }
    assert!(matches!(
        read(request(), 4, &mut |_| Ok(Interrupted)),
        Err(Error::Io {
            kind: io::ErrorKind::Interrupted,
            ..
        })
    ));
    struct Alternating {
        remaining: u32,
        progress: bool,
    }
    impl Read for Alternating {
        fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
            if self.remaining == 0 {
                return Ok(0);
            }
            self.progress = !self.progress;
            if self.progress {
                output[0] = b'x';
                Ok(1)
            } else {
                self.remaining -= 1;
                Err(io::ErrorKind::Interrupted.into())
            }
        }
    }
    assert_eq!(
        read(request(), 40, &mut |_| Ok(Alternating {
            remaining: 32,
            progress: false
        }))
        .unwrap()
        .0
        .len(),
        32
    );
    assert!(matches!(
        read(request(), 40, &mut |_| Ok(Alternating {
            remaining: 33,
            progress: false
        })),
        Err(Error::Io {
            kind: io::ErrorKind::Interrupted,
            ..
        })
    ));
    struct Bad;
    impl Read for Bad {
        fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
            Ok(usize::MAX)
        }
    }
    assert_eq!(
        read(request(), 4, &mut |_| Ok(Bad)).err(),
        Some(Error::Invalid)
    );
}

fn exchange(
    lease: &GenerationLease<TlsPolicies>,
    server: &str,
    source: Arc<SwitchClock>,
) -> Result<(), Error> {
    use crate::{ports::Deadline, tls_io::TlsIo, transport::TcpTransport};
    use std::net::{TcpListener, TcpStream};
    let relay = TlsPolicies::relay(lease)?;
    let server = TlsPolicies::listener(lease, server)?;
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let left = TcpStream::connect(listener.local_addr()?)?;
    let (right, _) = listener.accept()?;
    let deadline = Deadline::after(Tick(0), 1000)?;
    let wrap = |stream, id| {
        let (input, output) = buffers();
        TlsIo::new(
            TcpTransport::from_stream(stream).unwrap(),
            TlsPolicies::resolve(lease, id).unwrap().session().unwrap(),
            source.clone(),
            deadline,
            deadline,
            input,
            output,
        )
        .unwrap()
    };
    let mut client = wrap(left, relay);
    let mut server = wrap(right, server);
    for _ in 0..100_000 {
        let c = client.handshake()?;
        let s = server.handshake()?;
        if let (Some(c), Some(s)) = (c, s) {
            assert_eq!(c.peer, td_crypto::PeerEvidence::VerifiedServerName);
            assert_eq!(s.peer, td_crypto::PeerEvidence::Unauthenticated);
            return Ok(());
        }
        std::thread::yield_now();
    }
    panic!("local policy handshake did not finish");
}

#[test]
fn compiled_configs_enforce_relay_trust_protocol_and_gateway_client_auth() {
    let _serial = serial();
    let material = Material::new();
    let lease = material.published(&source(true));
    exchange(&lease, "smtp", material.source.clone()).unwrap();
    exchange(&lease, "https", material.source.clone()).unwrap(); // ALPN omission is permitted; application framing is separate.
    assert_eq!(
        exchange(&lease, "gateway", material.source.clone()),
        Err(Error::Tls)
    );
    let other = Material::new();
    let mut set = GenerationSet::at_startup();
    let config = resolved(&source(false));
    let prepared = TlsPolicies::prepare(
        set.reserve().unwrap(),
        &config,
        material.clock.clone(),
        |request| {
            if request.kind() == MaterialKind::RelayCa {
                Ok(Cursor::new(other.ca.as_slice()))
            } else {
                material.open(request)
            }
        },
    )
    .unwrap();
    drop(set.publish(prepared).unwrap());
    assert_eq!(
        exchange(&set.current().unwrap(), "smtp", material.source.clone()),
        Err(Error::Tls)
    );
    // Public roots are selected only by absence of the override, not its failure.
    let public = material.published(&source(false).replace("ca_file = \"/relay-ca\"\n", ""));
    assert_eq!(
        exchange(&public, "smtp", material.source.clone()),
        Err(Error::Tls)
    );
}

#[test]
fn https_profiles_narrow_shared_smtp_names_before_native_routing() {
    let _serial = serial();
    let material =
        Material::with_names(&["localhost", "smtp.example.test", "mta-sts.example.test"]);
    let source = source(false)
        .replace(
            "hostname = \"localhost\"",
            "hostname = \"smtp.example.test\"",
        )
        .replace(
            "server_name = \"localhost\"",
            "server_name = \"smtp.example.test\"",
        )
        .replace(
            "[domain \"example.test\"]",
            "[domain \"example.test\"]\nmta_sts = \"testing\"\nmta_sts_certificate = \"sts\"",
        )
        + r#"[certificate "sts"]
mode = "files"
chain_file = "/sts-chain"
key_file = "/sts-key"
[listener "smtp2"]
kind = "direct_smtp"
bind = "127.0.0.1:2527"
server_name = "smtp.example.test"
certificate = "sts"
session_limit = 1
per_peer_limit = 1
"#;
    let lease = material.published(&source);
    let https = TlsPolicies::listener(&lease, "https").unwrap();
    let Configuration::Server(config) = &TlsPolicies::resolve(&lease, https).unwrap().configuration
    else {
        panic!("not HTTPS")
    };
    assert_eq!(config.identity_count(), 2);
    assert_eq!(config.binding_count(), 2);
    exchange(&lease, "https", material.source.clone()).unwrap();
    // The relay sends localhost SNI, which is a valid SAN but not a binding
    // of the sts profile. Direct SMTP still serves its default identity.
    exchange(&lease, "smtp2", material.source.clone()).unwrap();
    let smtp_name =
        material.published(&source.replace("host = \"localhost\"", "host = \"smtp.example.test\""));
    assert_eq!(
        exchange(&smtp_name, "https", material.source.clone()),
        Err(Error::Tls)
    );
}

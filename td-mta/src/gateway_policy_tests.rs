#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use super::*;
use crate::{
    clock::TlsClockSource,
    config::{gateway, syntax::Location, text},
    ports::{Deadline, Tick},
    tls_io::{tests::*, TlsIo, TLS_WIRE_BYTES},
    transport::TcpTransport,
};
use std::net::{TcpListener, TcpStream};
use td_crypto::{ClientConfig, TlsPhase, P256_PKCS8_CAPACITY};

#[derive(Clone, Copy)]
struct Spec<'a> {
    name: &'a str,
    ca_file: &'a str,
    current: u8,
    next: Option<u8>,
    networks: &'a [&'a str],
}

impl Default for Spec<'_> {
    fn default() -> Self {
        Self {
            name: "upstream",
            ca_file: "/private/gateway-ca.pem",
            current: 0x11,
            next: Some(0x22),
            networks: &["192.0.2.0/24", "2001:db8::/32"],
        }
    }
}

fn with_gateway<R>(spec: Spec<'_>, visit: impl FnOnce(&mut Gateway<'_, '_>) -> R) -> R {
    let _serial = text::TEST_CONSTRUCTION_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut bytes = [0; 4096];
    let mut text = text::Builder::new(&mut bytes).unwrap();
    let mut slots = [gateway::Slot::EMPTY];
    let mut peers = [gateway::PeerSlot::EMPTY; MAX_PEERS];
    let mut builder = gateway::Builder::new(&text, &mut slots, &mut peers).unwrap();
    let current = format!("{:02x}", spec.current).repeat(32);
    let next = spec.next.map(|byte| format!("{byte:02x}").repeat(32));
    let at = Location {
        line: std::num::NonZeroU32::MIN,
        column: 1,
    };
    builder
        .gateway(
            &mut text,
            spec.name,
            gateway::Input {
                ca_file: spec.ca_file,
                client_cert_sha256: &current,
                next_client_cert_sha256: next.as_deref(),
            },
            at,
        )
        .unwrap();
    for network in spec.networks {
        builder.peer(&mut text, spec.name, network, at).unwrap();
    }
    let records = builder.finish().unwrap();
    let mut gateway = records
        .view_live(&text)
        .unwrap()
        .gateway(0)
        .unwrap()
        .unwrap();
    visit(&mut gateway)
}

fn policy(
    spec: Spec<'_>,
    ca: &[u8],
    identity: &Arc<ServerIdentity>,
    clock: &Arc<ClockHandle>,
) -> Result<GatewayPolicy, Error> {
    with_gateway(spec, |gateway| {
        GatewayPolicy::new(gateway, ca, identity.clone(), clock.clone())
    })
}

fn root_pem() -> Vec<u8> {
    let mut raw = [0; P256_PKCS8_CAPACITY];
    let count = Provider.generate_p256(&mut raw).unwrap();
    let key = Provider.load_p256(&raw[..count]).unwrap();
    pem("CERTIFICATE", &certificate(&key, &key, true))
}

#[test]
fn canonical_gateway_policy_ignores_only_representation_and_server_material() {
    let (identity, first) = identity_material();
    let (replacement, second) = identity_material();
    let clock = Arc::new(ClockHandle::new(TlsClockSource::new(TestClock::new())));
    let bundle = [first.clone(), second.clone()].concat();
    let base = policy(Spec::default(), &bundle, &identity, &clock).unwrap();
    let second = String::from_utf8(second).unwrap();
    let body: String = second
        .lines()
        .filter(|line| !line.starts_with("-----"))
        .collect();
    let rewrapped = format!("-----BEGIN CERTIFICATE-----\n{body}\n-----END CERTIFICATE-----\n");
    let reordered = String::from_utf8([rewrapped.into_bytes(), first.clone()].concat())
        .unwrap()
        .replace('\n', "\r\n");
    let reordered = format!("\n\t{reordered}\n");
    let equivalent = policy(
        Spec {
            ca_file: "/moved/gateway-ca.pem",
            current: 0x22,
            next: Some(0x11),
            networks: &["2001:0db8:0000::/32", "192.0.2.0/24"],
            ..Spec::default()
        },
        reordered.as_bytes(),
        &replacement,
        &clock,
    )
    .unwrap();
    assert_eq!(base.fingerprint(), equivalent.fingerprint());
    assert_eq!(base.name(), "upstream");
    assert_eq!(base.fingerprint().as_bytes().len(), 32);
    for spec in [
        Spec {
            name: "replacement",
            ..Spec::default()
        },
        Spec {
            current: 0x33,
            ..Spec::default()
        },
        Spec {
            next: None,
            ..Spec::default()
        },
        Spec {
            networks: &["192.0.2.0/24", "2001:db8::/32", "203.0.113.0/24"],
            ..Spec::default()
        },
        Spec {
            networks: &["192.0.0.0/16", "2001:db8::/32"],
            ..Spec::default()
        },
    ] {
        assert_ne!(
            base.fingerprint(),
            policy(spec, &bundle, &identity, &clock)
                .unwrap()
                .fingerprint()
        );
    }
    assert_ne!(
        base.fingerprint(),
        policy(Spec::default(), &first, &identity, &clock)
            .unwrap()
            .fingerprint()
    );
    let changed_ca = [first, root_pem()].concat();
    assert_ne!(
        base.fingerprint(),
        policy(Spec::default(), &changed_ca, &identity, &clock)
            .unwrap()
            .fingerprint()
    );
    let mut raw = [0; P256_PKCS8_CAPACITY];
    let count = Provider.generate_p256(&mut raw).unwrap();
    let key = Provider.load_p256(&raw[..count]).unwrap();
    let original_der = certificate_with_serial(&key, &key, true, 1);
    let reissued_der = certificate_with_serial(&key, &key, true, 3);
    assert_ne!(original_der, reissued_der);
    let original = policy(
        Spec::default(),
        &pem("CERTIFICATE", &original_der),
        &identity,
        &clock,
    )
    .unwrap();
    let reissued = policy(
        Spec::default(),
        &pem("CERTIFICATE", &reissued_der),
        &identity,
        &clock,
    )
    .unwrap();
    assert_ne!(original.fingerprint(), reissued.fingerprint());
    assert_eq!(format!("{base:?}"), "GatewayPolicy(<redacted>)");
    assert_eq!(
        format!("{:?}", base.fingerprint()),
        "GatewayFingerprint(<redacted>)"
    );
}

#[test]
fn gateway_filters_and_material_limits_refuse_without_fallback() {
    let (identity, ca) = identity_material();
    let clock = Arc::new(ClockHandle::new(TlsClockSource::new(TestClock::new())));
    let base = policy(Spec::default(), &ca, &identity, &clock).unwrap();
    for peer in ["192.0.2.1", "::ffff:192.0.2.1", "2001:db8::1"] {
        for pin in [[0x11; 32], [0x22; 32]] {
            assert!(base.matches(peer.parse().unwrap(), &pin).unwrap());
        }
        assert!(!base.matches(peer.parse().unwrap(), &[0x33; 32]).unwrap());
    }
    for peer in ["203.0.113.1", "2001:db9::1", "::ffff:203.0.113.1"] {
        assert!(!base.matches(peer.parse().unwrap(), &[0x11; 32]).unwrap());
    }
    let no_next = policy(
        Spec {
            next: None,
            ..Spec::default()
        },
        &ca,
        &identity,
        &clock,
    )
    .unwrap();
    assert!(!no_next
        .matches("192.0.2.1".parse().unwrap(), &[0x22; 32])
        .unwrap());
    let mut raw = [0; P256_PKCS8_CAPACITY];
    let count = Provider.generate_p256(&mut raw).unwrap();
    let key = Provider.load_p256(&raw[..count]).unwrap();
    let not_ca = pem("CERTIFICATE", &certificate(&key, &key, false));
    for invalid in [
        not_ca,
        Vec::new(),
        b"not PEM".to_vec(),
        [ca.clone(), ca.clone()].concat(),
    ] {
        assert!(matches!(
            policy(Spec::default(), &invalid, &identity, &clock),
            Err(Error::Tls)
        ));
    }
    assert!(matches!(
        policy(
            Spec {
                networks: &[],
                ..Spec::default()
            },
            &ca,
            &identity,
            &clock
        ),
        Err(Error::Invalid)
    ));
    with_gateway(Spec::default(), |gateway| {
        gateway.next_client_cert_sha256 = Some(gateway.client_cert_sha256);
        assert!(matches!(
            GatewayPolicy::new(gateway, &ca, identity.clone(), clock.clone()),
            Err(Error::Invalid)
        ));
        gateway.next_client_cert_sha256 = None;
        gateway.name = "INVALID";
        assert!(matches!(
            GatewayPolicy::new(gateway, &ca, identity.clone(), clock.clone()),
            Err(Error::Invalid)
        ));
    });
    let mut maximum = Vec::new();
    for _ in 0..MAX_ANCHORS {
        maximum.extend(root_pem());
    }
    let full = policy(
        Spec {
            networks: &[
                "192.0.2.0/24",
                "198.51.100.0/24",
                "203.0.113.0/24",
                "10.0.0.0/8",
                "172.16.0.0/12",
                "192.168.0.0/16",
                "2001:db8::/32",
                "fe80::/10",
            ],
            ..Spec::default()
        },
        &maximum,
        &identity,
        &clock,
    )
    .unwrap();
    assert!(full
        .matches("fe80::1".parse().unwrap(), &[0x22; 32])
        .unwrap());
    assert_eq!(
        full.new_session().unwrap().status().phase,
        TlsPhase::Handshaking
    );
    maximum.extend(root_pem());
    assert!(matches!(
        policy(Spec::default(), &maximum, &identity, &clock),
        Err(Error::Tls)
    ));
}

type Connection = TlsIo<TcpTransport, Box<[u8; TLS_WIRE_BYTES]>>;

fn connections(
    client: Arc<ClientConfig>,
    server: TlsSession,
    clock: &Arc<TestClock>,
) -> (Connection, Connection) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let left = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (right, _) = listener.accept().unwrap();
    let deadline = Deadline::after(Tick(0), 1000).unwrap();
    let wrap = |stream, session| {
        TlsIo::new(
            TcpTransport::from_stream(stream).unwrap(),
            session,
            clock.clone(),
            deadline,
            deadline,
            Box::new([0; TLS_WIRE_BYTES]),
            Box::new([0; TLS_WIRE_BYTES]),
        )
        .unwrap()
    };
    (
        wrap(left, TlsSession::client(client, "localhost").unwrap()),
        wrap(right, server),
    )
}

#[test]
fn gateway_tls_requires_a_client_certificate_on_the_same_valid_server() {
    let (identity, ca) = identity_material();
    let clock = TestClock::new();
    let crypto_clock = Arc::new(ClockHandle::new(TlsClockSource::new(clock.clone())));
    let roots = TrustStore::from_pem(&ca).unwrap();
    let client =
        Arc::new(ClientConfig::new(&roots, crypto_clock.clone(), TlsProtocol::Smtp).unwrap());
    let direct = Arc::new(
        ServerConfig::new(
            std::slice::from_ref(&identity),
            TlsProtocol::Smtp,
            IdentitySelection::MatchPresentName,
            None,
            crypto_clock.clone(),
        )
        .unwrap(),
    );
    let (mut left, mut right) =
        connections(client.clone(), TlsSession::server(direct).unwrap(), &clock);
    handshakes(&mut left, &mut right);
    drop((left, right));
    let gateway = policy(Spec::default(), &ca, &identity, &crypto_clock).unwrap();
    let (mut left, mut right) = connections(client, gateway.new_session().unwrap(), &clock);
    for _ in 0..100_000 {
        left.handshake().unwrap();
        match right.handshake() {
            Ok(None) => std::thread::yield_now(),
            Ok(Some(_)) => panic!("gateway accepted a peer without a client certificate"),
            Err(error) => {
                assert_eq!(error, Error::Tls);
                assert!(right.evidence().is_none());
                return;
            }
        }
    }
    panic!("gateway did not refuse missing client authentication");
}

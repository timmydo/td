#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use super::*;
use crate::{certificate_fixtures as f, pem::tests::pem, UtcClock};
use aws_lc_rs::signature::{EcdsaKeyPair, ECDSA_P256_SHA256_ASN1_SIGNING};
use std::sync::atomic::{AtomicU64, Ordering};
const NOW: u64 = 1_800_000_000;
struct Source(Arc<AtomicU64>);
impl UtcClock for Source {
    fn now(&self) -> Option<u64> {
        match self.0.load(Ordering::SeqCst) {
            0 => None,
            seconds => Some(seconds),
        }
    }
}
fn test_clock() -> (Arc<AtomicU64>, Arc<ClockHandle>) {
    let value = Arc::new(AtomicU64::new(NOW));
    (value.clone(), Arc::new(ClockHandle::new(Source(value))))
}
fn key() -> (Vec<u8>, EcdsaKeyPair) {
    let der = EcdsaKeyPair::generate_pkcs8(
        &ECDSA_P256_SHA256_ASN1_SIGNING,
        &aws_lc_rs::rand::SystemRandom::new(),
    )
    .unwrap();
    let key = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, der.as_ref()).unwrap();
    (der.as_ref().to_vec(), key)
}
struct Fixture {
    issuer: Vec<u8>,
    ca: EcdsaKeyPair,
    root: Vec<u8>,
    secret: Vec<u8>,
    chain: Vec<u8>,
}
impl Fixture {
    fn new(issuer: &[u8]) -> Self {
        let (_, ca) = key();
        let mut root = f::Parameters::new(true);
        root.subject = issuer.to_vec();
        root.issuer = issuer.to_vec();
        let root = f::make(&ca, &ca, &root).unwrap();
        let (secret, leaf) = key();
        let mut parameters = f::Parameters::new(false);
        parameters.issuer = issuer.to_vec();
        parameters.extensions[2] = f::extension(
            0x11,
            false,
            f::seq(&[f::der(0x82, b"*.mail.test"), f::der(0x82, b"localhost")]),
        );
        let leaf = f::make(&leaf, &ca, &parameters).unwrap();
        let chain = [pem("CERTIFICATE", &leaf), pem("CERTIFICATE", &root)].concat();
        Self {
            issuer: issuer.to_vec(),
            ca,
            root,
            secret,
            chain,
        }
    }
    fn identity(&self, names: &[&str]) -> Arc<ServerIdentity> {
        Arc::new(
            ServerIdentity::from_pem(
                &self.chain,
                &pem("PRIVATE KEY", &self.secret),
                names,
                Some(NOW),
            )
            .unwrap(),
        )
    }
    fn trust(&self) -> TrustStore {
        TrustStore::from_pem(&pem("CERTIFICATE", &self.root)).unwrap()
    }
    fn client_identity(
        &self,
        kind: &str,
    ) -> (
        Vec<CertificateDer<'static>>,
        rustls::pki_types::PrivateKeyDer<'static>,
    ) {
        let (secret, key) = key();
        let mut parameters = f::Parameters::new(false);
        parameters.issuer = self.issuer.clone();
        if kind != "wrong-usage" {
            parameters.extensions[3] =
                f::extension(0x25, false, f::seq(&[f::oid(&[0x2b, 6, 1, 5, 5, 7, 3, 2])]));
        }
        if kind == "expired" {
            parameters.not_after = b"250201000000Z".to_vec();
        }
        if kind == "oversized" {
            parameters
                .extensions
                .push(f::extension(120, false, vec![0; 16_384]));
        }
        let mut leaf = f::make(&key, &self.ca, &parameters).unwrap();
        if kind == "bad-signature" {
            *leaf.last_mut().unwrap() ^= 1;
        }
        let mut chain = vec![
            CertificateDer::from(leaf),
            CertificateDer::from(self.root.clone()),
        ];
        if kind == "too-many" {
            chain.extend((0..7).map(|_| CertificateDer::from(self.root.clone())));
        }
        (
            chain,
            rustls::pki_types::PrivatePkcs8KeyDer::from(secret).into(),
        )
    }
}
fn remote_client(
    trust: &TrustStore,
    version: &'static rustls::SupportedProtocolVersion,
    sni: bool,
    alpn: &[&[u8]],
    identity: Option<(
        Vec<CertificateDer<'static>>,
        rustls::pki_types::PrivateKeyDer<'static>,
    )>,
) -> Arc<rustls::ClientConfig> {
    let (_, clock) = test_clock();
    let builder = rustls::ClientConfig::builder_with_details(
        Arc::new(crate::tls_policy::provider().unwrap()),
        Arc::new(BackendClock(clock)),
    )
    .with_protocol_versions(&[version])
    .unwrap()
    .with_root_certificates(trust.roots.clone());
    let mut client = match identity {
        None => builder.with_no_client_auth(),
        Some((chain, key)) => builder.with_client_auth_cert(chain, key).unwrap(),
    };
    client.enable_sni = sni;
    client.alpn_protocols = alpn.iter().map(|p| p.to_vec()).collect();
    // The remote client deliberately retains its native resumption cache.
    Arc::new(client)
}
fn pair(
    client: Arc<rustls::ClientConfig>,
    server: &ServerConfig,
    name: &'static str,
) -> (rustls::Connection, rustls::Connection) {
    pair_native(client, server.native.clone(), name)
}
fn pair_native(
    client: Arc<rustls::ClientConfig>,
    server: Arc<rustls::ServerConfig>,
    name: &'static str,
) -> (rustls::Connection, rustls::Connection) {
    let mut client = rustls::Connection::Client(
        rustls::ClientConnection::new(client, name.try_into().unwrap()).unwrap(),
    );
    let mut server = rustls::Connection::Server(rustls::ServerConnection::new(server).unwrap());
    client.set_buffer_limit(Some(32 * 1024));
    server.set_buffer_limit(Some(32 * 1024));
    (client, server)
}
#[test]
fn server_configuration_bounds_bindings_roles_and_private_trust() {
    let fixture = Fixture::new(b"td-server-root");
    let identity = fixture.identity(&["one.mail.test"]);
    let (_, clock) = test_clock();
    let make = |identities: &[Arc<ServerIdentity>], protocol, selection, trust| {
        ServerConfig::new(identities, protocol, selection, trust, clock.clone())
    };
    assert!(matches!(
        make(
            &[],
            TlsProtocol::Http1,
            IdentitySelection::RequiredName,
            None
        ),
        Err(TlsError::Invalid)
    ));
    let seventeen = (0..17)
        .map(|i| fixture.identity(&[&format!("identity-{i}.mail.test")]))
        .collect::<Vec<_>>();
    assert!(matches!(
        make(
            &seventeen,
            TlsProtocol::Http1,
            IdentitySelection::RequiredName,
            None
        ),
        Err(TlsError::Invalid)
    ));
    let duplicate = fixture.identity(&["one.mail.test"]);
    assert!(!Arc::ptr_eq(&identity, &duplicate));
    assert!(matches!(
        make(
            &[identity.clone(), duplicate],
            TlsProtocol::Http1,
            IdentitySelection::RequiredName,
            None
        ),
        Err(TlsError::Invalid)
    ));
    for protocol in [TlsProtocol::Http1, TlsProtocol::Smtp] {
        for selection in [
            IdentitySelection::RequiredName,
            IdentitySelection::DefaultIdentity,
            IdentitySelection::MatchPresentName,
        ] {
            let allowed = matches!(
                (protocol, selection),
                (TlsProtocol::Http1, IdentitySelection::RequiredName)
                    | (
                        TlsProtocol::Smtp,
                        IdentitySelection::DefaultIdentity | IdentitySelection::MatchPresentName
                    )
            );
            assert_eq!(
                make(std::slice::from_ref(&identity), protocol, selection, None).is_ok(),
                allowed
            );
        }
    }
    let public = TrustStore::public_roots().unwrap();
    assert!(matches!(
        make(
            std::slice::from_ref(&identity),
            TlsProtocol::Smtp,
            IdentitySelection::MatchPresentName,
            Some(&public)
        ),
        Err(TlsError::Invalid)
    ));
    let mut identities = Vec::new();
    for i in 0..16 {
        let names = (0..32)
            .map(|j| format!("host-{i}-{j}.mail.test"))
            .collect::<Vec<_>>();
        identities.push(fixture.identity(&names.iter().map(String::as_str).collect::<Vec<_>>()));
    }
    let config = make(
        &identities,
        TlsProtocol::Http1,
        IdentitySelection::RequiredName,
        None,
    )
    .unwrap();
    assert_eq!(config.identity_count(), 16);
    assert_eq!(config.binding_count(), 512);
    assert_eq!(config.routing.select(Some("HOST-15-31.MAIL.TEST")), Ok(15));
    assert!(matches!(
        make(
            &identities[..2],
            TlsProtocol::Smtp,
            IdentitySelection::DefaultIdentity,
            None
        ),
        Err(TlsError::Invalid)
    ));
    assert!(format!("{config:?}").contains("identities: 16, bindings: 512"));
    assert!(!format!("{config:?}").contains("mail.test"));
    assert!(!format!("{config:?}").contains("host-"));
    assert!(rustls::crypto::CryptoProvider::get_default().is_none());
}
#[test]
fn server_configuration_routes_names_and_refuses_resumption() {
    let fixture = Fixture::new(b"td-server-root");
    let identity = fixture.identity(&["one.mail.test"]);
    let trust = fixture.trust();
    for (protocol, selection) in [
        (TlsProtocol::Http1, IdentitySelection::RequiredName),
        (TlsProtocol::Smtp, IdentitySelection::DefaultIdentity),
        (TlsProtocol::Smtp, IdentitySelection::MatchPresentName),
    ] {
        let (_, clock) = test_clock();
        let config = ServerConfig::new(
            std::slice::from_ref(&identity),
            protocol,
            selection,
            None,
            clock,
        )
        .unwrap();
        for name in [
            Some("one.mail.test"),
            Some("ONE.MAIL.TEST"),
            Some("other.mail.test"),
            None,
        ] {
            let accepted = name.is_some_and(|n| n.eq_ignore_ascii_case("one.mail.test"))
                || selection == IdentitySelection::DefaultIdentity
                || (selection == IdentitySelection::MatchPresentName && name.is_none());
            assert_eq!(config.routing.select(name).is_ok(), accepted);
            for version in [&rustls::version::TLS12, &rustls::version::TLS13] {
                let alpn = if protocol == TlsProtocol::Http1 {
                    vec![b"http/1.1".as_slice()]
                } else {
                    vec![]
                };
                let client = remote_client(&trust, version, name.is_some(), &alpn, None);
                for _ in 0..2 {
                    let (mut client, mut server) =
                        pair(client.clone(), &config, name.unwrap_or("one.mail.test"));
                    let result = crate::tls_smoke::drive(&mut client, &mut server);
                    if accepted {
                        result.unwrap();
                        for peer in [&client, &server] {
                            assert_eq!(peer.handshake_kind(), Some(rustls::HandshakeKind::Full));
                            assert!(!peer.is_handshaking());
                        }
                        assert_eq!(
                            client.peer_certificates().unwrap()[0].as_ref(),
                            identity.certificate_der(0).unwrap()
                        );
                    } else {
                        assert!(
                            matches!(result.unwrap_err().downcast_ref::<rustls::Error>(),Some(rustls::Error::General(message)) if message=="no server certificate chain resolved")
                        );
                    }
                }
            }
        }
        for malformed in [
            "",
            "127.0.0.1",
            "::1",
            "one.mail.test.",
            "one..mail.test",
            "*.mail.test",
            "_name.mail.test",
        ] {
            assert_eq!(
                config.routing.select(Some(malformed)),
                Err(TlsError::Protocol)
            );
        }
    }
}

#[test]
fn server_configuration_pins_disabled_backend_features() {
    fn shareable<T: Send + Sync>() {}
    shareable::<ServerConfig>();
    let fixture = Fixture::new(b"td-server-root");
    let identity = fixture.identity(&["one.mail.test"]);
    let (_, clock) = test_clock();
    let config = ServerConfig::new(
        &[identity],
        TlsProtocol::Smtp,
        IdentitySelection::DefaultIdentity,
        None,
        clock,
    )
    .unwrap();
    let native = &config.native;
    assert!(native.ignore_client_order && native.require_ems);
    assert_eq!(native.max_fragment_size, Some(16_384 + 5));
    assert!(!native.session_storage.can_cache());
    assert!(!native.session_storage.put(vec![1], vec![2]));
    assert!(native.session_storage.get(&[1]).is_none());
    assert!(native.session_storage.take(&[1]).is_none());
    assert!(!native.ticketer.enabled());
    assert_eq!(native.ticketer.lifetime(), 0);
    assert!(native.ticketer.encrypt(b"secret").is_none());
    assert!(native.ticketer.decrypt(b"untrusted").is_none());
    assert_eq!(native.send_tls13_tickets, 0);
    assert_eq!(native.max_tls13_tickets, 0);
    assert_eq!(native.max_early_data_size, 0);
    assert!(!native.send_half_rtt_data);
    assert!(!native.enable_secret_extraction);
    assert!(!native.key_log.will_log("CLIENT_RANDOM"));
    assert!(native.cert_compressors.is_empty() && native.cert_decompressors.is_empty());
    assert!(matches!(
        &*native.cert_compression_cache,
        rustls::compress::CompressionCache::Disabled
    ));
    assert!(native.alpn_protocols.is_empty());
    let actual = native.crypto_provider();
    let expected = crate::tls_policy::provider().unwrap();
    assert_eq!(
        actual
            .cipher_suites
            .iter()
            .map(|c| c.suite())
            .collect::<Vec<_>>(),
        expected
            .cipher_suites
            .iter()
            .map(|c| c.suite())
            .collect::<Vec<_>>()
    );
    assert_eq!(
        actual
            .kx_groups
            .iter()
            .map(|g| g.name())
            .collect::<Vec<_>>(),
        expected
            .kx_groups
            .iter()
            .map(|g| g.name())
            .collect::<Vec<_>>()
    );
    let actual = actual.signature_verification_algorithms;
    let expected = expected.signature_verification_algorithms;
    let compare = |a: &[&dyn rustls::pki_types::SignatureVerificationAlgorithm],
                   b: &[&dyn rustls::pki_types::SignatureVerificationAlgorithm]| {
        assert_eq!(a.len(), b.len());
        for (a, b) in a.iter().zip(b) {
            assert_eq!(a.public_key_alg_id(), b.public_key_alg_id());
            assert_eq!(a.signature_alg_id(), b.signature_alg_id());
        }
    };
    compare(actual.all, expected.all);
    assert_eq!(actual.mapping.len(), expected.mapping.len());
    for ((scheme, a), (wanted, b)) in actual.mapping.iter().zip(expected.mapping) {
        assert_eq!(scheme, wanted);
        compare(a, b);
    }
}
#[test]
fn server_configuration_requires_private_client_certificates() {
    let fixture = Fixture::new(b"td-server-root");
    let stranger = Fixture::new(b"td-other-root");
    let identity = fixture.identity(&["one.mail.test"]);
    let trust = fixture.trust();
    let (_, clock) = test_clock();
    let config = ServerConfig::new(
        &[identity],
        TlsProtocol::Smtp,
        IdentitySelection::MatchPresentName,
        Some(&trust),
        clock,
    )
    .unwrap();
    for version in [&rustls::version::TLS12, &rustls::version::TLS13] {
        let material = fixture.client_identity("valid");
        let leaf = material.0[0].clone();
        let client = remote_client(&trust, version, true, &[], Some(material));
        for _ in 0..2 {
            let (mut client, mut server) = pair(client.clone(), &config, "one.mail.test");
            crate::tls_smoke::drive(&mut client, &mut server).unwrap();
            assert!(!server.is_handshaking());
            assert_eq!(server.handshake_kind(), Some(rustls::HandshakeKind::Full));
            assert_eq!(server.peer_certificates().unwrap()[0], leaf);
        }
        for kind in [
            "missing",
            "untrusted",
            "expired",
            "wrong-usage",
            "bad-signature",
            "too-many",
            "oversized",
        ] {
            let material = match kind {
                "missing" => None,
                "untrusted" => Some(stranger.client_identity("valid")),
                _ => Some(fixture.client_identity(kind)),
            };
            let (mut client, mut server) = pair(
                remote_client(&trust, version, true, &[], material),
                &config,
                "one.mail.test",
            );
            let error = crate::tls_smoke::drive(&mut client, &mut server).unwrap_err();
            let error = error.downcast_ref::<rustls::Error>().unwrap();
            match (kind, error) {
                ("missing", rustls::Error::NoCertificatesPresented) => {}
                (
                    "untrusted",
                    rustls::Error::InvalidCertificate(rustls::CertificateError::UnknownIssuer),
                ) => {}
                (
                    "expired",
                    rustls::Error::InvalidCertificate(rustls::CertificateError::ExpiredContext {
                        ..
                    }),
                ) => {}
                (
                    "wrong-usage",
                    rustls::Error::InvalidCertificate(
                        rustls::CertificateError::InvalidPurposeContext { .. },
                    ),
                ) => {}
                (
                    "bad-signature",
                    rustls::Error::InvalidCertificate(rustls::CertificateError::BadSignature),
                ) => {}
                ("too-many" | "oversized", rustls::Error::Other(rustls::OtherError(error))) => {
                    assert_eq!(error.downcast_ref::<TlsError>(), Some(&TlsError::Capacity))
                }
                _ => panic!("unexpected {kind} refusal: {error:?}"),
            }
            assert!(server.is_handshaking());
        }
    }
}
#[test]
fn server_configuration_checks_cold_material_and_supplied_verifier_time() {
    let fixture = Fixture::new(b"td-server-root");
    let identity = fixture.identity(&["one.mail.test"]);
    let trust = fixture.trust();
    let (value, clock) = test_clock();
    let make = || {
        ServerConfig::new(
            std::slice::from_ref(&identity),
            TlsProtocol::Smtp,
            IdentitySelection::MatchPresentName,
            Some(&trust),
            clock.clone(),
        )
    };
    value.store(0, Ordering::SeqCst);
    assert!(matches!(make(), Err(TlsError::Clock)));
    value.store(2_051_222_401, Ordering::SeqCst);
    assert!(matches!(
        make(),
        Err(TlsError::Verification(crate::VerificationFailure::Expired))
    ));
    value.store(NOW, Ordering::SeqCst);
    let config = make().unwrap();
    value.store(0, Ordering::SeqCst);
    for version in [&rustls::version::TLS12, &rustls::version::TLS13] {
        let (mut client, mut server) = pair(
            remote_client(
                &trust,
                version,
                true,
                &[],
                Some(fixture.client_identity("valid")),
            ),
            &config,
            "one.mail.test",
        );
        let error = crate::tls_smoke::drive(&mut client, &mut server).unwrap_err();
        assert!(matches!(
            error.downcast_ref::<rustls::Error>(),
            Some(rustls::Error::FailedToGetCurrentTime)
        ));
    }
}
#[test]
fn backend_acceptor_discards_ip_literal_sni_before_routing() {
    let fixture = Fixture::new(b"td-server-root");
    let trust = fixture.trust();
    let client = remote_client(&trust, &rustls::version::TLS13, true, &[], None);
    let mut client =
        rustls::ClientConnection::new(client, "one.mail.test".try_into().unwrap()).unwrap();
    let mut wire = [0; 32 * 1024];
    let size = client
        .write_tls(&mut std::io::Cursor::new(wire.as_mut_slice()))
        .unwrap();
    assert!(!client.wants_write());
    let indexes = wire[..size]
        .windows(13)
        .enumerate()
        .filter_map(|(i, s)| (s == b"one.mail.test").then_some(i))
        .collect::<Vec<_>>();
    assert_eq!(indexes.len(), 1);
    let index = indexes[0];
    wire[index..index + 13].copy_from_slice(b"192.168.100.1");
    let identity = fixture.identity(&["one.mail.test"]);
    for (protocol, selection) in [
        (TlsProtocol::Http1, IdentitySelection::RequiredName),
        (TlsProtocol::Smtp, IdentitySelection::DefaultIdentity),
        (TlsProtocol::Smtp, IdentitySelection::MatchPresentName),
    ] {
        let mut acceptor = rustls::server::Acceptor::default();
        assert_eq!(
            acceptor
                .read_tls(&mut std::io::Cursor::new(&wire[..size]))
                .unwrap(),
            size
        );
        let accepted = acceptor.accept().unwrap().unwrap();
        assert!(accepted.client_hello().server_name().is_none());
        let (_, clock) = test_clock();
        let config = ServerConfig::new(
            std::slice::from_ref(&identity),
            protocol,
            selection,
            None,
            clock,
        )
        .unwrap();
        match accepted.into_connection(config.native.clone()) {
            Ok(connection) => {
                assert_eq!(protocol, TlsProtocol::Smtp);
                assert_eq!(config.routing.select(None), Ok(0));
                assert!(connection.is_handshaking() && connection.wants_write());
            }
            Err((error, _)) => {
                assert_eq!(protocol, TlsProtocol::Http1);
                assert!(
                    matches!(error, rustls::Error::General(message) if message == "no server certificate chain resolved")
                );
            }
        }
    }
    // This mutates the transcript: backend selection is not a full authenticated
    // handshake. M07c must refuse raw IP SNI before the Acceptor loses it.
}

#[test]
fn server_configuration_selects_distinct_owned_identities() {
    let first = Fixture::new(b"td-first-root");
    let second = Fixture::new(b"td-second-root");
    let identities = [
        first.identity(&["one.mail.test"]),
        second.identity(&["two.mail.test"]),
    ];
    let roots = TrustStore::from_pem(
        &[
            pem("CERTIFICATE", &first.root),
            pem("CERTIFICATE", &second.root),
        ]
        .concat(),
    )
    .unwrap();
    let (_, clock) = test_clock();
    let config = ServerConfig::new(
        &identities,
        TlsProtocol::Http1,
        IdentitySelection::RequiredName,
        None,
        clock,
    )
    .unwrap();
    for version in [&rustls::version::TLS12, &rustls::version::TLS13] {
        for (index, name) in [(0, "one.mail.test"), (1, "two.mail.test")] {
            for alpn in [vec![], vec![b"http/1.1".as_slice()]] {
                let (mut client, mut server) = pair(
                    remote_client(&roots, version, true, &alpn, None),
                    &config,
                    name,
                );
                crate::tls_smoke::drive(&mut client, &mut server).unwrap();
                assert!(!client.is_handshaking() && !server.is_handshaking());
                assert_eq!(
                    client.peer_certificates().unwrap()[0].as_ref(),
                    identities[index].certificate_der(0).unwrap()
                );
                assert_eq!(client.alpn_protocol(), alpn.first().copied());
            }
        }
    }
    for version in [&rustls::version::TLS12, &rustls::version::TLS13] {
        let (mut client, mut server) = pair(
            remote_client(&roots, version, true, &[b"h2"], None),
            &config,
            "one.mail.test",
        );
        let error = crate::tls_smoke::drive(&mut client, &mut server).unwrap_err();
        assert!(matches!(
            error.downcast_ref::<rustls::Error>(),
            Some(rustls::Error::NoApplicationProtocol)
        ));
        let (_, clock) = test_clock();
        let smtp = ServerConfig::new(
            &identities[..1],
            TlsProtocol::Smtp,
            IdentitySelection::DefaultIdentity,
            None,
            clock,
        )
        .unwrap();
        let (mut client, mut server) = pair(
            remote_client(&roots, version, true, &[b"h2", b"http/1.1"], None),
            &smtp,
            "one.mail.test",
        );
        crate::tls_smoke::drive(&mut client, &mut server).unwrap();
        assert_eq!(client.alpn_protocol(), None);
        assert_eq!(server.alpn_protocol(), None);
    }
}

#[test]
fn server_configuration_omits_oversized_ca_hint_lists() {
    use std::sync::atomic::AtomicUsize;
    let mut bundle = Vec::new();
    for i in 0..6 {
        let (_, ca) = key();
        let mut parameters = f::Parameters::new(true);
        parameters.subject = vec![b'a' + i; 12_000];
        parameters.issuer = b"td-short-issuer".to_vec();
        let certificate = f::make(&ca, &ca, &parameters).unwrap();
        assert!(certificate.len() < 16 * 1024);
        bundle.extend(pem("CERTIFICATE", &certificate));
    }
    assert!(bundle.len() <= 128 * 1024);
    let client_trust = TrustStore::from_pem(&bundle).unwrap();
    assert!(
        client_trust
            .roots
            .subjects()
            .iter()
            .map(|name| name.as_ref().len() + 2)
            .sum::<usize>()
            > u16::MAX as usize
    );
    let fixture = Fixture::new(b"td-server-root");
    let identity = fixture.identity(&["one.mail.test"]);
    let roots = fixture.trust();
    let (_, clock) = test_clock();
    let config = ServerConfig::new(
        &[identity],
        TlsProtocol::Smtp,
        IdentitySelection::MatchPresentName,
        Some(&client_trust),
        clock,
    )
    .unwrap();
    #[derive(Debug)]
    struct ObserveHints(Arc<AtomicUsize>);
    impl rustls::client::ResolvesClientCert for ObserveHints {
        fn resolve(
            &self,
            hints: &[&[u8]],
            _: &[SignatureScheme],
        ) -> Option<Arc<rustls::sign::CertifiedKey>> {
            assert!(hints.is_empty());
            self.0.fetch_add(1, Ordering::SeqCst);
            None
        }
        fn has_certs(&self) -> bool {
            true
        }
    }
    for version in [&rustls::version::TLS12, &rustls::version::TLS13] {
        let observed = Arc::new(AtomicUsize::new(0));
        let (_, clock) = test_clock();
        let client = rustls::ClientConfig::builder_with_details(
            Arc::new(crate::tls_policy::provider().unwrap()),
            Arc::new(BackendClock(clock)),
        )
        .with_protocol_versions(&[version])
        .unwrap()
        .with_root_certificates(roots.roots.clone())
        .with_client_cert_resolver(Arc::new(ObserveHints(observed.clone())));
        let (mut client, mut server) = pair(Arc::new(client), &config, "one.mail.test");
        let error = crate::tls_smoke::drive(&mut client, &mut server).unwrap_err();
        assert!(matches!(
            error.downcast_ref::<rustls::Error>(),
            Some(rustls::Error::NoCertificatesPresented)
        ));
        assert_eq!(observed.load(Ordering::SeqCst), 1);
    }
}

#[test]
fn server_configuration_refuses_offered_resumption() {
    let fixture = Fixture::new(b"td-server-root");
    let identity = fixture.identity(&["one.mail.test"]);
    let trust = fixture.trust();
    let (_, clock) = test_clock();
    let config = ServerConfig::new(
        &[identity],
        TlsProtocol::Smtp,
        IdentitySelection::DefaultIdentity,
        None,
        clock,
    )
    .unwrap();
    let mut issuing = (*config.native).clone();
    issuing.session_storage = rustls::server::ServerSessionMemoryCache::new(32);
    issuing.send_tls13_tickets = 2;
    let issuing = Arc::new(issuing);
    for version in [&rustls::version::TLS12, &rustls::version::TLS13] {
        let client_config = remote_client(&trust, version, true, &[], None);
        for wanted in [rustls::HandshakeKind::Full, rustls::HandshakeKind::Resumed] {
            let (mut client, mut server) =
                pair_native(client_config.clone(), issuing.clone(), "one.mail.test");
            crate::tls_smoke::drive(&mut client, &mut server).unwrap();
            assert_eq!(client.handshake_kind(), Some(wanted));
            assert_eq!(server.handshake_kind(), Some(wanted));
        }
        let (mut client, mut server) = pair(client_config, &config, "one.mail.test");
        let mut wire = [0; 32 * 1024];
        let size = client
            .write_tls(&mut std::io::Cursor::new(wire.as_mut_slice()))
            .unwrap();
        assert!(!client.wants_write());
        assert_eq!(wire[0], 22);
        assert_eq!(wire[5], 1);
        let session_id_length = usize::from(wire[43]);
        if version.version == rustls::ProtocolVersion::TLSv1_2 {
            assert!(session_id_length > 0);
        } else {
            // Inspect the generated ClientHello's pre_shared_key extension;
            // a nonempty legacy ID alone is not a TLS 1.3 resumption offer.
            let mut offset = 44 + session_id_length;
            let suites = usize::from(u16::from_be_bytes([wire[offset], wire[offset + 1]]));
            offset += 2 + suites;
            offset += 1 + usize::from(wire[offset]);
            let extensions = usize::from(u16::from_be_bytes([wire[offset], wire[offset + 1]]));
            offset += 2;
            let end = offset + extensions;
            assert!(end <= size);
            let mut offered = false;
            while offset < end {
                let kind = u16::from_be_bytes([wire[offset], wire[offset + 1]]);
                let len = usize::from(u16::from_be_bytes([wire[offset + 2], wire[offset + 3]]));
                offset += 4;
                assert!(offset + len <= end);
                if kind == 41 {
                    assert!(len > 4);
                    offered = true;
                }
                offset += len;
            }
            assert!(offered);
        }
        let mut input = std::io::Cursor::new(&wire[..size]);
        while input.position() < size as u64 {
            assert!(server.read_tls(&mut input).unwrap() > 0);
            server.process_new_packets().unwrap();
        }
        crate::tls_smoke::drive(&mut client, &mut server).unwrap();
        assert_eq!(client.handshake_kind(), Some(rustls::HandshakeKind::Full));
        assert_eq!(server.handshake_kind(), Some(rustls::HandshakeKind::Full));
        assert!(!client.is_handshaking() && !server.is_handshaking());
    }
}

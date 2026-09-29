#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use super::*;
use crate::{certificate_fixtures as f, pem::tests::pem, ServerIdentity, UtcClock};
use aws_lc_rs::signature::{EcdsaKeyPair, ECDSA_P256_SHA256_ASN1_SIGNING};
use std::{
    io::{Cursor, Read, Write},
    sync::atomic::{AtomicU64, Ordering},
};
const NOW: u64 = 1_800_000_000;
struct Source(Arc<AtomicU64>);
impl UtcClock for Source {
    fn now(&self) -> Option<u64> {
        match self.0.load(Ordering::SeqCst) {
            0 => None,
            u64::MAX => panic!("synthetic clock failure"),
            value => Some(value),
        }
    }
}
fn clock() -> (Arc<AtomicU64>, Arc<ClockHandle>) {
    let value = Arc::new(AtomicU64::new(NOW));
    let clock = Arc::new(ClockHandle::new(Source(value.clone())));
    (value, clock)
}
fn fixture() -> (ServerIdentity, TrustStore) {
    fixture_with_issuer(b"td-test-root")
}
fn fixture_with_issuer(issuer: &[u8]) -> (ServerIdentity, TrustStore) {
    let rng = aws_lc_rs::rand::SystemRandom::new();
    let generate = || {
        let der = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, &rng).unwrap();
        let key = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, der.as_ref()).unwrap();
        (der, key)
    };
    let (_, root) = generate();
    let (secret, leaf) = generate();
    let mut root_parameters = f::Parameters::new(true);
    root_parameters.subject = issuer.to_vec();
    root_parameters.issuer = issuer.to_vec();
    let root = f::make(&root, &root, &root_parameters)
        .map(|der| (root, der))
        .unwrap();
    let mut leaf_parameters = f::Parameters::new(false);
    leaf_parameters.issuer = issuer.to_vec();
    let leaf = f::make(&leaf, &root.0, &leaf_parameters).unwrap();
    let root_pem = pem("CERTIFICATE", &root.1);
    let chain = [pem("CERTIFICATE", &leaf), root_pem.clone()].concat();
    (
        ServerIdentity::from_pem(
            &chain,
            &pem("PRIVATE KEY", secret.as_ref()),
            &["localhost"],
            Some(NOW),
        )
        .unwrap(),
        TrustStore::from_pem(&root_pem).unwrap(),
    )
}
fn server(
    identity: &ServerIdentity,
    version: &'static rustls::SupportedProtocolVersion,
    alpn: &[&[u8]],
) -> Arc<rustls::ServerConfig> {
    let (_, clock) = clock();
    let mut server = rustls::ServerConfig::builder_with_details(
        Arc::new(crate::tls_policy::provider().unwrap()),
        Arc::new(BackendClock(clock)),
    )
    .with_protocol_versions(&[version])
    .unwrap()
    .with_no_client_auth()
    .with_cert_resolver(Arc::new(rustls::sign::SingleCertAndKey::from(
        identity.certified.clone(),
    )));
    server.alpn_protocols = alpn.iter().map(|p| p.to_vec()).collect();
    server.send_tls13_tickets = 2;
    Arc::new(server)
}
fn pair(
    client: &ClientConfig,
    server: Arc<rustls::ServerConfig>,
    name: &'static str,
) -> (rustls::Connection, rustls::Connection) {
    let mut client = rustls::Connection::Client(
        rustls::ClientConnection::new(client.native.clone(), name.try_into().unwrap()).unwrap(),
    );
    let mut server = rustls::Connection::Server(rustls::ServerConnection::new(server).unwrap());
    client.set_buffer_limit(Some(32 * 1024));
    server.set_buffer_limit(Some(32 * 1024));
    (client, server)
}
#[test]
fn client_configuration_pins_policy_and_redacts_state() {
    fn shareable<T: Send + Sync>() {}
    shareable::<ClientConfig>();
    let roots = TrustStore::public_roots().unwrap();
    for protocol in [TlsProtocol::Http1, TlsProtocol::Smtp] {
        let (_, clock) = clock();
        let config = ClientConfig::new(&roots, clock.clone(), protocol).unwrap();
        let native = &config.native;
        assert_eq!(
            native.alpn_protocols,
            if protocol == TlsProtocol::Http1 {
                vec![b"http/1.1".to_vec()]
            } else {
                vec![]
            }
        );
        assert!(native.check_selected_alpn && native.enable_sni && native.require_ems);
        assert!(!native.enable_early_data && !native.enable_secret_extraction);
        assert!(!native.key_log.will_log("CLIENT_RANDOM"));
        assert!(!native.client_auth_cert_resolver.has_certs());
        assert!(native.cert_compressors.is_empty() && native.cert_decompressors.is_empty());
        assert!(matches!(
            &*native.cert_compression_cache,
            rustls::compress::CompressionCache::Disabled
        ));
        assert!(native.send_ticket_request.is_none());
        assert_eq!(native.max_fragment_size, Some(16_384 + 5));
        assert_eq!(
            native
                .crypto_provider()
                .cipher_suites
                .iter()
                .map(|s| s.suite())
                .collect::<Vec<_>>(),
            crate::tls_policy::provider()
                .unwrap()
                .cipher_suites
                .iter()
                .map(|s| s.suite())
                .collect::<Vec<_>>()
        );
        let expected = crate::tls_policy::provider().unwrap();
        let actual = native.crypto_provider();
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
        let compare =
            |actual: &[&dyn rustls::pki_types::SignatureVerificationAlgorithm],
             expected: &[&dyn rustls::pki_types::SignatureVerificationAlgorithm]| {
                assert_eq!(actual.len(), expected.len());
                for (actual, expected) in actual.iter().zip(expected) {
                    assert_eq!(actual.public_key_alg_id(), expected.public_key_alg_id());
                    assert_eq!(actual.signature_alg_id(), expected.signature_alg_id());
                }
            };
        compare(actual.all, expected.all);
        assert_eq!(actual.mapping.len(), expected.mapping.len());
        for ((actual_scheme, actual), (expected_scheme, expected)) in
            actual.mapping.iter().zip(expected.mapping)
        {
            assert_eq!(actual_scheme, expected_scheme);
            compare(actual, expected);
        }
        assert_eq!(native.time_provider.current_time().unwrap().as_secs(), NOW);
        assert!(Arc::ptr_eq(&config.clock, &clock));
        assert_eq!(format!("{config:?}"), format!("ClientConfig {{ protocol: {protocol:?}, alpn_count: {}, clock: ClockHandle(<redacted>), .. }}", native.alpn_protocols.len()));
    }
    assert!(rustls::crypto::CryptoProvider::get_default().is_none());
}
#[test]
fn client_configuration_verifies_trust_name_alpn_and_full_handshakes() {
    let (identity, roots) = fixture();
    let (_, other) = fixture_with_issuer(b"td-test-other-root");
    for version in [&rustls::version::TLS12, &rustls::version::TLS13] {
        for (protocol, alpn) in [
            (TlsProtocol::Smtp, None),
            (TlsProtocol::Http1, None),
            (TlsProtocol::Http1, Some(b"http/1.1".as_slice())),
        ] {
            let (_, clock) = clock();
            let config = ClientConfig::new(&roots, clock, protocol).unwrap();
            let server = server(&identity, version, &alpn.into_iter().collect::<Vec<_>>());
            for _ in 0..2 {
                let (mut client, mut server) = pair(&config, server.clone(), "localhost");
                crate::tls_smoke::drive(&mut client, &mut server).unwrap();
                for peer in [&client, &server] {
                    assert!(!peer.is_handshaking());
                    assert_eq!(peer.protocol_version(), Some(version.version));
                    assert_eq!(peer.handshake_kind(), Some(rustls::HandshakeKind::Full));
                    assert_eq!(peer.alpn_protocol(), alpn);
                }
                let message = [0xa5; 16_384];
                client.writer().write_all(&message).unwrap();
                let mut wire = [0; 32 * 1024];
                let count = client
                    .write_tls(&mut Cursor::new(wire.as_mut_slice()))
                    .unwrap();
                assert!(!client.wants_write());
                assert_eq!(count, 5 + u16::from_be_bytes([wire[3], wire[4]]) as usize);
                let mut input = Cursor::new(&wire[..count]);
                while input.position() < count as u64 {
                    assert!(server.read_tls(&mut input).unwrap() > 0);
                    server.process_new_packets().unwrap();
                }
                let mut received = [0; 16_384];
                server.reader().read_exact(&mut received).unwrap();
                assert_eq!(received, message);
                if let rustls::Connection::Server(server) = server {
                    assert_eq!(server.server_name(), Some("localhost"));
                }
            }
        }
        for (trust, name, expected_name) in [
            (&roots, "wrong.example", true),
            (&other, "localhost", false),
        ] {
            let (_, clock) = clock();
            let config = ClientConfig::new(trust, clock, TlsProtocol::Smtp).unwrap();
            let (mut client, mut server) = pair(&config, server(&identity, version, &[]), name);
            let error = crate::tls_smoke::drive(&mut client, &mut server).unwrap_err();
            match error.downcast_ref::<rustls::Error>().unwrap() {
                rustls::Error::InvalidCertificate(
                    rustls::CertificateError::NotValidForNameContext { .. },
                ) if expected_name => {}
                rustls::Error::InvalidCertificate(rustls::CertificateError::UnknownIssuer)
                    if !expected_name => {}
                error => panic!("unexpected verification refusal {error:?}"),
            }
        }
        let (_, clock) = clock();
        let config = ClientConfig::new(&roots, clock, TlsProtocol::Http1).unwrap();
        let (mut client, mut server) =
            pair(&config, server(&identity, version, &[b"h2"]), "localhost");
        let error = crate::tls_smoke::drive(&mut client, &mut server).unwrap_err();
        assert!(matches!(
            error.downcast_ref::<rustls::Error>(),
            Some(rustls::Error::NoApplicationProtocol)
        ));
    }
}
#[test]
fn client_configuration_clock_errors_reach_handshake_and_open_tickets() {
    let (identity, roots) = fixture();
    let (value, clock) = clock();
    value.store(0, Ordering::SeqCst);
    assert!(matches!(
        ClientConfig::new(&roots, clock.clone(), TlsProtocol::Smtp),
        Err(TlsError::Clock)
    ));
    value.store(NOW, Ordering::SeqCst);
    let config = ClientConfig::new(&roots, clock.clone(), TlsProtocol::Smtp).unwrap();
    for (seconds, expired) in [(2_051_222_401, true), (1_735_689_599, false)] {
        value.store(seconds, Ordering::SeqCst);
        let (mut client, mut server) = pair(
            &config,
            server(&identity, &rustls::version::TLS13, &[]),
            "localhost",
        );
        let error = crate::tls_smoke::drive(&mut client, &mut server).unwrap_err();
        assert_eq!(
            matches!(
                error.downcast_ref::<rustls::Error>(),
                Some(rustls::Error::InvalidCertificate(
                    rustls::CertificateError::ExpiredContext { .. }
                ))
            ),
            expired
        );
        if !expired {
            assert!(matches!(
                error.downcast_ref::<rustls::Error>(),
                Some(rustls::Error::InvalidCertificate(
                    rustls::CertificateError::NotValidYetContext { .. }
                ))
            ));
        }
    }
    value.store(0, Ordering::SeqCst);
    for version in [&rustls::version::TLS12, &rustls::version::TLS13] {
        let (mut client, mut server) = pair(&config, server(&identity, version, &[]), "localhost");
        let error = crate::tls_smoke::drive(&mut client, &mut server).unwrap_err();
        assert!(matches!(
            error.downcast_ref::<rustls::Error>(),
            Some(rustls::Error::FailedToGetCurrentTime)
        ));
    }
    value.store(NOW, Ordering::SeqCst);
    let (mut client, mut server) = pair(
        &config,
        server(&identity, &rustls::version::TLS13, &[]),
        "localhost",
    );
    let transfer = |from: &mut rustls::Connection, to: &mut rustls::Connection| {
        let mut bytes = [0; 32 * 1024];
        let length = from
            .write_tls(&mut Cursor::new(bytes.as_mut_slice()))
            .unwrap();
        assert!(length > 0 && !from.wants_write());
        assert_eq!(
            to.read_tls(&mut Cursor::new(&bytes[..length])).unwrap(),
            length
        );
        to.process_new_packets()
    };
    let mut completed = false;
    for _ in 0..64 {
        transfer(&mut client, &mut server).unwrap();
        if !client.is_handshaking() && !server.is_handshaking() {
            completed = true;
            break;
        }
        transfer(&mut server, &mut client).unwrap();
    }
    assert!(completed && server.wants_write());
    value.store(0, Ordering::SeqCst);
    assert!(matches!(
        transfer(&mut server, &mut client),
        Err(rustls::Error::FailedToGetCurrentTime)
    ));
    value.store(u64::MAX, Ordering::SeqCst);
    assert!(matches!(
        ClientConfig::new(&roots, clock.clone(), TlsProtocol::Smtp),
        Err(TlsError::Crypto)
    ));
    value.store(NOW, Ordering::SeqCst);
    assert_eq!(config.clock.now(), Err(TlsError::Crypto));
    assert!(config.native.time_provider.current_time().is_none());
}
#[test]
fn client_verifier_checks_chain_bounds_before_path_work() {
    let roots = TrustStore::public_roots().unwrap();
    let verifier = BoundedVerifier(
        WebPkiServerVerifier::builder_with_provider(
            Arc::new(roots.roots.clone()),
            Arc::new(crate::tls_policy::provider().unwrap()),
        )
        .build()
        .unwrap(),
    );
    let name = "localhost".try_into().unwrap();
    let time = UnixTime::since_unix_epoch(std::time::Duration::from_secs(NOW));
    for (leaf_len, count, intermediate_len, allowed) in [
        (16_384, 0, 0, true),
        (16_385, 0, 0, false),
        (1, 7, 1, true),
        (1, 8, 1, false),
        (1, 1, 16_385, false),
        (16_384, 3, 16_384, true),
        (1, 4, 16_384, false),
    ] {
        let leaf = CertificateDer::from(vec![0; leaf_len]);
        let intermediates = vec![CertificateDer::from(vec![0; intermediate_len]); count];
        assert_eq!(chain_bounds(&leaf, &intermediates).is_ok(), allowed);
        let error = verifier
            .verify_server_cert(&leaf, &intermediates, &name, &[], time)
            .unwrap_err();
        if allowed {
            assert!(matches!(error, rustls::Error::InvalidCertificate(_)));
        } else {
            let rustls::Error::Other(rustls::OtherError(error)) = error else {
                panic!("unexpected bound failure")
            };
            assert_eq!(error.downcast_ref::<TlsError>(), Some(&TlsError::Capacity));
        }
    }
}

#[test]
fn tls12_session_save_can_ignore_transient_clock_failure() {
    use std::sync::atomic::AtomicUsize;
    struct FailSave {
        calls: Arc<AtomicUsize>,
        failures: Arc<AtomicUsize>,
        panic: bool,
    }
    impl UtcClock for FailSave {
        fn now(&self) -> Option<u64> {
            let call = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
            // Constructor, certificate verification, then post-Finished save.
            if call == 3 {
                self.failures.fetch_add(1, Ordering::SeqCst);
                assert!(!self.panic, "synthetic TLS 1.2 session-save clock panic");
                None
            } else {
                Some(NOW)
            }
        }
    }
    let (identity, roots) = fixture();
    for panic in [false, true] {
        let calls = Arc::new(AtomicUsize::new(0));
        let failures = Arc::new(AtomicUsize::new(0));
        let clock = Arc::new(ClockHandle::new(FailSave {
            calls: calls.clone(),
            failures: failures.clone(),
            panic,
        }));
        let config = ClientConfig::new(&roots, clock.clone(), TlsProtocol::Smtp).unwrap();
        let (mut client, mut server) = pair(
            &config,
            server(&identity, &rustls::version::TLS12, &[]),
            "localhost",
        );
        crate::tls_smoke::drive(&mut client, &mut server).unwrap();
        assert!(!client.is_handshaking() && !server.is_handshaking());
        assert_eq!(client.handshake_kind(), Some(rustls::HandshakeKind::Full));
        assert_eq!(calls.load(Ordering::SeqCst), 3);
        assert_eq!(failures.load(Ordering::SeqCst), 1);
        // A later poll misses the ordinary None. The future session facade
        // must retain a per-connection failure observation around backend work.
        assert_eq!(
            clock.now(),
            if panic {
                Err(TlsError::Crypto)
            } else {
                Ok(NOW)
            }
        );
    }
}

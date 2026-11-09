//! Local backend qualification only; no production TLS adapter.
//! Generated keys, certificates and fixed time keep this fixture offline.
use crate::certificate_fixtures::TestKey;
use std::io::{Cursor, Read, Write};
use std::sync::Arc;
use std::time::Duration;

const VALID_TIME: u64 = 1_800_000_000;
const EXPIRED_TIME: u64 = 2_100_000_000;
const BUFFER_BYTES: usize = 32 * 1024;
const WIRE_BYTES: usize = 256 * 1024;
const TURNS: usize = 64;

type Error = Box<dyn std::error::Error>;
type Result<T> = std::result::Result<T, Error>;
use crate::certificate_fixtures::{certificate, certificate_with};
#[derive(Debug)]
struct Time(u64);
impl rustls::time_provider::TimeProvider for Time {
    fn current_time(&self) -> Option<rustls::pki_types::UnixTime> {
        Some(rustls::pki_types::UnixTime::since_unix_epoch(
            Duration::from_secs(self.0),
        ))
    }
}
#[derive(Clone, Copy)]
enum Trust {
    Correct,
    Empty,
    WrongKey,
}

fn pair(
    version: &'static rustls::SupportedProtocolVersion,
    hostname: &'static str,
    trust: Trust,
    now: u64,
) -> Result<(rustls::Connection, rustls::Connection)> {
    pair_with_providers(
        version,
        hostname,
        trust,
        now,
        crate::tls_policy::provider()?,
        crate::tls_policy::provider()?,
    )
}

fn pair_with_providers(
    version: &'static rustls::SupportedProtocolVersion,
    hostname: &'static str,
    trust: Trust,
    now: u64,
    client_provider: rustls::crypto::CryptoProvider,
    server_provider: rustls::crypto::CryptoProvider,
) -> Result<(rustls::Connection, rustls::Connection)> {
    let root_bytes = TestKey::generate_pkcs8()?;
    let root = TestKey::from_pkcs8(root_bytes.as_ref())?;
    let leaf_bytes = TestKey::generate_pkcs8()?;
    let leaf = TestKey::from_pkcs8(leaf_bytes.as_ref())?;
    // Same issuer name but a different trusted public key forces verification.
    let root_cert = match trust {
        Trust::WrongKey => certificate(&leaf, &leaf, true)?,
        _ => certificate(&root, &root, true)?,
    };
    let leaf_cert = certificate(&leaf, &root, false)?;
    let provider = Arc::new(server_provider);
    let server = rustls::ServerConfig::builder_with_details(provider.clone(), Arc::new(Time(now)))
        .with_protocol_versions(&[version])?
        .with_no_client_auth()
        .with_single_cert(
            vec![leaf_cert.into()],
            rustls::pki_types::PrivatePkcs8KeyDer::from(leaf_bytes.clone()).into(),
        )?;
    let mut roots = rustls::RootCertStore::empty();
    if !matches!(trust, Trust::Empty) {
        roots.add(root_cert.into())?;
    }
    let client =
        rustls::ClientConfig::builder_with_details(Arc::new(client_provider), Arc::new(Time(now)))
            .with_protocol_versions(&[version])?
            .with_root_certificates(roots)
            .with_no_client_auth();
    let mut client = rustls::Connection::Client(rustls::ClientConnection::new(
        Arc::new(client),
        hostname.try_into()?,
    )?);
    let mut server = rustls::Connection::Server(rustls::ServerConnection::new(Arc::new(server))?);
    client.set_buffer_limit(Some(BUFFER_BYTES));
    server.set_buffer_limit(Some(BUFFER_BYTES));
    Ok((client, server))
}
fn transfer(
    from: &mut rustls::Connection,
    to: &mut rustls::Connection,
    bytes: &mut [u8],
    total: &mut usize,
) -> Result<usize> {
    let mut out = Cursor::new(bytes);
    let written = from.write_tls(&mut out)?;
    *total = total.checked_add(written).ok_or("overflow")?;
    if *total > WIRE_BYTES {
        return Err("wire budget".into());
    }
    let used = out.get_ref().get(..written).ok_or("write bound")?;
    let mut input = Cursor::new(used);
    while input.position() < (written as u64) {
        if to.read_tls(&mut input)? == 0 {
            return Err("no read progress".into());
        }
        to.process_new_packets()?;
    }
    Ok(written)
}
// Bound fixture work; this is not a bound on provider allocation or CPU time.
pub(super) fn drive(c: &mut rustls::Connection, s: &mut rustls::Connection) -> Result<()> {
    let mut bytes = [0; BUFFER_BYTES];
    let mut total = 0;
    for _ in 0..TURNS {
        let progress =
            transfer(c, s, &mut bytes, &mut total)? + transfer(s, c, &mut bytes, &mut total)?;
        if !c.is_handshaking() && !s.is_handshaking() && !c.wants_write() && !s.wants_write() {
            return Ok(());
        }
        if progress == 0 {
            return Err("no handshake progress".into());
        }
    }
    Err("turn budget".into())
}

fn accepted(version: &'static rustls::SupportedProtocolVersion) -> Result<()> {
    assert!(rustls::crypto::CryptoProvider::get_default().is_none());
    let (mut client, mut server) = pair(version, "localhost", Trust::Correct, VALID_TIME)?;
    drive(&mut client, &mut server)?;
    assert_eq!(client.protocol_version(), Some(version.version));
    assert_eq!(server.protocol_version(), Some(version.version));
    let cipher = if version.version == rustls::ProtocolVersion::TLSv1_2 {
        rustls::CipherSuite::TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384
    } else {
        rustls::CipherSuite::TLS13_AES_256_GCM_SHA384
    };
    for peer in [&client, &server] {
        assert_eq!(
            peer.negotiated_cipher_suite().map(|s| s.suite()),
            Some(cipher)
        );
        assert_eq!(
            peer.negotiated_key_exchange_group().map(|g| g.name()),
            Some(rustls::NamedGroup::X25519)
        );
    }
    let data = b"Subject: local TLS fixture\r\n\r\nhello\0\xff";
    client.writer().write_all(data)?;
    drive(&mut client, &mut server)?;
    let mut received = [0; 128];
    let slot = received.get_mut(..data.len()).ok_or("plaintext capacity")?;
    server.reader().read_exact(slot)?;
    assert_eq!(slot, data);
    server.writer().write_all(data)?;
    drive(&mut client, &mut server)?;
    client.reader().read_exact(slot)?;
    assert_eq!(slot, data);
    client.send_close_notify();
    drive(&mut client, &mut server)?;
    assert_eq!(server.reader().read(slot)?, 0);
    server.send_close_notify();
    drive(&mut client, &mut server)?;
    assert_eq!(client.reader().read(slot)?, 0);
    assert!(rustls::crypto::CryptoProvider::get_default().is_none());
    Ok(())
}
#[test]
fn tls12_local_round_trip() -> Result<()> {
    accepted(&rustls::version::TLS12)
}
#[test]
fn tls13_local_round_trip() -> Result<()> {
    accepted(&rustls::version::TLS13)
}

#[test]
fn rejects_wrong_server_name() -> Result<()> {
    let (mut client, mut server) = pair(
        &rustls::version::TLS13,
        "wrong.example",
        Trust::Correct,
        VALID_TIME,
    )?;
    let error = drive(&mut client, &mut server)
        .err()
        .ok_or("wrong name accepted")?;
    assert!(
        matches!(
            error.downcast_ref::<rustls::Error>(),
            Some(rustls::Error::InvalidCertificate(
                rustls::CertificateError::NotValidForNameContext { .. }
            ))
        ),
        "{error:?}"
    );
    assert!(rustls::crypto::CryptoProvider::get_default().is_none());
    Ok(())
}
#[test]
fn rejects_untrusted_chain() -> Result<()> {
    let (mut client, mut server) = pair(
        &rustls::version::TLS13,
        "localhost",
        Trust::Empty,
        VALID_TIME,
    )?;
    let error = drive(&mut client, &mut server)
        .err()
        .ok_or("untrusted chain accepted")?;
    assert!(
        matches!(
            error.downcast_ref::<rustls::Error>(),
            Some(rustls::Error::InvalidCertificate(
                rustls::CertificateError::UnknownIssuer
            ))
        ),
        "{error:?}"
    );
    assert!(rustls::crypto::CryptoProvider::get_default().is_none());
    Ok(())
}
#[test]
fn rejects_expired_certificate() -> Result<()> {
    let (mut client, mut server) = pair(
        &rustls::version::TLS13,
        "localhost",
        Trust::Correct,
        EXPIRED_TIME,
    )?;
    let error = drive(&mut client, &mut server)
        .err()
        .ok_or("expired certificate accepted")?;
    assert!(
        matches!(
            error.downcast_ref::<rustls::Error>(),
            Some(rustls::Error::InvalidCertificate(
                rustls::CertificateError::ExpiredContext { .. }
            ))
        ),
        "{error:?}"
    );
    assert!(rustls::crypto::CryptoProvider::get_default().is_none());
    Ok(())
}
#[test]
fn rejects_malformed_record() -> Result<()> {
    let (_, mut server) = pair(
        &rustls::version::TLS13,
        "localhost",
        Trust::Correct,
        VALID_TIME,
    )?;
    let bytes = [0xff, 3, 3, 0, 1, 0];
    assert_eq!(server.read_tls(&mut Cursor::new(bytes))?, bytes.len());
    let error = server.process_new_packets();
    assert!(
        matches!(
            error,
            Err(rustls::Error::InvalidMessage(
                rustls::InvalidMessage::InvalidContentType
            ))
        ),
        "{error:?}"
    );
    assert!(rustls::crypto::CryptoProvider::get_default().is_none());
    Ok(())
}

#[test]
fn rejects_bad_certificate_signature() -> Result<()> {
    let (mut client, mut server) = pair(
        &rustls::version::TLS13,
        "localhost",
        Trust::WrongKey,
        VALID_TIME,
    )?;
    let error = drive(&mut client, &mut server)
        .err()
        .ok_or("wrong signing key accepted")?;
    assert!(
        matches!(
            error.downcast_ref::<rustls::Error>(),
            Some(rustls::Error::InvalidCertificate(
                rustls::CertificateError::BadSignature
            ))
        ),
        "{error:?}"
    );
    assert!(rustls::crypto::CryptoProvider::get_default().is_none());
    Ok(())
}

#[test]
fn rejects_tampered_ciphertext() -> Result<()> {
    let (mut client, mut server) = pair(
        &rustls::version::TLS13,
        "localhost",
        Trust::Correct,
        VALID_TIME,
    )?;
    drive(&mut client, &mut server)?;
    client.writer().write_all(b"authenticated data")?;
    let mut bytes = [0; BUFFER_BYTES];
    let written = client.write_tls(&mut Cursor::new(bytes.as_mut_slice()))?;
    let record = bytes.get_mut(..written).ok_or("record capacity")?;
    *record.last_mut().ok_or("no encrypted record")? ^= 1;
    assert_eq!(server.read_tls(&mut Cursor::new(record))?, written);
    let error = server.process_new_packets();
    assert!(
        matches!(error, Err(rustls::Error::DecryptError)),
        "{error:?}"
    );
    assert!(rustls::crypto::CryptoProvider::get_default().is_none());
    Ok(())
}

#[derive(Clone, Copy)]
enum ClientCase {
    Valid,
    Missing,
    UnknownIssuer,
    Expired,
    ServerUsage,
    BadSignature,
    WrongKey,
}

struct MutualConfigs {
    client: Arc<rustls::ClientConfig>,
    server: Arc<rustls::ServerConfig>,
    leaf: Vec<u8>,
}

fn mutual_configs(
    version: &'static rustls::SupportedProtocolVersion,
    case: ClientCase,
) -> Result<MutualConfigs> {
    assert!(rustls::crypto::CryptoProvider::get_default().is_none());
    let root_bytes = TestKey::generate_pkcs8()?;
    let root = TestKey::from_pkcs8(root_bytes.as_ref())?;
    let server_bytes = TestKey::generate_pkcs8()?;
    let server_key = TestKey::from_pkcs8(server_bytes.as_ref())?;
    let client_bytes = TestKey::generate_pkcs8()?;
    let client_key = TestKey::from_pkcs8(client_bytes.as_ref())?;
    let root_cert = certificate(&root, &root, true)?;
    let server_cert = certificate(&server_key, &root, false)?;
    let client_cert = certificate_with(
        &client_key,
        if matches!(case, ClientCase::BadSignature) {
            &server_key
        } else {
            &root
        },
        false,
        if matches!(case, ClientCase::ServerUsage) {
            1
        } else {
            2
        },
        if matches!(case, ClientCase::UnknownIssuer) {
            b"unconfigured root"
        } else {
            b"td-test-root"
        },
        if matches!(case, ClientCase::Expired) {
            b"260101000000Z"
        } else {
            b"350101000000Z"
        },
        3,
    )?;
    let mut roots = rustls::RootCertStore::empty();
    roots.add(root_cert.into())?;
    let provider = Arc::new(crate::tls_policy::provider()?);
    let verifier = rustls::server::WebPkiClientVerifier::builder_with_provider(
        Arc::new(roots.clone()),
        provider.clone(),
    )
    .build()?;
    let mut server =
        rustls::ServerConfig::builder_with_details(provider.clone(), Arc::new(Time(VALID_TIME)))
            .with_protocol_versions(&[version])?
            .with_client_cert_verifier(verifier)
            .with_single_cert(
                vec![server_cert.into()],
                rustls::pki_types::PrivatePkcs8KeyDer::from(server_bytes.clone()).into(),
            )?;
    server.session_storage = Arc::new(rustls::server::NoServerSessionStorage {});
    server.send_tls13_tickets = 0;
    server.max_early_data_size = 0;
    let builder = rustls::ClientConfig::builder_with_details(provider, Arc::new(Time(VALID_TIME)))
        .with_protocol_versions(&[version])?
        .with_root_certificates(roots);
    let mut client = if matches!(case, ClientCase::Missing) {
        builder.with_no_client_auth()
    } else {
        let key: &[u8] = if matches!(case, ClientCase::WrongKey) {
            root_bytes.as_ref()
        } else {
            client_bytes.as_ref()
        };
        builder.with_client_auth_cert(
            vec![client_cert.clone().into()],
            rustls::pki_types::PrivatePkcs8KeyDer::from(key.to_vec()).into(),
        )?
    };
    client.resumption = rustls::client::Resumption::disabled();
    client.enable_early_data = false;
    Ok(MutualConfigs {
        client: Arc::new(client),
        server: Arc::new(server),
        leaf: client_cert,
    })
}

fn mutual_pair(
    client: Arc<rustls::ClientConfig>,
    server: Arc<rustls::ServerConfig>,
    hostname: &'static str,
) -> Result<(rustls::Connection, rustls::Connection)> {
    let mut client =
        rustls::Connection::Client(rustls::ClientConnection::new(client, hostname.try_into()?)?);
    let mut server = rustls::Connection::Server(rustls::ServerConnection::new(server)?);
    client.set_buffer_limit(Some(BUFFER_BYTES));
    server.set_buffer_limit(Some(BUFFER_BYTES));
    Ok((client, server))
}

fn mutual_accepted(version: &'static rustls::SupportedProtocolVersion) -> Result<()> {
    let MutualConfigs {
        client,
        server,
        leaf,
    } = mutual_configs(version, ClientCase::Valid)?;
    let mut remote_client = (*client).clone();
    remote_client.resumption = rustls::client::Resumption::default();
    let mut remote_server = (*server).clone();
    remote_server.session_storage = rustls::server::ServerSessionMemoryCache::new(32);
    remote_server.send_tls13_tickets = 2;
    // Each side's refusal must hold independently of its peer's preference.
    for (client, server) in [
        (client.clone(), server.clone()),
        (Arc::new(remote_client), server.clone()),
        (client.clone(), Arc::new(remote_server)),
    ] {
        mutual_round_trips(version, client, server, &leaf)?;
    }
    let (mut client, mut server) = mutual_pair(client, server, "wrong.example")?;
    let error = drive(&mut client, &mut server)
        .err()
        .ok_or("mutual client accepted wrong server name")?;
    assert!(
        matches!(
            error.downcast_ref::<rustls::Error>(),
            Some(rustls::Error::InvalidCertificate(
                rustls::CertificateError::NotValidForNameContext { .. }
            ))
        ),
        "{error:?}"
    );
    assert!(server.peer_certificates().is_none());
    assert!(rustls::crypto::CryptoProvider::get_default().is_none());
    Ok(())
}

fn mutual_round_trips(
    version: &'static rustls::SupportedProtocolVersion,
    client: Arc<rustls::ClientConfig>,
    server: Arc<rustls::ServerConfig>,
    leaf: &[u8],
) -> Result<()> {
    for _ in 0..2 {
        let (mut client, mut server) = mutual_pair(client.clone(), server.clone(), "localhost")?;
        assert!(client.is_handshaking() && server.is_handshaking());
        drive(&mut client, &mut server)?;
        for peer in [&client, &server] {
            assert!(!peer.is_handshaking());
            assert_eq!(peer.protocol_version(), Some(version.version));
            assert_eq!(peer.handshake_kind(), Some(rustls::HandshakeKind::Full));
        }
        let chain = server
            .peer_certificates()
            .ok_or("missing authenticated client chain")?;
        assert_eq!(chain.len(), 1);
        assert_eq!(chain.first().ok_or("empty client chain")?.as_ref(), leaf);
        let payload = b"gateway fixture after verified handshake";
        client.writer().write_all(payload)?;
        drive(&mut client, &mut server)?;
        let mut buffer = [0; 128];
        let output = buffer.get_mut(..payload.len()).ok_or("plaintext bound")?;
        server.reader().read_exact(output)?;
        assert_eq!(output, payload);
        assert!(rustls::crypto::CryptoProvider::get_default().is_none());
    }
    Ok(())
}

#[test]
fn tls12_mutual_authentication() -> Result<()> {
    mutual_accepted(&rustls::version::TLS12)
}
#[test]
fn tls13_mutual_authentication() -> Result<()> {
    mutual_accepted(&rustls::version::TLS13)
}

fn mutual_refused(case: ClientCase, expected: fn(&rustls::Error) -> bool) -> Result<()> {
    for version in [&rustls::version::TLS12, &rustls::version::TLS13] {
        let MutualConfigs { client, server, .. } = mutual_configs(version, case)?;
        let (mut client, mut server) = mutual_pair(client, server, "localhost")?;
        let error = drive(&mut client, &mut server)
            .err()
            .ok_or("unauthenticated client accepted")?;
        let error = error
            .downcast_ref::<rustls::Error>()
            .ok_or("non-TLS fixture failure")?;
        assert!(expected(error), "{version:?}: {error:?}");
        assert!(server.peer_certificates().is_none());
        assert!(rustls::crypto::CryptoProvider::get_default().is_none());
    }
    Ok(())
}

#[test]
fn mutual_authentication_requires_certificate() -> Result<()> {
    mutual_refused(ClientCase::Missing, |e| {
        matches!(e, rustls::Error::NoCertificatesPresented)
    })
}
#[test]
fn mutual_authentication_refuses_unknown_issuer() -> Result<()> {
    mutual_refused(ClientCase::UnknownIssuer, |e| {
        matches!(
            e,
            rustls::Error::InvalidCertificate(rustls::CertificateError::UnknownIssuer)
        )
    })
}
#[test]
fn mutual_authentication_refuses_expired_certificate() -> Result<()> {
    mutual_refused(ClientCase::Expired, |e| {
        matches!(
            e,
            rustls::Error::InvalidCertificate(rustls::CertificateError::ExpiredContext {
                time, not_after
            }) if time.as_secs() == VALID_TIME && not_after.as_secs() == 1_767_225_600
        )
    })
}
#[test]
fn mutual_authentication_refuses_server_only_usage() -> Result<()> {
    mutual_refused(ClientCase::ServerUsage, |e| {
        matches!(
            e,
            rustls::Error::InvalidCertificate(
                rustls::CertificateError::InvalidPurposeContext { .. }
            )
        )
    })
}
#[test]
fn mutual_authentication_refuses_bad_signature() -> Result<()> {
    mutual_refused(ClientCase::BadSignature, |e| {
        matches!(
            e,
            rustls::Error::InvalidCertificate(rustls::CertificateError::BadSignature)
        )
    })
}
#[test]
fn mutual_authentication_refuses_mismatched_key_before_connect() -> Result<()> {
    for version in [&rustls::version::TLS12, &rustls::version::TLS13] {
        let error = mutual_configs(version, ClientCase::WrongKey)
            .err()
            .ok_or("mismatched client key accepted")?;
        assert!(
            matches!(
                error.downcast_ref::<rustls::Error>(),
                Some(rustls::Error::InconsistentKeys(
                    rustls::InconsistentKeys::KeyMismatch
                ))
            ),
            "{error:?}"
        );
        assert!(rustls::crypto::CryptoProvider::get_default().is_none());
    }
    Ok(())
}

#[test]
fn explicit_policy_negotiates_classical_groups_and_suites() -> Result<()> {
    let base = crate::tls_policy::provider()?;
    for group in &base.kx_groups {
        for version in [&rustls::version::TLS12, &rustls::version::TLS13] {
            let mut client = crate::tls_policy::provider()?;
            client.kx_groups = vec![*group];
            let (mut client, mut server) = pair_with_providers(
                version,
                "localhost",
                Trust::Correct,
                VALID_TIME,
                client,
                crate::tls_policy::provider()?,
            )?;
            drive(&mut client, &mut server)?;
            for peer in [&client, &server] {
                assert_eq!(
                    peer.negotiated_key_exchange_group().map(|g| g.name()),
                    Some(group.name())
                );
                assert_eq!(peer.protocol_version(), Some(version.version));
            }
        }
    }
    for suite in base.cipher_suites.iter().take(6) {
        let mut client = crate::tls_policy::provider()?;
        client.cipher_suites = vec![*suite];
        let (mut client, mut server) = pair_with_providers(
            suite.version(),
            "localhost",
            Trust::Correct,
            VALID_TIME,
            client,
            crate::tls_policy::provider()?,
        )?;
        drive(&mut client, &mut server)?;
        for peer in [&client, &server] {
            assert_eq!(
                peer.negotiated_cipher_suite().map(|c| c.suite()),
                Some(suite.suite())
            );
        }
    }
    assert!(rustls::crypto::CryptoProvider::get_default().is_none());
    Ok(())
}

/// A group that only names X25519MLKEM768, offering a share of the hybrid's
/// length. A classical peer refuses it before any share is completed.
#[derive(Debug)]
struct NamedHybrid;
struct NamedShare(Vec<u8>);
impl rustls::crypto::SupportedKxGroup for NamedHybrid {
    fn start(
        &self,
    ) -> std::result::Result<Box<dyn rustls::crypto::ActiveKeyExchange>, rustls::Error> {
        Ok(Box::new(NamedShare(vec![0x42; 1216])))
    }
    fn name(&self) -> rustls::NamedGroup {
        rustls::NamedGroup::X25519MLKEM768
    }
}
impl rustls::crypto::ActiveKeyExchange for NamedShare {
    fn complete(
        self: Box<Self>,
        _peer: &[u8],
    ) -> std::result::Result<rustls::crypto::SharedSecret, rustls::Error> {
        Err(rustls::Error::General(
            "a named-only share never completes".into(),
        ))
    }
    fn pub_key(&self) -> &[u8] {
        &self.0
    }
    fn group(&self) -> rustls::NamedGroup {
        rustls::NamedGroup::X25519MLKEM768
    }
}

/// A server key that signs only with ML-DSA-44, and then only refuses.
#[derive(Debug)]
struct MlDsaOnly;
#[derive(Debug)]
struct RefusingSigner;
impl rustls::sign::SigningKey for MlDsaOnly {
    fn choose_scheme(
        &self,
        offered: &[rustls::SignatureScheme],
    ) -> Option<Box<dyn rustls::sign::Signer>> {
        offered
            .contains(&rustls::SignatureScheme::ML_DSA_44)
            .then(|| Box::new(RefusingSigner) as Box<dyn rustls::sign::Signer>)
    }
    fn algorithm(&self) -> rustls::SignatureAlgorithm {
        rustls::SignatureAlgorithm::Unknown(0)
    }
}
impl rustls::sign::Signer for RefusingSigner {
    fn sign(&self, _message: &[u8]) -> std::result::Result<Vec<u8>, rustls::Error> {
        Err(rustls::Error::General(
            "refusing ML-DSA fixture signer".into(),
        ))
    }
    fn scheme(&self) -> rustls::SignatureScheme {
        rustls::SignatureScheme::ML_DSA_44
    }
}

#[derive(Debug)]
struct Resolves(Arc<rustls::sign::CertifiedKey>);
impl rustls::server::ResolvesServerCert for Resolves {
    fn resolve(
        &self,
        _hello: rustls::server::ClientHello<'_>,
    ) -> Option<Arc<rustls::sign::CertifiedKey>> {
        Some(self.0.clone())
    }
}

#[test]
fn explicit_policy_refuses_excluded_peer_algorithms() -> Result<()> {
    for restricted_server in [false, true] {
        let remote = rustls::crypto::CryptoProvider {
            kx_groups: vec![&NamedHybrid],
            ..crate::tls_policy::provider()?
        };
        let local = crate::tls_policy::provider()?;
        let (client, server) = if restricted_server {
            (remote, local)
        } else {
            (local, remote)
        };
        let (mut client, mut server) = pair_with_providers(
            &rustls::version::TLS13,
            "localhost",
            Trust::Correct,
            VALID_TIME,
            client,
            server,
        )?;
        let error = drive(&mut client, &mut server)
            .err()
            .ok_or("excluded hybrid group accepted")?;
        assert!(
            matches!(
                error.downcast_ref::<rustls::Error>(),
                Some(rustls::Error::PeerIncompatible(
                    rustls::PeerIncompatible::NoKxGroupsInCommon
                ))
            ),
            "{error:?}"
        );
    }
    // The policy client offers no ML-DSA scheme, so a server holding only an
    // ML-DSA key finds none to sign with. The certificate is AWS-LC's
    // ML-DSA-44 root, which the server never reaches sending.
    let line = include_str!("fixtures/ml_dsa_path.txt");
    let root = line.split(' ').nth(2).ok_or("malformed ML-DSA fixture")?;
    let certified = Arc::new(rustls::sign::CertifiedKey::new(
        vec![crate::certificate_fixtures::unhex(root).into()],
        Arc::new(MlDsaOnly),
    ));
    let server = Arc::new(
        rustls::ServerConfig::builder_with_details(
            Arc::new(crate::tls_policy::provider()?),
            Arc::new(Time(VALID_TIME)),
        )
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_no_client_auth()
        .with_cert_resolver(Arc::new(Resolves(certified))),
    );
    let root_key = TestKey::generate();
    let mut roots = rustls::RootCertStore::empty();
    roots.add(certificate(&root_key, &root_key, true)?.into())?;
    let client = rustls::ClientConfig::builder_with_details(
        Arc::new(crate::tls_policy::provider()?),
        Arc::new(Time(VALID_TIME)),
    )
    .with_protocol_versions(&[&rustls::version::TLS13])?
    .with_root_certificates(roots)
    .with_no_client_auth();
    let mut client = rustls::Connection::Client(rustls::ClientConnection::new(
        Arc::new(client),
        "localhost".try_into()?,
    )?);
    let mut server = rustls::Connection::Server(rustls::ServerConnection::new(server)?);
    client.set_buffer_limit(Some(BUFFER_BYTES));
    server.set_buffer_limit(Some(BUFFER_BYTES));
    let error = drive(&mut client, &mut server)
        .err()
        .ok_or("excluded signer accepted by policy client")?;
    assert!(
        matches!(
            error.downcast_ref::<rustls::Error>(),
            Some(rustls::Error::PeerIncompatible(
                rustls::PeerIncompatible::NoSignatureSchemesInCommon
            ))
        ),
        "{error:?}"
    );
    assert!(rustls::crypto::CryptoProvider::get_default().is_none());
    Ok(())
}

//! Local backend qualification only; no production TLS adapter.
//! Generated keys, certificates and fixed time keep this fixture offline.
use aws_lc_rs::signature::{EcdsaKeyPair, KeyPair, ECDSA_P256_SHA256_ASN1_SIGNING};
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
fn der(tag: u8, bytes: &[u8]) -> Vec<u8> {
    let mut out = vec![tag];
    if bytes.len() < 128 {
        out.push(bytes.len() as u8)
    } else {
        let length = bytes.len().to_be_bytes();
        let length = length
            .iter()
            .skip_while(|b| **b == 0)
            .copied()
            .collect::<Vec<_>>();
        out.push(0x80 | length.len() as u8);
        out.extend_from_slice(&length);
    }
    out.extend_from_slice(bytes);
    out
}
fn seq(parts: &[Vec<u8>]) -> Vec<u8> {
    der(0x30, &parts.concat())
}
fn oid(bytes: &[u8]) -> Vec<u8> {
    der(6, bytes)
}
fn name(cn: &[u8]) -> Vec<u8> {
    seq(&[der(0x31, &seq(&[oid(&[0x55, 4, 3]), der(0x0c, cn)]))])
}
fn signature_algorithm() -> Vec<u8> {
    seq(&[oid(&[0x2a, 0x86, 0x48, 0xce, 0x3d, 4, 3, 2])])
}
fn extension(last: u8, critical: bool, value: Vec<u8>) -> Vec<u8> {
    let mut parts = vec![oid(&[0x55, 0x1d, last])];
    if critical {
        parts.push(der(1, &[0xff]))
    };
    parts.push(der(4, &value));
    seq(&parts)
}
// Fixture DER only: P-256 CA and localhost leaf, valid 2025-01-01 to 2035-01-01.
fn certificate(key: &EcdsaKeyPair, signer: &EcdsaKeyPair, ca: bool) -> Result<Vec<u8>> {
    let mut public = vec![0];
    public.extend_from_slice(key.public_key().as_ref());
    let spki = seq(&[
        seq(&[
            oid(&[0x2a, 0x86, 0x48, 0xce, 0x3d, 2, 1]),
            oid(&[0x2a, 0x86, 0x48, 0xce, 0x3d, 3, 1, 7]),
        ]),
        der(3, &public),
    ]);
    let mut extensions = vec![
        extension(
            0x13,
            true,
            if ca {
                seq(&[der(1, &[0xff])])
            } else {
                seq(&[])
            },
        ),
        extension(0x0f, true, der(3, if ca { &[1, 6] } else { &[7, 0x80] })),
    ];
    if !ca {
        extensions.push(extension(0x11, false, seq(&[der(0x82, b"localhost")])));
        extensions.push(extension(
            0x25,
            false,
            seq(&[oid(&[0x2b, 6, 1, 5, 5, 7, 3, 1])]),
        ));
    }
    let body = seq(&[
        der(0xa0, &der(2, &[2])),
        der(2, &[if ca { 1 } else { 2 }]),
        signature_algorithm(),
        name(b"td-test-root"),
        seq(&[der(0x17, b"250101000000Z"), der(0x17, b"350101000000Z")]),
        name(if ca { b"td-test-root" } else { b"localhost" }),
        spki,
        der(0xa3, &seq(&extensions)),
    ]);
    let signed = signer.sign(&aws_lc_rs::rand::SystemRandom::new(), &body)?;
    let mut bits = vec![0];
    bits.extend_from_slice(signed.as_ref());
    Ok(seq(&[body, signature_algorithm(), der(3, &bits)]))
}
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
    let rng = aws_lc_rs::rand::SystemRandom::new();
    let root_bytes = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, &rng)?;
    let root = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, root_bytes.as_ref())?;
    let leaf_bytes = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, &rng)?;
    let leaf = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, leaf_bytes.as_ref())?;
    // Same issuer name but a different trusted public key forces verification.
    let root_cert = match trust {
        Trust::WrongKey => certificate(&leaf, &leaf, true)?,
        _ => certificate(&root, &root, true)?,
    };
    let leaf_cert = certificate(&leaf, &root, false)?;
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let server = rustls::ServerConfig::builder_with_details(provider.clone(), Arc::new(Time(now)))
        .with_protocol_versions(&[version])?
        .with_no_client_auth()
        .with_single_cert(
            vec![leaf_cert.into()],
            rustls::pki_types::PrivatePkcs8KeyDer::from(leaf_bytes.as_ref().to_vec()).into(),
        )?;
    let mut roots = rustls::RootCertStore::empty();
    if !matches!(trust, Trust::Empty) {
        roots.add(root_cert.into())?;
    }
    let client = rustls::ClientConfig::builder_with_details(provider, Arc::new(Time(now)))
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
fn drive(c: &mut rustls::Connection, s: &mut rustls::Connection) -> Result<()> {
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

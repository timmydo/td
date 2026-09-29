#![allow(clippy::unwrap_used, clippy::indexing_slicing)]
use super::*;

#[test]
fn exact_tls_algorithm_inventory() {
    use rustls::{CipherSuite as C, NamedGroup as G};
    let provider = provider().unwrap();
    assert_eq!(
        provider
            .cipher_suites
            .iter()
            .map(|c| c.suite())
            .collect::<Vec<_>>(),
        [
            C::TLS13_AES_256_GCM_SHA384,
            C::TLS13_AES_128_GCM_SHA256,
            C::TLS13_CHACHA20_POLY1305_SHA256,
            C::TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384,
            C::TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256,
            C::TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256,
            C::TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384,
            C::TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256,
            C::TLS_ECDHE_RSA_WITH_CHACHA20_POLY1305_SHA256,
        ]
    );
    assert_eq!(
        provider
            .kx_groups
            .iter()
            .map(|g| g.name())
            .collect::<Vec<_>>(),
        [G::X25519, G::secp256r1, G::secp384r1,]
    );
    let algorithms = provider.signature_verification_algorithms;
    assert_eq!(algorithms.all.len(), 19);
    assert_eq!(
        algorithms.supported_schemes(),
        [
            Scheme::ECDSA_NISTP384_SHA384,
            Scheme::ECDSA_NISTP256_SHA256,
            Scheme::ECDSA_NISTP521_SHA512,
            Scheme::ED25519,
            Scheme::RSA_PSS_SHA512,
            Scheme::RSA_PSS_SHA384,
            Scheme::RSA_PSS_SHA256,
            Scheme::RSA_PKCS1_SHA512,
            Scheme::RSA_PKCS1_SHA384,
            Scheme::RSA_PKCS1_SHA256,
        ]
    );
    // The leading ECDSA entry is the only one TLS 1.3 uses for that scheme.
    for (index, expected) in [
        vec![4, 1, 7],
        vec![0, 3, 6],
        vec![8, 5, 2],
        vec![9],
        vec![12],
        vec![11],
        vec![10],
        vec![15],
        vec![14],
        vec![13],
    ]
    .iter()
    .enumerate()
    {
        let actual = algorithms.mapping[index].1;
        assert_eq!(actual.len(), expected.len());
        for (a, e) in actual.iter().zip(expected) {
            assert_eq!(
                a.public_key_alg_id(),
                algorithms.all[*e].public_key_alg_id()
            );
            assert_eq!(a.signature_alg_id(), algorithms.all[*e].signature_alg_id());
        }
    }
    assert!(CryptoProvider::get_default().is_none());
}

#[test]
fn handshake_mapping_drift_is_refused() {
    let native = backend::default_provider().signature_verification_algorithms;
    assert_eq!(
        select_mapping(native.all, native.mapping).unwrap().len(),
        10
    );
    for len in 0..native.mapping.len() {
        assert!(matches!(
            select_mapping(native.all, &native.mapping[..len]),
            Err(TlsError::Crypto)
        ));
    }
    let mut extra = native.mapping.to_vec();
    extra.push(native.mapping[0]);
    assert!(select_mapping(native.all, &extra).is_err());
    for index in 0..native.mapping.len() {
        let mut changed = native.mapping.to_vec();
        changed[index].0 = native.mapping[(index + 1) % native.mapping.len()].0;
        assert!(select_mapping(native.all, &changed).is_err());
        let mut changed = native.mapping.to_vec();
        changed.swap(index, (index + 1) % native.mapping.len());
        assert!(select_mapping(native.all, &changed).is_err());
        let mut changed = native.mapping.to_vec();
        changed[index].1 = native.mapping[(index + 1) % native.mapping.len()].1;
        assert!(select_mapping(native.all, &changed).is_err());
        changed[index].1 = &[];
        assert!(select_mapping(native.all, &changed).is_err());
    }
    // Same count and scheme, different internal ECDSA ordering.
    let original = native.mapping[0].1;
    let reversed: &'static [_] =
        Box::leak(vec![original[1], original[0], original[2]].into_boxed_slice());
    let mut changed = native.mapping.to_vec();
    changed[0].1 = reversed;
    assert!(select_mapping(native.all, &changed).is_err());
    assert!(select_mapping(&native.all[..21], native.mapping).is_err());
    assert!(CryptoProvider::get_default().is_none());
}

#[test]
fn excluded_certificate_signature_has_a_valid_baseline() -> crate::certificate_fixtures::Result<()>
{
    use crate::certificate_fixtures::{self as f, Parameters};
    use aws_lc_rs::{encoding::AsDer, signature as s};
    use rustls::pki_types::{CertificateDer, UnixTime};
    use s::KeyPair;
    let root = s::PqdsaKeyPair::generate(&s::ML_DSA_44_SIGNING)?;
    let rng = aws_lc_rs::rand::SystemRandom::new();
    let key_bytes = s::EcdsaKeyPair::generate_pkcs8(&s::ECDSA_P256_SHA256_ASN1_SIGNING, &rng)?;
    let leaf = s::EcdsaKeyPair::from_pkcs8(&s::ECDSA_P256_SHA256_ASN1_SIGNING, key_bytes.as_ref())?;
    let algorithm = f::seq(&[f::oid(&[0x60, 0x86, 0x48, 1, 0x65, 3, 4, 3, 0x11])]);
    let sign = |body: &[u8]| -> crate::certificate_fixtures::Result<Vec<u8>> {
        let mut signature = vec![0; s::ML_DSA_44_SIGNING.signature_len()];
        let length = root.sign(body, &mut signature)?;
        signature.truncate(length);
        Ok(signature)
    };
    let root_cert = f::build(
        root.public_key().as_der()?.as_ref().to_vec(),
        algorithm.clone(),
        &Parameters::new(true),
        sign,
    )?;
    let leaf = f::build(
        f::p256_spki(&leaf),
        algorithm,
        &Parameters::new(false),
        sign,
    )?;
    let leaf = CertificateDer::from(leaf);
    let parsed = rustls::server::ParsedCertificate::try_from(&leaf)?;
    let mut roots = rustls::RootCertStore::empty();
    roots.add(root_cert.into())?;
    let now = UnixTime::since_unix_epoch(std::time::Duration::from_secs(1_800_000_000));
    let verify = |provider: CryptoProvider| {
        rustls::client::verify_server_cert_signed_by_trust_anchor(
            &parsed,
            &roots,
            &[],
            now,
            provider.signature_verification_algorithms.all,
        )
    };
    verify(backend::default_provider())?;
    assert!(matches!(
        verify(provider()?),
        Err(rustls::Error::InvalidCertificate(
            rustls::CertificateError::UnsupportedSignatureAlgorithmContext { .. }
        ))
    ));
    assert!(CryptoProvider::get_default().is_none());
    Ok(())
}

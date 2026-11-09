#![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
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
    // td-crypto's own expectations, not td-tls-provider's constants: each
    // suite's confidentiality limit (records under one key) and each TLS 1.2
    // suite's server signature schemes, in order. The ECDHE-ECDSA list is
    // rustls's own, P-521 included (`tls12_ecdsa`).
    const ECDSA: &[Scheme] = &[
        Scheme::ED25519,
        Scheme::ECDSA_NISTP521_SHA512,
        Scheme::ECDSA_NISTP384_SHA384,
        Scheme::ECDSA_NISTP256_SHA256,
    ];
    const RSA: &[Scheme] = &[
        Scheme::RSA_PSS_SHA512,
        Scheme::RSA_PSS_SHA384,
        Scheme::RSA_PSS_SHA256,
        Scheme::RSA_PKCS1_SHA512,
        Scheme::RSA_PKCS1_SHA384,
        Scheme::RSA_PKCS1_SHA256,
    ];
    let expected: &[(u64, Option<&[Scheme]>)] = &[
        (1 << 24, None),
        (1 << 24, None),
        (u64::MAX, None),
        (1 << 24, Some(ECDSA)),
        (1 << 24, Some(ECDSA)),
        (u64::MAX, Some(ECDSA)),
        (1 << 24, Some(RSA)),
        (1 << 24, Some(RSA)),
        (u64::MAX, Some(RSA)),
    ];
    assert_eq!(provider.cipher_suites.len(), expected.len());
    for (suite, (limit, sign)) in provider.cipher_suites.iter().zip(expected) {
        match (suite, sign) {
            (rustls::SupportedCipherSuite::Tls13(tls13), None) => {
                assert_eq!(
                    tls13.common.confidentiality_limit,
                    *limit,
                    "{:?}",
                    suite.suite()
                );
            }
            (rustls::SupportedCipherSuite::Tls12(tls12), Some(sign)) => {
                assert_eq!(
                    tls12.common.confidentiality_limit,
                    *limit,
                    "{:?}",
                    suite.suite()
                );
                assert_eq!(tls12.sign, *sign, "{:?}", suite.suite());
            }
            _ => panic!("{:?} has the wrong protocol version", suite.suite()),
        }
    }
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
    let native = crate::certificate_algorithms::SIGNATURE_ALGORITHMS;
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
    assert!(select_mapping(&native.all[..18], native.mapping).is_err());
    assert!(CryptoProvider::get_default().is_none());
}

#[test]
fn excluded_certificate_signature_is_refused() -> crate::certificate_fixtures::Result<()> {
    use crate::certificate_fixtures::unhex;
    use rustls::pki_types::{CertificateDer, UnixTime};
    // An ML-DSA-44 root and the P-256 leaf it signed, which AWS-LC wrote and
    // verified with its default verifiers when it was the backend.
    let line = include_str!("fixtures/ml_dsa_path.txt");
    let fields: Vec<&str> = line.trim_end().split(' ').collect();
    let [_, "ML_DSA_44", root, leaf] = fields[..] else {
        return Err("malformed ML-DSA fixture".into());
    };
    let leaf = CertificateDer::from(unhex(leaf));
    let parsed = rustls::server::ParsedCertificate::try_from(&leaf)?;
    let mut roots = rustls::RootCertStore::empty();
    roots.add(CertificateDer::from(unhex(root)))?;
    let now = UnixTime::since_unix_epoch(std::time::Duration::from_secs(1_800_000_000));
    assert!(matches!(
        rustls::client::verify_server_cert_signed_by_trust_anchor(
            &parsed,
            &roots,
            &[],
            now,
            provider()?.signature_verification_algorithms.all,
        ),
        Err(rustls::Error::InvalidCertificate(
            rustls::CertificateError::UnsupportedSignatureAlgorithmContext { .. }
        ))
    ));
    assert!(CryptoProvider::get_default().is_none());
    Ok(())
}

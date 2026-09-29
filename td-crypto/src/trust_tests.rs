#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use super::*;
use crate::certificate_fixtures::{self as fixture, der, extension, oid, seq, Parameters};
use crate::pem::tests::pem;
use crate::VerificationFailure;
use aws_lc_rs::signature::{EcdsaKeyPair, KeyPair, ECDSA_P256_SHA256_ASN1_SIGNING};
use rustls::pki_types::{ServerName, UnixTime};
use std::time::Duration;

const NOW: u64 = 1_800_000_000;

fn key() -> EcdsaKeyPair {
    let bytes = EcdsaKeyPair::generate_pkcs8(
        &ECDSA_P256_SHA256_ASN1_SIGNING,
        &aws_lc_rs::rand::SystemRandom::new(),
    )
    .unwrap();
    EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, bytes.as_ref()).unwrap()
}

fn verify(store: &TrustStore, leaf: &[u8]) -> Result<(), TlsError> {
    let der = CertificateDer::from(leaf);
    let leaf = rustls::server::ParsedCertificate::try_from(&der).map_err(|_| TlsError::Invalid)?;
    rustls::client::verify_server_cert_signed_by_trust_anchor(
        &leaf,
        &store.roots,
        &[],
        UnixTime::since_unix_epoch(Duration::from_secs(NOW)),
        certificate_algorithms::certificate_algorithms()?,
    )
    .map_err(crate::tls_error::certificate_error)?;
    rustls::client::verify_server_name(&leaf, &ServerName::try_from("localhost").unwrap())
        .map_err(crate::tls_error::certificate_error)
}

#[test]
fn private_trust_replaces_public_and_other_private_roots() {
    let a = key();
    let b = key();
    let leaf_key = key();
    let root_a = fixture::certificate(&a, &a, true).unwrap();
    let mut b_parameters = Parameters::new(true);
    b_parameters.subject = b"root-b".to_vec();
    b_parameters.issuer = b_parameters.subject.clone();
    let root_b = fixture::make(&b, &b, &b_parameters).unwrap();
    let leaf_a = fixture::certificate(&leaf_key, &a, false).unwrap();
    let mut b_leaf = Parameters::new(false);
    b_leaf.issuer = b_parameters.subject.clone();
    let leaf_b = fixture::make(&leaf_key, &b, &b_leaf).unwrap();
    let mut input = pem("CERTIFICATE", &root_a);
    let a_store = TrustStore::from_pem(&input).unwrap();
    input.fill(0);
    let b_store = TrustStore::from_pem(&pem("CERTIFICATE", &root_b)).unwrap();
    let public = TrustStore::public_roots().unwrap();
    assert_eq!(a_store.anchor_count(), 1);
    assert!(!a_store.uses_public_roots());
    assert_eq!(
        format!("{a_store:?}"),
        "TrustStore { anchors: 1, public: false, .. }"
    );
    assert_eq!(verify(&a_store, &leaf_a), Ok(()));
    assert_eq!(verify(&b_store, &leaf_b), Ok(()));
    for (store, leaf) in [
        (&a_store, &leaf_b),
        (&b_store, &leaf_a),
        (&public, &leaf_a),
        (&public, &leaf_b),
    ] {
        assert_eq!(
            verify(store, leaf),
            Err(TlsError::Verification(VerificationFailure::Untrusted))
        );
    }
    // A private root permits ordinary client-auth chain verification too.
    let mut client = Parameters::new(false);
    client.extensions[3] = extension(0x25, false, seq(&[oid(&[0x2b, 6, 1, 5, 5, 7, 3, 2])]));
    let client = fixture::make(&leaf_key, &a, &client).unwrap();
    let verifier = rustls::server::WebPkiClientVerifier::builder_with_provider(
        std::sync::Arc::new(a_store.roots.clone()),
        std::sync::Arc::new(rustls::crypto::aws_lc_rs::default_provider()),
    )
    .build()
    .unwrap();
    assert!(verifier
        .verify_client_cert(
            &CertificateDer::from(client),
            &[],
            UnixTime::since_unix_epoch(Duration::from_secs(NOW))
        )
        .is_ok());
    assert!(verifier
        .verify_client_cert(
            &CertificateDer::from(leaf_a),
            &[],
            UnixTime::since_unix_epoch(Duration::from_secs(NOW))
        )
        .is_err());
    assert!(rustls::crypto::CryptoProvider::get_default().is_none());
}

#[test]
fn private_trust_bundle_limits_duplicates_and_atomic_refusal() {
    let key = key();
    let root = fixture::certificate(&key, &key, true).unwrap();
    let encoded = pem("CERTIFICATE", &root);
    assert_eq!(TrustStore::from_pem(&[]).err(), Some(TlsError::Invalid));
    assert_eq!(
        TrustStore::from_pem(&[encoded.clone(), encoded.clone()].concat()).err(),
        Some(TlsError::Invalid)
    );
    let mut changed = Parameters::new(true);
    changed.serial = 9;
    let changed = fixture::make(&key, &key, &changed).unwrap();
    assert_eq!(
        TrustStore::from_pem(&[encoded.clone(), pem("CERTIFICATE", &changed)].concat()).err(),
        Some(TlsError::Invalid)
    );
    let point = key.public_key().as_ref();
    let compressed = [vec![2 | (point[64] & 1)], point[1..33].to_vec()].concat();
    let spki = seq(&[
        der(0x30, certificate_algorithms::P256_KEY),
        der(3, &[vec![0], compressed].concat()),
    ]);
    let alias = fixture::build(
        spki,
        fixture::signature_algorithm(),
        &Parameters::new(true),
        |body| {
            Ok(key
                .sign(&aws_lc_rs::rand::SystemRandom::new(), body)?
                .as_ref()
                .to_vec())
        },
    )
    .unwrap();
    assert_eq!(
        TrustStore::from_pem(&pem("CERTIFICATE", &alias))
            .unwrap()
            .anchor_count(),
        1
    );
    assert_eq!(
        TrustStore::from_pem(&[encoded.clone(), pem("CERTIFICATE", &alias)].concat()).err(),
        Some(TlsError::Invalid)
    );
    let mut padded = encoded.clone();
    padded.resize(128 * 1024, b' ');
    assert_eq!(TrustStore::from_pem(&padded).unwrap().anchor_count(), 1);
    padded.push(b' ');
    assert_eq!(TrustStore::from_pem(&padded).err(), Some(TlsError::Invalid));
    for extra in [pem("CERTIFICATE", &seq(&[der(2, &[1])])), b"junk".to_vec()] {
        assert_eq!(
            TrustStore::from_pem(&[encoded.clone(), extra].concat()).err(),
            Some(TlsError::Invalid)
        );
    }
    let mut many = Vec::new();
    for n in 0..129 {
        let mut parameters = Parameters::new(true);
        parameters.subject = format!("root-{n}").into_bytes();
        let root = fixture::make(&key, &key, &parameters).unwrap();
        many.extend_from_slice(&pem("CERTIFICATE", &root));
        if n == 127 {
            assert_eq!(TrustStore::from_pem(&many).unwrap().anchor_count(), 128);
        }
    }
    assert!(many.len() < 128 * 1024);
    assert_eq!(TrustStore::from_pem(&many).err(), Some(TlsError::Invalid));
}

#[test]
fn private_trust_anchor_policy_and_ignored_self_signature() {
    let root_key = key();
    let leaf_key = key();
    let leaf = fixture::certificate(&leaf_key, &root_key, false).unwrap();
    // Anchor public-key parsing covers every admitted key family. Their
    // issuing certificate signature is deliberately a separate P-256 key.
    use aws_lc_rs::signature as native;
    let mut public_keys = vec![(
        certificate_algorithms::P256_KEY,
        root_key.public_key().as_ref().to_vec(),
    )];
    for (algorithm, identifier) in [
        (
            &native::ECDSA_P384_SHA384_ASN1_SIGNING,
            certificate_algorithms::P384_KEY,
        ),
        (
            &native::ECDSA_P521_SHA512_ASN1_SIGNING,
            certificate_algorithms::P521_KEY,
        ),
    ] {
        let document =
            EcdsaKeyPair::generate_pkcs8(algorithm, &aws_lc_rs::rand::SystemRandom::new()).unwrap();
        let key = EcdsaKeyPair::from_pkcs8(algorithm, document.as_ref()).unwrap();
        public_keys.push((identifier, key.public_key().as_ref().to_vec()));
    }
    let ed = native::Ed25519KeyPair::generate_pkcs8(&aws_lc_rs::rand::SystemRandom::new()).unwrap();
    let ed = native::Ed25519KeyPair::from_pkcs8(ed.as_ref()).unwrap();
    public_keys.push((
        certificate_algorithms::ED25519_KEY,
        ed.public_key().as_ref().to_vec(),
    ));
    let rsa = aws_lc_rs::rsa::KeyPair::generate(aws_lc_rs::rsa::KeySize::Rsa2048).unwrap();
    public_keys.push((
        certificate_algorithms::RSA_KEY,
        rsa.public_key().as_ref().to_vec(),
    ));
    for (identifier, public) in public_keys {
        let spki = seq(&[
            der(0x30, identifier),
            der(3, &[vec![0], public.clone()].concat()),
        ]);
        let mut ca = Parameters::new(true);
        ca.issuer = b"external-issuer".to_vec();
        let bytes = fixture::build(spki.clone(), fixture::signature_algorithm(), &ca, |body| {
            Ok(root_key
                .sign(&aws_lc_rs::rand::SystemRandom::new(), body)?
                .as_ref()
                .to_vec())
        })
        .unwrap();
        assert_eq!(
            TrustStore::from_pem(&pem("CERTIFICATE", &bytes))
                .unwrap()
                .anchor_count(),
            1
        );
        for raw in [
            spki.clone(),
            [spki, b"trailing junk".to_vec()].concat(),
            [public, b"trailing junk".to_vec()].concat(),
        ] {
            let bad = seq(&[der(0x30, identifier), der(3, &[vec![0], raw].concat())]);
            let bytes = fixture::build(bad, fixture::signature_algorithm(), &ca, |body| {
                Ok(root_key
                    .sign(&aws_lc_rs::rand::SystemRandom::new(), body)?
                    .as_ref()
                    .to_vec())
            })
            .unwrap();
            assert_eq!(
                TrustStore::from_pem(&pem("CERTIFICATE", &bytes)).err(),
                Some(TlsError::Invalid)
            );
        }
    }
    let mut root = Parameters::new(true);
    // Anchor dates and self-signatures are not peer path validation inputs.
    root.not_after = b"260101000000Z".to_vec();
    let mut expired = fixture::make(&root_key, &root_key, &root).unwrap();
    *expired.last_mut().unwrap() ^= 1;
    let store = TrustStore::from_pem(&pem("CERTIFICATE", &expired)).unwrap();
    assert_eq!(verify(&store, &leaf), Ok(()));
    let mut non_ca = Parameters::new(true);
    non_ca.extensions[0] = extension(0x13, true, seq(&[]));
    let mut wrong_usage = Parameters::new(true);
    wrong_usage.extensions[1] = extension(0x0f, true, der(3, &[7, 0x80]));
    for root in [non_ca, wrong_usage] {
        let bytes = fixture::make(&root_key, &root_key, &root).unwrap();
        assert_eq!(
            TrustStore::from_pem(&pem("CERTIFICATE", &bytes)).err(),
            Some(TlsError::Invalid)
        );
    }
    for constraint in [
        extension(0x25, false, seq(&[oid(&[0x2b, 6, 1, 5, 5, 7, 3, 1])])),
        extension(
            0x1e,
            true,
            seq(&[der(0xa0, &seq(&[der(0x82, b"example.com")]))]),
        ),
        extension(0x7f, true, der(5, &[])),
    ] {
        let mut root = Parameters::new(true);
        root.extensions.push(constraint);
        let bytes = fixture::make(&root_key, &root_key, &root).unwrap();
        assert_eq!(
            TrustStore::from_pem(&pem("CERTIFICATE", &bytes)).err(),
            Some(TlsError::Invalid)
        );
    }
    let mut root = Parameters::new(true);
    root.extensions[0] = extension(0x13, true, seq(&[der(1, &[0xff]), der(2, &[0])]));
    let bytes = fixture::make(&root_key, &root_key, &root).unwrap();
    assert_eq!(
        TrustStore::from_pem(&pem("CERTIFICATE", &bytes)).err(),
        Some(TlsError::Invalid)
    );
    let spki = seq(&[
        der(0x30, certificate_algorithms::P256_KEY),
        der(3, &[0; 66]),
    ]);
    let bytes = fixture::build(
        spki,
        fixture::signature_algorithm(),
        &Parameters::new(true),
        |body| {
            Ok(root_key
                .sign(&aws_lc_rs::rand::SystemRandom::new(), body)?
                .as_ref()
                .to_vec())
        },
    )
    .unwrap();
    assert_eq!(
        TrustStore::from_pem(&pem("CERTIFICATE", &bytes)).err(),
        Some(TlsError::Invalid)
    );
}

#[test]
fn public_trust_inventory_and_source_are_fixed() {
    fn send_sync<T: Send + Sync>() {}
    send_sync::<TrustStore>();
    let store = TrustStore::public_roots().unwrap();
    assert!(store.uses_public_roots());
    assert_eq!(store.anchor_count(), 118);
    assert_eq!(store.roots.roots, webpki_roots::TLS_SERVER_ROOTS);
    assert!(rustls::crypto::CryptoProvider::get_default().is_none());
}

#[test]
fn trust_construction_unwind_drops_unpublished_state() {
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    };
    struct Flag(Arc<AtomicBool>);
    impl Drop for Flag {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }
    let dropped = Arc::new(AtomicBool::new(false));
    let observe = dropped.clone();
    let result = boundary(|| {
        let _store = TrustStore::public_roots()?;
        let _flag = Flag(observe);
        panic!("synthetic trust construction unwind");
    });
    assert_eq!(result.err(), Some(TlsError::Crypto));
    assert!(dropped.load(Ordering::SeqCst));
}

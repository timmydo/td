#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use super::*;
use crate::certificate_fixtures::{self as f, Parameters};
use aws_lc_rs::signature::{EcdsaKeyPair, ECDSA_P256_SHA256_ASN1_SIGNING};

fn fixture() -> (Arc<P256Key>, Vec<Vec<u8>>) {
    let rng = aws_lc_rs::rand::SystemRandom::new();
    let bytes = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, &rng).unwrap();
    let native = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, bytes.as_ref()).unwrap();
    let root_bytes = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, &rng).unwrap();
    let root =
        EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, root_bytes.as_ref()).unwrap();
    let certificate = f::make(&native, &root, &Parameters::new(false)).unwrap();
    let root = f::make(&root, &root, &Parameters::new(true)).unwrap();
    (
        Arc::new(Provider.load_p256(bytes.as_ref()).unwrap()),
        vec![certificate, root],
    )
}

#[test]
fn canonical_tls_signature_encoding_boundaries() {
    let mut raw = [0; 64];
    raw[31] = 1;
    raw[63] = 1;
    assert_eq!(der_signature(&raw).unwrap(), [0x30, 6, 2, 1, 1, 2, 1, 1]);
    let expected_integer = |bytes: &[u8]| {
        let used = bytes
            .iter()
            .copied()
            .skip_while(|b| *b == 0)
            .collect::<Vec<_>>();
        let mut value = if used[0] >= 128 { vec![0] } else { vec![] };
        value.extend(used);
        f::der(2, &value)
    };
    for r_start in 0..32 {
        for s_start in 0..32 {
            for first in [1, 0x7f, 0x80, 0xff] {
                let mut raw = [0; 64];
                raw[r_start] = first;
                raw[32 + s_start] = first;
                let expected =
                    f::seq(&[expected_integer(&raw[..32]), expected_integer(&raw[32..])]);
                let actual = der_signature(&raw).unwrap();
                assert_eq!(actual, expected);
                assert!((8..=72).contains(&actual.len()));
            }
        }
    }
    assert_eq!(der_signature(&[0; 64]), Err(Error::Crypto));
    raw[..32].fill(0);
    assert_eq!(der_signature(&raw), Err(Error::Crypto));
    raw[31] = 1;
    raw[32..].fill(0);
    assert_eq!(der_signature(&raw), Err(Error::Crypto));
}

#[test]
fn tls_signer_uses_owned_key_and_hashes_once() {
    fn unwind_safe<T: std::panic::UnwindSafe + std::panic::RefUnwindSafe>() {}
    unwind_safe::<Key>();
    unwind_safe::<Operation>();
    let (key, chain) = fixture();
    let certified = certified_key(chain, key.clone()).unwrap();
    certified.keys_match().unwrap();
    assert_eq!(certified.key.algorithm(), SignatureAlgorithm::ECDSA);
    for offered in [
        vec![],
        vec![SignatureScheme::ECDSA_NISTP384_SHA384],
        vec![SignatureScheme::RSA_PSS_SHA256],
    ] {
        assert!(certified.key.choose_scheme(&offered).is_none());
    }
    let signer = certified
        .key
        .choose_scheme(&[
            SignatureScheme::ECDSA_NISTP384_SHA384,
            SignatureScheme::ECDSA_NISTP256_SHA256,
        ])
        .unwrap();
    assert_eq!(signer.scheme(), SignatureScheme::ECDSA_NISTP256_SHA256);
    assert_eq!(format!("{:?}", certified.key), "TlsSigningKey(<redacted>)");
    assert_eq!(format!("{signer:?}"), "TlsSigner(<redacted>)");
    let mut public = [0; 65];
    Provider.p256_public(&key, &mut public).unwrap();
    let verifier = aws_lc_rs::signature::UnparsedPublicKey::new(
        &aws_lc_rs::signature::ECDSA_P256_SHA256_ASN1,
        public,
    );
    for message in [b"".as_slice(), b"TLS transcript", &[0xa5; 1024]] {
        let signature = signer.sign(message).unwrap();
        assert!(signature.len() <= 72);
        verifier.verify(message, &signature).unwrap();
        let digest = aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA256, message);
        assert!(verifier.verify(digest.as_ref(), &signature).is_err());
        let mut changed = message.to_vec();
        changed.push(0);
        assert!(verifier.verify(&changed, &signature).is_err());
    }
}

#[test]
fn shared_tls_signers_observe_transform_retirement() {
    for unwind in [false, true] {
        let (key, chain) = fixture();
        let first = certified_key(chain.clone(), key.clone()).unwrap();
        let second = certified_key(chain.clone(), key.clone()).unwrap();
        let signers = [
            first
                .key
                .choose_scheme(&[SignatureScheme::ECDSA_NISTP256_SHA256])
                .unwrap(),
            second
                .key
                .choose_scheme(&[SignatureScheme::ECDSA_NISTP256_SHA256])
                .unwrap(),
        ];
        for signer in &signers {
            signer.sign(b"before retirement").unwrap();
        }
        let failure = key.sign_es256_with::<()>(b"encoding failure", |_| {
            if unwind {
                panic!("fixture TLS transform unwind");
            }
            Err(Error::Capacity)
        });
        assert_eq!(
            failure,
            Err(if unwind {
                Error::Crypto
            } else {
                Error::Capacity
            })
        );
        for signer in &signers {
            let error = signer.sign(b"after retirement").unwrap_err();
            match error {
                rustls::Error::Other(rustls::OtherError(error)) => {
                    assert_eq!(error.downcast_ref::<TlsError>(), Some(&TlsError::Crypto))
                }
                _ => panic!("unexpected signing error"),
            }
        }
        assert!(matches!(certified_key(chain, key), Err(TlsError::Crypto)));
    }
}

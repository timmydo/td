//! Exact certificate algorithm metadata for the admitted backend.
use crate::TlsError;
use rustls::pki_types::SignatureVerificationAlgorithm;
pub(super) const P256_KEY: &[u8] = &[
    0x06, 0x07, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01, 0x06, 0x08, 0x2a, 0x86, 0x48, 0xce, 0x3d,
    0x03, 0x01, 0x07,
];
pub(super) const P384_KEY: &[u8] = &[
    0x06, 0x07, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01, 0x06, 0x05, 0x2b, 0x81, 0x04, 0x00, 0x22,
];
pub(super) const P521_KEY: &[u8] = &[
    0x06, 0x07, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01, 0x06, 0x05, 0x2b, 0x81, 0x04, 0x00, 0x23,
];
pub(super) const ED25519_KEY: &[u8] = &[0x06, 0x03, 0x2b, 0x65, 0x70];
pub(super) const RSA_KEY: &[u8] = &[
    0x06, 0x09, 0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x01, 0x05, 0x00,
];
const ECDSA_SHA256: &[u8] = &[0x06, 0x08, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x04, 0x03, 0x02];
const ECDSA_SHA384: &[u8] = &[0x06, 0x08, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x04, 0x03, 0x03];
const ECDSA_SHA512: &[u8] = &[0x06, 0x08, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x04, 0x03, 0x04];
const ED25519_SIGNATURE: &[u8] = &[0x06, 0x03, 0x2b, 0x65, 0x70];
const PSS_SHA256: &[u8] = &[
    0x06, 0x09, 0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x0a, 0x30, 0x34, 0xa0, 0x0f, 0x30,
    0x0d, 0x06, 0x09, 0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x01, 0x05, 0x00, 0xa1, 0x1c,
    0x30, 0x1a, 0x06, 0x09, 0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x08, 0x30, 0x0d, 0x06,
    0x09, 0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x01, 0x05, 0x00, 0xa2, 0x03, 0x02, 0x01,
    0x20,
];
const PSS_SHA384: &[u8] = &[
    0x06, 0x09, 0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x0a, 0x30, 0x34, 0xa0, 0x0f, 0x30,
    0x0d, 0x06, 0x09, 0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x02, 0x05, 0x00, 0xa1, 0x1c,
    0x30, 0x1a, 0x06, 0x09, 0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x08, 0x30, 0x0d, 0x06,
    0x09, 0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x02, 0x05, 0x00, 0xa2, 0x03, 0x02, 0x01,
    0x30,
];
const PSS_SHA512: &[u8] = &[
    0x06, 0x09, 0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x0a, 0x30, 0x34, 0xa0, 0x0f, 0x30,
    0x0d, 0x06, 0x09, 0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x03, 0x05, 0x00, 0xa1, 0x1c,
    0x30, 0x1a, 0x06, 0x09, 0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x08, 0x30, 0x0d, 0x06,
    0x09, 0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x03, 0x05, 0x00, 0xa2, 0x03, 0x02, 0x01,
    0x40,
];
const PKCS1_SHA256: &[u8] = &[
    0x06, 0x09, 0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x0b, 0x05, 0x00,
];
const PKCS1_SHA384: &[u8] = &[
    0x06, 0x09, 0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x0c, 0x05, 0x00,
];
const PKCS1_SHA512: &[u8] = &[
    0x06, 0x09, 0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x0d, 0x05, 0x00,
];
const PKCS1_SHA256_ABSENT: &[u8] = &[
    0x06, 0x09, 0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x0b,
];
const PKCS1_SHA384_ABSENT: &[u8] = &[
    0x06, 0x09, 0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x0c,
];
const PKCS1_SHA512_ABSENT: &[u8] = &[
    0x06, 0x09, 0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x0d,
];
const ML_DSA_44: &[u8] = &[
    0x06, 0x09, 0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x03, 0x11,
];
const ML_DSA_65: &[u8] = &[
    0x06, 0x09, 0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x03, 0x12,
];
const ML_DSA_87: &[u8] = &[
    0x06, 0x09, 0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x03, 0x13,
];
const EXCLUDED_ALGORITHMS: &[(&[u8], &[u8])] = &[
    (ML_DSA_44, ML_DSA_44),
    (ML_DSA_65, ML_DSA_65),
    (ML_DSA_87, ML_DSA_87),
];
const CERTIFICATE_ALGORITHMS: &[(&[u8], &[u8])] = &[
    (P256_KEY, ECDSA_SHA256),
    (P256_KEY, ECDSA_SHA384),
    (P256_KEY, ECDSA_SHA512),
    (P384_KEY, ECDSA_SHA256),
    (P384_KEY, ECDSA_SHA384),
    (P384_KEY, ECDSA_SHA512),
    (P521_KEY, ECDSA_SHA256),
    (P521_KEY, ECDSA_SHA384),
    (P521_KEY, ECDSA_SHA512),
    (ED25519_KEY, ED25519_SIGNATURE),
    (RSA_KEY, PSS_SHA256),
    (RSA_KEY, PSS_SHA384),
    (RSA_KEY, PSS_SHA512),
    (RSA_KEY, PKCS1_SHA256),
    (RSA_KEY, PKCS1_SHA384),
    (RSA_KEY, PKCS1_SHA512),
    (RSA_KEY, PKCS1_SHA256_ABSENT),
    (RSA_KEY, PKCS1_SHA384_ABSENT),
    (RSA_KEY, PKCS1_SHA512_ABSENT),
];

pub(super) fn certificate_algorithms(
) -> Result<&'static [&'static dyn SignatureVerificationAlgorithm], TlsError> {
    Ok(crate::tls_policy::provider()?
        .signature_verification_algorithms
        .all)
}

pub(super) fn select<'a>(
    all: &'a [&'static dyn SignatureVerificationAlgorithm],
) -> Result<&'a [&'static dyn SignatureVerificationAlgorithm], TlsError> {
    if all.len() != 22 {
        return Err(TlsError::Crypto);
    }
    let selected = all
        .get(..CERTIFICATE_ALGORITHMS.len())
        .ok_or(TlsError::Crypto)?;
    for (algorithm, (key, signature)) in all
        .iter()
        .zip(CERTIFICATE_ALGORITHMS.iter().chain(EXCLUDED_ALGORITHMS))
    {
        if algorithm.public_key_alg_id().as_ref() != *key
            || algorithm.signature_alg_id().as_ref() != *signature
        {
            return Err(TlsError::Crypto);
        }
    }
    Ok(selected)
}

pub(super) fn canonical_key(
    certificate: &crate::certificate::Certificate<'_>,
) -> Result<Vec<u8>, TlsError> {
    use aws_lc_rs::{encoding::AsDer, signature as native};
    let bytes = certificate.public_key;
    let ec_shape = |plain, compressed| match bytes.first() {
        Some(4) => bytes.len() == plain,
        Some(2 | 3) => bytes.len() == compressed,
        _ => false,
    };
    let (algorithm, valid): (&'static dyn native::VerificationAlgorithm, bool) =
        match certificate.key_algorithm {
            P256_KEY => (&native::ECDSA_P256_SHA256_ASN1, ec_shape(65, 33)),
            P384_KEY => (&native::ECDSA_P384_SHA384_ASN1, ec_shape(97, 49)),
            P521_KEY => (&native::ECDSA_P521_SHA512_ASN1, ec_shape(133, 67)),
            ED25519_KEY => (&native::ED25519, bytes.len() == 32),
            RSA_KEY => {
                certificate.rsa_size()?;
                (&native::RSA_PKCS1_2048_8192_SHA256, true)
            }
            _ => return Err(TlsError::Invalid),
        };
    if !valid {
        return Err(TlsError::Invalid);
    }
    let key = native::ParsedPublicKey::new(algorithm, certificate.public_key)
        .map_err(|_| TlsError::Invalid)?;
    let encoded = key.as_der().map_err(|_| TlsError::Crypto)?;
    let bytes = encoded.as_ref();
    if bytes.len() > crate::CERTIFICATE_DER_CAPACITY {
        return Err(TlsError::Crypto);
    }
    let mut owned = Vec::new();
    owned
        .try_reserve_exact(bytes.len())
        .map_err(|_| TlsError::Capacity)?;
    owned.extend_from_slice(bytes);
    Ok(owned)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[test]
    fn certificate_algorithm_inventory_fails_closed() {
        let all = rustls::crypto::aws_lc_rs::default_provider()
            .signature_verification_algorithms
            .all;
        let selected = certificate_algorithms().unwrap();
        assert_eq!(selected.len(), 19);
        assert_eq!(
            selected
                .iter()
                .map(|algorithm| (
                    algorithm.public_key_alg_id().as_ref().to_vec(),
                    algorithm.signature_alg_id().as_ref().to_vec()
                ))
                .collect::<std::collections::BTreeSet<_>>()
                .len(),
            19
        );
        for excluded in &all[19..] {
            assert!(!selected
                .iter()
                .any(|algorithm| algorithm.signature_alg_id() == excluded.signature_alg_id()));
        }
        assert_eq!(select(&all[..21]).err(), Some(TlsError::Crypto));
        let mut changed = all.to_vec();
        changed.swap(0, 1);
        assert_eq!(select(&changed).err(), Some(TlsError::Crypto));
        changed[0] = all[19];
        assert_eq!(select(&changed).err(), Some(TlsError::Crypto));
        let mut changed = all.to_vec();
        changed.swap(19, 20);
        assert_eq!(select(&changed).err(), Some(TlsError::Crypto));
        changed[19] = all[0];
        assert_eq!(select(&changed).err(), Some(TlsError::Crypto));
        assert!(rustls::crypto::CryptoProvider::get_default().is_none());
    }
}

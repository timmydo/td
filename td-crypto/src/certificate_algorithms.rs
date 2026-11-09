//! Exact certificate algorithm metadata and verifiers, from td-tls-provider.
use crate::TlsError;
use rustls::crypto::WebPkiSupportedAlgorithms;
use rustls::pki_types::SignatureVerificationAlgorithm;
use rustls::SignatureScheme as Scheme;
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

/// TLS.md's nineteen certificate algorithms in CERTIFICATE_ALGORITHMS' order,
/// over td-tls-provider's verifiers, and its ten handshake schemes, each ECDSA
/// scheme's own curve first: TLS 1.3 takes only the first verifier mapped.
/// `select` and `tls_policy::select_mapping` check both against the
/// identifiers above before a provider holds them.
pub(super) static SIGNATURE_ALGORITHMS: WebPkiSupportedAlgorithms = {
    use td_tls_provider::verifiers::*;
    WebPkiSupportedAlgorithms {
        all: &[
            ECDSA_P256_SHA256,
            ECDSA_P256_SHA384,
            ECDSA_P256_SHA512,
            ECDSA_P384_SHA256,
            ECDSA_P384_SHA384,
            ECDSA_P384_SHA512,
            ECDSA_P521_SHA256,
            ECDSA_P521_SHA384,
            ECDSA_P521_SHA512,
            ED25519,
            RSA_PSS_SHA256,
            RSA_PSS_SHA384,
            RSA_PSS_SHA512,
            RSA_PKCS1_SHA256,
            RSA_PKCS1_SHA384,
            RSA_PKCS1_SHA512,
            RSA_PKCS1_SHA256_BARE,
            RSA_PKCS1_SHA384_BARE,
            RSA_PKCS1_SHA512_BARE,
        ],
        mapping: &[
            (
                Scheme::ECDSA_NISTP384_SHA384,
                &[ECDSA_P384_SHA384, ECDSA_P256_SHA384, ECDSA_P521_SHA384],
            ),
            (
                Scheme::ECDSA_NISTP256_SHA256,
                &[ECDSA_P256_SHA256, ECDSA_P384_SHA256, ECDSA_P521_SHA256],
            ),
            (
                Scheme::ECDSA_NISTP521_SHA512,
                &[ECDSA_P521_SHA512, ECDSA_P384_SHA512, ECDSA_P256_SHA512],
            ),
            (Scheme::ED25519, &[ED25519]),
            (Scheme::RSA_PSS_SHA512, &[RSA_PSS_SHA512]),
            (Scheme::RSA_PSS_SHA384, &[RSA_PSS_SHA384]),
            (Scheme::RSA_PSS_SHA256, &[RSA_PSS_SHA256]),
            (Scheme::RSA_PKCS1_SHA512, &[RSA_PKCS1_SHA512]),
            (Scheme::RSA_PKCS1_SHA384, &[RSA_PKCS1_SHA384]),
            (Scheme::RSA_PKCS1_SHA256, &[RSA_PKCS1_SHA256]),
        ],
    }
};

pub(super) fn certificate_algorithms(
) -> Result<&'static [&'static dyn SignatureVerificationAlgorithm], TlsError> {
    Ok(crate::tls_policy::provider()?
        .signature_verification_algorithms
        .all)
}

pub(super) fn select<'a>(
    all: &'a [&'static dyn SignatureVerificationAlgorithm],
) -> Result<&'a [&'static dyn SignatureVerificationAlgorithm], TlsError> {
    if all.len() != CERTIFICATE_ALGORITHMS.len() {
        return Err(TlsError::Crypto);
    }
    for (algorithm, (key, signature)) in all.iter().zip(CERTIFICATE_ALGORITHMS) {
        if algorithm.public_key_alg_id().as_ref() != *key
            || algorithm.signature_alg_id().as_ref() != *signature
        {
            return Err(TlsError::Crypto);
        }
    }
    Ok(all)
}

/// A DER element: tag, minimal definite length, then `parts` in order.
fn element(tag: u8, parts: &[&[u8]], out: &mut Vec<u8>) -> Result<(), TlsError> {
    let length: usize = parts.iter().map(|part| part.len()).sum();
    let width = match length {
        0..=0x7f => 0,
        0x80..=0xff => 1,
        0x100..=0xffff => 2,
        _ => return Err(TlsError::Crypto),
    };
    out.try_reserve(2 + width + length)
        .map_err(|_| TlsError::Capacity)?;
    out.push(tag);
    if width == 0 {
        out.extend_from_slice(&[length as u8]);
    } else {
        out.push(0x80 | width as u8);
        out.extend_from_slice(
            length
                .to_be_bytes()
                .get(8 - width..)
                .ok_or(TlsError::Crypto)?,
        );
    }
    for part in parts {
        out.extend_from_slice(part);
    }
    Ok(())
}

/// SubjectPublicKeyInfo with the key's identifier and its canonical raw form:
/// an EC point validated on its curve and uncompressed, an Ed25519 key's 32
/// bytes, or an RSA key's minimal PKCS#1 encoding, validated by td-primitives
/// (2048 to 8192 bits, odd exponent of 3 to 33 bits). Compressed and
/// uncompressed aliases of one EC point therefore compare equal.
pub(super) fn canonical_key(
    certificate: &crate::certificate::Certificate<'_>,
) -> Result<Vec<u8>, TlsError> {
    use td_primitives::ec::{p256, p384, p521};
    let bytes = certificate.public_key;
    let ec_shape = |plain, compressed| match bytes.first() {
        Some(4) => bytes.len() == plain,
        Some(2 | 3) => bytes.len() == compressed,
        _ => false,
    };
    let invalid = |_| TlsError::Invalid;
    let mut point = [0u8; 133];
    let raw: &[u8] = match certificate.key_algorithm {
        P256_KEY if ec_shape(65, 33) => {
            let out = point.get_mut(..65).ok_or(TlsError::Crypto)?;
            out.copy_from_slice(&p256::uncompressed(bytes).map_err(invalid)?);
            out
        }
        P384_KEY if ec_shape(97, 49) => {
            let out = point.get_mut(..97).ok_or(TlsError::Crypto)?;
            out.copy_from_slice(&p384::uncompressed(bytes).map_err(invalid)?);
            out
        }
        P521_KEY if ec_shape(133, 67) => {
            point = p521::uncompressed(bytes).map_err(invalid)?;
            &point
        }
        // Any 32 bytes, as the key is only compared here.
        ED25519_KEY if bytes.len() == 32 => bytes,
        RSA_KEY => {
            certificate.rsa_size()?;
            td_primitives::rsa::PublicKey::from_der(bytes).map_err(invalid)?;
            bytes
        }
        _ => return Err(TlsError::Invalid),
    };
    let mut algorithm = Vec::new();
    element(0x30, &[certificate.key_algorithm], &mut algorithm)?;
    let mut key = Vec::new();
    element(0x03, &[&[0], raw], &mut key)?;
    let mut owned = Vec::new();
    element(0x30, &[&algorithm, &key], &mut owned)?;
    if owned.len() > crate::CERTIFICATE_DER_CAPACITY {
        return Err(TlsError::Crypto);
    }
    Ok(owned)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn certificate_algorithm_inventory_fails_closed() {
        let all = SIGNATURE_ALGORITHMS.all;
        let selected = certificate_algorithms().unwrap();
        assert_eq!(selected.len(), 19);
        assert!(selected
            .iter()
            .zip(all)
            .all(|(a, b)| std::ptr::addr_eq(*a, *b)));
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
        assert_eq!(select(&all[..18]).err(), Some(TlsError::Crypto));
        let mut changed = all.to_vec();
        changed.swap(0, 1);
        assert_eq!(select(&changed).err(), Some(TlsError::Crypto));
        let mut changed = all.to_vec();
        changed[16] = all[13];
        assert_eq!(select(&changed).err(), Some(TlsError::Crypto));
        let mut changed = all.to_vec();
        changed.push(all[0]);
        assert_eq!(select(&changed).err(), Some(TlsError::Crypto));
        assert!(rustls::crypto::CryptoProvider::get_default().is_none());
    }

    /// Keys built with Python's big integers (fixtures/canonical_keys.py),
    /// independent of td-primitives: EC points valid, off the curve, with a
    /// coordinate of p, without a square root, hybrid, at infinity and
    /// truncated, in both forms; RSA keys at and past each modulus and
    /// exponent bound.
    #[test]
    fn certificate_key_admission_known_answers() {
        let mut canonical = std::collections::BTreeMap::new();
        let mut rows = 0;
        for line in include_str!("fixtures/canonical_keys.txt").lines() {
            let fields: Vec<&str> = line.split(' ').collect();
            let [name, form, verdict, group, key] = fields[..] else {
                panic!("malformed row: {line}");
            };
            let key = crate::certificate_fixtures::unhex(key);
            let algorithm = match name {
                "P256" => P256_KEY,
                "P384" => P384_KEY,
                "P521" => P521_KEY,
                "RSA" => RSA_KEY,
                _ => panic!("unknown algorithm {name}"),
            };
            let certificate = crate::certificate::Certificate::for_key(algorithm, &key);
            let result = canonical_key(&certificate);
            let context = format!("{name} {form}");
            match verdict {
                "accept" => {
                    let owned = result.unwrap_or_else(|e| panic!("{context}: {e:?}"));
                    if name == "RSA" {
                        assert!(owned.ends_with(&key), "{context}");
                    } else {
                        let width = match name {
                            "P256" => 65,
                            "P384" => 97,
                            _ => 133,
                        };
                        let raw = &owned[owned.len() - width..];
                        if form == "uncompressed" {
                            assert_eq!(raw, key.as_slice(), "{context}");
                        }
                        let previous = canonical.insert(group.to_string(), owned.clone());
                        if let Some(previous) = previous {
                            assert_eq!(previous, owned, "{context}: aliases differ");
                        }
                    }
                }
                "refuse" => assert_eq!(result.err(), Some(TlsError::Invalid), "{context}"),
                _ => panic!("unknown verdict {verdict}"),
            }
            rows += 1;
        }
        assert_eq!(rows, 49);
        // A point's other y is another key.
        for (group, owned) in &canonical {
            if let Some(base) = group.strip_suffix("-neg") {
                assert_ne!(canonical.get(base), Some(owned), "{group}");
            }
        }
        assert_eq!(canonical.len(), 12);
    }
}

//! Signature verification for the SHA-256 DKIM algorithms.
use crate::{certificate_algorithms::RSA_KEY, der::Der, provider::provider, Error};
use td_primitives::{curve25519::ed25519, rsa};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DkimAlgorithm {
    Rsa,
    Ed25519,
}

impl DkimAlgorithm {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Rsa => "rsa-sha256",
            Self::Ed25519 => "ed25519-sha256",
        }
    }
}

/// Size in bits of the parsed RSA modulus or encoded Ed25519 key.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DkimVerification {
    pub key_bits: u16,
    pub verified: bool,
}

/// Verify the SHA-256 digest of canonicalized DKIM headers, using decoded
/// DNS key bytes and signature. Invalid key material and a provider failure
/// are distinct from a mismatch under a successfully parsed key.
pub fn verify_dkim_sha256(
    algorithm: DkimAlgorithm,
    key: &[u8],
    header_digest: &[u8; 32],
    signature: &[u8],
) -> Result<DkimVerification, Error> {
    provider(|| {
        let (key_bits, verified) = match algorithm {
            DkimAlgorithm::Rsa => {
                let raw = rsa_key(key).map_err(|_| Error::Invalid)?;
                rsa::verify_dkim_sha256(raw, header_digest, signature)
                    .map_err(|_| Error::Invalid)?
            }
            DkimAlgorithm::Ed25519 => {
                let key = key.try_into().map_err(|_| Error::Invalid)?;
                let verified = ed25519::verify_untrusted(key, header_digest, signature)
                    .map_err(|_| Error::Invalid)?;
                (256, verified)
            }
        };
        Ok(DkimVerification { key_bits, verified })
    })
}

// RFC 6376 section 3.6.1 specifies PKCS#1. RFC 8463 Appendix A.2
// illustrates the deployed rsaEncryption SubjectPublicKeyInfo spelling.
fn rsa_key(key: &[u8]) -> Result<&[u8], crate::TlsError> {
    let mut input = Der(key);
    let mut body = Der(input.take(0x30)?.value);
    input.end()?;
    if body.0.first() == Some(&0x02) {
        return Ok(key);
    }
    if body.take(0x30)?.value != RSA_KEY {
        return Err(crate::TlsError::Invalid);
    }
    let raw = body
        .take(0x03)?
        .value
        .strip_prefix(&[0])
        .ok_or(crate::TlsError::Invalid)?;
    body.end()?;
    Ok(raw)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::certificate_fixtures::{der, seq, unhex, SIGNATURE_ANSWERS};
    use td_primitives::sha::{Digest, Sha256};

    // RFC 8463 Appendix A.2 and A.3. Digests use the relaxed headers,
    // followed by the signature header with an empty b= and no final CRLF.
    const RSA_KEY_HEX: &str = concat!(
        "30819f300d06092a864886f70d010101050003818d0030818902818100",
        "e41e5390a014f3591886b3957a3693f789d63a5a4d3c6b9aa1751eb0cb96dd38",
        "a6f68cb42263f275e89622d10bf1fdbc98cc235bdd6f75cb716eb400837f3c10",
        "fc10bf7bff2e06c091fb2cf52f18fe3cbea51efa8c2ab337ac6e21b2d4f9423",
        "bc73bd3f3a316725582f305f3b611e0b7229dc6fb692bc8e4c8293fa5fcae30dd0203010001"
    );
    const RSA_SIG: &str = concat!(
        "178e5d5560df31b40318724595750d0761ca7db09e2d1ca10d7805a442fc1b0a",
        "6c45ed087888b13537b70e1095954ad28d5e01c1c55c385ebe145dd9a3d469a35b",
        "4539e4bd98f6104e719d6921ac81e79f3ac936273740f8bd744de62df418037d",
        "0359b4a74dd563bffca808a973fb39d1051382e315835c531650e0500363ef"
    );

    #[test]
    fn dkim_verifies_rfc_8463_digests_and_records_key_size() {
        for (alg, key, sig, digest, bits) in [
            (
                DkimAlgorithm::Rsa,
                RSA_KEY_HEX,
                RSA_SIG,
                "fed71e6d70fb123bb5c353adc42821b304b6b9d54fc867f304f7c61daaaa1111",
                1024,
            ),
            (
                DkimAlgorithm::Ed25519,
                "d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a",
                concat!(
                    "fe00ab8a7a5c40ea087ee1cd4086eae29821f64c882b701051db7d39da907a14",
                    "b084422e8380f5d41bac15addb4f7158e4eb14ed96e72842eaf9e5dda754350f"
                ),
                "48ce9a2c710ece1710ff156996b836a7f45470e43efe5643074d6e1690ed62e7",
                256,
            ),
        ] {
            let key = unhex(key);
            let sig = unhex(sig);
            let digest: [u8; 32] = unhex(digest).try_into().unwrap();
            let verify =
                |digest, signature: &[u8]| verify_dkim_sha256(alg, &key, &digest, signature);
            assert_eq!(
                verify(digest, &sig),
                Ok(DkimVerification {
                    key_bits: bits,
                    verified: true
                })
            );
            assert_eq!(
                verify([0; 32], &sig),
                Ok(DkimVerification {
                    key_bits: bits,
                    verified: false
                })
            );
            assert!(!verify(digest, &sig[1..]).unwrap().verified);
            if alg == DkimAlgorithm::Rsa {
                let raw = rsa_key(&key).unwrap();
                assert!(
                    verify_dkim_sha256(alg, raw, &digest, &sig)
                        .unwrap()
                        .verified
                );
                // The RSA constructor used by TLS certificates and OpenPGP
                // continues to refuse the very same correctly signed key.
                assert!(rsa::PublicKey::from_der(raw).is_err());
            }
        }
    }

    #[test]
    fn dkim_verifies_2048_bit_rsa() {
        let mut row = SIGNATURE_ANSWERS
            .lines()
            .map(str::split_whitespace)
            .find(|fields| fields.clone().next() == Some("RSA_PKCS1_SHA256"))
            .unwrap()
            .skip(1);
        let key = unhex(row.next().unwrap());
        let digest = Sha256::digest(&unhex(row.next().unwrap()));
        let sig = unhex(row.next().unwrap());
        assert_eq!(
            verify_dkim_sha256(DkimAlgorithm::Rsa, &key, &digest, &sig),
            Ok(DkimVerification {
                key_bits: 2048,
                verified: true
            })
        );
    }

    // Python big integers: n = (2^1279 - 1)(2^607 - 1), e = 65537.
    // Both factors are known Mersenne primes. Apply pow(EM, d, n) to the
    // RFC 8017 SHA-256 DigestInfo for the test message with d = e^-1 mod
    // lcm(p-1,q-1). The modulus spans 1,886 bits in 236 encoded bytes.
    #[test]
    fn dkim_verifies_a_sub_2048_non_byte_aligned_modulus() {
        let modulus = unhex(concat!(
            "3fffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff",
            "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff",
            "ffffffffffffffffffffffff7fffffffffffffffffffffffffffffffffffffff",
            "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff",
            "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff",
            "8000000000000000000000000000000000000000000000000000000000000000",
            "0000000000000000000000000000000000000000000000000000000000000000",
            "000000000000000000000001"
        ));
        let key = seq(&[der(2, &modulus), der(2, &[1, 0, 1])]);
        let signature = unhex(concat!(
            "2e200ab574cd5620921ee4ec6e167d87cbeaa35b6c5451a816a3fe16caada896",
            "cf883d67e71f5bebf806cb0e49067b95644cb7e568fbdcce5e5a46dd357ac3d5",
            "c88a55c242349454a61e67053b5e0c069dbf751929fa47f523b865da9239d60a",
            "6570d9cf3f958ae95e0c84426e00ce99f799c9584b0c750d91f0d433573eb4c1",
            "f33dc1ddd3fbb374880d8cd73920f5f80d1d411142c25c8f33b8cf163d41304f",
            "279dcd8bfd5cecb8b4c9ce114f96b697095abd20b0e5411d1a4063affd1210d7",
            "6b5be83c42520f58c10354af5b9e95efcd8b6b3f75ae4b5210791bd4e58e0f34",
            "9f8541376ec58d9ff43dcd5a"
        ));
        let digest = Sha256::digest(b"DKIM odd modulus fixture");
        assert_eq!(
            verify_dkim_sha256(DkimAlgorithm::Rsa, &key, &digest, &signature),
            Ok(DkimVerification {
                key_bits: 1886,
                verified: true
            })
        );
        assert!(rsa::PublicKey::from_der(&key).is_err());
    }

    #[test]
    fn dkim_counts_modulus_bits_and_enforces_both_ends() {
        for bits in [1023usize, 1024, 1025, 1537, 2047, 2048, 4095, 4096, 4097] {
            let mut n = vec![0x5b; bits.div_ceil(8)];
            n[0] = 1 << ((bits - 1) % 8);
            if bits % 8 == 0 {
                n.insert(0, 0);
            }
            let key = seq(&[der(2, &n), der(2, &[1, 0, 1])]);
            let observed = verify_dkim_sha256(DkimAlgorithm::Rsa, &key, &[0; 32], &[]);
            if (1024..=4096).contains(&bits) {
                assert_eq!(
                    observed,
                    Ok(DkimVerification {
                        key_bits: bits as u16,
                        verified: false
                    })
                );
            } else {
                assert_eq!(observed, Err(Error::Invalid));
            }
        }
    }

    #[test]
    fn dkim_refuses_malformed_and_small_order_keys() {
        let key = unhex(RSA_KEY_HEX);
        let raw = rsa_key(&key).unwrap();
        let spki = |alg: &[u8], pad, tail: &[u8]| {
            seq(&[der(0x30, alg), der(3, &[&[pad], raw, tail].concat())])
        };
        for bad in [
            [&key[..], &[0]].concat(),
            spki(crate::certificate_algorithms::ED25519_KEY, 0, &[]),
            spki(RSA_KEY, 1, &[]),
            spki(RSA_KEY, 0, &[0]),
            raw[1..].to_vec(),
        ] {
            assert_eq!(
                verify_dkim_sha256(DkimAlgorithm::Rsa, &bad, &[0; 32], &[]),
                Err(Error::Invalid)
            );
        }
        let mut not_a_point = [0; 32];
        not_a_point[0] = 2;
        for bad in [&[0u8; 31][..], &[0; 32], &not_a_point] {
            assert_eq!(
                verify_dkim_sha256(DkimAlgorithm::Ed25519, bad, &[0; 32], &[0; 64]),
                Err(Error::Invalid)
            );
        }
    }
}

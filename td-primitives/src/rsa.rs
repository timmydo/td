//! RSA signature verification (RFC 8017): PKCS#1 v1.5 and PSS. Everything here
//! is public, so it may branch. There is deliberately no decryption and no
//! signing: td holds no RSA private key, and those are the paths RSA's timing
//! attacks live on.

use crate::bignum::{add_mod, less_mask, mont_mul, negated_inverse};
use crate::sha::{Digest, Sha256, Sha384, Sha512};
use crate::{der, Refused};

const MIN_MODULUS_BITS: usize = 2048;
const MAX_MODULUS_BITS: usize = 8192;
/// Refused above this, as `ring` did: no CA issues them, and it bounds the work.
const MAX_EXPONENT_BITS: u32 = 33;

/// A hash RSA signatures name, with its PKCS#1 DigestInfo prefix.
pub trait RsaHash: Digest {
    const DIGEST_INFO: &'static [u8];
}

impl RsaHash for Sha256 {
    const DIGEST_INFO: &'static [u8] = &[
        0x30, 0x31, 0x30, 0x0d, 0x06, 0x09, 0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x01,
        0x05, 0x00, 0x04, 0x20,
    ];
}

impl RsaHash for Sha384 {
    const DIGEST_INFO: &'static [u8] = &[
        0x30, 0x41, 0x30, 0x0d, 0x06, 0x09, 0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x02,
        0x05, 0x00, 0x04, 0x30,
    ];
}

impl RsaHash for Sha512 {
    const DIGEST_INFO: &'static [u8] = &[
        0x30, 0x51, 0x30, 0x0d, 0x06, 0x09, 0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x03,
        0x05, 0x00, 0x04, 0x40,
    ];
}

pub struct PublicKey {
    /// Little-endian limbs.
    modulus: Vec<u64>,
    n0: u64,
    r2: Vec<u64>,
    exponent: u64,
    modulus_bits: usize,
}

impl PublicKey {
    /// A PKCS#1 `RSAPublicKey`.
    pub fn from_der(der_key: &[u8]) -> Result<Self, Refused> {
        let (n, e) = der::two_integers(der_key)?;
        Self::from_parts(n, e)
    }

    /// Big-endian modulus and exponent with no leading zero byte: 2048 to 8192
    /// bits, and an odd exponent of at least 3 and at most 33 bits.
    pub fn from_parts(n: &[u8], e: &[u8]) -> Result<Self, Refused> {
        let &first = n.first().ok_or(Refused)?;
        if first == 0 || e.first().is_none_or(|high| *high == 0) || e.len() > 8 {
            return Err(Refused);
        }
        let modulus_bits = 8 * n.len() - first.leading_zeros() as usize;
        if !(MIN_MODULUS_BITS..=MAX_MODULUS_BITS).contains(&modulus_bits) {
            return Err(Refused);
        }
        let exponent = e
            .iter()
            .fold(0u64, |sum, byte| (sum << 8) | u64::from(*byte));
        if exponent < 3 || exponent & 1 == 0 || 64 - exponent.leading_zeros() > MAX_EXPONENT_BITS {
            return Err(Refused);
        }
        let modulus = limbs_from_be(n, n.len().div_ceil(8))?;
        if modulus.first().is_none_or(|low| low & 1 == 0) {
            return Err(Refused);
        }
        let n0 = negated_inverse(modulus.first().copied().unwrap_or_default());
        let r2 = r_squared(&modulus);
        Ok(Self {
            modulus,
            n0,
            r2,
            exponent,
            modulus_bits,
        })
    }

    /// The modulus length in bytes, which is every signature's length.
    fn len(&self) -> usize {
        self.modulus_bits.div_ceil(8)
    }

    /// RSAVP1: signature^e mod n, as `len()` big-endian bytes. A signature of
    /// another length or at or above n is refused.
    fn public_operation(&self, signature: &[u8]) -> Result<Vec<u8>, Refused> {
        if signature.len() != self.len() {
            return Err(Refused);
        }
        let limbs = self.modulus.len();
        let s = limbs_from_be(signature, limbs)?;
        if less_mask(&s, &self.modulus) == 0 {
            return Err(Refused);
        }
        let mut base = vec![0u64; limbs];
        mont_mul(&mut base, &s, &self.r2, &self.modulus, self.n0);
        let mut result = base.clone();
        let mut scratch = vec![0u64; limbs];
        let top = 63 - self.exponent.leading_zeros();
        for bit in (0..top).rev() {
            mont_mul(&mut scratch, &result, &result, &self.modulus, self.n0);
            std::mem::swap(&mut result, &mut scratch);
            if (self.exponent >> bit) & 1 == 1 {
                mont_mul(&mut scratch, &result, &base, &self.modulus, self.n0);
                std::mem::swap(&mut result, &mut scratch);
            }
        }
        let mut unit = vec![0u64; limbs];
        if let Some(low) = unit.first_mut() {
            *low = 1;
        }
        mont_mul(&mut scratch, &result, &unit, &self.modulus, self.n0);
        let mut out = vec![0u8; self.len()];
        crate::bignum::to_be(&scratch, &mut out);
        Ok(out)
    }

    /// RSASSA-PKCS1-v1_5-VERIFY. It builds the one valid encoding and compares
    /// whole, never parsing the signature's: parsing is what Bleichenbacher's
    /// 2006 forgery exploited.
    pub fn verify_pkcs1<D: RsaHash>(
        &self,
        message: &[u8],
        signature: &[u8],
    ) -> Result<(), Refused> {
        let recovered = self.public_operation(signature)?;
        let digest = D::digest(message);
        let tail = D::DIGEST_INFO.len() + D::OUTPUT_LEN;
        // RFC 8017 asks for at least 8 bytes of padding, which the 2048-bit floor
        // leaves with room to spare under every hash here.
        let padding = self.len().checked_sub(tail + 3).ok_or(Refused)?;
        let mut expected = Vec::with_capacity(self.len());
        expected.extend_from_slice(&[0, 1]);
        expected.resize(2 + padding, 0xff);
        expected.push(0);
        expected.extend_from_slice(D::DIGEST_INFO);
        expected.extend_from_slice(digest.as_ref());
        if expected == recovered {
            Ok(())
        } else {
            Err(Refused)
        }
    }

    /// RSASSA-PSS-VERIFY with MGF1 over the same hash and a salt of the hash's
    /// length, the only form TLS 1.3 allows (RFC 8446 4.2.3).
    pub fn verify_pss<D: Digest>(&self, message: &[u8], signature: &[u8]) -> Result<(), Refused> {
        let recovered = self.public_operation(signature)?;
        let em_bits = self.modulus_bits - 1;
        let em_len = em_bits.div_ceil(8);
        // RFC 8017 8.1.2: when emBits is a multiple of 8 the leading byte is
        // not part of EM and must be zero.
        let em = recovered.get(recovered.len() - em_len..).ok_or(Refused)?;
        let lead = recovered.get(..recovered.len() - em_len).ok_or(Refused)?;
        if lead.iter().any(|byte| *byte != 0) {
            return Err(Refused);
        }

        let hash_len = D::OUTPUT_LEN;
        let salt_len = hash_len;
        if em_len < hash_len + salt_len + 2 {
            return Err(Refused);
        }
        let db_len = em_len - hash_len - 1;
        let (masked_db, rest) = em.split_at_checked(db_len).ok_or(Refused)?;
        let (h, trailer) = rest.split_at_checked(hash_len).ok_or(Refused)?;
        if trailer != [0xbc] {
            return Err(Refused);
        }
        let top_bits = 8 * em_len - em_bits;
        let top_mask = 0xffu8.checked_shr(top_bits as u32).unwrap_or(0);
        if masked_db.first().is_none_or(|first| first & !top_mask != 0) {
            return Err(Refused);
        }

        let mut db = masked_db.to_vec();
        mgf1_xor::<D>(h, &mut db);
        if let Some(first) = db.first_mut() {
            *first &= top_mask;
        }
        let zeros = db_len - salt_len - 1;
        let (padding, marked) = db.split_at_checked(zeros).ok_or(Refused)?;
        let (marker, salt) = marked.split_first().ok_or(Refused)?;
        if padding.iter().any(|byte| *byte != 0) || *marker != 1 {
            return Err(Refused);
        }

        let mut hash = D::new();
        hash.update(&[0; 8]);
        hash.update(D::digest(message).as_ref());
        hash.update(salt);
        if hash.finalize().as_ref() == h {
            Ok(())
        } else {
            Err(Refused)
        }
    }
}

fn mgf1_xor<D: Digest>(seed: &[u8], out: &mut [u8]) {
    let mut counter = 0u32;
    for chunk in out.chunks_mut(D::OUTPUT_LEN) {
        let mut hash = D::new();
        hash.update(seed);
        hash.update(&counter.to_be_bytes());
        for (byte, mask) in chunk.iter_mut().zip(hash.finalize().as_ref()) {
            *byte ^= mask;
        }
        counter = counter.wrapping_add(1);
    }
}

fn limbs_from_be(bytes: &[u8], limbs: usize) -> Result<Vec<u64>, Refused> {
    if bytes.len() > limbs * 8 {
        return Err(Refused);
    }
    let mut out = Vec::with_capacity(limbs);
    for chunk in bytes.rchunks(8) {
        let mut word = [0u8; 8];
        let start = word.len() - chunk.len();
        if let Some(room) = word.get_mut(start..) {
            room.copy_from_slice(chunk);
        }
        out.push(u64::from_be_bytes(word));
    }
    out.resize(limbs, 0);
    Ok(out)
}

/// R^2 mod n by doubling from 1, for R = 2^(64 * limbs).
fn r_squared(modulus: &[u64]) -> Vec<u64> {
    let mut value = vec![0u64; modulus.len()];
    if let Some(low) = value.first_mut() {
        *low = 1;
    }
    let mut next = vec![0u64; modulus.len()];
    for _ in 0..2 * 64 * modulus.len() {
        add_mod(&mut next, &value, &value, modulus);
        std::mem::swap(&mut value, &mut next);
    }
    value
}

#[cfg(test)]
mod tests {
    #![allow(clippy::indexing_slicing, clippy::unwrap_used)]
    use super::PublicKey;

    #[test]
    fn keys_outside_the_bounds_are_refused() {
        let good: Vec<u8> = std::iter::once(0xc0)
            .chain(std::iter::repeat_n(0x11, 255))
            .collect();
        let exponent = [1u8, 0, 1];
        assert!(PublicKey::from_parts(&good, &exponent).is_ok());
        // Short, long, even, leading zero.
        assert!(PublicKey::from_parts(&good[1..], &exponent).is_err());
        assert!(PublicKey::from_parts(&vec![0xff; 1025], &exponent).is_err());
        let mut even = good.clone();
        *even.last_mut().unwrap() = 0x10;
        assert!(PublicKey::from_parts(&even, &exponent).is_err());
        let mut padded = vec![0u8];
        padded.extend_from_slice(&good);
        assert!(PublicKey::from_parts(&padded, &exponent).is_err());
        // Exponents of 1, even, leading zero, and above 33 bits.
        assert!(PublicKey::from_parts(&good, &[1]).is_err());
        assert!(PublicKey::from_parts(&good, &[4]).is_err());
        assert!(PublicKey::from_parts(&good, &[0, 3]).is_err());
        assert!(PublicKey::from_parts(&good, &[3, 0, 0, 0, 1]).is_err());
        assert!(PublicKey::from_parts(&good, &[1, 0, 0, 0, 1]).is_ok());
    }

    #[test]
    fn a_signature_of_the_wrong_length_or_at_the_modulus_is_refused() {
        let modulus: Vec<u8> = std::iter::once(0xc0)
            .chain(std::iter::repeat_n(0x11, 255))
            .collect();
        let key = PublicKey::from_parts(&modulus, &[1, 0, 1]).unwrap();
        assert!(key.public_operation(&modulus).is_err());
        assert!(key.public_operation(&[0; 255]).is_err());
        assert!(key.public_operation(&[0; 257]).is_err());
        assert_eq!(key.public_operation(&[0; 256]).unwrap(), vec![0; 256]);
        let mut one = vec![0u8; 256];
        one[255] = 1;
        assert_eq!(key.public_operation(&one).unwrap(), one);
    }
}

//! The engine's SHA-256 and the shared digest, HMAC-SHA256 and HKDF
//! (`hmac.rs`), by the names td-secret's store crypto also gives them, so
//! a file td-firstboot and td-portal compile beside that module reads the
//! same calls.

#[path = "../../engine/src/sha256.rs"]
#[allow(
    dead_code,
    reason = "the shared hash also supports build artifact files"
)]
mod sha256;
#[path = "hmac.rs"]
mod sha256_hmac;

pub(crate) use sha256_hmac::{digest, hkdf, hmac};
pub use sha256_hmac::{
    hkdf_expand, hmac_sha256, pbkdf2_sha256, HKDF_MAX_OKM, PBKDF2_MAX_ITERATIONS, PBKDF2_MAX_OUTPUT,
};

/// Whether two secrets are equal, comparing every byte whatever differs.
pub fn same(a: &[u8], b: &[u8]) -> bool {
    let difference = a
        .iter()
        .zip(b)
        .fold(0u8, |difference, (a, b)| difference | (a ^ b));
    a.len() == b.len() && std::hint::black_box(difference) == 0
}

/// Overwrites a secret before it is dropped. Best effort in safe Rust:
/// `black_box` keeps the stores, not a guarantee about the optimizer.
pub(crate) fn zero(bytes: &mut [u8]) {
    bytes.fill(0);
    std::hint::black_box(bytes);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_compares_length_and_every_byte() {
        assert!(same(&[1, 2, 3], &[1, 2, 3]));
        assert!(same(&[], &[]));
        for other in [&[1, 2, 4][..], &[0, 2, 3], &[1, 2], &[1, 2, 3, 0]] {
            assert!(!same(&[1, 2, 3], other), "{other:?}");
        }
    }

    #[test]
    fn zero_clears_every_byte() {
        let mut secret = [0xa5; 33];
        zero(&mut secret);
        assert_eq!(secret, [0; 33]);
    }
}

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

pub use sha256_hmac::hmac_sha256;
pub(crate) use sha256_hmac::{digest, hkdf, hmac};

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
    fn zero_clears_every_byte() {
        let mut secret = [0xa5; 33];
        zero(&mut secret);
        assert_eq!(secret, [0; 33]);
    }
}

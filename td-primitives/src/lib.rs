//! td's own cryptographic primitives. DESIGN.md is normative: it says what
//! is here, what each one claims and what none of them claims.

pub mod aes_gcm;
mod bignum;
pub mod chacha20_poly1305;
pub mod ct;
pub mod curve25519;
mod der;
pub mod ec;
pub mod hmac;
pub mod random;
pub mod rsa;
pub mod sha;

/// A check failed or an input is out of bounds. It deliberately says no more:
/// a reason is an oracle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Refused;

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("refused by a cryptographic check")
    }
}

impl std::error::Error for Refused {}

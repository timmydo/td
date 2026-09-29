//! Provider-independent cryptographic contracts; no backend is implemented yet.
//!
//! These checks reject the two named root exports; backend integration must
//! also check nested exports, aliases and public signatures.
//! ```compile_fail,E0432
//! use td_crypto::rustls;
//! ```
//! ```compile_fail,E0432
//! use td_crypto::aws_lc_rs;
//! ```
#![forbid(unsafe_code)]

/// Fixed failures carry neither backend diagnostics nor secret input.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Capacity,
    Invalid,
    Entropy,
    Crypto,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let message = match self {
            Self::Capacity => "cryptographic output capacity exceeded",
            Self::Invalid => "invalid cryptographic input",
            Self::Entropy => "secure randomness unavailable",
            Self::Crypto => "cryptographic operation failed",
        };
        f.write_str(message)
    }
}

impl std::error::Error for Error {}

pub trait Entropy {
    fn fill(&mut self, output: &mut [u8]) -> Result<(), Error>;
}
pub trait Digest: Send {
    fn update(&mut self, bytes: &[u8]) -> Result<(), Error>;
    fn finish(self) -> Result<[u8; 32], Error>;
}
pub trait Crypto: Send + Sync {
    type Sha256: Digest;
    type SigningKey: Send + Sync;
    fn sha256(&self) -> Self::Sha256;
    /// Provider-backed constant-time equality for fixed-size digests.
    fn equal_digest(&self, left: &[u8; 32], right: &[u8; 32]) -> bool;
    /// Cold path only; output is a complete PKCS#8 P-256 private key.
    fn generate_p256(&self, output: &mut [u8]) -> Result<usize, Error>;
    /// Cold path only; callers account for the key's retained provider storage.
    fn load_p256(&self, pkcs8: &[u8]) -> Result<Self::SigningKey, Error>;
    /// Uncompressed SEC1 point: 0x04 followed by fixed-width X and Y.
    fn p256_public(&self, key: &Self::SigningKey, output: &mut [u8; 65]) -> Result<(), Error>;
    /// SHA-256 ECDSA signature in fixed-width r||s form, without DER wrapping.
    fn sign_es256(
        &self,
        key: &Self::SigningKey,
        message: &[u8],
        output: &mut [u8; 64],
    ) -> Result<(), Error>;
}

#[cfg(test)]
mod tests {
    #[test]
    fn contracts_have_no_dependency_tables() {
        // The shared roster gate rejects alternate TOML spellings; this also
        // refuses local dependencies until backend admission changes the rule.
        for line in include_str!("../Cargo.toml").lines().map(str::trim) {
            if line.starts_with('[') {
                assert!(!line.contains("dependencies"));
            }
        }
    }
}

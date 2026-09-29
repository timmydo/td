//! Opaque SHA-256 and entropy APIs over the private AWS-LC backend.
//! Signing-key operations and TLS sessions remain unimplemented.
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

#[cfg(panic = "abort")]
compile_error!("td-crypto requires panic=unwind for its provider error boundary");

mod sha256;
pub use sha256::Sha256;
mod entropy;
pub use entropy::SystemEntropy;

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
/// Streaming SHA-256. After an update error, discard the operation.
/// Implementations must refuse every later update and finish, including mocks.
pub trait Digest: Send {
    fn update(&mut self, bytes: &[u8]) -> Result<(), Error>;
    fn finish(self) -> Result<[u8; 32], Error>;
}
pub trait Crypto: Send + Sync {
    type Sha256: Digest;
    type SigningKey: Send + Sync;
    /// Cold creation may fail before a digest state is available.
    fn sha256(&self) -> Result<Self::Sha256, Error>;
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
mod tls_smoke;

#[cfg(test)]
mod tests {
    #[test]
    fn explicit_aws_provider_and_roots_construct_without_global_default(
    ) -> Result<(), rustls::Error> {
        assert!(rustls::crypto::CryptoProvider::get_default().is_none());
        let provider = std::sync::Arc::new(rustls::crypto::aws_lc_rs::default_provider());
        let roots =
            rustls::RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        assert!(!roots.is_empty());
        let _config = rustls::ClientConfig::builder_with_provider(provider)
            .with_protocol_versions(&[&rustls::version::TLS13, &rustls::version::TLS12])?
            .with_root_certificates(roots)
            .with_no_client_auth();
        assert!(rustls::crypto::CryptoProvider::get_default().is_none());
        Ok(())
    }

    #[test]
    fn admitted_native_backend_sha256_smoke() {
        let digest = aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA256, b"abc");
        assert_eq!(
            digest.as_ref(),
            &[
                0xba, 0x78, 0x16, 0xbf, 0x8f, 0x01, 0xcf, 0xea, 0x41, 0x41, 0x40, 0xde, 0x5d, 0xae,
                0x22, 0x23, 0xb0, 0x03, 0x61, 0xa3, 0x96, 0x17, 0x7a, 0x9c, 0xb4, 0x10, 0xff, 0x61,
                0xf2, 0x00, 0x15, 0xad,
            ]
        );
    }
}

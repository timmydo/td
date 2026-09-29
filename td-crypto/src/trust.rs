//! Cold trust-anchor construction; no peer identity or authorization evidence.
use crate::{
    certificate::Certificate, certificate_algorithms, Error, PemCertificates, TlsError,
    CERTIFICATE_DER_CAPACITY,
};
use rustls::pki_types::CertificateDer;
use std::panic::catch_unwind;

/// Owned immutable anchors from one explicit source. Construction does not
/// authenticate a peer; configurations apply role, name, time and usage policy.
pub struct TrustStore {
    pub(super) roots: rustls::RootCertStore,
    public: bool,
}

impl TrustStore {
    /// Load a complete bounded private CA bundle, without public-root fallback.
    /// EKU, path-length and name constraints are refused in this initial
    /// private-CA subset; they must not be silently discarded at an anchor.
    pub fn from_pem(input: &[u8]) -> Result<Self, TlsError> {
        boundary(|| explicit(input))
    }

    /// Load the compiled, checksum-pinned public server root set.
    pub fn public_roots() -> Result<Self, TlsError> {
        boundary(|| {
            let mut roots = rustls::RootCertStore::empty();
            roots
                .roots
                .try_reserve_exact(webpki_roots::TLS_SERVER_ROOTS.len())
                .map_err(|_| TlsError::Capacity)?;
            roots
                .roots
                .extend_from_slice(webpki_roots::TLS_SERVER_ROOTS);
            Ok(Self {
                roots,
                public: true,
            })
        })
    }

    pub fn anchor_count(&self) -> usize {
        self.roots.len()
    }

    /// Public roots are for outbound server verification, never mandatory
    /// client-certificate authentication in a server configuration.
    pub fn uses_public_roots(&self) -> bool {
        self.public
    }
}

impl std::fmt::Debug for TrustStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TrustStore")
            .field("anchors", &self.roots.len())
            .field("public", &self.public)
            .finish_non_exhaustive()
    }
}

fn boundary(
    operation: impl FnOnce() -> Result<TrustStore, TlsError> + std::panic::UnwindSafe,
) -> Result<TrustStore, TlsError> {
    catch_unwind(operation).map_err(|_| TlsError::Crypto)?
}

fn syntax_error(error: Error) -> TlsError {
    match error {
        Error::Capacity => TlsError::Capacity,
        Error::Invalid => TlsError::Invalid,
        Error::Crypto | Error::Entropy => TlsError::Crypto,
    }
}

fn explicit(input: &[u8]) -> Result<TrustStore, TlsError> {
    let mut pem = PemCertificates::trust_bundle(input).map_err(syntax_error)?;
    let mut roots = rustls::RootCertStore::empty();
    roots
        .roots
        .try_reserve_exact(pem.remaining())
        .map_err(|_| TlsError::Capacity)?;
    certificate_algorithms::certificate_algorithms()?;
    let mut keys = Vec::new();
    keys.try_reserve_exact(pem.remaining())
        .map_err(|_| TlsError::Capacity)?;
    let mut scratch = Vec::new();
    scratch
        .try_reserve_exact(CERTIFICATE_DER_CAPACITY)
        .map_err(|_| TlsError::Capacity)?;
    scratch.resize(CERTIFICATE_DER_CAPACITY, 0);
    while let Some(length) = pem.decode_next(&mut scratch).map_err(syntax_error)? {
        let bytes = scratch.get(..length).ok_or(TlsError::Crypto)?;
        let certificate = Certificate::parse(bytes)?;
        if !certificate.ca || certificate.key_usage.is_some_and(|bits| bits & 0x0400 == 0) {
            return Err(TlsError::Invalid);
        }
        if certificate.path_length.is_some()
            || certificate.extended_usage.is_some()
            || certificate.has_name_constraints
        {
            return Err(TlsError::Invalid);
        }
        let canonical = certificate_algorithms::canonical_key(&certificate)?;
        let der = CertificateDer::from(bytes);
        rustls::server::ParsedCertificate::try_from(&der).map_err(|_| TlsError::Invalid)?;
        roots.add(der).map_err(|_| TlsError::Invalid)?;
        let last = roots.roots.last().ok_or(TlsError::Crypto)?;
        let previous = roots
            .roots
            .get(..roots.roots.len().checked_sub(1).ok_or(TlsError::Crypto)?)
            .ok_or(TlsError::Crypto)?;
        if previous
            .iter()
            .zip(&keys)
            .any(|(anchor, key)| anchor.subject == last.subject && *key == canonical)
        {
            return Err(TlsError::Invalid);
        }
        keys.push(canonical);
    }
    Ok(TrustStore {
        roots,
        public: false,
    })
}

#[cfg(test)]
#[path = "trust_tests.rs"]
mod tests;

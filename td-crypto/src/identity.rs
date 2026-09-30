//! Cold local identity admission, separate from remote trust and authorization.
use crate::{
    certificate::Certificate, certificate_algorithms, Crypto, Error, P256Key, PemCertificates,
    Provider, TlsError, VerificationFailure, CERTIFICATE_DER_CAPACITY,
};
use rustls::pki_types::{CertificateDer, ServerName, SignatureVerificationAlgorithm, UnixTime};
use std::{panic::catch_unwind, sync::Arc, time::Duration};

/// An owned local key, certificate chain and exact DNS bindings.
/// Admission checks local consistency; it establishes no remote trust or mail
/// authorization. Native allocations and failure limits still apply.
pub struct ServerIdentity {
    pub(super) certified: Arc<rustls::sign::CertifiedKey>,
    names: Vec<String>,
    key: Arc<P256Key>,
    not_before: u64,
    not_after: u64,
}

// The erased SigningKey is exclusively our adapter: immutable public data
// and a mutex-fenced key that retires before an unwind returns.
impl std::panic::UnwindSafe for ServerIdentity {}
impl std::panic::RefUnwindSafe for ServerIdentity {}

impl ServerIdentity {
    /// Cold admission of a bounded PEM chain and P-256 key, with 1..=32 names.
    /// Time is UTC seconds since Unix epoch; None refuses admission. Returned
    /// errors publish no identity, and no caller input slice is retained.
    pub fn from_pem(
        chain: &[u8],
        key: &[u8],
        names: &[&str],
        now: Option<u64>,
    ) -> Result<Self, TlsError> {
        admission_boundary(|| admit(chain, key, names, now))
    }

    /// Number of certificates retained in their supplied leaf-first order.
    pub fn certificate_count(&self) -> usize {
        self.certified.cert.len()
    }

    /// Public certificate bytes for inspection; out-of-range indices yield None.
    pub fn certificate_der(&self, index: usize) -> Option<&[u8]> {
        self.certified.cert.get(index).map(|der| der.as_ref())
    }

    /// Number of checked exact DNS bindings.
    pub fn name_count(&self) -> usize {
        self.names.len()
    }

    /// A folded ASCII binding; out-of-range indices yield None.
    pub fn name(&self, index: usize) -> Option<&str> {
        self.names.get(index).map(String::as_str)
    }

    /// Inclusive UTC validity intersection of every supplied certificate.
    pub fn validity(&self) -> (u64, u64) {
        (self.not_before, self.not_after)
    }

    /// Recheck time and key health. Admission does not grant permanent validity.
    pub fn check_validity(&self, now: Option<u64>) -> Result<(), TlsError> {
        check_time(now, self.not_before, self.not_after)?;
        let mut point = [0; 65];
        Provider
            .p256_public(&self.key, &mut point)
            .map_err(crypto_error)
    }
}

fn admission_boundary(
    operation: impl FnOnce() -> Result<ServerIdentity, TlsError> + std::panic::UnwindSafe,
) -> Result<ServerIdentity, TlsError> {
    catch_unwind(operation).map_err(|_| TlsError::Crypto)?
}

impl std::fmt::Debug for ServerIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServerIdentity")
            .field("certificates", &self.certified.cert.len())
            .field("names", &self.names.len())
            .finish_non_exhaustive()
    }
}

fn check_time(now: Option<u64>, start: u64, end: u64) -> Result<(), TlsError> {
    let now = now.ok_or(TlsError::Clock)?;
    if now < start {
        return Err(TlsError::Verification(VerificationFailure::NotYetValid));
    }
    if now > end {
        return Err(TlsError::Verification(VerificationFailure::Expired));
    }
    Ok(())
}

fn crypto_error(error: Error) -> TlsError {
    match error {
        Error::Capacity => TlsError::Capacity,
        Error::Invalid => TlsError::Invalid,
        Error::Entropy | Error::Crypto => TlsError::Crypto,
    }
}

pub(super) fn validate_name(name: &str) -> Result<(), TlsError> {
    if name.is_empty()
        || name.len() > 253
        || !name.is_ascii()
        || name.ends_with('.')
        || name.parse::<std::net::IpAddr>().is_ok()
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-'))
    {
        return Err(TlsError::Invalid);
    }
    rustls::pki_types::DnsName::try_from(name).map_err(|_| TlsError::Invalid)?;
    Ok(())
}

fn names(input: &[&str]) -> Result<Vec<String>, TlsError> {
    if input.is_empty() || input.len() > 32 {
        return Err(TlsError::Invalid);
    }
    let mut names = Vec::new();
    names
        .try_reserve_exact(input.len())
        .map_err(|_| TlsError::Capacity)?;
    for &name in input {
        validate_name(name)?;
        let mut folded = String::new();
        folded
            .try_reserve_exact(name.len())
            .map_err(|_| TlsError::Capacity)?;
        for byte in name.bytes() {
            folded.push(char::from(byte.to_ascii_lowercase()));
        }
        if names.contains(&folded) {
            return Err(TlsError::Invalid);
        }
        names.push(folded);
    }
    Ok(names)
}

fn decoded_chain(input: &[u8]) -> Result<Vec<Vec<u8>>, TlsError> {
    let mut pem = PemCertificates::chain(input).map_err(crypto_error)?;
    let mut chain = Vec::new();
    chain
        .try_reserve_exact(pem.remaining())
        .map_err(|_| TlsError::Capacity)?;
    let mut scratch = Vec::new();
    scratch
        .try_reserve_exact(CERTIFICATE_DER_CAPACITY)
        .map_err(|_| TlsError::Capacity)?;
    scratch.resize(CERTIFICATE_DER_CAPACITY, 0);
    while let Some(length) = pem.decode_next(&mut scratch).map_err(crypto_error)? {
        let der = scratch.get(..length).ok_or(TlsError::Crypto)?;
        if chain
            .iter()
            .any(|previous: &Vec<u8>| previous.as_slice() == der)
        {
            return Err(TlsError::Invalid);
        }
        let mut owned = Vec::new();
        owned
            .try_reserve_exact(length)
            .map_err(|_| TlsError::Capacity)?;
        owned.extend_from_slice(der);
        chain.push(owned);
    }
    Ok(chain)
}

fn pair(
    child: &Certificate<'_>,
    issuer: &Certificate<'_>,
    algorithms: &[&dyn SignatureVerificationAlgorithm],
) -> Result<(), TlsError> {
    if child.issuer != issuer.subject {
        return Err(TlsError::Invalid);
    }
    let algorithm = algorithms
        .iter()
        .find(|algorithm| {
            algorithm.public_key_alg_id().as_ref() == issuer.key_algorithm
                && algorithm.signature_alg_id().as_ref() == child.signature_algorithm
        })
        .ok_or(TlsError::Invalid)?;
    algorithm
        .verify_signature(issuer.public_key, child.tbs, child.signature)
        .map_err(|_| TlsError::Verification(VerificationFailure::Signature))
}

fn admission(
    chain: &[Vec<u8>],
    names: &[String],
    public: &[u8; 65],
    now: u64,
) -> Result<(u64, u64), TlsError> {
    let algorithms = certificate_algorithms::certificate_algorithms()?;
    let mut parsed = Vec::new();
    parsed
        .try_reserve_exact(chain.len())
        .map_err(|_| TlsError::Capacity)?;
    let mut keys = Vec::new();
    keys.try_reserve_exact(chain.len())
        .map_err(|_| TlsError::Capacity)?;
    for (index, der) in chain.iter().enumerate() {
        let certificate = Certificate::parse(der)?;
        let canonical = certificate_algorithms::canonical_key(&certificate)?;
        let native = CertificateDer::from(der.as_slice());
        rustls::server::ParsedCertificate::try_from(&native).map_err(|_| TlsError::Invalid)?;
        certificate.valid_at(now)?;
        certificate.server_usage(index != 0)?;
        if !algorithms
            .iter()
            .any(|algorithm| algorithm.public_key_alg_id().as_ref() == certificate.key_algorithm)
            || !algorithms.iter().any(|algorithm| {
                algorithm.signature_alg_id().as_ref() == certificate.signature_algorithm
            })
        {
            return Err(TlsError::Invalid);
        }
        keys.push(canonical);
        parsed.push(certificate);
    }
    let leaf = parsed.first().ok_or(TlsError::Invalid)?;
    if leaf.key_algorithm != certificate_algorithms::P256_KEY {
        return Err(TlsError::Invalid);
    }
    if leaf.public_key.len() != 65 || leaf.public_key.first() != Some(&4) {
        return Err(TlsError::Invalid);
    }
    if leaf.public_key != public {
        return Err(TlsError::KeyMismatch);
    }
    for (index, issuer) in parsed.iter().enumerate().skip(1) {
        if parsed
            .get(1..index)
            .ok_or(TlsError::Crypto)?
            .iter()
            .zip(keys.get(1..index).ok_or(TlsError::Crypto)?)
            .any(|(earlier, key)| earlier.subject == issuer.subject && Some(key) == keys.get(index))
        {
            return Err(TlsError::Invalid);
        }
    }
    for adjacent in parsed.windows(2) {
        let [child, issuer] = adjacent else {
            return Err(TlsError::Crypto);
        };
        pair(child, issuer, algorithms)?;
    }
    for (index, certificate) in parsed.iter().enumerate().skip(1) {
        let non_self_issued = parsed
            .get(1..index)
            .ok_or(TlsError::Crypto)?
            .iter()
            .filter(|certificate| certificate.issuer != certificate.subject)
            .count();
        if certificate
            .path_length
            .is_some_and(|limit| u64::try_from(non_self_issued).map_or(true, |count| count > limit))
        {
            return Err(TlsError::Verification(VerificationFailure::Usage));
        }
    }
    let last = parsed.last().ok_or(TlsError::Invalid)?;
    if last.issuer == last.subject {
        pair(last, last, algorithms)?;
    }
    let leaf_der = CertificateDer::from(chain.first().ok_or(TlsError::Invalid)?.as_slice());
    let native_leaf =
        rustls::server::ParsedCertificate::try_from(&leaf_der).map_err(|_| TlsError::Invalid)?;
    for name in names {
        let name = ServerName::try_from(name.as_str()).map_err(|_| TlsError::Invalid)?;
        rustls::client::verify_server_name(&native_leaf, &name)
            .map_err(crate::tls_error::certificate_error)?;
    }
    if chain.len() > 1 {
        let mut roots = rustls::RootCertStore::empty();
        roots
            .add(CertificateDer::from(
                chain.last().ok_or(TlsError::Crypto)?.as_slice(),
            ))
            .map_err(|_| TlsError::Invalid)?;
        let end = chain.len().checked_sub(1).ok_or(TlsError::Crypto)?;
        let mut middle = Vec::new();
        middle
            .try_reserve_exact(end)
            .map_err(|_| TlsError::Capacity)?;
        for der in chain.get(1..end).ok_or(TlsError::Crypto)? {
            middle.push(CertificateDer::from(der.as_slice()));
        }
        rustls::client::verify_server_cert_signed_by_trust_anchor(
            &native_leaf,
            &roots,
            &middle,
            UnixTime::since_unix_epoch(Duration::from_secs(now)),
            algorithms,
        )
        .map_err(crate::tls_error::certificate_error)?;
    }
    let start = parsed
        .iter()
        .map(|cert| cert.not_before)
        .max()
        .ok_or(TlsError::Crypto)?;
    let end = parsed
        .iter()
        .map(|cert| cert.not_after)
        .min()
        .ok_or(TlsError::Crypto)?;
    Ok((start, end))
}

fn admit(
    chain: &[u8],
    key: &[u8],
    bindings: &[&str],
    now: Option<u64>,
) -> Result<ServerIdentity, TlsError> {
    let now = now.ok_or(TlsError::Clock)?;
    let names = names(bindings)?;
    let chain = decoded_chain(chain)?;
    let key = Provider.load_p256_pem(key).map_err(crypto_error)?;
    let mut public = [0; 65];
    Provider
        .p256_public(&key, &mut public)
        .map_err(crypto_error)?;
    let (not_before, not_after) = admission(&chain, &names, &public, now)?;
    let key = Arc::new(key);
    let certified = Arc::new(crate::tls_signer::certified_key(chain, key.clone())?);
    Ok(ServerIdentity {
        certified,
        names,
        key,
        not_before,
        not_after,
    })
}

#[cfg(test)]
#[path = "identity_tests.rs"]
mod tests;

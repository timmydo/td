//! Immutable outbound configuration. Socket-free sessions are a later layer.
use crate::{tls_clock::BackendClock, ClockHandle, TlsError, TrustStore};
use rustls::{
    client::{
        danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
        WebPkiServerVerifier,
    },
    pki_types::{CertificateDer, ServerName, UnixTime},
    DigitallySignedStruct, SignatureScheme,
};
use std::sync::Arc;

/// Fixed application-protocol choice; no arbitrary ALPN list is accepted.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TlsProtocol {
    Http1,
    Smtp,
}

/// Opaque immutable outbound TLS policy. Construction performs no I/O and does
/// not authenticate a peer. Share the handle with Arc; sessions remain future work.
pub struct ClientConfig {
    pub(super) native: Arc<rustls::ClientConfig>,
    pub(super) clock: Arc<ClockHandle>,
    protocol: TlsProtocol,
}
impl ClientConfig {
    /// Cold construction from exactly one admitted trust store. Explicit roots
    /// replace public roots. No client key, system clock or OS trust is used.
    pub fn new(
        trust: &TrustStore,
        clock: Arc<ClockHandle>,
        protocol: TlsProtocol,
    ) -> Result<Self, TlsError> {
        std::panic::catch_unwind(|| build(trust, clock, protocol)).map_err(|_| TlsError::Crypto)?
    }
}
impl std::fmt::Debug for ClientConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClientConfig")
            .field("protocol", &self.protocol)
            .field("alpn_count", &self.native.alpn_protocols.len())
            .field("clock", &self.clock)
            .finish_non_exhaustive()
    }
}

fn build(
    trust: &TrustStore,
    clock: Arc<ClockHandle>,
    protocol: TlsProtocol,
) -> Result<ClientConfig, TlsError> {
    clock.now()?;
    let provider = Arc::new(crate::tls_policy::provider()?);
    let verifier = WebPkiServerVerifier::builder_with_provider(
        Arc::new(trust.roots.clone()),
        provider.clone(),
    )
    .build()
    .map_err(|_| TlsError::Crypto)?;
    let mut native =
        rustls::ClientConfig::builder_with_details(provider, Arc::new(BackendClock(clock.clone())))
            .with_protocol_versions(&[&rustls::version::TLS13, &rustls::version::TLS12])
            .map_err(|_| TlsError::Crypto)?
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(BoundedVerifier(verifier)))
            .with_no_client_auth();
    native.alpn_protocols = match protocol {
        TlsProtocol::Http1 => vec![b"http/1.1".to_vec()],
        TlsProtocol::Smtp => Vec::new(),
    };
    native.check_selected_alpn = true;
    native.enable_sni = true;
    native.resumption = rustls::client::Resumption::disabled();
    // The pinned fragmenter subtracts the five-byte record header here.
    native.max_fragment_size = Some(16_384 + 5);
    native.enable_early_data = false;
    native.require_ems = true;
    native.key_log = Arc::new(rustls::NoKeyLog {});
    native.enable_secret_extraction = false;
    native.cert_compressors.clear();
    native.cert_decompressors.clear();
    native.cert_compression_cache = Arc::new(rustls::compress::CompressionCache::Disabled);
    native.send_ticket_request = None;
    Ok(ClientConfig {
        native: Arc::new(native),
        clock,
        protocol,
    })
}

struct BoundedVerifier(Arc<WebPkiServerVerifier>);
impl std::fmt::Debug for BoundedVerifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TlsServerVerifier(<redacted>)")
    }
}
pub(super) fn chain_bounds(
    leaf: &CertificateDer<'_>,
    intermediates: &[CertificateDer<'_>],
) -> Result<(), rustls::Error> {
    let refused = || rustls::Error::Other(rustls::OtherError(Arc::new(TlsError::Capacity)));
    if intermediates.len() >= 8 {
        return Err(refused());
    }
    let mut total = 0usize;
    for cert in std::iter::once(leaf).chain(intermediates) {
        if cert.len() > 16 * 1024 {
            return Err(refused());
        }
        total = total.checked_add(cert.len()).ok_or_else(refused)?;
        if total > 64 * 1024 {
            return Err(refused());
        }
    }
    Ok(())
}
impl ServerCertVerifier for BoundedVerifier {
    fn verify_server_cert(
        &self,
        leaf: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        name: &ServerName<'_>,
        ocsp: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        chain_bounds(leaf, intermediates)?;
        self.0
            .verify_server_cert(leaf, intermediates, name, ocsp, now)
    }
    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        signature: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.0.verify_tls12_signature(message, cert, signature)
    }
    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        signature: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.0.verify_tls13_signature(message, cert, signature)
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.supported_verify_schemes()
    }
}

#[cfg(test)]
#[path = "tls_client_tests.rs"]
mod tests;

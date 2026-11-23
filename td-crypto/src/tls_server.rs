//! Cold server policy and immutable name routing retained by server sessions.
use crate::{
    tls_clock::BackendClock, ClockHandle, ServerIdentity, TlsError, TlsProtocol, TrustStore,
};
use rustls::{
    client::danger::HandshakeSignatureValid,
    pki_types::{CertificateDer, UnixTime},
    server::{
        danger::{ClientCertVerified, ClientCertVerifier},
        ClientHello, ProducesTickets, ResolvesServerCert, WebPkiClientVerifier,
    },
    DigitallySignedStruct, DistinguishedName, SignatureScheme,
};
use std::sync::Arc;

/// Exact-name selection policy. HTTP/1.1 requires RequiredName; SMTP uses one
/// identity with DefaultIdentity or MatchPresentName. SNI is not authentication.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IdentitySelection {
    RequiredName,
    DefaultIdentity,
    MatchPresentName,
}

/// Opaque immutable inbound policy; construction supplies no peer evidence.
pub struct ServerConfig {
    pub(super) native: Arc<rustls::ServerConfig>,
    pub(super) clock: Arc<ClockHandle>,
    pub(super) routing: Arc<Routing>,
    pub(super) protocol: TlsProtocol,
    pub(super) mandatory_client_auth: bool,
}
impl ServerConfig {
    /// Cold construction. None disables client authentication; Some requires a
    /// certificate under exactly that private CA store. Public roots are refused.
    pub fn new(
        identities: &[Arc<ServerIdentity>],
        protocol: TlsProtocol,
        selection: IdentitySelection,
        client_trust: Option<&TrustStore>,
        clock: Arc<ClockHandle>,
    ) -> Result<Self, TlsError> {
        std::panic::catch_unwind(|| build(identities, protocol, selection, client_trust, clock))
            .map_err(|_| TlsError::Crypto)?
    }

    pub fn identity_count(&self) -> usize {
        self.routing.identities.len()
    }
    pub fn binding_count(&self) -> usize {
        self.routing.bindings
    }
}
impl std::fmt::Debug for ServerConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServerConfig")
            .field("identities", &self.identity_count())
            .field("bindings", &self.binding_count())
            .field("protocol", &self.protocol)
            .field("selection", &self.routing.selection)
            .field("mandatory_client_auth", &self.mandatory_client_auth)
            .field("alpn_count", &self.native.alpn_protocols.len())
            .field("clock", &self.clock)
            .finish_non_exhaustive()
    }
}

pub(super) struct Routing {
    pub(super) identities: Vec<Arc<ServerIdentity>>,
    selection: IdentitySelection,
    bindings: usize,
}
impl Routing {
    // Pure lookup: the session checks selected material/time before backend
    // signing and again after Finished. The backend repeats this same lookup.
    pub(super) fn select(&self, name: Option<&str>) -> Result<usize, TlsError> {
        if let Some(name) = name {
            crate::identity::validate_name(name).map_err(|_| TlsError::Protocol)?;
            for (index, identity) in self.identities.iter().enumerate() {
                for binding in 0..identity.name_count() {
                    if identity
                        .name(binding)
                        .is_some_and(|value| value.eq_ignore_ascii_case(name))
                    {
                        return Ok(index);
                    }
                }
            }
        }
        match (self.selection, name) {
            (IdentitySelection::DefaultIdentity, _)
            | (IdentitySelection::MatchPresentName, None) => Ok(0),
            _ => Err(TlsError::Protocol),
        }
    }
}
impl std::fmt::Debug for Routing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TlsIdentityRouting(<redacted>)")
    }
}
impl ResolvesServerCert for Routing {
    fn resolve(&self, hello: ClientHello<'_>) -> Option<Arc<rustls::sign::CertifiedKey>> {
        let index = self.select(hello.server_name()).ok()?;
        self.identities
            .get(index)
            .map(|identity| identity.certified.clone())
    }
}
fn build(
    input: &[Arc<ServerIdentity>],
    protocol: TlsProtocol,
    selection: IdentitySelection,
    client_trust: Option<&TrustStore>,
    clock: Arc<ClockHandle>,
) -> Result<ServerConfig, TlsError> {
    if input.is_empty() || input.len() > 16 {
        return Err(TlsError::Invalid);
    }
    match (protocol, selection) {
        (TlsProtocol::Http1, IdentitySelection::RequiredName) => {}
        (
            TlsProtocol::Smtp,
            IdentitySelection::DefaultIdentity | IdentitySelection::MatchPresentName,
        ) if input.len() == 1 => {}
        _ => return Err(TlsError::Invalid),
    }
    if client_trust.is_some_and(TrustStore::uses_public_roots) {
        return Err(TlsError::Invalid);
    }
    let now = clock.now()?;
    let mut bindings = 0usize;
    let mut identities = Vec::new();
    identities
        .try_reserve_exact(input.len())
        .map_err(|_| TlsError::Capacity)?;
    for (index, identity) in input.iter().enumerate() {
        identity.check_validity(Some(now))?;
        bindings = bindings
            .checked_add(identity.name_count())
            .ok_or(TlsError::Capacity)?;
        if bindings > 512 {
            return Err(TlsError::Invalid);
        }
        for binding in 0..identity.name_count() {
            let name = identity.name(binding).ok_or(TlsError::Crypto)?;
            for previous in input.get(..index).ok_or(TlsError::Crypto)? {
                for previous_binding in 0..previous.name_count() {
                    if previous.name(previous_binding) == Some(name) {
                        return Err(TlsError::Invalid);
                    }
                }
            }
        }
        identities.push(identity.clone());
    }
    let routing = Arc::new(Routing {
        identities,
        selection,
        bindings,
    });
    let provider = Arc::new(crate::tls_policy::provider()?);
    let verifier: Arc<dyn ClientCertVerifier> = match client_trust {
        None => WebPkiClientVerifier::no_client_auth(),
        Some(trust) => {
            let verifier = WebPkiClientVerifier::builder_with_provider(
                Arc::new(trust.roots.clone()),
                provider.clone(),
            )
            .clear_root_hint_subjects()
            .build()
            .map_err(|_| TlsError::Crypto)?;
            Arc::new(BoundedClientVerifier(verifier))
        }
    };
    let mut native =
        rustls::ServerConfig::builder_with_details(provider, Arc::new(BackendClock(clock.clone())))
            .with_protocol_versions(&[&rustls::version::TLS13, &rustls::version::TLS12])
            .map_err(|_| TlsError::Crypto)?
            .with_client_cert_verifier(verifier)
            .with_cert_resolver(routing.clone());
    native.ignore_client_order = true;
    // The pinned fragmenter subtracts the five-byte record header.
    native.max_fragment_size = Some(16_384 + 5);
    native.session_storage = Arc::new(rustls::server::NoServerSessionStorage {});
    native.ticketer = Arc::new(NoTickets);
    native.send_tls13_tickets = 0;
    native.max_tls13_tickets = 0;
    native.max_early_data_size = 0;
    native.send_half_rtt_data = false;
    native.require_ems = true;
    native.alpn_protocols = match protocol {
        TlsProtocol::Http1 => vec![b"http/1.1".to_vec()],
        TlsProtocol::Smtp => Vec::new(),
    };
    native.key_log = Arc::new(rustls::NoKeyLog);
    native.enable_secret_extraction = false;
    native.cert_compressors.clear();
    native.cert_decompressors.clear();
    native.cert_compression_cache = Arc::new(rustls::compress::CompressionCache::Disabled);
    Ok(ServerConfig {
        native: Arc::new(native),
        clock,
        routing,
        protocol,
        mandatory_client_auth: client_trust.is_some(),
    })
}
#[derive(Debug)]
struct NoTickets;
impl ProducesTickets for NoTickets {
    fn enabled(&self) -> bool {
        false
    }
    fn lifetime(&self) -> u32 {
        0
    }
    fn encrypt(&self, _: &[u8]) -> Option<Vec<u8>> {
        None
    }
    fn decrypt(&self, _: &[u8]) -> Option<Vec<u8>> {
        None
    }
}
struct BoundedClientVerifier(Arc<dyn ClientCertVerifier>);
impl std::fmt::Debug for BoundedClientVerifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TlsClientVerifier(<redacted>)")
    }
}
impl ClientCertVerifier for BoundedClientVerifier {
    fn offer_client_auth(&self) -> bool {
        true
    }
    fn client_auth_mandatory(&self) -> bool {
        true
    }
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        self.0.root_hint_subjects()
    }
    fn verify_client_cert(
        &self,
        leaf: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        now: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        crate::tls_client::chain_bounds(leaf, intermediates)?;
        self.0.verify_client_cert(leaf, intermediates, now)
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
#[path = "tls_server_tests.rs"]
mod tests;

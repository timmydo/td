//! Cold gateway client policy. Matching supplied values is not peer evidence.
use crate::{
    config::{endpoint::Prefix, gateway::Gateway, values},
    ports::Error,
};
use std::{net::IpAddr, sync::Arc};
use td_crypto::{
    ClockHandle, Crypto, Digest, IdentitySelection, PemCertificates, Provider, ServerConfig,
    ServerIdentity, Sha256, TlsProtocol, TlsSession, TrustStore, CERTIFICATE_DER_CAPACITY,
};

const MAX_PEERS: usize = crate::config::gateway::MAX_PEERS_PER_GATEWAY;
const MAX_ANCHORS: usize = 128;

/// Canonical gateway client policy identity, independent of a TLS session.
#[derive(Clone, Copy, Eq, PartialEq)]
pub struct GatewayFingerprint([u8; 32]);

impl GatewayFingerprint {
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl std::fmt::Debug for GatewayFingerprint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("GatewayFingerprint(<redacted>)")
    }
}

/// An immutable mandatory-client TLS configuration and its gateway constraints.
/// Construction is cold; the admitting runtime owns generations and resources.
pub struct GatewayPolicy {
    config: Arc<ServerConfig>,
    name: String,
    current: [u8; 32],
    next: Option<[u8; 32]>,
    peers: [Option<Prefix>; MAX_PEERS],
    fingerprint: GatewayFingerprint,
}

impl GatewayPolicy {
    /// Build from the selected gateway row and its protected CA file contents.
    /// The caller binds this material and identity to the validated listener.
    pub fn new(
        gateway: &Gateway<'_, '_>,
        ca_pem: &[u8],
        identity: Arc<ServerIdentity>,
        clock: Arc<ClockHandle>,
    ) -> Result<Self, Error> {
        values::profile_name(gateway.name).map_err(|_| Error::Invalid)?;
        let count = gateway.peer_count();
        if count == 0 || count > MAX_PEERS {
            return Err(Error::Invalid);
        }
        let current = *gateway.client_cert_sha256.as_bytes();
        let next = gateway.next_client_cert_sha256.map(|pin| *pin.as_bytes());
        if next == Some(current) {
            return Err(Error::Invalid);
        }
        let mut peers = [None; MAX_PEERS];
        for (index, target) in peers.iter_mut().take(count).enumerate() {
            let peer = gateway
                .peer(index)
                .map_err(|_| Error::Invalid)?
                .ok_or(Error::Invalid)?;
            *target = Some(peer);
        }
        let trust = TrustStore::from_pem(ca_pem).map_err(|_| Error::Tls)?;
        let fingerprint = fingerprint(gateway.name, ca_pem, current, next, &peers)?;
        let config = ServerConfig::new(
            &[identity],
            TlsProtocol::Smtp,
            IdentitySelection::MatchPresentName,
            Some(&trust),
            clock,
        )
        .map_err(|_| Error::Tls)?;
        let mut name = String::new();
        name.try_reserve_exact(gateway.name.len())
            .map_err(|_| Error::Capacity)?;
        name.push_str(gateway.name);
        Ok(Self {
            config: Arc::new(config),
            name,
            current,
            next,
            peers,
            fingerprint,
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub const fn fingerprint(&self) -> GatewayFingerprint {
        self.fingerprint
    }

    /// The runtime must reserve session/handshake resources before this call.
    /// The raw session still grants no mail or gateway authorization.
    pub fn new_session(&self) -> Result<TlsSession, Error> {
        TlsSession::server(Arc::clone(&self.config)).map_err(|_| Error::Tls)
    }

    /// A bounded predicate over caller-supplied values, never an admission proof.
    /// The admitting transport must supply this session's verified client leaf
    /// and its actual socket peer, then recheck current policy before mutation.
    pub fn matches(&self, peer: IpAddr, leaf_sha256: &[u8; 32]) -> Result<bool, Error> {
        let current = Provider.equal_digest(&self.current, leaf_sha256);
        let next = self
            .next
            .as_ref()
            .is_some_and(|pin| Provider.equal_digest(pin, leaf_sha256));
        if !(current | next) {
            return Ok(false);
        }
        for prefix in self.peers.iter().flatten() {
            if prefix.contains(peer).map_err(|_| Error::Invalid)? {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

impl std::fmt::Debug for GatewayPolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("GatewayPolicy(<redacted>)")
    }
}

#[derive(Clone, Copy, Eq, Ord, PartialEq, PartialOrd)]
struct CanonicalPrefix {
    family: u8,
    bits: u8,
    address: [u8; 16],
}

impl CanonicalPrefix {
    const EMPTY: Self = Self {
        family: 0,
        bits: 0,
        address: [0; 16],
    };

    fn new(prefix: Prefix) -> Self {
        let (family, address) = match prefix.network() {
            IpAddr::V4(ip) => {
                let [a, b, c, d] = ip.octets();
                (4, [a, b, c, d, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0])
            }
            IpAddr::V6(ip) => (6, ip.octets()),
        };
        Self {
            family,
            bits: prefix.bits(),
            address,
        }
    }
}

fn fingerprint(
    name: &str,
    ca_pem: &[u8],
    current: [u8; 32],
    next: Option<[u8; 32]>,
    peers: &[Option<Prefix>; MAX_PEERS],
) -> Result<GatewayFingerprint, Error> {
    let mut certificates = PemCertificates::trust_bundle(ca_pem)?;
    let count = certificates.remaining();
    let mut anchors = [[0; 32]; MAX_ANCHORS];
    let anchors = anchors.get_mut(..count).ok_or(Error::Capacity)?;
    let mut scratch = [0; CERTIFICATE_DER_CAPACITY];
    for digest in anchors.iter_mut() {
        let length = certificates
            .decode_next(&mut scratch)?
            .ok_or(Error::Invalid)?;
        let mut hash = Sha256::try_new()?;
        hash.update(scratch.get(..length).ok_or(Error::Invalid)?)?;
        *digest = hash.finish()?;
    }
    if certificates.remaining() != 0 {
        return Err(Error::Invalid);
    }
    anchors.sort_unstable();
    let mut hash = Sha256::try_new()?;
    hash.update(b"td-mta/gateway-policy/v1\0")?;
    hash.update(
        &u16::try_from(name.len())
            .map_err(|_| Error::Invalid)?
            .to_be_bytes(),
    )?;
    hash.update(name.as_bytes())?;
    hash.update(
        &u16::try_from(count)
            .map_err(|_| Error::Invalid)?
            .to_be_bytes(),
    )?;
    for anchor in anchors {
        hash.update(anchor)?;
    }
    let mut pins = [current, next.unwrap_or(current)];
    let pins = pins
        .get_mut(..if next.is_some() { 2 } else { 1 })
        .ok_or(Error::Invalid)?;
    pins.sort_unstable();
    hash.update(&[u8::try_from(pins.len()).map_err(|_| Error::Invalid)?])?;
    for pin in pins {
        hash.update(pin)?;
    }
    let mut prefixes = [CanonicalPrefix::EMPTY; MAX_PEERS];
    let count = peers.iter().flatten().count();
    let prefixes = prefixes.get_mut(..count).ok_or(Error::Invalid)?;
    for (out, prefix) in prefixes.iter_mut().zip(peers.iter().flatten()) {
        *out = CanonicalPrefix::new(*prefix);
    }
    prefixes.sort_unstable();
    hash.update(&[u8::try_from(prefixes.len()).map_err(|_| Error::Invalid)?])?;
    for prefix in prefixes {
        hash.update(&[prefix.family, prefix.bits])?;
        hash.update(&prefix.address)?;
    }
    Ok(GatewayFingerprint(hash.finish()?))
}

#[cfg(test)]
#[path = "gateway_policy_tests.rs"]
mod tests;

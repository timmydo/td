//! Cold TLS policy compilation and retained session reservations.
use crate::{
    config::{certificate, graph, inputs, listener, materialize::ResolvedText, outbound},
    gateway_policy::{GatewayFingerprint, GatewayPolicy},
    generations::{GenerationConstruction, GenerationLease, PreparedGeneration},
    ports::{Error, TlsPolicyId},
    tls_admission::{HandshakePermit, HandshakePool},
    tls_io::TlsWireStorage,
};
use std::{io::Read, net::SocketAddr, sync::Arc};
use td_crypto::{
    ClientConfig, ClockHandle, IdentitySelection, ServerConfig, ServerIdentity, TlsProtocol,
    TlsSession, TlsStatus, TrustStore,
};

const MAX_POLICIES: usize = listener::MAX_LISTENERS + 2;

#[path = "tls_policy_io.rs"]
mod io;
pub use io::TlsConnection;

#[path = "tls_policy_starttls.rs"]
mod starttls;
pub use starttls::{ServerStartTls, ServerUpgradeProgress};

#[path = "tls_policy_client_starttls.rs"]
mod client_starttls;
pub use client_starttls::{ClientStartTls, ClientUpgradeProgress, UpgradeScratch};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MaterialKind {
    Chain,
    Key,
    GatewayCa,
    RelayCa,
    AcmeCa,
}

/// A trusted loader request, not evidence of file ownership or permissions.
/// ACME chain/key requests have a profile but no operator path.
pub struct MaterialRequest<'a> {
    kind: MaterialKind,
    profile: Option<&'a str>,
    path: Option<&'a str>,
}

impl MaterialRequest<'_> {
    pub const fn kind(&self) -> MaterialKind {
        self.kind
    }
    pub fn profile(&self) -> Option<&str> {
        self.profile
    }
    pub fn path(&self) -> Option<&str> {
        self.path
    }
}

impl std::fmt::Debug for MaterialRequest<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TlsMaterialRequest")
            .field("kind", &self.kind)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PolicyRole {
    DirectSmtp,
    GatewaySmtp,
    Https,
    Relay,
    Acme,
}

#[derive(Eq, PartialEq)]
struct ListenerBinding {
    name: String,
    kind: listener::Kind,
    bind: SocketAddr,
    server: Option<String>,
    certificate: Option<String>,
    gateway: Option<String>,
    sessions: Option<usize>,
    per_peer: Option<usize>,
}

enum Binding {
    Listener(ListenerBinding),
    Relay {
        host: String,
        port: u16,
        transport: outbound::Transport,
    },
    Acme {
        host: String,
        port: u16,
    },
}

enum Configuration {
    Server(Arc<ServerConfig>),
    Gateway(Box<GatewayPolicy>),
    Client(Arc<ClientConfig>),
}

struct HttpsIdentity<'a> {
    profile: &'a str,
    owns_origin: bool,
    identity: Option<Arc<ServerIdentity>>,
}

struct Policy {
    receiving_validity: Option<(u64, u64)>,
    binding: Binding,
    configuration: Configuration,
}

impl Policy {
    fn role(&self) -> Result<PolicyRole, Error> {
        match &self.binding {
            Binding::Listener(binding) => match binding.kind {
                listener::Kind::DirectSmtp => Ok(PolicyRole::DirectSmtp),
                listener::Kind::GatewaySmtp => Ok(PolicyRole::GatewaySmtp),
                listener::Kind::Https => Ok(PolicyRole::Https),
                _ => Err(Error::Invalid),
            },
            Binding::Relay { .. } => Ok(PolicyRole::Relay),
            Binding::Acme { .. } => Ok(PolicyRole::Acme),
        }
    }

    fn session(&self) -> Result<TlsSession, Error> {
        match (&self.configuration, &self.binding) {
            (Configuration::Server(config), Binding::Listener(_)) => {
                TlsSession::server(config.clone()).map_err(|_| Error::Tls)
            }
            (Configuration::Gateway(policy), Binding::Listener(_)) => policy.new_session(),
            (
                Configuration::Client(config),
                Binding::Relay { host, .. } | Binding::Acme { host, .. },
            ) => TlsSession::client(config.clone(), host).map_err(|_| Error::Tls),
            _ => Err(Error::Invalid),
        }
    }
}

/// An immutable TLS table. Construct only inside a reserved generation.
/// Counts/input caps are not a native allocation or aggregate byte qualification.
pub struct TlsPolicies {
    policies: Vec<Policy>,
    coverage: PolicyCoverage,
}

/// Which configured roles were compiled, not runtime health or publication.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PolicyCoverage {
    ClientsOnly,
    Complete,
}

impl std::fmt::Debug for TlsPolicies {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TlsPolicies")
            .field("count", &self.policies.len())
            .finish_non_exhaustive()
    }
}

impl TlsPolicies {
    /// Cold control-worker operation. The opener supplies bounded/deadlined,
    /// protected file readers (or retained ACME material), never network issuance.
    /// This verifies TLS content, not descriptor trust or runtime publication.
    pub fn prepare<R: Read>(
        reserved: GenerationConstruction<Self>,
        configuration: &ResolvedText,
        clock: Arc<ClockHandle>,
        open: impl FnMut(MaterialRequest<'_>) -> Result<R, Error>,
    ) -> Result<PreparedGeneration<Self>, Error> {
        Self::prepare_coverage(
            reserved,
            configuration,
            clock,
            open,
            PolicyCoverage::Complete,
        )
    }

    /// Explicit startup/recovery stage: compile relay and optional ACME trust
    /// without opening any server identity or gateway material. No server role
    /// is admitted. Use the same generation domain as subsequent complete tables.
    pub fn prepare_clients<R: Read>(
        reserved: GenerationConstruction<Self>,
        configuration: &ResolvedText,
        clock: Arc<ClockHandle>,
        open: impl FnMut(MaterialRequest<'_>) -> Result<R, Error>,
    ) -> Result<PreparedGeneration<Self>, Error> {
        Self::prepare_coverage(
            reserved,
            configuration,
            clock,
            open,
            PolicyCoverage::ClientsOnly,
        )
    }

    fn prepare_coverage<R: Read>(
        reserved: GenerationConstruction<Self>,
        configuration: &ResolvedText,
        clock: Arc<ClockHandle>,
        mut open: impl FnMut(MaterialRequest<'_>) -> Result<R, Error>,
        coverage: PolicyCoverage,
    ) -> Result<PreparedGeneration<Self>, Error> {
        reserved.construct(|| {
            configuration
                .candidate()
                .with_graph(|records, text| {
                    let graph = records.view(text).map_err(|_| Error::Invalid)?;
                    build(configuration, graph, clock, &mut open, coverage).map(Box::new)
                })
                .map_err(|_| Error::Invalid)?
        })
    }

    fn policy(&self, index: u16) -> Result<&Policy, Error> {
        self.policies.get(usize::from(index)).ok_or(Error::NotFound)
    }

    pub const fn coverage(&self) -> PolicyCoverage {
        self.coverage
    }

    pub fn len(&self) -> usize {
        self.policies.len()
    }
    pub fn is_empty(&self) -> bool {
        self.policies.is_empty()
    }

    pub fn listener(lease: &GenerationLease<Self>, name: &str) -> Result<TlsPolicyId, Error> {
        Self::find(
            lease,
            |policy| matches!(&policy.binding, Binding::Listener(b) if b.name == name),
        )
    }
    /// Cold validation of the complete listener binding, including its limits.
    pub fn bound_listener(
        lease: &GenerationLease<Self>,
        row: listener::Listener<'_>,
    ) -> Result<TlsPolicyId, Error> {
        let expected = listener_binding(row)?;
        Self::find(lease, |policy| {
            matches!((&policy.binding, &expected),
                (Binding::Listener(actual), Binding::Listener(expected)) if actual == expected)
        })
    }
    pub fn relay(lease: &GenerationLease<Self>) -> Result<TlsPolicyId, Error> {
        Self::find(lease, |policy| {
            matches!(policy.binding, Binding::Relay { .. })
        })
    }
    pub fn acme(lease: &GenerationLease<Self>) -> Result<TlsPolicyId, Error> {
        Self::find(lease, |policy| {
            matches!(policy.binding, Binding::Acme { .. })
        })
    }
    fn find(
        lease: &GenerationLease<Self>,
        predicate: impl Fn(&Policy) -> bool,
    ) -> Result<TlsPolicyId, Error> {
        let index = lease
            .value()
            .policies
            .iter()
            .position(predicate)
            .ok_or(Error::NotFound)?;
        Ok(TlsPolicyId {
            generation: lease.id().get(),
            index: u16::try_from(index).map_err(|_| Error::Invalid)?,
        })
    }
    fn resolve(lease: &GenerationLease<Self>, id: TlsPolicyId) -> Result<&Policy, Error> {
        if id.generation != lease.id().get() {
            return Err(Error::Conflict);
        }
        lease.value().policy(id.index)
    }
    pub fn role(lease: &GenerationLease<Self>, id: TlsPolicyId) -> Result<PolicyRole, Error> {
        Self::resolve(lease, id)?.role()
    }
    /// Cold-registered endpoint metadata; never permission to send credentials.
    pub fn destination(
        lease: &GenerationLease<Self>,
        id: TlsPolicyId,
    ) -> Result<Option<(&str, u16, outbound::Transport)>, Error> {
        Ok(match &Self::resolve(lease, id)?.binding {
            Binding::Relay {
                host,
                port,
                transport,
            } => Some((host, *port, *transport)),
            Binding::Acme { host, port } => Some((host, *port, outbound::Transport::ImplicitTls)),
            Binding::Listener(_) => None,
        })
    }
    /// Exact policy/binding comparison only. The caller supplies the actual
    /// active generation and serializes this check with mutation publication.
    /// This is not authenticated peer evidence or a durable revocation fence.
    pub fn gateway_unchanged(
        old: &GenerationLease<Self>,
        id: TlsPolicyId,
        current: &GenerationLease<Self>,
    ) -> Result<bool, Error> {
        let old = Self::resolve(old, id)?;
        let (Binding::Listener(binding), Configuration::Gateway(policy)) =
            (&old.binding, &old.configuration)
        else {
            return Err(Error::Invalid);
        };
        Ok(current.value().policies.iter().any(|candidate| {
            matches!((&candidate.binding, &candidate.configuration), (Binding::Listener(b), Configuration::Gateway(p))
                if b == binding && p.fingerprint() == policy.fingerprint())
        }))
    }
    pub fn gateway_fingerprint(
        lease: &GenerationLease<Self>,
        id: TlsPolicyId,
    ) -> Result<GatewayFingerprint, Error> {
        match &Self::resolve(lease, id)?.configuration {
            Configuration::Gateway(policy) => Ok(policy.fingerprint()),
            _ => Err(Error::Invalid),
        }
    }

    /// Recheck a direct receiving listener's admitted chain against current UTC.
    /// This also gates plaintext SMTP, which never constructs a TLS session.
    pub fn check_receiving_certificate(
        lease: &GenerationLease<Self>,
        id: TlsPolicyId,
        utc_ms: i64,
    ) -> Result<(), Error> {
        let policy = Self::resolve(lease, id)?;
        let (first, last) = policy.receiving_validity.ok_or(Error::Invalid)?;
        let now = u64::try_from(utc_ms).map_err(|_| Error::Tls)? / 1000;
        if now < first || now > last {
            return Err(Error::Tls);
        }
        Ok(())
    }

    /// Reserve before native session construction. Wire arrays must already
    /// belong to the runtime's session slot. No socket/STARTTLS is performed.
    pub fn reserve_session<B: TlsWireStorage>(
        lease: GenerationLease<Self>,
        id: TlsPolicyId,
        pool: &HandshakePool,
        input: B,
        output: B,
    ) -> Result<SessionPreparation<B>, SessionRefusal<B>> {
        let result = (|| {
            Self::resolve(&lease, id)?;
            let permit = pool.reserve()?;
            Ok(permit)
        })();
        match result {
            Ok(permit) => Ok(SessionPreparation {
                lease,
                permit,
                id,
                input,
                output,
            }),
            Err(error) => Err(SessionRefusal {
                error,
                input,
                output,
            }),
        }
    }
}

/// Allocation-free reservation for queued work. Move to a TLS worker before
/// constructing native state. Drop releases capacity; recover arrays explicitly.
///
/// ```compile_fail,E0277
/// use td_mta::{tls_policy::SessionPreparation, tls_io::TLS_WIRE_BYTES};
/// fn requires_clone<T: Clone>() {}
/// requires_clone::<SessionPreparation<Box<[u8; TLS_WIRE_BYTES]>>>();
/// ```
#[must_use = "construct on a TLS worker or recover the reserved buffers"]
pub struct SessionPreparation<B: TlsWireStorage> {
    lease: GenerationLease<TlsPolicies>,
    permit: HandshakePermit,
    id: TlsPolicyId,
    input: B,
    output: B,
}
impl<B: TlsWireStorage> SessionPreparation<B> {
    pub fn construct(self) -> Result<SessionReservation<B>, SessionRefusal<B>> {
        match TlsPolicies::resolve(&self.lease, self.id).and_then(Policy::session) {
            Ok(session) => Ok(SessionReservation {
                session,
                lease: self.lease,
                permit: self.permit,
                id: self.id,
                input: self.input,
                output: self.output,
            }),
            Err(error) => Err(SessionRefusal {
                error,
                input: self.input,
                output: self.output,
            }),
        }
    }
    pub fn into_buffers(self) -> (B, B) {
        (self.input, self.output)
    }
}
impl<B: TlsWireStorage> std::fmt::Debug for SessionPreparation<B> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TlsSessionPreparation(<redacted>)")
    }
}

/// Owns a native handshaking session, its generation, count permit and buffers.
/// No raw session/configuration can escape this owner. The next adapter stage
/// consumes it together with a socket; no service endpoint uses it yet.
///
/// ```compile_fail,E0277
/// use td_mta::{tls_policy::SessionReservation, tls_io::TLS_WIRE_BYTES};
/// fn requires_clone<T: Clone>() {}
/// requires_clone::<SessionReservation<Box<[u8; TLS_WIRE_BYTES]>>>();
/// ```
#[must_use = "retain the reservation or recover its buffers"]
pub struct SessionReservation<B: TlsWireStorage> {
    // Drop order retires native state before material and handshake capacity.
    session: TlsSession,
    lease: GenerationLease<TlsPolicies>,
    permit: HandshakePermit,
    id: TlsPolicyId,
    input: B,
    output: B,
}
impl<B: TlsWireStorage> SessionReservation<B> {
    pub const fn policy_id(&self) -> TlsPolicyId {
        self.id
    }
    pub fn status(&self) -> TlsStatus {
        self.session.status()
    }
    pub fn role(&self) -> Result<PolicyRole, Error> {
        TlsPolicies::role(&self.lease, self.id)
    }
    /// Abort native state before releasing its generation/permit; reuse buffers
    /// explicitly. Ordinary Drop frees owned arrays instead of pooling them.
    pub fn into_buffers(self) -> (B, B) {
        let Self {
            mut session,
            lease,
            permit,
            id: _,
            input,
            output,
        } = self;
        session.abort();
        drop(session);
        drop(lease);
        drop(permit);
        (input, output)
    }
}
impl<B: TlsWireStorage> std::fmt::Debug for SessionReservation<B> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TlsSessionReservation(<redacted>)")
    }
}
#[must_use = "recover the reserved wire buffers"]
pub struct SessionRefusal<B: TlsWireStorage> {
    error: Error,
    input: B,
    output: B,
}
impl<B: TlsWireStorage> SessionRefusal<B> {
    pub const fn error(&self) -> Error {
        self.error
    }
    pub fn into_buffers(self) -> (B, B) {
        (self.input, self.output)
    }
}
impl<B: TlsWireStorage> std::fmt::Debug for SessionRefusal<B> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TlsSessionRefusal")
            .field("error", &self.error)
            .finish_non_exhaustive()
    }
}

fn owned(text: &str) -> Result<String, Error> {
    let mut value = String::new();
    value
        .try_reserve_exact(text.len())
        .map_err(|_| Error::Capacity)?;
    value.push_str(text);
    Ok(value)
}

struct Raw(Vec<u8>);
impl Drop for Raw {
    fn drop(&mut self) {
        self.0.fill(0);
    }
}

fn read<R: Read>(
    request: MaterialRequest<'_>,
    maximum: usize,
    open: &mut impl FnMut(MaterialRequest<'_>) -> Result<R, Error>,
) -> Result<Raw, Error> {
    let capacity = maximum.checked_add(1).ok_or(Error::Capacity)?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(capacity)
        .map_err(|_| Error::Capacity)?;
    bytes.resize(capacity, 0);
    let mut raw = Raw(bytes);
    let mut reader = open(request)?;
    let mut used = 0usize;
    let mut interrupted = 0u32;
    loop {
        let output = raw.0.get_mut(used..).ok_or(Error::Invalid)?;
        let count = match reader.read(output) {
            Ok(count) if count <= output.len() => count,
            Ok(_) => return Err(Error::Invalid),
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {
                interrupted = interrupted.checked_add(1).ok_or(Error::Io {
                    kind: error.kind(),
                    os_code: error.raw_os_error(),
                })?;
                if interrupted > crate::config::material::MAX_INTERRUPTED_READS {
                    return Err(error.into());
                }
                continue;
            }
            Err(error) => return Err(error.into()),
        };
        if count == 0 {
            raw.0.truncate(used);
            return Ok(raw);
        }
        used = used.checked_add(count).ok_or(Error::Capacity)?;
        if used > maximum {
            return Err(Error::Capacity);
        }
    }
}

fn client<R: Read>(
    path: Option<&str>,
    kind: MaterialKind,
    protocol: TlsProtocol,
    clock: &Arc<ClockHandle>,
    open: &mut impl FnMut(MaterialRequest<'_>) -> Result<R, Error>,
) -> Result<Configuration, Error> {
    let trust = if let Some(path) = path {
        let bytes = read(
            MaterialRequest {
                kind,
                profile: None,
                path: Some(path),
            },
            inputs::MAX_CA_BYTES,
            open,
        )?;
        TrustStore::from_pem(&bytes.0).map_err(|_| Error::Tls)?
    } else {
        TrustStore::public_roots().map_err(|_| Error::Tls)?
    };
    Ok(Configuration::Client(Arc::new(
        ClientConfig::new(&trust, clock.clone(), protocol).map_err(|_| Error::Tls)?,
    )))
}

fn push(policies: &mut Vec<Policy>, policy: Policy) -> Result<(), Error> {
    if policies.len() >= MAX_POLICIES {
        return Err(Error::Capacity);
    }
    policies.push(policy);
    Ok(())
}
fn listener_binding(row: listener::Listener<'_>) -> Result<Binding, Error> {
    Ok(Binding::Listener(ListenerBinding {
        name: owned(row.name)?,
        kind: row.kind,
        bind: row.bind,
        server: row.server_name.map(owned).transpose()?,
        certificate: row.certificate.map(owned).transpose()?,
        gateway: row.gateway.map(owned).transpose()?,
        sessions: row.session_limit,
        per_peer: row.per_peer_limit,
    }))
}

fn build<R: Read>(
    configuration: &ResolvedText,
    graph: graph::View<'_, '_>,
    clock: Arc<ClockHandle>,
    open: &mut impl FnMut(MaterialRequest<'_>) -> Result<R, Error>,
    coverage: PolicyCoverage,
) -> Result<TlsPolicies, Error> {
    let profiles = graph.certificates().map_err(|_| Error::Invalid)?;
    let mut policies = Vec::new();
    let capacity = match coverage {
        PolicyCoverage::ClientsOnly => 2,
        PolicyCoverage::Complete => MAX_POLICIES,
    };
    policies
        .try_reserve_exact(capacity)
        .map_err(|_| Error::Capacity)?;
    if coverage == PolicyCoverage::ClientsOnly {
        append_clients(&mut policies, configuration, profiles, &clock, open)?;
        return Ok(TlsPolicies { policies, coverage });
    }
    let mut identities = Vec::new();
    identities
        .try_reserve_exact(profiles.len())
        .map_err(|_| Error::Capacity)?;
    for index in 0..profiles.len() {
        let profile = profiles
            .profile(index)
            .map_err(|_| Error::Invalid)?
            .ok_or(Error::Invalid)?;
        let mut names = [""; graph::MAX_NAMES_PER_CERTIFICATE];
        let mut count = 0usize;
        for index in 0..graph.binding_count() {
            let binding = graph
                .binding(index)
                .map_err(|_| Error::Invalid)?
                .ok_or(Error::Invalid)?;
            if binding.certificate.name == profile.name {
                *names.get_mut(count).ok_or(Error::Capacity)? = binding.name;
                count = count.checked_add(1).ok_or(Error::Capacity)?;
            }
        }
        let chain = read(
            MaterialRequest {
                kind: MaterialKind::Chain,
                profile: Some(profile.name),
                path: profile.chain_file,
            },
            inputs::MAX_CHAIN_BYTES,
            open,
        )?;
        let key = read(
            MaterialRequest {
                kind: MaterialKind::Key,
                profile: Some(profile.name),
                path: profile.key_file,
            },
            inputs::MAX_KEY_BYTES,
            open,
        )?;
        let identity = ServerIdentity::from_pem(
            &chain.0,
            &key.0,
            names.get(..count).ok_or(Error::Invalid)?,
            Some(clock.now().map_err(|_| Error::Tls)?),
        )
        .map_err(|_| Error::Tls)?;
        identities.push((profile.name, Arc::new(identity)));
    }
    let identity = |name: &str| {
        identities
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, value)| value.clone())
            .ok_or(Error::Invalid)
    };
    // Within this graph a profile has only two HTTPS views: with/without JMAP.
    let mut https_identities: Vec<HttpsIdentity<'_>> = Vec::new();
    let https_capacity = certificate::MAX_PROFILES * 2;
    https_identities
        .try_reserve_exact(https_capacity)
        .map_err(|_| Error::Capacity)?;
    let listeners = graph.listeners().map_err(|_| Error::Invalid)?;
    for index in 0..listeners.len() {
        let row = listeners
            .listener(index)
            .map_err(|_| Error::Invalid)?
            .ok_or(Error::Invalid)?;
        if matches!(
            row.kind,
            listener::Kind::Http01
                | listener::Kind::LoopbackSmtpFixture
                | listener::Kind::GatewaySmtp
        ) {
            continue;
        }
        let primary = identity(row.certificate.ok_or(Error::Invalid)?)?;
        let receiving_validity =
            (row.kind == listener::Kind::DirectSmtp).then(|| primary.validity());
        let native = match row.kind {
            listener::Kind::DirectSmtp => Configuration::Server(Arc::new(
                ServerConfig::new(
                    &[primary],
                    TlsProtocol::Smtp,
                    IdentitySelection::DefaultIdentity,
                    None,
                    clock.clone(),
                )
                .map_err(|_| Error::Tls)?,
            )),
            listener::Kind::Https => {
                let mut selected = Vec::new();
                selected
                    .try_reserve_exact(certificate::MAX_PROFILES)
                    .map_err(|_| Error::Capacity)?;
                let origin = graph.origin().map_err(|_| Error::Invalid)?;
                let domains = graph.domains().map_err(|_| Error::Invalid)?;
                for (profile, full) in &identities {
                    let owns_origin = Some(*profile) == row.certificate;
                    if let Some(cached) = https_identities.iter().find(|cached| {
                        cached.profile == *profile && cached.owns_origin == owns_origin
                    }) {
                        if let Some(identity) = &cached.identity {
                            selected.push(identity.clone());
                        }
                        continue;
                    }
                    let mut names = Vec::new();
                    names
                        .try_reserve_exact(graph::MAX_NAMES_PER_CERTIFICATE)
                        .map_err(|_| Error::Capacity)?;
                    if owns_origin {
                        names.push(origin.host());
                    }
                    // Borrow canonical names already checked by identity admission.
                    for i in 0..domains.len() {
                        let domain = domains
                            .domain(i)
                            .map_err(|_| Error::Invalid)?
                            .ok_or(Error::Invalid)?;
                        if domain.certificate != Some(*profile) {
                            continue;
                        }
                        let name = (0..full.name_count())
                            .filter_map(|i| full.name(i))
                            .find(|name| {
                                name.strip_prefix("mta-sts.")
                                    .is_some_and(|suffix| suffix == domain.name)
                            })
                            .ok_or(Error::Invalid)?;
                        if !names.contains(&name) {
                            names.push(name);
                        }
                    }
                    let identity = if names.is_empty() {
                        None
                    } else {
                        Some(Arc::new(
                            full.restrict_names(&names).map_err(|_| Error::Tls)?,
                        ))
                    };
                    if https_identities.len() >= https_capacity {
                        return Err(Error::Capacity);
                    }
                    https_identities.push(HttpsIdentity {
                        profile,
                        owns_origin,
                        identity: identity.clone(),
                    });
                    if let Some(identity) = identity {
                        selected.push(identity);
                    }
                }
                Configuration::Server(Arc::new(
                    ServerConfig::new(
                        &selected,
                        TlsProtocol::Http1,
                        IdentitySelection::RequiredName,
                        None,
                        clock.clone(),
                    )
                    .map_err(|_| Error::Tls)?,
                ))
            }
            _ => return Err(Error::Invalid),
        };
        push(
            &mut policies,
            Policy {
                receiving_validity,
                binding: listener_binding(row)?,
                configuration: native,
            },
        )?;
    }
    // One read per gateway: every listener in this generation uses the same CA bytes.
    let gateways = graph.gateways().map_err(|_| Error::Invalid)?;
    for i in 0..gateways.len() {
        let gateway = gateways
            .gateway(i)
            .map_err(|_| Error::Invalid)?
            .ok_or(Error::Invalid)?;
        let ca = read(
            MaterialRequest {
                kind: MaterialKind::GatewayCa,
                profile: Some(gateway.name),
                path: Some(gateway.ca_file),
            },
            inputs::MAX_CA_BYTES,
            open,
        )?;
        let mut used = false;
        for index in 0..listeners.len() {
            let row = listeners
                .listener(index)
                .map_err(|_| Error::Invalid)?
                .ok_or(Error::Invalid)?;
            if row.kind != listener::Kind::GatewaySmtp || row.gateway != Some(gateway.name) {
                continue;
            }
            let native = GatewayPolicy::new(
                &gateway,
                &ca.0,
                Arc::new(
                    identity(row.certificate.ok_or(Error::Invalid)?)?
                        .restrict_names(&[row.server_name.ok_or(Error::Invalid)?])
                        .map_err(|_| Error::Tls)?,
                ),
                clock.clone(),
            )?;
            push(
                &mut policies,
                Policy {
                    receiving_validity: None,
                    binding: listener_binding(row)?,
                    configuration: Configuration::Gateway(Box::new(native)),
                },
            )?;
            used = true;
        }
        if !used {
            TrustStore::from_pem(&ca.0).map_err(|_| Error::Tls)?;
        }
    }
    append_clients(&mut policies, configuration, profiles, &clock, open)?;
    if policies.len() > MAX_POLICIES {
        return Err(Error::Capacity);
    }
    Ok(TlsPolicies { policies, coverage })
}

fn append_clients<R: Read>(
    policies: &mut Vec<Policy>,
    configuration: &ResolvedText,
    profiles: certificate::View<'_, '_>,
    clock: &Arc<ClockHandle>,
    open: &mut impl FnMut(MaterialRequest<'_>) -> Result<R, Error>,
) -> Result<(), Error> {
    let relay = configuration
        .candidate()
        .outbound()
        .map_err(|_| Error::Invalid)?
        .relay()
        .map_err(|_| Error::Invalid)?;
    push(
        policies,
        Policy {
            receiving_validity: None,
            binding: Binding::Relay {
                host: owned(relay.host)?,
                port: relay.port,
                transport: relay.transport,
            },
            configuration: client(
                relay.ca_file,
                MaterialKind::RelayCa,
                TlsProtocol::Smtp,
                clock,
                open,
            )?,
        },
    )?;
    if let Some(acme) = profiles.acme().map_err(|_| Error::Invalid)? {
        push(
            policies,
            Policy {
                receiving_validity: None,
                binding: Binding::Acme {
                    host: owned(acme.directory.host())?,
                    port: acme.directory.port(),
                },
                configuration: client(
                    acme.ca_file,
                    MaterialKind::AcmeCa,
                    TlsProtocol::Http1,
                    clock,
                    open,
                )?,
            },
        )?;
    }
    Ok(())
}

#[cfg(test)]
#[path = "tls_policy_tests.rs"]
mod tests;

//! Cold allocation of private typed configuration regions; no validated authority.
use super::{
    certificate, dispatch, gateway, graph, identities, identity, listener, policy, routing,
    stanza::Pending, text,
};
use std::{fmt, mem::size_of};

const SNAPSHOT_BYTES: usize = 1024 * 1024;
const GLOBAL_BYTES: usize = 228 * 1024;
const PLANS_BYTES: usize = 4 * 1024;
const DEVICE_BYTES: usize = 32 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Region {
    Text,
    RoutingText,
    Domains,
    Aliases,
    Policies,
    Identities,
    Addresses,
    Certificates,
    Gateways,
    Peers,
    Listeners,
    Bindings,
    // Owner/header headroom and aggregate diagnostics; no backing vector.
    Global,
}
impl Region {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::RoutingText => "routing_text",
            Self::Domains => "domains",
            Self::Aliases => "aliases",
            Self::Policies => "policies",
            Self::Identities => "identities",
            Self::Addresses => "addresses",
            Self::Certificates => "certificates",
            Self::Gateways => "gateways",
            Self::Peers => "peers",
            Self::Listeners => "listeners",
            Self::Bindings => "bindings",
            Self::Global => "global",
        }
    }
    pub const fn ceiling(self) -> usize {
        match self {
            Self::Text => 192 * 1024,
            Self::RoutingText => 320 * 1024,
            Self::Domains | Self::Gateways | Self::Peers => 4 * 1024,
            Self::Aliases => 128 * 1024,
            Self::Policies | Self::Bindings => 16 * 1024,
            Self::Identities => 8 * 1024,
            Self::Addresses => 64 * 1024,
            Self::Certificates | Self::Listeners => 2 * 1024,
            Self::Global => GLOBAL_BYTES,
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Code {
    Allocation,
    Layout,
}
impl Code {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Allocation => "config_storage_allocation",
            Self::Layout => "config_storage_layout",
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Error {
    pub code: Code,
    pub region: Region,
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} in {}", self.code.name(), self.region.name())
    }
}
impl std::error::Error for Error {}
fn layout(region: Region) -> Error {
    Error {
        code: Code::Layout,
        region,
    }
}
fn payload<T>(count: usize, region: Region) -> Result<usize, Error> {
    if size_of::<T>() == 0 {
        return Err(layout(region));
    }
    let bytes = count
        .checked_mul(size_of::<T>())
        .ok_or_else(|| layout(region))?;
    if bytes > region.ceiling() {
        return Err(layout(region));
    }
    Ok(bytes)
}
fn allocate<T: Copy>(count: usize, empty: T, region: Region) -> Result<Vec<T>, Error> {
    allocate_with(count, empty, region, Vec::try_reserve_exact)
}
fn allocate_with<T: Copy>(
    count: usize,
    empty: T,
    region: Region,
    reserve: impl FnOnce(&mut Vec<T>, usize) -> Result<(), std::collections::TryReserveError>,
) -> Result<Vec<T>, Error> {
    payload::<T>(count, region)?;
    let mut cells = Vec::new();
    reserve(&mut cells, count).map_err(|_| Error {
        code: Code::Allocation,
        region,
    })?;
    payload::<T>(cells.capacity(), region)?;
    if cells.capacity() < count {
        return Err(layout(region));
    }
    // Spare capacity within the partition stays private; only count cells
    // become initialized and are lent to builders.
    cells.resize(count, empty);
    Ok(cells)
}

pub struct Storage {
    text: Vec<u8>,
    routing_text: Vec<u8>,
    domains: Vec<routing::DomainSlot>,
    aliases: Vec<routing::AliasSlot>,
    policies: Vec<policy::Slot>,
    identities: Vec<identities::IdentitySlot>,
    addresses: Vec<identities::AddressSlot>,
    certificates: Vec<certificate::Slot>,
    gateways: Vec<gateway::Slot>,
    peers: Vec<gateway::PeerSlot>,
    listeners: Vec<listener::Slot>,
    bindings: Vec<graph::BindingSlot>,
}
impl fmt::Debug for Storage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ConfigurationStorage(<redacted>)")
    }
}
const _: [(); 1] = [(); (size_of::<Storage>() <= 1024) as usize];
const TABLE_BYTES: usize = Region::Text.ceiling()
    + Region::RoutingText.ceiling()
    + Region::Domains.ceiling()
    + Region::Aliases.ceiling()
    + Region::Policies.ceiling()
    + Region::Identities.ceiling()
    + Region::Addresses.ceiling()
    + Region::Certificates.ceiling()
    + Region::Gateways.ceiling()
    + Region::Peers.ceiling()
    + Region::Listeners.ceiling()
    + Region::Bindings.ceiling();
const _: [(); 1] =
    [(); (TABLE_BYTES + DEVICE_BYTES + PLANS_BYTES + GLOBAL_BYTES == SNAPSHOT_BYTES) as usize];
macro_rules! fits {
    ($ty:ty, $count:expr, $region:ident) => {
        const _: [(); 1] = [(); ($count <= Region::$region.ceiling() / size_of::<$ty>()) as usize];
    };
}
fits!(u8, text::MAX_BYTES, Text);
fits!(u8, routing::MAX_TEXT_BYTES, RoutingText);
fits!(routing::DomainSlot, routing::MAX_DOMAINS, Domains);
fits!(routing::AliasSlot, routing::MAX_ALIASES, Aliases);
fits!(policy::Slot, routing::MAX_DOMAINS, Policies);
fits!(
    identities::IdentitySlot,
    identity::MAX_IDENTITIES,
    Identities
);
fits!(
    identities::AddressSlot,
    identities::MAX_ADDRESS_CELLS,
    Addresses
);
fits!(certificate::Slot, certificate::MAX_PROFILES, Certificates);
fits!(gateway::Slot, gateway::MAX_GATEWAYS, Gateways);
fits!(gateway::PeerSlot, gateway::MAX_PEERS, Peers);
fits!(listener::Slot, listener::MAX_LISTENERS, Listeners);
fits!(graph::BindingSlot, graph::MAX_BINDINGS, Bindings);
impl Storage {
    /// Call before serving. Subsequent builder/view operations never grow these regions.
    pub fn try_new() -> Result<Self, Error> {
        let storage = Self {
            text: allocate(text::MAX_BYTES, 0, Region::Text)?,
            routing_text: allocate(routing::MAX_TEXT_BYTES, 0, Region::RoutingText)?,
            domains: allocate(
                routing::MAX_DOMAINS,
                routing::DomainSlot::EMPTY,
                Region::Domains,
            )?,
            aliases: allocate(
                routing::MAX_ALIASES,
                routing::AliasSlot::EMPTY,
                Region::Aliases,
            )?,
            policies: allocate(routing::MAX_DOMAINS, policy::Slot::EMPTY, Region::Policies)?,
            identities: allocate(
                identity::MAX_IDENTITIES,
                identities::IdentitySlot::EMPTY,
                Region::Identities,
            )?,
            addresses: allocate(
                identities::MAX_ADDRESS_CELLS,
                identities::AddressSlot::EMPTY,
                Region::Addresses,
            )?,
            certificates: allocate(
                certificate::MAX_PROFILES,
                certificate::Slot::EMPTY,
                Region::Certificates,
            )?,
            gateways: allocate(
                gateway::MAX_GATEWAYS,
                gateway::Slot::EMPTY,
                Region::Gateways,
            )?,
            peers: allocate(gateway::MAX_PEERS, gateway::PeerSlot::EMPTY, Region::Peers)?,
            listeners: allocate(
                listener::MAX_LISTENERS,
                listener::Slot::EMPTY,
                Region::Listeners,
            )?,
            bindings: allocate(
                graph::MAX_BINDINGS,
                graph::BindingSlot::EMPTY,
                Region::Bindings,
            )?,
        };
        storage.allocated_bytes()?;
        Ok(storage)
    }
    /// Vec payload capacities plus this owner, excluding allocator overhead and
    /// sealed headers/plans (counted by Candidate) and future device storage.
    pub fn allocated_bytes(&self) -> Result<usize, Error> {
        let regions = [
            payload::<u8>(self.text.capacity(), Region::Text)?,
            payload::<u8>(self.routing_text.capacity(), Region::RoutingText)?,
            payload::<routing::DomainSlot>(self.domains.capacity(), Region::Domains)?,
            payload::<routing::AliasSlot>(self.aliases.capacity(), Region::Aliases)?,
            payload::<policy::Slot>(self.policies.capacity(), Region::Policies)?,
            payload::<identities::IdentitySlot>(self.identities.capacity(), Region::Identities)?,
            payload::<identities::AddressSlot>(self.addresses.capacity(), Region::Addresses)?,
            payload::<certificate::Slot>(self.certificates.capacity(), Region::Certificates)?,
            payload::<gateway::Slot>(self.gateways.capacity(), Region::Gateways)?,
            payload::<gateway::PeerSlot>(self.peers.capacity(), Region::Peers)?,
            payload::<listener::Slot>(self.listeners.capacity(), Region::Listeners)?,
            payload::<graph::BindingSlot>(self.bindings.capacity(), Region::Bindings)?,
        ];
        let mut total = size_of::<Self>();
        for bytes in regions {
            total = total
                .checked_add(bytes)
                .ok_or_else(|| layout(Region::Global))?;
        }
        // Defense against an accounting-list change; individual regions already
        // imply this bound. Unbuilt metadata keeps its own reservation.
        let ceiling = TABLE_BYTES
            .checked_add(size_of::<Self>())
            .ok_or_else(|| layout(Region::Global))?;
        if total > ceiling {
            return Err(layout(Region::Global));
        }
        Ok(total)
    }
    /// Results borrow this owner. Drop them before rebuilding or moving it.
    pub fn builder<'a, 'w>(
        &'a mut self,
        pending: &'w mut Pending,
    ) -> Result<dispatch::Builder<'a, 'w>, dispatch::Error> {
        dispatch::Builder::new(
            &mut self.text,
            dispatch::Tables {
                route_text: &mut self.routing_text,
                domains: &mut self.domains,
                aliases: &mut self.aliases,
                policies: &mut self.policies,
                identities: &mut self.identities,
                addresses: &mut self.addresses,
                certificates: &mut self.certificates,
                gateways: &mut self.gateways,
                peers: &mut self.peers,
                listeners: &mut self.listeners,
                bindings: &mut self.bindings,
            },
            pending,
        )
    }
}

/// A complete set of supplied statements; not reader EOF or runtime authority.
///
/// A borrowed view prevents consuming its owner:
/// ```compile_fail,E0505
/// use td_mta::config::{dispatch, storage::Candidate};
/// fn reuse_while_borrowed(candidate: Candidate) -> Result<(), dispatch::Error> {
///     let identities = candidate.identities()?;
///     let storage = candidate.into_storage();
///     let count = identities.len();
///     Ok(())
/// }
/// ```
pub struct Candidate {
    storage: Storage,
    header: dispatch::Header,
}
impl fmt::Debug for Candidate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ConfigurationCandidate(<redacted>)")
    }
}
/// Failure returns storage to its caller, without any validated record headers.
pub struct Failed<E> {
    storage: Storage,
    error: BuildError<E>,
}
impl<E> fmt::Debug for Failed<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("FailedConfiguration(<redacted>)")
    }
}
impl<E> Failed<E> {
    pub fn error(&self) -> &BuildError<E> {
        &self.error
    }
    pub fn into_parts(self) -> (Storage, BuildError<E>) {
        (self.storage, self.error)
    }
}
pub enum BuildError<E> {
    Input(E),
    Configuration(dispatch::Error),
}
impl<E> fmt::Debug for BuildError<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Input(_) => f.write_str("ConfigurationInputError(<redacted>)"),
            Self::Configuration(e) => e.fmt(f),
        }
    }
}
impl<E> fmt::Display for BuildError<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Input(_) => f.write_str("config_candidate_input"),
            Self::Configuration(e) => e.fmt(f),
        }
    }
}
impl<E> std::error::Error for BuildError<E> {}
fn configuration(cause: dispatch::Cause) -> dispatch::Error {
    dispatch::Error {
        cause,
        context: None,
    }
}
/// Restricted statement sink: callers cannot replace the owner-bound builder.
/// ```compile_fail,E0616
/// use td_mta::config::{dispatch::Builder, storage::Statements};
/// fn extract<'a, 'w, 's>(input: Statements<'a, 'w, 's>) -> &'s mut Builder<'a, 'w> {
///     input.builder
/// }
/// ```
pub struct Statements<'a, 'w, 's> {
    builder: &'s mut dispatch::Builder<'a, 'w>,
}
impl fmt::Debug for Statements<'_, '_, '_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ConfigurationStatements(<redacted>)")
    }
}
impl Statements<'_, '_, '_> {
    pub fn accept(
        &mut self,
        statement: super::syntax::Statement<'_>,
    ) -> Result<(), dispatch::Error> {
        self.builder.accept(statement)
    }
}
impl Storage {
    // Returning the preallocated owner avoids allocating error storage.
    #[allow(clippy::result_large_err)]
    pub fn build_stanzas<E>(
        mut self,
        pending: &mut Pending,
        feed: impl FnOnce(Statements<'_, '_, '_>) -> Result<(), E>,
    ) -> Result<Candidate, Failed<E>> {
        let result = (|| {
            let mut builder = self.builder(pending).map_err(BuildError::Configuration)?;
            if let Err(input) = feed(Statements {
                builder: &mut builder,
            }) {
                return Err(match builder.failure() {
                    Some(error) => BuildError::Configuration(error),
                    None => BuildError::Input(input),
                });
            }
            builder
                .finish_stanzas()
                .map(dispatch::Parsed::seal)
                .map_err(BuildError::Configuration)
        })();
        match result {
            Ok(header) => Ok(Candidate {
                storage: self,
                header,
            }),
            Err(error) => Err(Failed {
                storage: self,
                error,
            }),
        }
    }
}
const _: [(); 1] = [(); (size_of::<Candidate>() <= GLOBAL_BYTES + PLANS_BYTES) as usize];
const _: [(); 1] = [(); (size_of::<dispatch::Header>() <= 4 * 1024) as usize];
const _: [(); 1] = [(); (size_of::<super::resources::Validated>() <= PLANS_BYTES) as usize];
const _: [(); 1] = [(); (size_of::<graph::Header>() <= 1024) as usize];
const _: [(); 1] = [(); (size_of::<identities::Header>() <= 128) as usize];
impl Candidate {
    pub(super) fn input_owner(&self) -> Result<std::num::NonZeroU64, dispatch::Error> {
        Ok(self.text()?.owner())
    }
    fn text(&self) -> Result<text::View<'_>, dispatch::Error> {
        self.header
            .text
            .view(&self.storage.text)
            .map_err(|e| configuration(dispatch::Cause::Text(e)))
    }
    pub fn identities(&self) -> Result<identities::View<'_, '_>, dispatch::Error> {
        self.header
            .identities
            .view(
                &self.storage.identities,
                &self.storage.addresses,
                self.text()?,
            )
            .map_err(|e| configuration(dispatch::Cause::Identities(e)))
    }
    pub fn outbound(&self) -> Result<super::outbound::View<'_, '_>, dispatch::Error> {
        self.header
            .outbound
            .view(self.text()?)
            .map_err(|e| configuration(dispatch::Cause::Outbound(e)))
    }
    pub fn globals(&self) -> Result<super::globals::View<'_, '_>, dispatch::Error> {
        self.header
            .globals
            .view(self.text()?)
            .map_err(|e| configuration(dispatch::Cause::Globals(e)))
    }
    pub fn server(&self) -> super::globals::Server {
        self.header.globals.server()
    }
    pub fn minimum_severity(&self) -> crate::observability::Severity {
        self.header.globals.minimum_severity()
    }
    pub fn resources(&self) -> &super::resources::Validated {
        &self.header.resources
    }
    /// The small graph header is temporary; no borrowed graph view can escape.
    /// ```compile_fail,E0521
    /// use td_mta::config::{graph, storage::Candidate};
    /// fn escape<'a>(candidate: &'a Candidate)
    ///     -> Option<Result<graph::View<'a, 'a>, graph::Error>> {
    ///     let mut escaped = None;
    ///     candidate.with_graph(|records, text| { escaped = Some(records.view(text)); }).ok();
    ///     escaped
    /// }
    /// ```
    pub fn with_graph<R>(
        &self,
        read: impl FnOnce(&graph::Records<'_>, text::View<'_>) -> R,
    ) -> Result<R, dispatch::Error> {
        let s = &self.storage;
        let records = self
            .header
            .graph
            .reopen(graph::Backing {
                routing_text: &s.routing_text,
                domains: &s.domains,
                aliases: &s.aliases,
                policies: &s.policies,
                listeners: &s.listeners,
                certificates: &s.certificates,
                gateways: &s.gateways,
                peers: &s.peers,
                bindings: &s.bindings,
            })
            .map_err(|e| configuration(dispatch::Cause::Graph(e)))?;
        let text = self.text()?;
        records
            .view(text)
            .map_err(|e| configuration(dispatch::Cause::Graph(e)))?;
        Ok(read(&records, text))
    }
    pub fn allocated_bytes(&self) -> Result<usize, Error> {
        self.storage
            .allocated_bytes()?
            .checked_sub(size_of::<Storage>())
            .and_then(|n| n.checked_add(size_of::<Self>()))
            .ok_or_else(|| layout(Region::Global))
    }
    /// Consumes all validation headers. No borrowed view may outlive this move.
    pub fn into_storage(self) -> Storage {
        self.storage
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use std::num::NonZeroU32;
    fn lock() -> std::sync::MutexGuard<'static, ()> {
        text::TEST_CONSTRUCTION_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }
    fn source(name: &str, extra: bool) -> String {
        let mut source = format!(
            r#"version = 1
[server]
hostname = "mail.example.test"
jmap_origin = "https://jmap.example.test"
[account "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"]
username = "{name}"
[domain "example.test"]
[alias "main@example.test"]
account = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
[identity "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"]
account = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
email = "main@example.test"
[resolver "primary"]
address = "127.0.0.1:53"
[relay]
host = "relay.example.test"
port = 465
username = "{name}"
password_file = "/password"
[certificate "public"]
mode = "files"
chain_file = "/chain"
key_file = "/key"
[listener "smtp"]
kind = "direct_smtp"
bind = "0.0.0.0:25"
server_name = "mail.example.test"
certificate = "public"
session_limit = 1
per_peer_limit = 1
[listener "https"]
kind = "https"
bind = "0.0.0.0:443"
certificate = "public"
"#
        );
        if extra {
            source.push_str(
                "[alias \"old@example.test\"]\naccount = \"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\"\n",
            );
        }
        source
    }
    fn build<'a>(
        storage: &'a mut Storage,
        source: &str,
    ) -> Result<dispatch::Parsed<'a>, dispatch::Error> {
        let mut pending = Pending::new();
        let mut builder = storage.builder(&mut pending)?;
        let mut decoded = [0; 4096];
        for (index, line) in source.lines().enumerate() {
            builder.accept(
                super::super::syntax::parse_line(
                    NonZeroU32::new(index as u32 + 1).unwrap(),
                    line.as_bytes(),
                    &mut decoded,
                )
                .unwrap(),
            )?;
        }
        builder.finish_stanzas()
    }
    fn expanded_source() -> String {
        source("longer-name", true).replace(
            "email = \"main@example.test\"",
            "email = \"main@example.test\"\nreply_to = true",
        ) + &format!(
            r#"[domain "extra.test"]
mta_sts = "testing"
mta_sts_certificate = "public"
[identity "cccccccccccccccccccccccccccccccc"]
account = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
email = "extra@example.test"
[identity_address "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"]
kind = "reply_to"
name = "old-address"
email = "reply@example.test"
[certificate "other"]
mode = "files"
chain_file = "/other-chain"
key_file = "/other-key"
[gateway "trusted"]
ca_file = "/gateway-ca"
client_cert_sha256 = "{}"
[gateway_peer "trusted"]
network = "192.0.2.0/24"
[listener "gateway"]
kind = "gateway_smtp"
bind = "127.0.0.1:2526"
server_name = "gateway.example.test"
certificate = "other"
gateway = "trusted"
session_limit = 1
per_peer_limit = 1
[resolver "second"]
address = "127.0.0.2:53"
"#,
            "a".repeat(64)
        )
    }
    fn allocation_ids(storage: &Storage) -> [(usize, usize); 12] {
        macro_rules! id {
            ($field:ident) => {
                (storage.$field.as_ptr() as usize, storage.$field.capacity())
            };
        }
        [
            id!(text),
            id!(routing_text),
            id!(domains),
            id!(aliases),
            id!(policies),
            id!(identities),
            id!(addresses),
            id!(certificates),
            id!(gateways),
            id!(peers),
            id!(listeners),
            id!(bindings),
        ]
    }
    #[test]
    fn complete_table_layout_fits_and_initializes_every_region() {
        let mut storage = Storage::try_new().unwrap();
        assert_eq!(storage.text.len(), text::MAX_BYTES);
        assert_eq!(storage.routing_text.len(), routing::MAX_TEXT_BYTES);
        assert_eq!(storage.domains.len(), routing::MAX_DOMAINS);
        assert_eq!(storage.aliases.len(), routing::MAX_ALIASES);
        assert_eq!(storage.policies.len(), routing::MAX_DOMAINS);
        assert_eq!(storage.identities.len(), identity::MAX_IDENTITIES);
        assert_eq!(storage.addresses.len(), identities::MAX_ADDRESS_CELLS);
        assert_eq!(storage.certificates.len(), certificate::MAX_PROFILES);
        assert_eq!(storage.gateways.len(), gateway::MAX_GATEWAYS);
        assert_eq!(storage.peers.len(), gateway::MAX_PEERS);
        assert_eq!(storage.listeners.len(), listener::MAX_LISTENERS);
        assert_eq!(storage.bindings.len(), graph::MAX_BINDINGS);
        assert!(storage.text.iter().all(|b| *b == 0));
        assert!(storage.routing_text.iter().all(|b| *b == 0));
        let total = storage.allocated_bytes().unwrap();
        assert!(
            total
                <= SNAPSHOT_BYTES - GLOBAL_BYTES - PLANS_BYTES - DEVICE_BYTES
                    + size_of::<Storage>()
        );
        let actual: usize = [
            storage.text.capacity(),
            storage.routing_text.capacity(),
            storage.domains.capacity() * size_of::<routing::DomainSlot>(),
            storage.aliases.capacity() * size_of::<routing::AliasSlot>(),
            storage.policies.capacity() * size_of::<policy::Slot>(),
            storage.identities.capacity() * size_of::<identities::IdentitySlot>(),
            storage.addresses.capacity() * size_of::<identities::AddressSlot>(),
            storage.certificates.capacity() * size_of::<certificate::Slot>(),
            storage.gateways.capacity() * size_of::<gateway::Slot>(),
            storage.peers.capacity() * size_of::<gateway::PeerSlot>(),
            storage.listeners.capacity() * size_of::<listener::Slot>(),
            storage.bindings.capacity() * size_of::<graph::BindingSlot>(),
            size_of::<Storage>(),
        ]
        .into_iter()
        .sum();
        assert_eq!(total, actual);
        let old_capacity = storage.policies.capacity();
        storage.policies.try_reserve_exact(1).unwrap();
        assert!(storage.policies.capacity() > old_capacity);
        let added = (storage.policies.capacity() - old_capacity) * size_of::<policy::Slot>();
        assert_eq!(storage.allocated_bytes().unwrap(), total + added);
    }
    #[test]
    fn allocation_failures_and_excess_capacity_are_explicit() {
        let never = |_: &mut Vec<u8>, _: usize| -> Result<(), std::collections::TryReserveError> {
            panic!("precheck must refuse before allocation")
        };
        assert_eq!(
            allocate_with(Region::Text.ceiling() + 1, 0u8, Region::Text, never),
            Err(layout(Region::Text))
        );
        assert_eq!(
            payload::<u64>(usize::MAX, Region::Global),
            Err(layout(Region::Global))
        );
        assert_eq!(
            payload::<()>(1, Region::Global),
            Err(layout(Region::Global))
        );
        let failed = allocate_with(1, 0u8, Region::Text, |v, _| v.try_reserve_exact(usize::MAX));
        assert_eq!(
            failed,
            Err(Error {
                code: Code::Allocation,
                region: Region::Text
            })
        );
        let excess = allocate_with(1, 0u8, Region::Listeners, |v, _| {
            v.try_reserve_exact(Region::Listeners.ceiling() + 1)
        });
        assert_eq!(excess, Err(layout(Region::Listeners)));
        let short = allocate_with(1, 0u8, Region::Text, |_, _| Ok(()));
        assert_eq!(short, Err(layout(Region::Text)));
        let rounded =
            allocate_with(1, 0xa5u8, Region::Listeners, |v, _| v.try_reserve_exact(2)).unwrap();
        assert_eq!(rounded.len(), 1);
        assert!(rounded.capacity() >= 2);
        assert_eq!(rounded.as_slice(), &[0xa5]);
        let exact = allocate(Region::Listeners.ceiling(), 0xa5u8, Region::Listeners).unwrap();
        assert_eq!(exact.len(), Region::Listeners.ceiling());
        assert!(exact.iter().all(|b| *b == 0xa5));
    }
    #[test]
    fn reuse_after_success_and_failure_retains_regions_and_hides_old_prefixes() {
        let _guard = lock();
        let mut storage = Storage::try_new().unwrap();
        let allocations = allocation_ids(&storage);
        {
            let first = build(&mut storage, &expanded_source()).unwrap();
            assert_eq!(first.graph.routes().alias_count(), 2);
            assert_eq!(first.graph.routes().domain_count(), 2);
            let text = first.text().unwrap();
            let identities = first.identities.view(text).unwrap();
            assert_eq!(identities.len(), 2);
            assert_eq!(
                identities
                    .identity(0)
                    .unwrap()
                    .unwrap()
                    .reply_to
                    .unwrap()
                    .count(),
                1
            );
            let graph = first.graph.view(text).unwrap();
            assert_eq!(graph.certificates().unwrap().len(), 2);
            assert_eq!(graph.listeners().unwrap().len(), 3);
            assert_eq!(graph.binding_count(), 4);
            let gateway = graph.gateways().unwrap().gateway(0).unwrap().unwrap();
            assert_eq!(gateway.peer_count(), 1);
            assert_eq!(first.outbound.view(text).unwrap().resolver_count(), 2);
            assert!(first
                .graph
                .routes()
                .resolve("old@example.test")
                .unwrap()
                .is_some());
        }
        assert_eq!(allocation_ids(&storage), allocations);
        for _ in 0..3 {
            let invalid = source("broken", true)
                .replace("certificate = \"public\"", "certificate = \"missing\"");
            assert!(build(&mut storage, &invalid).is_err());
            assert_eq!(allocation_ids(&storage), allocations);
            {
                let next = build(&mut storage, &source("new", false)).unwrap();
                assert_eq!(next.graph.routes().alias_count(), 1);
                assert_eq!(next.graph.routes().domain_count(), 1);
                assert_eq!(next.graph.routes().domain_name(1).unwrap(), None);
                let text = next.text().unwrap();
                let identities = next.identities.view(text).unwrap();
                assert_eq!(identities.len(), 1);
                assert!(identities.identity(1).unwrap().is_none());
                let identity = identities.identity(0).unwrap().unwrap();
                assert!(identity.reply_to.is_none());
                assert!(identity.bcc.is_none());
                let graph = next.graph.view(text).unwrap();
                assert_eq!(graph.certificates().unwrap().len(), 1);
                assert!(graph.certificates().unwrap().profile(1).unwrap().is_none());
                assert_eq!(graph.listeners().unwrap().len(), 2);
                assert_eq!(graph.binding_count(), 2);
                assert!(graph.binding(2).unwrap().is_none());
                assert_eq!(graph.gateways().unwrap().len(), 0);
                assert!(graph.gateways().unwrap().gateway(0).unwrap().is_none());
                assert_eq!(graph.domains().unwrap().len(), 1);
                assert!(graph.domains().unwrap().domain(1).unwrap().is_none());
                assert_eq!(next.outbound.view(text).unwrap().resolver_count(), 1);
                assert_eq!(
                    next.graph.routes().resolve("old@example.test").unwrap(),
                    None
                );
                assert_eq!(
                    next.identities
                        .view(next.text().unwrap())
                        .unwrap()
                        .account()
                        .unwrap()
                        .username,
                    "new"
                );
            }
            assert_eq!(allocation_ids(&storage), allocations);
        }
        let mut moved = Box::new(storage);
        assert_eq!(allocation_ids(&moved), allocations);
        let parsed = build(&mut moved, &source("moved", false)).unwrap();
        assert_eq!(
            parsed
                .outbound
                .view(parsed.text().unwrap())
                .unwrap()
                .relay()
                .unwrap()
                .username,
            "moved"
        );
    }
    #[test]
    fn replacement_failure_preserves_other_storage_and_diagnostics_are_redacted() {
        let _guard = lock();
        let mut old_storage = Storage::try_new().unwrap();
        let old = build(&mut old_storage, &source("private-old", true)).unwrap();
        let mut new_storage = Storage::try_new().unwrap();
        let error = build(
            &mut new_storage,
            &source("private-new", false).replace(
                "password_file = \"/password\"",
                "password_file = \"private-relative-path\"",
            ),
        )
        .unwrap_err();
        assert_eq!(
            old.identities
                .view(old.text().unwrap())
                .unwrap()
                .account()
                .unwrap()
                .username,
            "private-old"
        );
        assert!(!format!("{new_storage:?} {old:?} {error:?} {error}").contains("private"));
        assert!(std::error::Error::source(&error).is_none());
        let allocation = Error {
            code: Code::Allocation,
            region: Region::Text,
        };
        assert_eq!(format!("{allocation}"), "config_storage_allocation in text");
        let error = layout(Region::Text);
        assert_eq!(format!("{error}"), "config_storage_layout in text");
        assert!(std::error::Error::source(&error).is_none());
    }
    #[allow(clippy::result_large_err)]
    fn candidate(storage: Storage, source: &str) -> Result<Candidate, Failed<dispatch::Error>> {
        let mut pending = Pending::new();
        storage.build_stanzas(&mut pending, |mut builder| {
            let mut decoded = [0; 4096];
            for (index, line) in source.lines().enumerate() {
                builder.accept(
                    super::super::syntax::parse_line(
                        NonZeroU32::new(index as u32 + 1).unwrap(),
                        line.as_bytes(),
                        &mut decoded,
                    )
                    .unwrap(),
                )?;
            }
            Ok(())
        })
    }
    #[test]
    fn sealed_candidate_moves_and_views_use_the_original_regions() {
        let _guard = lock();
        let storage = Storage::try_new().unwrap();
        let allocations = allocation_ids(&storage);
        let allocated = storage.allocated_bytes().unwrap();
        let value = candidate(storage, &expanded_source()).unwrap();
        assert_eq!(
            value.allocated_bytes().unwrap(),
            allocated - size_of::<Storage>() + size_of::<Candidate>()
        );
        assert!(value.allocated_bytes().unwrap() < SNAPSHOT_BYTES - DEVICE_BYTES);
        assert_eq!(allocation_ids(&value.storage), allocations);
        let boxed = Box::new(value);
        let identities = boxed.identities().unwrap();
        assert_eq!(identities.len(), 2);
        assert_eq!(
            identities
                .identity(0)
                .unwrap()
                .unwrap()
                .reply_to
                .unwrap()
                .count(),
            1
        );
        assert_eq!(
            boxed.identities().unwrap().account().unwrap().username,
            "longer-name"
        );
        assert_eq!(
            boxed.outbound().unwrap().relay().unwrap().username,
            "longer-name"
        );
        assert_eq!(
            boxed.globals().unwrap().data().unwrap(),
            super::super::globals::DEFAULT_DATA
        );
        boxed
            .with_graph(|records, text| {
                assert_eq!(records.routes().alias_count(), 2);
                assert!(records
                    .routes()
                    .resolve("old@example.test")
                    .unwrap()
                    .is_some());
                let graph = records.view(text).unwrap();
                assert_eq!(graph.binding_count(), 4);
                assert_eq!(graph.certificates().unwrap().len(), 2);
                assert_eq!(graph.listeners().unwrap().len(), 3);
                let gateways = graph.gateways().unwrap();
                assert_eq!(gateways.len(), 1);
                assert_eq!(gateways.gateway(0).unwrap().unwrap().peer_count(), 1);
                assert!(gateways
                    .gateway(0)
                    .unwrap()
                    .unwrap()
                    .peer(0)
                    .unwrap()
                    .is_some());
                assert_eq!(graph.origin().unwrap().host(), "jmap.example.test");
            })
            .unwrap();
        assert_eq!(allocation_ids(&boxed.storage), allocations);
        let storage = boxed.into_storage();
        let rebuilt = candidate(storage, &source("short", false)).unwrap();
        assert_eq!(allocation_ids(&rebuilt.storage), allocations);
        assert_eq!(rebuilt.identities().unwrap().len(), 1);
        assert!(rebuilt
            .identities()
            .unwrap()
            .identity(0)
            .unwrap()
            .unwrap()
            .reply_to
            .is_none());
        assert_eq!(
            rebuilt.identities().unwrap().account().unwrap().username,
            "short"
        );
        rebuilt
            .with_graph(|records, text| {
                assert_eq!(records.routes().alias_count(), 1);
                assert_eq!(records.routes().domain_count(), 1);
                assert_eq!(records.routes().resolve("old@example.test").unwrap(), None);
                let graph = records.view(text).unwrap();
                assert_eq!(graph.binding_count(), 2);
                assert_eq!(graph.certificates().unwrap().len(), 1);
                assert_eq!(graph.listeners().unwrap().len(), 2);
                assert_eq!(graph.gateways().unwrap().len(), 0);
                assert!(graph.gateways().unwrap().gateway(0).unwrap().is_none());
            })
            .unwrap();
    }
    #[test]
    fn failed_input_and_finalization_return_reusable_storage_without_headers() {
        let _guard = lock();
        let old = candidate(Storage::try_new().unwrap(), &source("old", true)).unwrap();
        let mut storage = Storage::try_new().unwrap();
        let allocations = allocation_ids(&storage);
        let mut pending = Pending::new();
        let failure = storage
            .build_stanzas(&mut pending, |_| Err("private supplied failure"))
            .unwrap_err();
        assert!(
            !format!("{failure:?} {:?} {}", failure.error(), failure.error()).contains("private")
        );
        assert!(std::error::Error::source(failure.error()).is_none());
        let (returned, cause) = failure.into_parts();
        assert!(matches!(
            cause,
            BuildError::Input("private supplied failure")
        ));
        storage = returned;
        let malformed =
            source("new", false).replace("certificate = \"public\"", "certificate = \"missing\"");
        let failure = candidate(storage, &malformed).unwrap_err();
        assert!(matches!(failure.error(), BuildError::Configuration(_)));
        let (returned, _) = failure.into_parts();
        assert_eq!(allocation_ids(&returned), allocations);
        assert_eq!(old.identities().unwrap().account().unwrap().username, "old");
        let next = candidate(returned, &source("new", false)).unwrap();
        assert_eq!(allocation_ids(&next.storage), allocations);
        assert_eq!(
            next.identities().unwrap().account().unwrap().username,
            "new"
        );
    }
    #[test]
    fn propagated_and_ignored_accept_errors_keep_the_first_diagnostic() {
        let _guard = lock();
        let source = source("valid", false) + "private_unknown = true\n";
        let mut storage = Storage::try_new().unwrap();
        let mut pending = Pending::new();
        let mut scratch = vec![0; super::super::stream::SCRATCH_BYTES];
        for ignored in [false, true] {
            let mut observed = None;
            let failed = storage
                .build_stanzas(&mut pending, |mut sink| {
                    super::super::stream::read(&mut source.as_bytes(), &mut scratch, |statement| {
                        let result = sink.accept(statement);
                        if let Err(error) = result {
                            observed.get_or_insert(error);
                            if ignored {
                                return Ok(());
                            }
                        }
                        result
                    })
                    .map(|_| ())
                })
                .unwrap_err();
            let wanted = observed.unwrap();
            assert!(matches!(failed.error(), BuildError::Configuration(error) if *error == wanted));
            assert_eq!(failed.error().to_string(), wanted.to_string());
            (storage, _) = failed.into_parts();
        }
    }

    #[test]
    fn sealed_owner_and_used_prefix_checks_refuse_internal_mismatch() {
        let _guard = lock();
        let mut first = candidate(Storage::try_new().unwrap(), &source("first", false)).unwrap();
        let mut second = candidate(Storage::try_new().unwrap(), &source("second", true)).unwrap();
        std::mem::swap(&mut first.header.text, &mut second.header.text);
        assert!(first.identities().is_err());
        assert!(first.outbound().is_err());
        assert!(first.globals().is_err());
        assert!(first
            .with_graph(|_, _| panic!("foreign owner reached callback"))
            .is_err());
        std::mem::swap(&mut first.header.text, &mut second.header.text);
        first.storage.identities.clear();
        assert!(first.identities().is_err());
        second.storage.bindings.clear();
        assert!(second.with_graph(|_, _| ()).is_err());
    }
    #[test]
    fn every_sealed_prefix_rejects_truncated_backing() {
        let _guard = lock();
        let mut value = candidate(Storage::try_new().unwrap(), &expanded_source()).unwrap();
        macro_rules! reject_graph {
            ($field:ident) => {{
                let mut cells = Vec::new();
                std::mem::swap(&mut cells, &mut value.storage.$field);
                assert!(value.with_graph(|_, _| ()).is_err(), stringify!($field));
                std::mem::swap(&mut cells, &mut value.storage.$field);
            }};
        }
        reject_graph!(routing_text);
        reject_graph!(domains);
        reject_graph!(aliases);
        reject_graph!(policies);
        reject_graph!(listeners);
        reject_graph!(certificates);
        reject_graph!(gateways);
        reject_graph!(peers);
        reject_graph!(bindings);
        let mut addresses = Vec::new();
        std::mem::swap(&mut addresses, &mut value.storage.addresses);
        assert!(value.identities().is_err());
        std::mem::swap(&mut addresses, &mut value.storage.addresses);
        assert!(value.identities().is_ok());
        assert!(value.with_graph(|_, _| ()).is_ok());
    }
}

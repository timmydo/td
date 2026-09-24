//! Typed stanza handoff. Only the later whole-reader loader can prove EOF.
use super::{
    certificate,
    endpoint::Origin,
    gateway, globals, graph, identities, listener, outbound, policy, resources, routing,
    stanza::{self, Pending, Section, Stanza},
    syntax::{Location, Statement, Value},
    text, values,
};
use crate::ids::{AccountId, IdentityId};
use std::fmt;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Code {
    VersionFirst,
    Version,
    DuplicateVersion,
    RootField,
    UnknownSection,
    ForbiddenLabel,
    DuplicateSection,
    MissingServer,
    MissingVersion,
    Field,
    Id,
    CertificateChainRequired,
    CertificateKeyRequired,
    CertificateChainForbidden,
    CertificateKeyForbidden,
    Invariant,
}
impl Code {
    pub const fn name(self) -> &'static str {
        match self {
            Self::VersionFirst => "config_dispatch_version_first",
            Self::Version => "config_dispatch_version",
            Self::DuplicateVersion => "config_dispatch_duplicate_version",
            Self::RootField => "config_dispatch_root_field",
            Self::UnknownSection => "config_dispatch_unknown_section",
            Self::ForbiddenLabel => "config_dispatch_forbidden_label",
            Self::DuplicateSection => "config_dispatch_duplicate_section",
            Self::MissingServer => "config_dispatch_missing_server",
            Self::MissingVersion => "config_dispatch_missing_version",
            Self::Field => "config_dispatch_field",
            Self::Id => "config_dispatch_id",
            Self::CertificateChainRequired => "config_certificate_chain_required",
            Self::CertificateKeyRequired => "config_certificate_key_required",
            Self::CertificateChainForbidden => "config_certificate_chain_forbidden",
            Self::CertificateKeyForbidden => "config_certificate_key_forbidden",
            Self::Invariant => "config_dispatch_invariant",
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Diagnostic {
    pub code: Code,
    pub section: Option<Section>,
    pub field: Option<&'static str>,
    pub location: Option<Location>,
    pub previous: Option<Location>,
}
impl fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.code.name())?;
        if let Some(section) = self.section {
            write!(f, " in {}", section.name())?;
        }
        if let Some(field) = self.field {
            write!(f, " for {field}")?;
        }
        if let Some(at) = self.location {
            write!(f, " at line {}, byte column {}", at.line, at.column)?;
        }
        if let Some(at) = self.previous {
            write!(
                f,
                " (previously declared at line {}, byte column {})",
                at.line, at.column
            )?;
        }
        Ok(())
    }
}
impl std::error::Error for Diagnostic {}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Cause {
    Dispatch(Diagnostic),
    Stanza(stanza::Error),
    Resources(resources::Error),
    Globals(globals::Error),
    Identities(identities::Error),
    Policy(policy::Error),
    Outbound(outbound::Error),
    Certificate(certificate::Error),
    Gateway(gateway::Error),
    Listener(listener::Error),
    Graph(graph::Error),
    Text(text::Code),
}
impl fmt::Display for Cause {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Dispatch(e) => e.fmt(f),
            Self::Stanza(e) => e.fmt(f),
            Self::Resources(e) => e.fmt(f),
            Self::Globals(e) => e.fmt(f),
            Self::Identities(e) => e.fmt(f),
            Self::Policy(e) => e.fmt(f),
            Self::Outbound(e) => e.fmt(f),
            Self::Certificate(e) => e.fmt(f),
            Self::Gateway(e) => e.fmt(f),
            Self::Listener(e) => e.fmt(f),
            Self::Graph(e) => e.fmt(f),
            Self::Text(e) => e.fmt(f),
        }
    }
}
impl std::error::Error for Cause {}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SourceContext {
    pub section: Section,
    pub field: Option<&'static str>,
    pub location: Location,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Error {
    pub cause: Cause,
    pub context: Option<SourceContext>,
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.cause.fmt(f)?;
        if let Some(c) = self.context {
            write!(f, " in {}", c.section.name())?;
            if let Some(field) = c.field {
                write!(f, " for {field}")?;
            }
            write!(
                f,
                " at line {}, byte column {}",
                c.location.line, c.location.column
            )?;
        }
        Ok(())
    }
}
impl std::error::Error for Error {}
impl Error {
    fn from_dispatch(cause: Diagnostic) -> Self {
        Self {
            cause: Cause::Dispatch(cause),
            context: None,
        }
    }
    fn from_stanza(cause: stanza::Error) -> Self {
        Self {
            cause: Cause::Stanza(cause),
            context: None,
        }
    }
    fn from_resources(cause: resources::Error) -> Self {
        Self {
            cause: Cause::Resources(cause),
            context: None,
        }
    }
    fn from_globals(cause: globals::Error) -> Self {
        Self {
            cause: Cause::Globals(cause),
            context: None,
        }
    }
    fn from_identities(cause: identities::Error) -> Self {
        Self {
            cause: Cause::Identities(cause),
            context: None,
        }
    }
    fn from_policy(cause: policy::Error) -> Self {
        Self {
            cause: Cause::Policy(cause),
            context: None,
        }
    }
    fn from_outbound(cause: outbound::Error) -> Self {
        Self {
            cause: Cause::Outbound(cause),
            context: None,
        }
    }
    fn from_certificate(cause: certificate::Error) -> Self {
        Self {
            cause: Cause::Certificate(cause),
            context: None,
        }
    }
    fn from_gateway(cause: gateway::Error) -> Self {
        Self {
            cause: Cause::Gateway(cause),
            context: None,
        }
    }
    fn from_listener(cause: listener::Error) -> Self {
        Self {
            cause: Cause::Listener(cause),
            context: None,
        }
    }
    fn from_graph(cause: graph::Error) -> Self {
        Self {
            cause: Cause::Graph(cause),
            context: None,
        }
    }
    fn from_text(cause: text::Code) -> Self {
        Self {
            cause: Cause::Text(cause),
            context: None,
        }
    }
}
fn diagnostic(
    code: Code,
    section: Option<Section>,
    field: Option<&'static str>,
    location: Option<Location>,
) -> Error {
    Error::from_dispatch(Diagnostic {
        code,
        section,
        field,
        location,
        previous: None,
    })
}
fn invariant() -> Error {
    diagnostic(Code::Invariant, None, None, None)
}

pub struct Tables<'a> {
    pub route_text: &'a mut [u8],
    pub domains: &'a mut [routing::DomainSlot],
    pub aliases: &'a mut [routing::AliasSlot],
    pub policies: &'a mut [policy::Slot],
    pub identities: &'a mut [identities::IdentitySlot],
    pub addresses: &'a mut [identities::AddressSlot],
    pub certificates: &'a mut [certificate::Slot],
    pub gateways: &'a mut [gateway::Slot],
    pub peers: &'a mut [gateway::PeerSlot],
    pub listeners: &'a mut [listener::Slot],
    pub bindings: &'a mut [graph::BindingSlot],
}
impl fmt::Debug for Tables<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ConfigurationTables(<redacted>)")
    }
}
struct Targets<'a> {
    resources: resources::Builder,
    globals: globals::Builder,
    identities: identities::Builder<'a>,
    policy: policy::Builder<'a>,
    outbound: outbound::Builder,
    certificates: certificate::Builder<'a>,
    gateways: gateway::Builder<'a>,
    listeners: listener::Builder<'a>,
    bindings: &'a mut [graph::BindingSlot],
}
struct ServerNames {
    hostname: [u8; values::MAX_DOMAIN_BYTES],
    hostname_len: usize,
    hostname_at: Location,
    origin: [u8; graph::MAX_CANONICAL_ORIGIN_BYTES + 1],
    origin_len: usize,
    origin_at: Location,
}
impl ServerNames {
    fn hostname(&self) -> Result<&str, Error> {
        std::str::from_utf8(
            self.hostname
                .get(..self.hostname_len)
                .ok_or_else(invariant)?,
        )
        .map_err(|_| invariant())
    }
    fn origin(&self) -> Result<Origin<'_>, Error> {
        Origin::parse(
            std::str::from_utf8(self.origin.get(..self.origin_len).ok_or_else(invariant)?)
                .map_err(|_| invariant())?,
        )
        .map_err(|_| invariant())
    }
}
#[derive(Clone, Copy)]
enum Active {
    None,
    Pending,
    Resource(resources::Section),
}
pub struct Builder<'a, 'w> {
    arena: text::Builder<'a>,
    targets: Targets<'a>,
    pending: &'w mut Pending,
    server: Option<ServerNames>,
    version: Option<Location>,
    singletons: [Option<Location>; 10],
    active: Active,
    failure: Option<Error>,
}
const _: [(); 1] = [(); (std::mem::size_of::<Builder<'_, '_>>() + std::mem::size_of::<Pending>()
    <= 36 * 1024) as usize];
impl fmt::Debug for Builder<'_, '_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("StanzaDispatcher(<redacted>)")
    }
}
fn singleton(section: Section) -> Option<usize> {
    match section {
        Section::Server => Some(0),
        Section::Paths => Some(1),
        Section::Logging => Some(2),
        Section::Account => Some(3),
        Section::Relay => Some(4),
        Section::Acme => Some(5),
        Section::Resource(resources::Section::Limits) => Some(6),
        Section::Resource(resources::Section::Disk) => Some(7),
        Section::Resource(resources::Section::Work) => Some(8),
        Section::Resource(resources::Section::Network) => Some(9),
        _ => None,
    }
}
impl<'a, 'w> Builder<'a, 'w> {
    pub fn new(
        bytes: &'a mut [u8],
        tables: Tables<'a>,
        pending: &'w mut Pending,
    ) -> Result<Self, Error> {
        let arena = text::Builder::new(bytes).map_err(Error::from_text)?;
        let targets = Targets {
            resources: resources::Builder::default(),
            globals: globals::Builder::new(&arena),
            identities: identities::Builder::new(&arena, tables.identities, tables.addresses)
                .map_err(Error::from_identities)?,
            policy: policy::Builder::new(
                &arena,
                tables.route_text,
                tables.domains,
                tables.aliases,
                tables.policies,
            )
            .map_err(Error::from_policy)?,
            outbound: outbound::Builder::new(&arena),
            certificates: certificate::Builder::new(&arena, tables.certificates)
                .map_err(Error::from_certificate)?,
            gateways: gateway::Builder::new(&arena, tables.gateways, tables.peers)
                .map_err(Error::from_gateway)?,
            listeners: listener::Builder::new(&arena, tables.listeners)
                .map_err(Error::from_listener)?,
            bindings: tables.bindings,
        };
        *pending = Pending::new();
        Ok(Self {
            arena,
            targets,
            pending,
            server: None,
            version: None,
            singletons: [None; 10],
            active: Active::None,
            failure: None,
        })
    }
    pub fn accept(&mut self, statement: Statement<'_>) -> Result<(), Error> {
        if let Some(e) = self.failure {
            return Err(e);
        }
        let result = self.accept_inner(statement);
        if let Err(e) = result {
            self.failure = Some(e);
        }
        result
    }
    fn flush(&mut self) -> Result<(), Error> {
        if matches!(self.active, Active::Pending) {
            let stanza = self.pending.finish().map_err(Error::from_stanza)?;
            apply(stanza, &mut self.targets, &mut self.arena, &mut self.server)?;
        }
        self.active = Active::None;
        Ok(())
    }
    fn accept_inner(&mut self, statement: Statement<'_>) -> Result<(), Error> {
        match statement {
            Statement::Empty => Ok(()),
            Statement::Section {
                location,
                name,
                label,
            } => {
                if self.version.is_none() {
                    return Err(diagnostic(Code::VersionFirst, None, None, Some(location)));
                }
                self.flush()?;
                let section = Section::parse(name)
                    .ok_or_else(|| diagnostic(Code::UnknownSection, None, None, Some(location)))?;
                if let Some(index) = singleton(section) {
                    let old = self.singletons.get_mut(index).ok_or_else(invariant)?;
                    if let Some(previous) = *old {
                        return Err(Error::from_dispatch(Diagnostic {
                            previous: Some(previous),
                            code: Code::DuplicateSection,
                            section: Some(section),
                            field: None,
                            location: Some(location),
                        }));
                    }
                    *old = Some(location);
                }
                if let Section::Resource(resource) = section {
                    if label.is_some() {
                        return Err(diagnostic(
                            Code::ForbiddenLabel,
                            Some(section),
                            None,
                            Some(location),
                        ));
                    }
                    self.targets
                        .resources
                        .begin(resource, location)
                        .map_err(Error::from_resources)?;
                    self.active = Active::Resource(resource);
                } else {
                    self.pending
                        .begin(section, label, location)
                        .map_err(Error::from_stanza)?;
                    self.active = Active::Pending;
                }
                Ok(())
            }
            Statement::Assignment {
                location,
                key,
                value_location,
                value,
            } => {
                if self.version.is_none() {
                    if key != "version" {
                        return Err(diagnostic(Code::VersionFirst, None, None, Some(location)));
                    }
                    if value != Value::Integer(1) {
                        return Err(diagnostic(
                            Code::Version,
                            None,
                            Some("version"),
                            Some(value_location),
                        ));
                    }
                    self.version = Some(location);
                    return Ok(());
                }
                match self.active {
                    Active::None => {
                        let mut e = Diagnostic {
                            code: Code::RootField,
                            section: None,
                            field: None,
                            location: Some(location),
                            previous: None,
                        };
                        if key == "version" {
                            e.code = Code::DuplicateVersion;
                            e.field = Some("version");
                            e.previous = self.version;
                        }
                        Err(Error::from_dispatch(e))
                    }
                    Active::Pending => self
                        .pending
                        .assign(key, value, location, value_location)
                        .map_err(Error::from_stanza),
                    Active::Resource(section) => self
                        .targets
                        .resources
                        .assign(section, key, value, location)
                        .map_err(Error::from_resources),
                }
            }
        }
    }
    /// Finalizes supplied statements only. This is never proof of reader EOF.
    pub fn finish_stanzas(mut self) -> Result<Parsed<'a>, Error> {
        if let Some(e) = self.failure {
            return Err(e);
        }
        if self.version.is_none() {
            return Err(diagnostic(Code::MissingVersion, None, None, None));
        }
        self.flush()?;
        let names = self
            .server
            .ok_or_else(|| diagnostic(Code::MissingServer, Some(Section::Server), None, None))?;
        let globals = self
            .targets
            .globals
            .finish(&mut self.arena)
            .map_err(Error::from_globals)?;
        let resources = self
            .targets
            .resources
            .finish(globals.server().view_mode)
            .map_err(Error::from_resources)?;
        let identities = self
            .targets
            .identities
            .finish()
            .map_err(Error::from_identities)?;
        let domains = self
            .targets
            .policy
            .finish(&mut self.arena, names.hostname()?, names.hostname_at)
            .map_err(Error::from_policy)?;
        let outbound = self
            .targets
            .outbound
            .finish()
            .map_err(Error::from_outbound)?;
        let certificates = self
            .targets
            .certificates
            .finish()
            .map_err(Error::from_certificate)?;
        let gateways = self
            .targets
            .gateways
            .finish()
            .map_err(Error::from_gateway)?;
        let listeners = self
            .targets
            .listeners
            .finish(resources.resources())
            .map_err(Error::from_listener)?;
        let graph = graph::bind(
            &mut self.arena,
            self.targets.bindings,
            names.origin()?,
            names.origin_at,
            graph::Inputs {
                listeners,
                certificates,
                gateways,
                domains,
            },
        )
        .map_err(Error::from_graph)?;
        Ok(Parsed {
            graph,
            identities,
            outbound,
            globals,
            resources,
            text: self.arena,
        })
    }
}
pub struct Parsed<'a> {
    pub graph: graph::Records<'a>,
    pub identities: identities::Records<'a>,
    pub outbound: outbound::Records,
    pub globals: globals::Records,
    pub resources: resources::Validated,
    text: text::Builder<'a>,
}
impl fmt::Debug for Parsed<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ParsedStanzas(<redacted>)")
    }
}
impl Parsed<'_> {
    pub fn text(&self) -> Result<text::View<'_>, Error> {
        self.text.borrowed_view().map_err(Error::from_text)
    }
}
struct Context<'a> {
    stanza: Stanza<'a>,
    section: Section,
    at: Location,
}
impl<'a> Context<'a> {
    fn error(&self, code: Code, field: Option<&'static str>, at: Location) -> Error {
        diagnostic(code, Some(self.section), field, Some(at))
    }
    fn entry(&self, key: &'static str) -> Result<Option<stanza::Entry<'a>>, Error> {
        self.stanza.entry(key).map_err(Error::from_stanza)
    }
    fn at(&self, key: &'static str) -> Result<Location, Error> {
        Ok(self
            .entry(key)?
            .map_or(self.at, |entry| entry.value_location))
    }
    fn text(&self, key: &'static str) -> Result<Option<&'a str>, Error> {
        match self.entry(key)? {
            None => Ok(None),
            Some(entry) => match entry.value {
                Value::Text(value) => Ok(Some(value)),
                _ => Err(invariant()),
            },
        }
    }
    fn required(&self, key: &'static str, code: Code) -> Result<&'a str, Error> {
        self.text(key)?
            .ok_or_else(|| self.error(code, Some(key), self.at))
    }
    fn required_text(&self, key: &'static str) -> Result<&'a str, Error> {
        self.required(key, Code::Field)
    }
    fn integer(&self, key: &'static str) -> Result<Option<u64>, Error> {
        match self.entry(key)? {
            None => Ok(None),
            Some(entry) => match entry.value {
                Value::Integer(value) => Ok(Some(value)),
                _ => Err(invariant()),
            },
        }
    }
    fn boolean(&self, key: &'static str, default: bool) -> Result<bool, Error> {
        match self.entry(key)? {
            None => Ok(default),
            Some(entry) => match entry.value {
                Value::Boolean(value) => Ok(value),
                _ => Err(invariant()),
            },
        }
    }
    fn label(&self) -> Result<&'a str, Error> {
        self.stanza
            .label()
            .map_err(Error::from_stanza)?
            .ok_or_else(invariant)
    }
    fn account(&self, key: &'static str) -> Result<AccountId, Error> {
        let at = self.at(key)?;
        AccountId::parse(self.required_text(key)?).map_err(|_| self.error(Code::Id, Some(key), at))
    }
    fn forbid(&self, key: &'static str, code: Code) -> Result<(), Error> {
        if let Some(entry) = self.entry(key)? {
            Err(self.error(code, Some(key), entry.value_location))
        } else {
            Ok(())
        }
    }
    fn annotate(&self, mut error: Error) -> Error {
        let field = match error.cause {
            Cause::Globals(e) => e.field.map(globals::Field::name),
            Cause::Listener(e) => e.field.filter(|f| *f != listener::Field::Name).map(listener::Field::name),
            Cause::Outbound(e) => match e.code {
                outbound::Code::Endpoint | outbound::Code::DuplicateResolverEndpoint => Some("address"),
                outbound::Code::Hostname => Some("host"),
                outbound::Code::Port => Some("port"),
                outbound::Code::Username => Some("username"),
                outbound::Code::PasswordPath => Some("password_file"),
                outbound::Code::CaPath => Some("ca_file"),
                _ => None,
            },
            Cause::Certificate(e) => match e.code {
                certificate::Code::ChainPath => Some("chain_file"),
                certificate::Code::KeyPath => Some("key_file"),
                certificate::Code::CaPath => Some("ca_file"),
                certificate::Code::Directory => Some("directory"),
                certificate::Code::Contact => Some("contact"),
                certificate::Code::Terms => Some("terms_accepted"),
                _ => None,
            },
            Cause::Identities(e) => match e.code {
                identities::Code::Username => Some("username"),
                identities::Code::Name => Some("name"),
                identities::Code::Mailbox | identities::Code::Wildcard => Some("email"),
                identities::Code::SignaturePath => self.invalid_path(&["text_signature_file", "html_signature_file"]),
                _ => None,
            },
            Cause::Policy(e) => match e.code {
                policy::Code::DnsName => Some("mx_host"),
                policy::Code::Preference => Some("mx_preference"),
                policy::Code::Age => Some("mta_sts_max_age_seconds"),
                policy::Code::CertificateRequired | policy::Code::CertificateForbidden | policy::Code::Profile => Some("mta_sts_certificate"),
                _ => None,
            },
            Cause::Gateway(e) => match e.code {
                gateway::Code::Path => Some("ca_file"),
                gateway::Code::Prefix => Some("network"),
                gateway::Code::SamePins => Some("next_client_cert_sha256"),
                gateway::Code::Pin => ["client_cert_sha256", "next_client_cert_sha256"].into_iter().find(|key| matches!(self.text(key), Ok(Some(value)) if gateway::LeafPin::parse(value).is_err())),
                _ => None,
            },
            Cause::Dispatch(_) | Cause::Stanza(_) => return error,
            _ => None,
        };
        let location = match field {
            Some(key) => match self.at(key) {
                Ok(at) => at,
                Err(e) => return e,
            },
            None => self.at,
        };
        error.context = Some(SourceContext {
            section: self.section,
            field,
            location,
        });
        error
    }
    fn invalid_path(&self, keys: &[&'static str]) -> Option<&'static str> {
        keys.iter().copied().find(|key| matches!(self.text(key), Ok(Some(value)) if values::absolute_path(value).is_err()))
    }
    fn value_error(&self, key: &'static str) -> Error {
        match self.at(key) {
            Ok(at) => self.error(Code::Field, Some(key), at),
            Err(e) => e,
        }
    }
}
fn apply(
    stanza: Stanza<'_>,
    targets: &mut Targets<'_>,
    arena: &mut text::Builder<'_>,
    server: &mut Option<ServerNames>,
) -> Result<(), Error> {
    let c = Context {
        section: stanza.section().map_err(Error::from_stanza)?,
        at: stanza.location().map_err(Error::from_stanza)?,
        stanza,
    };
    let result = apply_context(&c, targets, arena, server);
    result.map_err(|e| c.annotate(e))
}
fn apply_context(
    c: &Context<'_>,
    targets: &mut Targets<'_>,
    arena: &mut text::Builder<'_>,
    server: &mut Option<ServerNames>,
) -> Result<(), Error> {
    match c.section {
        Section::Server => {
            let hostname = c.required_text("hostname")?;
            values::dns_name(hostname).map_err(|_| c.value_error("hostname"))?;
            let origin = c.required_text("jmap_origin")?;
            Origin::parse(origin).map_err(|_| c.value_error("jmap_origin"))?;
            let mut names = ServerNames {
                hostname: [0; values::MAX_DOMAIN_BYTES],
                hostname_len: hostname.len(),
                hostname_at: c.at("hostname")?,
                origin: [0; graph::MAX_CANONICAL_ORIGIN_BYTES + 1],
                origin_len: origin.len(),
                origin_at: c.at("jmap_origin")?,
            };
            names
                .hostname
                .get_mut(..hostname.len())
                .ok_or_else(invariant)?
                .copy_from_slice(hostname.as_bytes());
            names
                .origin
                .get_mut(..origin.len())
                .ok_or_else(invariant)?
                .copy_from_slice(origin.as_bytes());
            targets
                .globals
                .server(
                    arena,
                    globals::ServerInput {
                        online_background: c.boolean(
                            "online_background",
                            globals::ServerInput::default().online_background,
                        )?,
                        public_ipv4: c.text("public_ipv4")?,
                        public_ipv6: c.text("public_ipv6")?,
                    },
                    c.at,
                )
                .map_err(Error::from_globals)?;
            *server = Some(names);
            Ok(())
        }
        Section::Paths => targets
            .globals
            .paths(
                arena,
                globals::PathsInput {
                    data: c.text("data")?,
                    runtime: c.text("runtime")?,
                    logs: c.text("logs")?,
                },
                c.at,
            )
            .map_err(Error::from_globals),
        Section::Logging => targets
            .globals
            .logging(arena, c.text("minimum_severity")?, c.at)
            .map_err(Error::from_globals),
        Section::Resolver => targets
            .outbound
            .resolver(arena, c.label()?, c.required_text("address")?, c.at)
            .map_err(Error::from_outbound),
        Section::Account => {
            let id = AccountId::parse(c.label()?).map_err(|_| c.error(Code::Id, None, c.at))?;
            targets
                .identities
                .account(
                    arena,
                    id,
                    c.required_text("username")?,
                    c.text("name")?.unwrap_or(""),
                    c.at,
                )
                .map_err(Error::from_identities)?;
            targets.policy.account(id, c.at).map_err(Error::from_policy)
        }
        Section::Domain => {
            let mode = match c.text("mta_sts")?.unwrap_or("off") {
                "off" => policy::Mode::Off,
                "testing" => policy::Mode::Testing,
                "enforce" => policy::Mode::Enforce,
                "none" => policy::Mode::None,
                _ => return Err(c.value_error("mta_sts")),
            };
            targets
                .policy
                .domain(
                    arena,
                    c.label()?,
                    policy::Input {
                        mx_host: c.text("mx_host")?,
                        mx_preference: c
                            .integer("mx_preference")?
                            .unwrap_or(u64::from(policy::DEFAULT_PREFERENCE)),
                        mode,
                        max_age_seconds: c
                            .integer("mta_sts_max_age_seconds")?
                            .unwrap_or(u64::from(policy::DEFAULT_MAX_AGE_SECONDS)),
                        certificate: c.text("mta_sts_certificate")?,
                    },
                    c.at,
                )
                .map_err(Error::from_policy)
        }
        Section::Alias => targets
            .policy
            .alias(c.label()?, c.account("account")?, c.at)
            .map_err(Error::from_policy),
        Section::Identity => {
            let id = IdentityId::parse(c.label()?).map_err(|_| c.error(Code::Id, None, c.at))?;
            targets
                .identities
                .identity(
                    arena,
                    identities::IdentityInput {
                        id,
                        account: c.account("account")?,
                        name: c.text("name")?.unwrap_or(""),
                        email: c.required_text("email")?,
                        reply_to: c.boolean("reply_to", false)?,
                        bcc: c.boolean("bcc", false)?,
                        text_signature_file: c.text("text_signature_file")?,
                        html_signature_file: c.text("html_signature_file")?,
                    },
                    c.at,
                )
                .map_err(Error::from_identities)
        }
        Section::IdentityAddress => {
            let identity =
                IdentityId::parse(c.label()?).map_err(|_| c.error(Code::Id, None, c.at))?;
            let list = match c.required_text("kind")? {
                "reply_to" => identities::List::ReplyTo,
                "bcc" => identities::List::Bcc,
                _ => return Err(c.value_error("kind")),
            };
            targets
                .identities
                .address(
                    arena,
                    identities::AddressInput {
                        identity,
                        list,
                        name: c.text("name")?,
                        email: c.required_text("email")?,
                    },
                    c.at,
                )
                .map_err(Error::from_identities)
        }
        Section::Relay => {
            let transport = match c.text("transport")? {
                None => outbound::Transport::default(),
                Some("implicit_tls") => outbound::Transport::ImplicitTls,
                Some("required_starttls") => outbound::Transport::RequiredStartTls,
                _ => return Err(c.value_error("transport")),
            };
            targets
                .outbound
                .relay(
                    arena,
                    outbound::RelayInput {
                        host: c.required_text("host")?,
                        port: c.integer("port")?.ok_or_else(invariant)?,
                        transport,
                        username: c.required_text("username")?,
                        password_file: c.required_text("password_file")?,
                        ca_file: c.text("ca_file")?,
                    },
                    c.at,
                )
                .map_err(Error::from_outbound)
        }
        Section::Certificate => {
            let input = match c.required_text("mode")? {
                "acme" => {
                    c.forbid("chain_file", Code::CertificateChainForbidden)?;
                    c.forbid("key_file", Code::CertificateKeyForbidden)?;
                    certificate::ProfileInput::Acme
                }
                "files" => certificate::ProfileInput::Files {
                    chain_file: c.required("chain_file", Code::CertificateChainRequired)?,
                    key_file: c.required("key_file", Code::CertificateKeyRequired)?,
                },
                _ => return Err(c.value_error("mode")),
            };
            targets
                .certificates
                .profile(arena, c.label()?, input, c.at)
                .map_err(Error::from_certificate)
        }
        Section::Acme => targets
            .certificates
            .acme(
                arena,
                certificate::AcmeInput {
                    directory: c.required_text("directory")?,
                    contact: c.required_text("contact")?,
                    terms_accepted: c.boolean("terms_accepted", false)?,
                    ca_file: c.text("ca_file")?,
                },
                c.at,
            )
            .map_err(Error::from_certificate),
        Section::Gateway => targets
            .gateways
            .gateway(
                arena,
                c.label()?,
                gateway::Input {
                    ca_file: c.required_text("ca_file")?,
                    client_cert_sha256: c.required_text("client_cert_sha256")?,
                    next_client_cert_sha256: c.text("next_client_cert_sha256")?,
                },
                c.at,
            )
            .map_err(Error::from_gateway),
        Section::GatewayPeer => targets
            .gateways
            .peer(arena, c.label()?, c.required_text("network")?, c.at)
            .map_err(Error::from_gateway),
        Section::Listener => {
            let kind = match c.required_text("kind")? {
                "direct_smtp" => listener::Kind::DirectSmtp,
                "gateway_smtp" => listener::Kind::GatewaySmtp,
                "https" => listener::Kind::Https,
                "http01" => listener::Kind::Http01,
                "loopback_smtp_fixture" => listener::Kind::LoopbackSmtpFixture,
                _ => return Err(c.value_error("kind")),
            };
            targets
                .listeners
                .listener(
                    arena,
                    c.label()?,
                    listener::Input {
                        kind,
                        bind: c.required_text("bind")?,
                        server_name: c.text("server_name")?,
                        certificate: c.text("certificate")?,
                        gateway: c.text("gateway")?,
                        session_limit: c.integer("session_limit")?,
                        per_peer_limit: c.integer("per_peer_limit")?,
                    },
                    c.at,
                )
                .map_err(Error::from_listener)
        }
        Section::Resource(_) => Err(invariant()),
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod tests {
    use super::*;
    use std::num::NonZeroU32;
    const ACCOUNT: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const IDENTITY: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const SOURCE: &str = r#"version = 1
[server]
hostname = "mail.example.test"
jmap_origin = "https://jmap.example.test"
[account "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"]
username = "operator"
[domain "example.test"]
[alias "operator@example.test"]
account = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
[identity "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"]
account = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
email = "operator@example.test"
[resolver "primary"]
address = "127.0.0.1:53"
[relay]
host = "relay.example.test"
port = 465
username = "operator"
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
"#;
    struct Backing {
        text: Vec<u8>,
        route_text: Vec<u8>,
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
    impl Backing {
        fn new() -> Self {
            Self {
                text: vec![0; 192 * 1024],
                route_text: vec![0; 32 * 1024],
                domains: vec![routing::DomainSlot::EMPTY; 8],
                aliases: vec![routing::AliasSlot::EMPTY; 16],
                policies: vec![policy::Slot::EMPTY; 8],
                identities: vec![identities::IdentitySlot::EMPTY; 4],
                addresses: vec![identities::AddressSlot::EMPTY; 8],
                certificates: vec![certificate::Slot::EMPTY; 4],
                gateways: vec![gateway::Slot::EMPTY; 4],
                peers: vec![gateway::PeerSlot::EMPTY; 8],
                listeners: vec![listener::Slot::EMPTY; 8],
                bindings: vec![graph::BindingSlot::EMPTY; 64],
            }
        }
        fn builder<'a, 'w>(&'a mut self, pending: &'w mut Pending) -> Builder<'a, 'w> {
            Builder::new(
                &mut self.text,
                Tables {
                    route_text: &mut self.route_text,
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
            .unwrap()
        }
    }
    fn feed(builder: &mut Builder<'_, '_>, source: &str) -> Result<(), Error> {
        let mut decoded = [0; 4096];
        for (index, line) in source.lines().enumerate() {
            let at = NonZeroU32::new(u32::try_from(index + 1).unwrap()).unwrap();
            let statement =
                super::super::syntax::parse_line(at, line.as_bytes(), &mut decoded).unwrap();
            builder.accept(statement)?;
            decoded.fill(0xa5);
        }
        Ok(())
    }
    fn parse<'a>(storage: &'a mut Backing, source: &str) -> Result<Parsed<'a>, Error> {
        let mut pending = Pending::new();
        let mut builder = storage.builder(&mut pending);
        feed(&mut builder, source)?;
        builder.finish_stanzas()
    }
    fn lock() -> std::sync::MutexGuard<'static, ()> {
        text::TEST_CONSTRUCTION_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }
    fn dispatch_code(error: Error) -> Code {
        match error.cause {
            Cause::Dispatch(d) => d.code,
            e => panic!("unexpected {e:?}"),
        }
    }
    #[test]
    fn complete_source_copies_and_closes_every_required_builder() {
        let _guard = lock();
        let mut storage = Backing::new();
        let parsed = parse(&mut storage, SOURCE).unwrap();
        let text = parsed.text().unwrap();
        let graph = parsed.graph.view(text).unwrap();
        assert_eq!(graph.binding_count(), 2);
        assert_eq!(
            parsed
                .graph
                .routes()
                .resolve("operator@EXAMPLE.TEST")
                .unwrap(),
            Some(AccountId::parse(ACCOUNT).unwrap())
        );
        assert_eq!(
            parsed.graph.routes().domain_name(0).unwrap(),
            Some("example.test")
        );
        let globals = parsed.globals.view(text).unwrap();
        assert_eq!(globals.data().unwrap(), globals::DEFAULT_DATA);
        assert_eq!(globals.runtime().unwrap(), globals::DEFAULT_RUNTIME);
        assert_eq!(globals.logs().unwrap(), globals::DEFAULT_LOGS);
        let identities = parsed.identities.view(text).unwrap();
        assert_eq!(identities.account().unwrap().username, "operator");
        assert_eq!(identities.len(), 1);
        let identity = identities.identity(0).unwrap().unwrap();
        assert_eq!(identity.id, IdentityId::parse(IDENTITY).unwrap());
        assert!(identity.reply_to.is_none());
        let outbound = parsed.outbound.view(text).unwrap();
        assert_eq!(outbound.resolver_count(), 1);
        assert_eq!(
            outbound.resolver(0).unwrap().unwrap().endpoint.to_string(),
            "127.0.0.1:53"
        );
        let relay = outbound.relay().unwrap();
        assert_eq!(relay.transport, outbound::Transport::ImplicitTls);
        assert_eq!(relay.password_file, "/password");
        assert!(relay.ca_file.is_none());
        assert_eq!(parsed.globals.minimum_severity(), globals::DEFAULT_SEVERITY);
        assert!(
            std::mem::size_of::<Builder<'_, '_>>() + std::mem::size_of::<Pending>() <= 36 * 1024
        );
    }
    #[test]
    fn declarations_can_follow_consumers_and_optional_settings_survive() {
        let _guard = lock();
        let source = SOURCE.replace("version = 1\n", "");
        let mut sections: Vec<String> = Vec::new();
        for line in source.lines() {
            if line.starts_with('[') {
                sections.push(String::new());
            }
            let last = sections.last_mut().unwrap();
            last.push_str(line);
            last.push('\n');
        }
        sections.reverse();
        let prefix = format!("version = 1\n[identity_address \"{IDENTITY}\"]\nkind = \"reply_to\"\nname = \"\"\nemail = \"reply@example.test\"\n");
        let source = (prefix + &sections.concat())
            .replace("email = \"operator@example.test\"", "email = \"operator@example.test\"\nreply_to = true\ntext_signature_file = \"/signature\"")
            .replace("[server]\n", "[server]\nonline_background = false\npublic_ipv4 = \"127.0.0.1\"\npublic_ipv6 = \"::1\"\n")
            .replace("[relay]\n", "[relay]\ntransport = \"required_starttls\"\nca_file = \"/relay-ca\"\n")
            + "[paths]\ndata = \"/data\"\nruntime = \"/runtime\"\nlogs = \"/logs\"\n[logging]\nminimum_severity = \"warning\"\n[resolver \"second\"]\naddress = \"[::1]:53\"\n";
        let mut storage = Backing::new();
        let parsed = parse(&mut storage, &source).unwrap();
        let text = parsed.text().unwrap();
        assert_eq!(parsed.globals.view(text).unwrap().data().unwrap(), "/data");
        assert_eq!(
            parsed.globals.minimum_severity(),
            crate::observability::Severity::Warning
        );
        let view = parsed.identities.view(text).unwrap();
        let identity = view.identity(0).unwrap().unwrap();
        assert_eq!(identity.text_signature_file, Some("/signature"));
        let addresses: Vec<_> = identity.reply_to.unwrap().collect();
        assert_eq!(addresses.len(), 1);
        let address = addresses[0].as_ref().unwrap();
        assert_eq!(address.name, Some(""));
        assert_eq!(address.email, "reply@example.test");
        let outbound = parsed.outbound.view(text).unwrap();
        assert_eq!(outbound.resolver_count(), 2);
        assert_eq!(outbound.resolver(1).unwrap().unwrap().name, "second");
        assert_eq!(
            outbound.relay().unwrap().transport,
            outbound::Transport::RequiredStartTls
        );
    }
    #[test]
    fn certificate_mode_presence_matrix_is_checked_before_typed_conversion() {
        let _guard = lock();
        for mode in ["files", "acme"] {
            for chain in [false, true] {
                for key in [false, true] {
                    let mut certificate = format!("mode = \"{mode}\"\n");
                    if chain {
                        certificate.push_str("chain_file = \"/chain\"\n");
                    }
                    if key {
                        certificate.push_str("key_file = \"/key\"\n");
                    }
                    let mut source = SOURCE.replace(
                        "mode = \"files\"\nchain_file = \"/chain\"\nkey_file = \"/key\"\n",
                        &certificate,
                    );
                    if mode == "acme" {
                        source.push_str("[acme]\ndirectory = \"https://acme.example.test/directory\"\ncontact = \"operator@example.test\"\nterms_accepted = true\n[listener \"challenge\"]\nkind = \"http01\"\nbind = \"0.0.0.0:80\"\n");
                    }
                    let expected = match (mode, chain, key) {
                        ("files", false, _) => Some(Code::CertificateChainRequired),
                        ("files", true, false) => Some(Code::CertificateKeyRequired),
                        ("acme", true, _) => Some(Code::CertificateChainForbidden),
                        ("acme", false, true) => Some(Code::CertificateKeyForbidden),
                        _ => None,
                    };
                    let mut storage = Backing::new();
                    let result = parse(&mut storage, &source);
                    match expected {
                        Some(code) => assert_eq!(dispatch_code(result.unwrap_err()), code),
                        None => {
                            result.unwrap();
                        }
                    }
                }
            }
        }
    }
    #[test]
    fn root_rules_and_resource_labels_are_strict_and_sticky() {
        let _guard = lock();
        let cases = [
            ("", Code::MissingVersion),
            ("[server]", Code::VersionFirst),
            ("other = 1", Code::VersionFirst),
            ("version = 2", Code::Version),
            ("version = true", Code::Version),
            ("version = \"1\"", Code::Version),
            ("version = 1\nversion = 1", Code::DuplicateVersion),
            ("version = 1\nsecret = 1", Code::RootField),
            ("version = 1\n[secret]", Code::UnknownSection),
            ("version = 1", Code::MissingServer),
        ];
        for (source, expected) in cases {
            let mut storage = Backing::new();
            let mut pending = Pending::new();
            let mut builder = storage.builder(&mut pending);
            let result = feed(&mut builder, source);
            if let Err(error) = result {
                assert_eq!(builder.accept(Statement::Empty), Err(error));
                assert_eq!(builder.finish_stanzas().unwrap_err(), error);
                assert_eq!(dispatch_code(error), expected);
            } else {
                assert_eq!(
                    dispatch_code(builder.finish_stanzas().unwrap_err()),
                    expected
                );
            }
        }
        for resource in ["limits", "disk", "work", "network"] {
            let mut storage = Backing::new();
            let source = format!("version = 1\n[{resource} \"secret\"]");
            assert_eq!(
                dispatch_code(parse(&mut storage, &source).unwrap_err()),
                Code::ForbiddenLabel
            );
        }
    }
    #[test]
    fn duplicate_singletons_refuse_at_header_before_staging() {
        let _guard = lock();
        let singletons = [
            ("server", "hostname = \"mail.example.test\"\njmap_origin = \"https://jmap.example.test\"\n"),
            ("paths", ""), ("logging", ""),
            ("account \"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\"", "username = \"operator\"\n"),
            ("relay", "host = \"relay.example.test\"\nport = 465\nusername = \"operator\"\npassword_file = \"/password\"\n"),
            ("acme", "directory = \"https://acme.example.test/directory\"\ncontact = \"operator@example.test\"\nterms_accepted = true\n"),
            ("limits", ""), ("disk", ""), ("work", ""), ("network", ""),
        ];
        for (section, fields) in singletons {
            let source = format!("version = 1\n[{section}]\n{fields}[{section}]\ninvalid = true\n");
            let mut storage = Backing::new();
            let error = parse(&mut storage, &source).unwrap_err();
            let Cause::Dispatch(d) = error.cause else {
                panic!("{error:?}")
            };
            assert_eq!(d.code, Code::DuplicateSection, "{section}");
            assert_eq!(d.previous.unwrap().line.get(), 2);
            assert_eq!(
                d.location.unwrap().line.get(),
                3 + fields.lines().count() as u32
            );
        }
    }
    #[test]
    fn typed_failures_keep_field_coordinates_and_do_not_echo_values() {
        let _guard = lock();
        let cases = [
            (
                "hostname = \"mail.example.test\"",
                "hostname = \"bad secret\"",
                "hostname",
            ),
            ("port = 465", "port = 0", "port"),
            (
                "password_file = \"/password\"",
                "password_file = \"relative-secret\"",
                "password_file",
            ),
            (
                "email = \"operator@example.test\"",
                "email = \"bad secret\"",
                "email",
            ),
            (
                "address = \"127.0.0.1:53\"",
                "address = \"secret.example.test:53\"",
                "address",
            ),
            (
                "chain_file = \"/chain\"",
                "chain_file = \"relative-secret\"",
                "chain_file",
            ),
            ("bind = \"0.0.0.0:25\"", "bind = \"secret:25\"", "bind"),
        ];
        for (before, after, field) in cases {
            let mut storage = Backing::new();
            let source = SOURCE.replace(before, after);
            let error = parse(&mut storage, &source).unwrap_err();
            let (actual_field, location) = match error.cause {
                Cause::Dispatch(d) => (d.field, d.location.unwrap()),
                _ => {
                    let c = error.context.unwrap();
                    (c.field, c.location)
                }
            };
            assert_eq!(actual_field, Some(field));
            assert_eq!(
                location.line.get() as usize,
                source.lines().position(|l| l == after).unwrap() + 1
            );
            assert!(location.column > 1);
            assert!(!format!("{error} {error:?}").contains("secret"));
            assert!(std::error::Error::source(&error).is_none());
        }
    }
    #[test]
    fn finalization_refuses_unclosed_references_and_preserves_prior_records() {
        let _guard = lock();
        let mut old_storage = Backing::new();
        let old = parse(&mut old_storage, SOURCE).unwrap();
        let sources = [
            SOURCE.replace("certificate = \"public\"", "certificate = \"missing\""),
            SOURCE.replace("server_name = \"mail.example.test\"", "server_name = \"other.example.test\""),
            SOURCE.replace("session_limit = 1", "session_limit = 9"),
            SOURCE.replace("[domain \"example.test\"]", "[domain \"other.test\"]"),
            SOURCE.replace("account = \"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\"", "account = \"cccccccccccccccccccccccccccccccc\""),
            SOURCE.replace("[resolver \"primary\"]\naddress = \"127.0.0.1:53\"\n", ""),
            SOURCE.replace("[relay]\nhost = \"relay.example.test\"\nport = 465\nusername = \"operator\"\npassword_file = \"/password\"\n", ""),
        ];
        for source in sources {
            let mut storage = Backing::new();
            assert!(parse(&mut storage, &source).is_err());
            assert_eq!(
                old.graph.routes().resolve("operator@example.test").unwrap(),
                Some(AccountId::parse(ACCOUNT).unwrap())
            );
            assert_eq!(
                old.identities
                    .view(old.text().unwrap())
                    .unwrap()
                    .account()
                    .unwrap()
                    .username,
                "operator"
            );
        }
        let mut storage = Backing::new();
        storage.text.truncate(8);
        assert!(parse(&mut storage, SOURCE).is_err());
        let mut storage = Backing::new();
        storage.bindings.truncate(1);
        assert!(parse(&mut storage, SOURCE).is_err());
    }
    #[test]
    fn gateway_forward_peers_policies_and_resources_reach_final_views() {
        let _guard = lock();
        let source = SOURCE
            .replace("[domain \"example.test\"]", "[domain \"example.test\"]\nmx_host = \"upstream.example.test\"\nmx_preference = 20\nmta_sts = \"testing\"\nmta_sts_certificate = \"public\"\nmta_sts_max_age_seconds = 123")
            .replace("kind = \"direct_smtp\"", "kind = \"gateway_smtp\"\ngateway = \"trusted\"")
            + &format!("[gateway_peer \"trusted\"]\nnetwork = \"192.0.2.0/24\"\n[gateway \"trusted\"]\nca_file = \"/gateway-ca\"\nclient_cert_sha256 = \"{}\"\nnext_client_cert_sha256 = \"{}\"\n[limits]\nsmtp_sessions = 4\n[disk]\n[work]\n[network]\n", "a".repeat(64), "b".repeat(64));
        let mut storage = Backing::new();
        let parsed = parse(&mut storage, &source).unwrap();
        assert_eq!(parsed.resources.resources().limits().smtp_sessions, 4);
        let graph = parsed.graph.view(parsed.text().unwrap()).unwrap();
        assert_eq!(graph.binding_count(), 3);
        let gateways = graph.gateways().unwrap();
        let gateway = gateways.gateway(0).unwrap().unwrap();
        assert_eq!(gateway.name, "trusted");
        assert_eq!(gateway.peer_count(), 1);
        assert_eq!(gateway.client_cert_sha256.as_bytes(), &[0xaa; 32]);
        let policies = graph.domains().unwrap();
        let domain = policies.domain(0).unwrap().unwrap();
        assert_eq!(domain.mode, policy::Mode::Testing);
        assert_eq!(domain.mx_host, "upstream.example.test");
        assert_eq!(domain.max_age_seconds, 123);
    }
    #[test]
    fn all_listener_role_fields_are_forwarded_including_forbidden_values() {
        let _guard = lock();
        // Each otherwise-valid isolated stanza exercises one of the five
        // optional fields; handoff happens at the following header.
        let roles = [
            ("direct_smtp", "0.0.0.0:25", [true, true, false, true, true]),
            (
                "gateway_smtp",
                "0.0.0.0:465",
                [true, true, true, true, true],
            ),
            ("https", "0.0.0.0:443", [false, true, false, false, false]),
            ("http01", "0.0.0.0:80", [false, false, false, false, false]),
            (
                "loopback_smtp_fixture",
                "127.0.0.1:2525",
                [false, false, false, true, true],
            ),
        ];
        let fields = [
            ("server_name", "\"mail.example.test\""),
            ("certificate", "\"public\""),
            ("gateway", "\"trusted\""),
            ("session_limit", "1"),
            ("per_peer_limit", "1"),
        ];
        for (role, bind, required) in roles {
            for (changed, (field, value)) in fields.iter().enumerate() {
                let mut source = format!(
                    "version = 1\n[listener \"test\"]\nkind = \"{role}\"\nbind = \"{bind}\"\n"
                );
                for (index, (key, value)) in fields.iter().enumerate() {
                    if required[index] && index != changed {
                        source.push_str(&format!("{key} = {value}\n"));
                    }
                }
                if !required[changed] {
                    source.push_str(&format!("{field} = {value}\n"));
                }
                source.push_str("[logging]\n");
                let mut storage = Backing::new();
                let error = parse(&mut storage, &source).unwrap_err();
                let Cause::Listener(e) = error.cause else {
                    panic!("{role}/{field}: {error:?}")
                };
                assert_eq!(
                    e.code,
                    if required[changed] {
                        listener::Code::Required
                    } else {
                        listener::Code::Forbidden
                    }
                );
                assert_eq!(e.field.unwrap().name(), *field);
            }
        }
    }
    #[test]
    fn integrated_schema_rejects_unknown_duplicate_type_missing_and_invalid_scalars() {
        let _guard = lock();
        let bad = [
            SOURCE.replace("[server]", "[server \"extra\"]"),
            SOURCE.replace("[resolver \"primary\"]", "[resolver \"BAD\"]"),
            SOURCE.replace("[domain \"example.test\"]", "[domain \"bad secret\"]"),
            SOURCE.replace(IDENTITY, "invalid-id"),
            SOURCE.replace(
                "host = \"relay.example.test\"",
                "host = \"relay.example.test\"\ntransport = \"optional_starttls\"",
            ),
            SOURCE.replace(
                "[domain \"example.test\"]",
                "[domain \"example.test\"]\nmta_sts = \"invalid\"",
            ),
            SOURCE.replace("mode = \"files\"", "mode = \"invalid\""),
            SOURCE.replace("kind = \"direct_smtp\"", "kind = \"invalid\""),
            SOURCE.replace("[server]", "[server]\nunknown = true"),
            SOURCE.replace(
                "hostname = \"mail.example.test\"",
                "hostname = \"mail.example.test\"\nhostname = false",
            ),
            SOURCE.replace("port = 465", "port = \"465\""),
            SOURCE.replace("hostname = \"mail.example.test\"\n", ""),
            SOURCE.to_owned() + "[network]\nunknown = 1\n",
            SOURCE.to_owned() + "[limits]\nsmtp_sessions = true\n",
            SOURCE.to_owned() + "[limits]\nsmtp_sessions = 0\n",
            SOURCE.to_owned() + "[limits]\nsmtp_sessions = 2\nsmtp_sessions = 2\n",
        ];
        for source in bad {
            let mut storage = Backing::new();
            assert!(parse(&mut storage, &source).is_err(), "{source}");
        }
    }
    #[test]
    fn label_diagnostics_are_distinct_from_mx_field_diagnostics() {
        let _guard = lock();
        for fields in ["", "\nmx_host = \"mail.example.test\""] {
            let mut storage = Backing::new();
            let source = format!("version = 1\n[domain \"bad secret\"]{fields}\n[logging]\n");
            let error = parse(&mut storage, &source).unwrap_err();
            assert!(matches!(
                error.cause,
                Cause::Policy(policy::Error {
                    code: policy::Code::Routing,
                    ..
                })
            ));
            let context = error.context.unwrap();
            assert_eq!(context.section, Section::Domain);
            assert_eq!(context.field, None);
            assert_eq!(context.location.line.get(), 2);
        }
        let mut storage = Backing::new();
        let error = parse(
            &mut storage,
            "version = 1\n[domain \"example.test\"]\nmx_host = \"bad secret\"\n[logging]\n",
        )
        .unwrap_err();
        assert!(matches!(
            error.cause,
            Cause::Policy(policy::Error {
                code: policy::Code::DnsName,
                ..
            })
        ));
        assert_eq!(error.context.unwrap().field, Some("mx_host"));
        assert_eq!(error.context.unwrap().location.line.get(), 3);
        let mut storage = Backing::new();
        let source = SOURCE.replace(
            "[alias \"operator@example.test\"]",
            "[alias \"operator@bad secret\"]",
        );
        let error = parse(&mut storage, &source).unwrap_err();
        assert!(matches!(
            error.cause,
            Cause::Policy(policy::Error {
                code: policy::Code::Routing,
                ..
            })
        ));
        assert_eq!(error.context.unwrap().field, None);
        assert_eq!(error.context.unwrap().section, Section::Alias);
    }
    #[test]
    fn longest_raw_origin_fits_and_missing_port_fails_in_pending() {
        let _guard = lock();
        let host = format!(
            "{}.{}.{}.{}",
            "a".repeat(63),
            "b".repeat(63),
            "c".repeat(63),
            "d".repeat(51)
        );
        assert_eq!(host.len(), values::MAX_DOMAIN_BYTES);
        for port in [443, 65535] {
            let origin = format!("https://{host}:{port}/");
            if port == 65535 {
                assert_eq!(origin.len(), graph::MAX_CANONICAL_ORIGIN_BYTES + 1);
            }
            let source = SOURCE
                .replace("https://jmap.example.test", &origin)
                .replace(
                    "bind = \"0.0.0.0:443\"",
                    &format!("bind = \"0.0.0.0:{port}\""),
                );
            let mut storage = Backing::new();
            let parsed = parse(&mut storage, &source).unwrap();
            let graph = parsed.graph.view(parsed.text().unwrap()).unwrap();
            assert_eq!(graph.origin().unwrap().host(), host);
            assert_eq!(graph.origin().unwrap().port(), port);
        }
        for origin in [
            format!("https://{host}x:65535/"),
            "https://[::1]/".to_owned(),
            "http://jmap.example.test/".to_owned(),
        ] {
            let mut storage = Backing::new();
            let source = SOURCE.replace("https://jmap.example.test", &origin);
            let error = parse(&mut storage, &source).unwrap_err();
            let Cause::Dispatch(d) = error.cause else {
                panic!("{error:?}")
            };
            assert_eq!(d.code, Code::Field);
            assert_eq!(d.field, Some("jmap_origin"));
        }
        let mut storage = Backing::new();
        let error = parse(&mut storage, &SOURCE.replace("port = 465\n", "")).unwrap_err();
        let Cause::Stanza(e) = error.cause else {
            panic!("{error:?}")
        };
        assert_eq!(e.code, stanza::Code::MissingField);
        assert_eq!(e.field, Some("port"));
    }
}

//! Bounded pending stanza syntax; complete schema/reference checks are separate.
use super::{
    resources,
    syntax::{Location, Value},
};
use std::fmt;

pub const MAX_FIELDS: usize = 8;
pub const TEXT_BYTES: usize = 12_672;
pub const WORKSPACE_BYTES: usize = 13 * 1024;
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Scalar {
    Text,
    Integer,
    Boolean,
}
impl Scalar {
    fn accepts(self, value: Value<'_>) -> bool {
        matches!(
            (self, value),
            (Self::Text, Value::Text(_))
                | (Self::Integer, Value::Integer(_))
                | (Self::Boolean, Value::Boolean(_))
        )
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Label {
    None,
    Profile,
    AccountId,
    IdentityId,
    Domain,
    Mailbox,
}
impl Label {
    const fn max_bytes(self) -> usize {
        match self {
            Self::None => 0,
            Self::Profile => super::values::MAX_PROFILE_BYTES,
            Self::AccountId | Self::IdentityId => 32,
            Self::Domain => super::values::MAX_DOMAIN_BYTES,
            Self::Mailbox => crate::format::row::MAX_ADDRESS,
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Section {
    Server,
    Paths,
    Resolver,
    Logging,
    Account,
    Domain,
    Alias,
    Identity,
    IdentityAddress,
    Relay,
    Certificate,
    Acme,
    Gateway,
    GatewayPeer,
    Listener,
    Resource(resources::Section),
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Field {
    pub name: &'static str,
    pub scalar: Scalar,
    pub required: bool,
}
macro_rules! field {
    ($name:literal, $scalar:ident, $required:literal) => {
        Field {
            name: $name,
            scalar: $scalar,
            required: $required,
        }
    };
}
impl Section {
    pub fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "server" => Self::Server,
            "paths" => Self::Paths,
            "resolver" => Self::Resolver,
            "logging" => Self::Logging,
            "account" => Self::Account,
            "domain" => Self::Domain,
            "alias" => Self::Alias,
            "identity" => Self::Identity,
            "identity_address" => Self::IdentityAddress,
            "relay" => Self::Relay,
            "certificate" => Self::Certificate,
            "acme" => Self::Acme,
            "gateway" => Self::Gateway,
            "gateway_peer" => Self::GatewayPeer,
            "listener" => Self::Listener,
            _ => Self::Resource(resources::Section::from_name(name)?),
        })
    }
    pub const fn name(self) -> &'static str {
        match self {
            Self::Server => "server",
            Self::Paths => "paths",
            Self::Resolver => "resolver",
            Self::Logging => "logging",
            Self::Account => "account",
            Self::Domain => "domain",
            Self::Alias => "alias",
            Self::Identity => "identity",
            Self::IdentityAddress => "identity_address",
            Self::Relay => "relay",
            Self::Certificate => "certificate",
            Self::Acme => "acme",
            Self::Gateway => "gateway",
            Self::GatewayPeer => "gateway_peer",
            Self::Listener => "listener",
            Self::Resource(section) => section.name(),
        }
    }
    pub const fn label(self) -> Label {
        match self {
            Self::Account => Label::AccountId,
            Self::Identity | Self::IdentityAddress => Label::IdentityId,
            Self::Domain => Label::Domain,
            Self::Alias => Label::Mailbox,
            Self::Resolver
            | Self::Certificate
            | Self::Gateway
            | Self::GatewayPeer
            | Self::Listener => Label::Profile,
            Self::Server
            | Self::Paths
            | Self::Logging
            | Self::Relay
            | Self::Acme
            | Self::Resource(_) => Label::None,
        }
    }
    /// Resource fields stay in their existing builder rather than pending cells.
    pub const fn fields(self) -> &'static [Field] {
        use Scalar::{Boolean, Integer, Text};
        match self {
            Self::Server => &[
                field!("hostname", Text, true),
                field!("jmap_origin", Text, true),
                field!("online_background", Boolean, false),
                field!("public_ipv4", Text, false),
                field!("public_ipv6", Text, false),
            ],
            Self::Paths => &[
                field!("data", Text, false),
                field!("runtime", Text, false),
                field!("logs", Text, false),
            ],
            Self::Resolver => &[field!("address", Text, true)],
            Self::Logging => &[field!("minimum_severity", Text, false)],
            Self::Account => &[field!("username", Text, true), field!("name", Text, false)],
            Self::Domain => &[
                field!("mx_host", Text, false),
                field!("mx_preference", Integer, false),
                field!("mta_sts", Text, false),
                field!("mta_sts_max_age_seconds", Integer, false),
                field!("mta_sts_certificate", Text, false),
            ],
            Self::Alias => &[field!("account", Text, true)],
            Self::Identity => &[
                field!("account", Text, true),
                field!("name", Text, false),
                field!("email", Text, true),
                field!("reply_to", Boolean, false),
                field!("bcc", Boolean, false),
                field!("text_signature_file", Text, false),
                field!("html_signature_file", Text, false),
            ],
            Self::IdentityAddress => &[
                field!("kind", Text, true),
                field!("name", Text, false),
                field!("email", Text, true),
            ],
            Self::Relay => &[
                field!("host", Text, true),
                field!("port", Integer, true),
                field!("transport", Text, false),
                field!("username", Text, true),
                field!("password_file", Text, true),
                field!("ca_file", Text, false),
            ],
            Self::Certificate => &[
                field!("mode", Text, true),
                field!("chain_file", Text, false),
                field!("key_file", Text, false),
            ],
            Self::Acme => &[
                field!("directory", Text, true),
                field!("contact", Text, true),
                field!("terms_accepted", Boolean, false),
                field!("ca_file", Text, false),
            ],
            Self::Gateway => &[
                field!("ca_file", Text, true),
                field!("client_cert_sha256", Text, true),
                field!("next_client_cert_sha256", Text, false),
            ],
            Self::GatewayPeer => &[field!("network", Text, true)],
            Self::Listener => &[
                field!("kind", Text, true),
                field!("bind", Text, true),
                field!("server_name", Text, false),
                field!("certificate", Text, false),
                field!("gateway", Text, false),
                field!("session_limit", Integer, false),
                field!("per_peer_limit", Integer, false),
            ],
            Self::Resource(_) => &[],
        }
    }
    pub fn field(self, name: &str) -> Option<(usize, Field)> {
        self.fields()
            .iter()
            .copied()
            .enumerate()
            .find(|(_, field)| field.name == name)
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Code {
    State,
    ResourceSection,
    MissingLabel,
    ForbiddenLabel,
    LabelLength,
    UnknownField,
    DuplicateField,
    Type,
    MissingField,
    TextLength,
    Capacity,
    Invariant,
}
impl Code {
    pub const fn name(self) -> &'static str {
        match self {
            Self::State => "config_stanza_state",
            Self::ResourceSection => "config_stanza_resource_section",
            Self::MissingLabel => "config_stanza_missing_label",
            Self::ForbiddenLabel => "config_stanza_forbidden_label",
            Self::LabelLength => "config_stanza_label_length",
            Self::UnknownField => "config_stanza_unknown_field",
            Self::DuplicateField => "config_stanza_duplicate_field",
            Self::Type => "config_stanza_type",
            Self::MissingField => "config_stanza_missing_field",
            Self::Capacity => "config_stanza_capacity",
            Self::TextLength => "config_stanza_text_length",
            Self::Invariant => "config_stanza_invariant",
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Error {
    pub code: Code,
    pub section: Option<Section>,
    pub field: Option<&'static str>,
    pub location: Option<Location>,
    pub previous: Option<Location>,
}
impl fmt::Display for Error {
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
                " (previously assigned at line {}, byte column {})",
                at.line, at.column
            )?;
        }
        Ok(())
    }
}
impl std::error::Error for Error {}
#[derive(Clone, Copy)]
struct Span {
    start: u16,
    len: u16,
}
impl Span {
    const EMPTY: Self = Self { start: 0, len: 0 };
}
#[derive(Clone, Copy)]
enum StoredValue {
    Text(Span),
    Integer(u64),
    Boolean(bool),
}
#[derive(Clone, Copy)]
struct Stored {
    value: StoredValue,
    key_at: Location,
    value_at: Location,
}
#[derive(Clone, Copy, Eq, PartialEq)]
enum Phase {
    Empty,
    Building,
    Finished,
}
pub struct Pending {
    text: [u8; TEXT_BYTES],
    fields: [Option<Stored>; MAX_FIELDS],
    used: usize,
    label: Span,
    section: Option<Section>,
    location: Option<Location>,
    phase: Phase,
    failure: Option<Error>,
}
const _: [(); 1] = [(); (std::mem::size_of::<Pending>() <= WORKSPACE_BYTES) as usize];
const _: [(); 1] = [(); (TEXT_BYTES <= u16::MAX as usize) as usize];
impl Default for Pending {
    fn default() -> Self {
        Self::new()
    }
}
impl fmt::Debug for Pending {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PendingStanza(<redacted>)")
    }
}
impl Pending {
    pub const fn new() -> Self {
        Self {
            text: [0; TEXT_BYTES],
            fields: [None; MAX_FIELDS],
            used: 0,
            label: Span::EMPTY,
            section: None,
            location: None,
            phase: Phase::Empty,
            failure: None,
        }
    }
    fn error(&self, code: Code, field: Option<&'static str>, location: Option<Location>) -> Error {
        Error {
            code,
            section: self.section,
            field,
            location,
            previous: None,
        }
    }
    fn operation<T>(
        &mut self,
        action: impl FnOnce(&mut Self) -> Result<T, Error>,
    ) -> Result<T, Error> {
        if let Some(e) = self.failure {
            return Err(e);
        }
        let result = action(self);
        if let Err(e) = result {
            self.failure = Some(e);
        }
        result
    }
    fn store(
        &mut self,
        input: &str,
        field: Option<&'static str>,
        at: Location,
    ) -> Result<Span, Error> {
        let end = self
            .used
            .checked_add(input.len())
            .ok_or_else(|| self.error(Code::Capacity, field, Some(at)))?;
        let capacity = self.error(Code::Capacity, field, Some(at));
        let target = self.text.get_mut(self.used..end).ok_or(capacity)?;
        let span = Span {
            start: u16::try_from(self.used).map_err(|_| capacity)?,
            len: u16::try_from(input.len()).map_err(|_| capacity)?,
        };
        target.copy_from_slice(input.as_bytes());
        self.used = end;
        Ok(span)
    }
    pub fn begin(
        &mut self,
        section: Section,
        label: Option<&str>,
        at: Location,
    ) -> Result<(), Error> {
        self.operation(|this| {
            if this.phase == Phase::Building {
                return Err(Error {
                    section: Some(section),
                    ..this.error(Code::State, None, Some(at))
                });
            }
            this.section = Some(section);
            this.location = Some(at);
            if matches!(section, Section::Resource(_)) {
                return Err(this.error(Code::ResourceSection, None, Some(at)));
            }
            match (section.label(), label) {
                (Label::None, Some(_)) => {
                    return Err(this.error(Code::ForbiddenLabel, None, Some(at)))
                }
                (Label::None, None) => {}
                (_, None) => return Err(this.error(Code::MissingLabel, None, Some(at))),
                (kind, Some(value)) if value.is_empty() || value.len() > kind.max_bytes() => {
                    return Err(this.error(Code::LabelLength, None, Some(at)))
                }
                _ => {}
            }
            if section.fields().len() > MAX_FIELDS {
                return Err(this.error(Code::Capacity, None, Some(at)));
            }
            this.fields.fill(None);
            this.used = 0;
            this.label = Span::EMPTY;
            if let Some(label) = label {
                this.label = this.store(label, None, at)?;
            }
            this.phase = Phase::Building;
            Ok(())
        })
    }
    pub fn assign(
        &mut self,
        key: &str,
        value: Value<'_>,
        key_at: Location,
        value_at: Location,
    ) -> Result<(), Error> {
        self.operation(|this| {
            if this.phase != Phase::Building {
                return Err(this.error(Code::State, None, Some(key_at)));
            }
            let section = this
                .section
                .ok_or_else(|| this.error(Code::Invariant, None, Some(key_at)))?;
            let (index, field) = section
                .field(key)
                .ok_or_else(|| this.error(Code::UnknownField, None, Some(key_at)))?;
            if let Some(previous) = this
                .fields
                .get(index)
                .ok_or_else(|| this.error(Code::Invariant, Some(field.name), Some(key_at)))?
            {
                return Err(Error {
                    previous: Some(previous.key_at),
                    ..this.error(Code::DuplicateField, Some(field.name), Some(key_at))
                });
            }
            if !field.scalar.accepts(value) {
                return Err(this.error(Code::Type, Some(field.name), Some(value_at)));
            }
            if matches!(value, Value::Text(s) if s.len() > super::syntax::MAX_STRING_BYTES) {
                return Err(this.error(Code::TextLength, Some(field.name), Some(value_at)));
            }
            let value = match value {
                Value::Text(s) => StoredValue::Text(this.store(s, Some(field.name), value_at)?),
                Value::Integer(v) => StoredValue::Integer(v),
                Value::Boolean(v) => StoredValue::Boolean(v),
            };
            let error = this.error(Code::Invariant, Some(field.name), Some(key_at));
            *this.fields.get_mut(index).ok_or(error)? = Some(Stored {
                value,
                key_at,
                value_at,
            });
            Ok(())
        })
    }
    pub fn finish(&mut self) -> Result<Stanza<'_>, Error> {
        self.operation(|this| {
            if this.phase != Phase::Building {
                return Err(this.error(Code::State, None, this.location));
            }
            let section = this
                .section
                .ok_or_else(|| this.error(Code::Invariant, None, this.location))?;
            for (index, field) in section.fields().iter().enumerate() {
                if field.required && this.fields.get(index).is_none_or(Option::is_none) {
                    return Err(this.error(Code::MissingField, Some(field.name), this.location));
                }
            }
            this.phase = Phase::Finished;
            Ok(())
        })?;
        Ok(Stanza { pending: self })
    }
}
pub struct Entry<'a> {
    pub value: Value<'a>,
    pub key_location: Location,
    pub value_location: Location,
}
impl fmt::Debug for Entry<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("StanzaEntry(<redacted>)")
    }
}
pub struct Stanza<'a> {
    pending: &'a Pending,
}
impl fmt::Debug for Stanza<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Stanza(<redacted>)")
    }
}
impl<'a> Stanza<'a> {
    fn invariant(&self) -> Error {
        self.pending
            .error(Code::Invariant, None, self.pending.location)
    }
    fn read(&self, span: Span) -> Result<&'a str, Error> {
        let start = usize::from(span.start);
        let end = start
            .checked_add(usize::from(span.len))
            .ok_or_else(|| self.invariant())?;
        let prefix = self
            .pending
            .text
            .get(..self.pending.used)
            .ok_or_else(|| self.invariant())?;
        std::str::from_utf8(prefix.get(start..end).ok_or_else(|| self.invariant())?)
            .map_err(|_| self.invariant())
    }
    pub fn section(&self) -> Result<Section, Error> {
        self.pending.section.ok_or_else(|| self.invariant())
    }
    pub fn location(&self) -> Result<Location, Error> {
        self.pending.location.ok_or_else(|| self.invariant())
    }
    pub fn label(&self) -> Result<Option<&'a str>, Error> {
        if self.section()?.label() == Label::None {
            Ok(None)
        } else {
            Ok(Some(self.read(self.pending.label)?))
        }
    }
    pub fn entry(&self, key: &str) -> Result<Option<Entry<'a>>, Error> {
        let (index, _) = self.section()?.field(key).ok_or_else(|| self.invariant())?;
        let Some(stored) = self
            .pending
            .fields
            .get(index)
            .ok_or_else(|| self.invariant())?
        else {
            return Ok(None);
        };
        let value = match stored.value {
            StoredValue::Text(span) => Value::Text(self.read(span)?),
            StoredValue::Integer(v) => Value::Integer(v),
            StoredValue::Boolean(v) => Value::Boolean(v),
        };
        Ok(Some(Entry {
            value,
            key_location: stored.key_at,
            value_location: stored.value_at,
        }))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use std::num::NonZeroU32;
    fn at(line: u32, column: u16) -> Location {
        Location {
            line: NonZeroU32::new(line).unwrap(),
            column,
        }
    }
    fn assign(p: &mut Pending, key: &str, value: Value<'_>) {
        p.assign(key, value, at(2, 1), at(2, 12)).unwrap();
    }
    #[test]
    fn copied_source_survives_decode_reuse_and_preserves_typed_presence() {
        let mut p = Pending::new();
        let mut label = String::from("0123456789abcdef0123456789abcdef");
        p.begin(Section::Identity, Some(&label), at(1, 1)).unwrap();
        label.clear();
        let mut decoded = String::from("fedcba9876543210fedcba9876543210");
        assign(&mut p, "account", Value::Text(&decoded));
        decoded.clear();
        decoded.push_str("Sender@example.test");
        assign(&mut p, "email", Value::Text(&decoded));
        decoded.clear();
        assign(&mut p, "name", Value::Text(""));
        assign(&mut p, "reply_to", Value::Boolean(false));
        let s = p.finish().unwrap();
        assert_eq!(s.section().unwrap(), Section::Identity);
        assert_eq!(s.label().unwrap(), Some("0123456789abcdef0123456789abcdef"));
        assert_eq!(
            s.entry("email").unwrap().unwrap().value,
            Value::Text("Sender@example.test")
        );
        assert_eq!(s.entry("name").unwrap().unwrap().value, Value::Text(""));
        assert_eq!(
            s.entry("reply_to").unwrap().unwrap().value,
            Value::Boolean(false)
        );
        assert!(s.entry("bcc").unwrap().is_none());
        assert_eq!(s.entry("unknown").unwrap_err().code, Code::Invariant);
        let entry = s.entry("account").unwrap().unwrap();
        assert_eq!(entry.key_location, at(2, 1));
        assert_eq!(entry.value_location, at(2, 12));
        assert_eq!(s.location().unwrap(), at(1, 1));
        p.begin(Section::Domain, Some("example.test"), at(3, 1))
            .unwrap();
        assign(&mut p, "mx_preference", Value::Integer(u64::MAX));
        let s = p.finish().unwrap();
        assert_eq!(
            s.entry("mx_preference").unwrap().unwrap().value,
            Value::Integer(u64::MAX)
        );
        assert!(s.entry("mx_host").unwrap().is_none());
        p.begin(Section::Paths, None, at(4, 1)).unwrap();
        let s = p.finish().unwrap();
        assert_eq!(s.label().unwrap(), None);
        assert!(s.entry("data").unwrap().is_none());
    }
    #[test]
    fn required_fields_type_errors_unknown_fields_and_duplicates_are_distinct() {
        let mut p = Pending::new();
        p.begin(Section::Server, None, at(1, 1)).unwrap();
        let e = p.finish().unwrap_err();
        assert_eq!(e.code, Code::MissingField);
        assert_eq!(e.field, Some("hostname"));
        assert_eq!(p.begin(Section::Paths, None, at(3, 1)).unwrap_err(), e);
        let mut p = Pending::new();
        p.begin(Section::Server, None, at(1, 1)).unwrap();
        let e = p
            .assign(
                "online_background",
                Value::Text("true"),
                at(2, 1),
                at(2, 21),
            )
            .unwrap_err();
        assert_eq!(e.code, Code::Type);
        assert_eq!(e.location, Some(at(2, 21)));
        assert_eq!(e.field, Some("online_background"));
        let mut p = Pending::new();
        p.begin(Section::Relay, None, at(1, 1)).unwrap();
        assign(&mut p, "port", Value::Integer(465));
        let e = p
            .assign("port", Value::Boolean(true), at(3, 1), at(3, 8))
            .unwrap_err();
        assert_eq!(e.code, Code::DuplicateField);
        assert_eq!(e.previous, Some(at(2, 1)));
        assert_eq!(e.location, Some(at(3, 1)));
        assert_eq!(p.finish().unwrap_err(), e);
        let mut p = Pending::new();
        p.begin(Section::Logging, None, at(1, 1)).unwrap();
        let e = p
            .assign(
                "private_secret_field",
                Value::Text("secret"),
                at(2, 1),
                at(2, 24),
            )
            .unwrap_err();
        assert_eq!(e.code, Code::UnknownField);
        assert_eq!(e.field, None);
        assert!(!format!("{e:?} {e}").contains("secret"));
        assert!(std::error::Error::source(&e).is_none());
        assert_eq!(p.finish().unwrap_err(), e);
    }
    #[test]
    fn label_shapes_and_resource_handoff_are_explicit() {
        for (section, label, code) in [
            (Section::Server, Some("unexpected"), Code::ForbiddenLabel),
            (Section::Identity, None, Code::MissingLabel),
            (Section::Resolver, Some(""), Code::LabelLength),
            (
                Section::Account,
                Some("123456789012345678901234567890123"),
                Code::LabelLength,
            ),
            (
                Section::Resource(resources::Section::Limits),
                None,
                Code::ResourceSection,
            ),
        ] {
            let mut p = Pending::new();
            let e = p.begin(section, label, at(1, 1)).unwrap_err();
            assert_eq!(e.code, code);
            assert_eq!(p.finish().unwrap_err(), e);
        }
        for name in ["limits", "disk", "work", "network"] {
            assert!(matches!(Section::parse(name), Some(Section::Resource(_))));
        }
        for name in ["Server", "server ", "version", "smtp", "devices"] {
            assert!(Section::parse(name).is_none());
        }
        let mut p = Pending::new();
        p.begin(Section::Alias, Some(&"x".repeat(254)), at(1, 1))
            .unwrap();
        assign(&mut p, "account", Value::Text("invalid-id"));
        assert_eq!(p.finish().unwrap().label().unwrap().unwrap().len(), 254);
        // Length staging alone does not claim mailbox, ID or profile validity.
    }
    #[test]
    fn all_declared_stanzas_accept_their_literal_source_types() {
        let fixture = r#"
[server]
hostname = "mail.example.test"
jmap_origin = "https://jmap.example.test"
online_background = true
public_ipv4 = "192.0.2.1"
public_ipv6 = "2001:db8::1"
[paths]
data = "/data"
runtime = "/run"
logs = "/logs"
[resolver "primary"]
address = "192.0.2.53:53"
[logging]
minimum_severity = "info"
[account "0123456789abcdef0123456789abcdef"]
username = "operator"
name = ""
[domain "example.test"]
mx_host = "mail.example.test"
mx_preference = 10
mta_sts = "testing"
mta_sts_max_age_seconds = 86400
mta_sts_certificate = "public"
[alias "operator@example.test"]
account = "0123456789abcdef0123456789abcdef"
[identity "fedcba9876543210fedcba9876543210"]
account = "0123456789abcdef0123456789abcdef"
name = "Operator"
email = "operator@example.test"
reply_to = true
bcc = false
text_signature_file = "/text"
html_signature_file = "/html"
[identity_address "fedcba9876543210fedcba9876543210"]
kind = "reply_to"
name = ""
email = "reply@example.test"
[relay]
host = "relay.example.test"
port = 465
transport = "implicit_tls"
username = "operator"
password_file = "/password"
ca_file = "/ca"
[certificate "public"]
mode = "files"
chain_file = "/chain"
key_file = "/key"
[acme]
directory = "https://ca.example.test/directory"
contact = "operator@example.test"
terms_accepted = true
ca_file = "/ca"
[gateway "upstream"]
ca_file = "/gateway-ca"
client_cert_sha256 = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
next_client_cert_sha256 = "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210"
[gateway_peer "upstream"]
network = "192.0.2.0/24"
[listener "incoming"]
kind = "gateway_smtp"
bind = "192.0.2.1:25"
server_name = "mail.example.test"
certificate = "public"
gateway = "upstream"
session_limit = 2
per_peer_limit = 1
"#;
        let mut p = Pending::new();
        let mut decoded = [0; super::super::syntax::MAX_STRING_BYTES];
        let mut count = 0;
        let mut assigned = 0;
        let mut total_assigned = 0;
        for (line, source) in fixture.lines().enumerate() {
            match super::super::syntax::parse_line(
                NonZeroU32::new(line as u32 + 1).unwrap(),
                source.as_bytes(),
                &mut decoded,
            )
            .unwrap()
            {
                super::super::syntax::Statement::Empty => {}
                super::super::syntax::Statement::Section {
                    name,
                    label,
                    location,
                } => {
                    if count > 0 {
                        let stanza = p.finish().unwrap();
                        assert_eq!(assigned, stanza.section().unwrap().fields().len());
                        assigned = 0;
                    }
                    let section = Section::parse(name).unwrap();
                    assert_eq!(section.name(), name);
                    p.begin(section, label, location).unwrap();
                    count += 1;
                }
                super::super::syntax::Statement::Assignment {
                    key,
                    value,
                    location,
                    value_location,
                } => {
                    p.assign(key, value, location, value_location).unwrap();
                    assigned += 1;
                    total_assigned += 1;
                }
            }
        }
        let stanza = p.finish().unwrap();
        assert_eq!(assigned, stanza.section().unwrap().fields().len());
        assert_eq!(total_assigned, 52);
        assert_eq!(count, 15);
    }
    #[test]
    fn largest_identity_and_root_stanzas_fit_the_shared_workspace() {
        let mut p = Pending::new();
        let id = "0123456789abcdef0123456789abcdef";
        p.begin(Section::Identity, Some(id), at(1, 1)).unwrap();
        assign(&mut p, "account", Value::Text(id));
        assign(&mut p, "name", Value::Text(&"x".repeat(4096)));
        assign(&mut p, "email", Value::Text(&"x".repeat(254)));
        for key in ["text_signature_file", "html_signature_file"] {
            assign(&mut p, key, Value::Text(&format!("/{}", "x".repeat(4094))));
        }
        assign(&mut p, "reply_to", Value::Boolean(true));
        assign(&mut p, "bcc", Value::Boolean(false));
        let s = p.finish().unwrap();
        assert_eq!(
            s.entry("name").unwrap().unwrap().value,
            Value::Text(&"x".repeat(4096))
        );
        assert_eq!(p.used, 12604);
        assert!(std::mem::size_of::<Pending>() <= 13 * 1024);
        p.begin(Section::Paths, None, at(3, 1)).unwrap();
        for key in ["data", "runtime", "logs"] {
            assign(&mut p, key, Value::Text(&format!("/{}", "x".repeat(4094))));
        }
        p.finish().unwrap();
        assert_eq!(p.used, 12285);
    }
    #[test]
    fn text_and_state_capacity_failures_poison_pending_stanzas() {
        let mut p = Pending::new();
        let e = p.finish().unwrap_err();
        assert_eq!(e.code, Code::State);
        assert_eq!(p.begin(Section::Paths, None, at(1, 1)).unwrap_err(), e);
        let mut p = Pending::new();
        p.begin(Section::Paths, None, at(1, 1)).unwrap();
        let e = p.begin(Section::Logging, None, at(2, 1)).unwrap_err();
        assert_eq!(e.code, Code::State);
        assert_eq!(e.section, Some(Section::Logging));
        assert_eq!(e.location, Some(at(2, 1)));
        let mut p = Pending::new();
        p.begin(Section::Paths, None, at(1, 1)).unwrap();
        p.finish().unwrap();
        assert_eq!(
            p.assign("data", Value::Text("/a"), at(2, 1), at(2, 8))
                .unwrap_err()
                .code,
            Code::State
        );
        let mut p = Pending::new();
        p.begin(Section::Paths, None, at(1, 1)).unwrap();
        let e = p
            .assign("data", Value::Text(&"x".repeat(4097)), at(2, 1), at(2, 8))
            .unwrap_err();
        assert_eq!(e.code, Code::TextLength);
        assert_eq!(p.used, 0);
        assert_eq!(p.finish().unwrap_err(), e);
        let mut p = Pending::new();
        p.begin(Section::Relay, None, at(1, 1)).unwrap();
        for key in ["host", "username", "password_file"] {
            assign(&mut p, key, Value::Text(&"x".repeat(4096)));
        }
        let before = p.used;
        let e = p
            .assign(
                "ca_file",
                Value::Text(&"x".repeat(4096)),
                at(3, 1),
                at(3, 11),
            )
            .unwrap_err();
        assert_eq!(e.code, Code::Capacity);
        assert_eq!(p.used, before);
        assert_eq!(p.finish().unwrap_err(), e);
    }
    #[test]
    fn formatting_never_exposes_pending_values() {
        let mut p = Pending::new();
        p.begin(Section::Paths, None, at(1, 1)).unwrap();
        assign(&mut p, "data", Value::Text("/secret-root"));
        assert_eq!(format!("{p:?}"), "PendingStanza(<redacted>)");
        let s = p.finish().unwrap();
        assert_eq!(format!("{s:?}"), "Stanza(<redacted>)");
        assert_eq!(
            format!("{:?}", s.entry("data").unwrap().unwrap()),
            "StanzaEntry(<redacted>)"
        );
        p.begin(Section::Relay, None, at(3, 1)).unwrap();
        assign(&mut p, "port", Value::Integer(25));
        let e = p
            .assign("port", Value::Text("secret"), at(4, 2), at(4, 10))
            .unwrap_err();
        assert_eq!(e.to_string(),"config_stanza_duplicate_field in relay for port at line 4, byte column 2 (previously assigned at line 2, byte column 1)");
    }
    #[test]
    fn every_unconditional_required_field_is_enforced() {
        let cases: &[(Section, &[(&str, Value<'_>)])] = &[
            (
                Section::Server,
                &[
                    ("hostname", Value::Text("mail.test")),
                    ("jmap_origin", Value::Text("https://jmap.test")),
                ],
            ),
            (
                Section::Resolver,
                &[("address", Value::Text("192.0.2.1:53"))],
            ),
            (Section::Account, &[("username", Value::Text("user"))]),
            (
                Section::Alias,
                &[("account", Value::Text("0123456789abcdef0123456789abcdef"))],
            ),
            (
                Section::Identity,
                &[
                    ("account", Value::Text("0123456789abcdef0123456789abcdef")),
                    ("email", Value::Text("user@example.test")),
                ],
            ),
            (
                Section::IdentityAddress,
                &[
                    ("kind", Value::Text("bcc")),
                    ("email", Value::Text("user@example.test")),
                ],
            ),
            (
                Section::Relay,
                &[
                    ("host", Value::Text("relay.test")),
                    ("port", Value::Integer(465)),
                    ("username", Value::Text("user")),
                    ("password_file", Value::Text("/password")),
                ],
            ),
            (Section::Certificate, &[("mode", Value::Text("acme"))]),
            (
                Section::Acme,
                &[
                    ("directory", Value::Text("https://ca.test")),
                    ("contact", Value::Text("user@example.test")),
                ],
            ),
            (
                Section::Gateway,
                &[
                    ("ca_file", Value::Text("/ca")),
                    (
                        "client_cert_sha256",
                        Value::Text(
                            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
                        ),
                    ),
                ],
            ),
            (
                Section::GatewayPeer,
                &[("network", Value::Text("192.0.2.0/24"))],
            ),
            (
                Section::Listener,
                &[
                    ("kind", Value::Text("https")),
                    ("bind", Value::Text("192.0.2.1:443")),
                ],
            ),
        ];
        for &(section, values) in cases {
            let label = match section.label() {
                Label::None => None,
                Label::AccountId | Label::IdentityId => Some("0123456789abcdef0123456789abcdef"),
                Label::Domain => Some("example.test"),
                Label::Mailbox => Some("user@example.test"),
                Label::Profile => Some("profile"),
            };
            for omitted in 0..=values.len() {
                let mut p = Pending::new();
                p.begin(section, label, at(1, 1)).unwrap();
                for (index, &(key, value)) in values.iter().enumerate() {
                    if index != omitted {
                        assign(&mut p, key, value);
                    }
                }
                if let Some(&(missing, _)) = values.get(omitted) {
                    let e = p.finish().unwrap_err();
                    assert_eq!(e.code, Code::MissingField);
                    assert_eq!(e.field, Some(missing));
                } else {
                    p.finish().unwrap();
                }
            }
        }
        for (section, label) in [
            (Section::Paths, None),
            (Section::Logging, None),
            (Section::Domain, Some("example.test")),
        ] {
            let mut p = Pending::new();
            p.begin(section, label, at(1, 1)).unwrap();
            p.finish().unwrap();
        }
    }
    #[test]
    fn scalar_classes_never_coerce_each_other() {
        for (section, key, wrong) in [
            (Section::Paths, "data", Value::Integer(1)),
            (Section::Paths, "data", Value::Boolean(true)),
            (Section::Relay, "port", Value::Text("465")),
            (Section::Relay, "port", Value::Boolean(false)),
            (Section::Server, "online_background", Value::Integer(1)),
            (Section::Server, "online_background", Value::Text("false")),
        ] {
            let mut p = Pending::new();
            p.begin(section, None, at(1, 1)).unwrap();
            let e = p.assign(key, wrong, at(2, 1), at(2, 20)).unwrap_err();
            assert_eq!(e.code, Code::Type);
            assert_eq!(e.field, Some(key));
            assert_eq!(e.location, Some(at(2, 20)));
            assert_eq!(p.finish().unwrap_err(), e);
        }
    }
    #[test]
    fn every_section_label_class_has_exact_byte_boundaries() {
        let mappings = [
            (Section::Server, Label::None),
            (Section::Paths, Label::None),
            (Section::Resolver, Label::Profile),
            (Section::Logging, Label::None),
            (Section::Account, Label::AccountId),
            (Section::Domain, Label::Domain),
            (Section::Alias, Label::Mailbox),
            (Section::Identity, Label::IdentityId),
            (Section::IdentityAddress, Label::IdentityId),
            (Section::Relay, Label::None),
            (Section::Certificate, Label::Profile),
            (Section::Acme, Label::None),
            (Section::Gateway, Label::Profile),
            (Section::GatewayPeer, Label::Profile),
            (Section::Listener, Label::Profile),
            (Section::Resource(resources::Section::Limits), Label::None),
            (Section::Resource(resources::Section::Disk), Label::None),
            (Section::Resource(resources::Section::Work), Label::None),
            (Section::Resource(resources::Section::Network), Label::None),
        ];
        let domain = format!(
            "{}.{}.{}.{}",
            "d".repeat(63),
            "e".repeat(63),
            "f".repeat(63),
            "g".repeat(51)
        );
        for (section, kind) in mappings {
            assert_eq!(section.label(), kind);
            if matches!(section, Section::Resource(_)) {
                continue;
            }
            if kind == Label::None {
                let mut p = Pending::new();
                p.begin(section, None, at(1, 1)).unwrap();
                let mut p = Pending::new();
                assert_eq!(
                    p.begin(section, Some(""), at(1, 1)).unwrap_err().code,
                    Code::ForbiddenLabel
                );
                continue;
            }
            let (label, limit) = match kind {
                Label::Profile => ("p".repeat(64), 64),
                Label::AccountId | Label::IdentityId => ("a".repeat(32), 32),
                Label::Domain => (domain.clone(), 243),
                Label::Mailbox => (format!("{}@{domain}", "m".repeat(10)), 254),
                Label::None => continue,
            };
            assert_eq!(label.len(), limit);
            let mut p = Pending::new();
            p.begin(section, Some(&label), at(1, 1)).unwrap();
            let mut p = Pending::new();
            let e = p
                .begin(section, Some(&format!("{label}x")), at(1, 1))
                .unwrap_err();
            assert_eq!(e.code, Code::LabelLength);
            assert_eq!(e.section, Some(section));
            assert_eq!(p.finish().unwrap_err(), e);
        }
    }
}

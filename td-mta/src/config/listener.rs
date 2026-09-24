//! Structural listener records; no socket or TLS authority.
use super::{endpoint, syntax::Location, text, values};
use crate::limits::ResourcePlan;
use std::{fmt, net::SocketAddr, num::NonZeroU64};
pub const MAX_LISTENERS: usize = 16;
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Kind {
    DirectSmtp,
    GatewaySmtp,
    Https,
    Http01,
    LoopbackSmtpFixture,
}
impl Kind {
    pub const fn name(self) -> &'static str {
        match self {
            Self::DirectSmtp => "direct_smtp",
            Self::GatewaySmtp => "gateway_smtp",
            Self::Https => "https",
            Self::Http01 => "http01",
            Self::LoopbackSmtpFixture => "loopback_smtp_fixture",
        }
    }
    pub const fn is_smtp(self) -> bool {
        matches!(
            self,
            Self::DirectSmtp | Self::GatewaySmtp | Self::LoopbackSmtpFixture
        )
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Field {
    Name,
    Bind,
    ServerName,
    Certificate,
    Gateway,
    SessionLimit,
    PerPeerLimit,
}
impl Field {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Name => "name",
            Self::Bind => "bind",
            Self::ServerName => "server_name",
            Self::Certificate => "certificate",
            Self::Gateway => "gateway",
            Self::SessionLimit => "session_limit",
            Self::PerPeerLimit => "per_peer_limit",
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Code {
    Capacity,
    ForeignArena,
    Required,
    Forbidden,
    Profile,
    Hostname,
    Endpoint,
    Range,
    Loopback,
    Http01Port,
    Duplicate,
    BindConflict,
    MissingSmtp,
    MissingHttps,
    SessionBudget,
    Text,
    Invariant,
}
impl Code {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Capacity => "config_listener_capacity",
            Self::ForeignArena => "config_listener_foreign_arena",
            Self::Required => "config_listener_required",
            Self::Forbidden => "config_listener_forbidden",
            Self::Profile => "config_listener_profile",
            Self::Hostname => "config_listener_hostname",
            Self::Endpoint => "config_listener_endpoint",
            Self::Range => "config_listener_range",
            Self::Loopback => "config_listener_loopback",
            Self::Http01Port => "config_listener_http01_port",
            Self::Duplicate => "config_listener_duplicate",
            Self::BindConflict => "config_listener_bind_conflict",
            Self::MissingSmtp => "config_listener_missing_smtp",
            Self::MissingHttps => "config_listener_missing_https",
            Self::SessionBudget => "config_listener_session_budget",
            Self::Text => "config_listener_text",
            Self::Invariant => "config_listener_invariant",
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Error {
    pub code: Code,
    pub field: Option<Field>,
    pub location: Option<Location>,
    pub previous: Option<Location>,
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.code.name())?;
        if let Some(field) = self.field {
            write!(f, " for {}", field.name())?;
        }
        if let Some(at) = self.location {
            write!(f, " at line {}, byte column {}", at.line, at.column)?;
        }
        if let Some(at) = self.previous {
            write!(
                f,
                " (previously configured at line {}, byte column {})",
                at.line, at.column
            )?;
        }
        Ok(())
    }
}
impl std::error::Error for Error {}
fn err(code: Code, field: Option<Field>, at: Option<Location>) -> Error {
    Error {
        code,
        field,
        location: at,
        previous: None,
    }
}
fn invariant() -> Error {
    err(Code::Invariant, None, None)
}
fn conflict(code: Code, field: Field, at: Location, previous: Location) -> Error {
    Error {
        code,
        field: Some(field),
        location: Some(at),
        previous: Some(previous),
    }
}
#[derive(Clone, Copy)]
pub struct Input<'a> {
    pub kind: Kind,
    pub bind: &'a str,
    pub server_name: Option<&'a str>,
    pub certificate: Option<&'a str>,
    pub gateway: Option<&'a str>,
    pub session_limit: Option<u64>,
    pub per_peer_limit: Option<u64>,
}
impl fmt::Debug for Input<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ListenerInput(<redacted>)")
    }
}
#[derive(Clone, Copy)]
pub struct Slot {
    name: text::Span,
    server: text::Span,
    certificate: text::Span,
    gateway: text::Span,
    bind: Option<SocketAddr>,
    kind: Kind,
    sessions: usize,
    per_peer: usize,
    location: Option<Location>,
}
impl Slot {
    pub const EMPTY: Self = Self {
        name: text::Span::EMPTY,
        server: text::Span::EMPTY,
        certificate: text::Span::EMPTY,
        gateway: text::Span::EMPTY,
        bind: None,
        kind: Kind::Http01,
        sessions: 0,
        per_peer: 0,
        location: None,
    };
}
const _: [(); 1] = [(); (std::mem::size_of::<Slot>() <= 128) as usize];
pub struct Builder<'a> {
    slots: &'a mut [Slot],
    count: usize,
    owner: NonZeroU64,
    failure: Option<Error>,
}
const _: [(); 1] = [(); (std::mem::size_of::<Builder<'_>>() <= 128) as usize];
impl fmt::Debug for Builder<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ListenerBuilder(<redacted>)")
    }
}
impl<'a> Builder<'a> {
    pub fn new(arena: &text::Builder<'_>, slots: &'a mut [Slot]) -> Result<Self, Error> {
        if slots.is_empty() || slots.len() > MAX_LISTENERS {
            return Err(err(Code::Capacity, None, None));
        }
        Ok(Self {
            slots,
            count: 0,
            owner: arena.owner(),
            failure: None,
        })
    }
    pub fn listener(
        &mut self,
        arena: &mut text::Builder<'_>,
        name: &str,
        input: Input<'_>,
        at: Location,
    ) -> Result<(), Error> {
        if let Some(e) = self.failure {
            return Err(e);
        }
        let result = if self.owner != arena.owner() {
            Err(err(Code::ForeignArena, None, Some(at)))
        } else {
            self.insert(arena, name, input, at)
        };
        if let Err(e) = result {
            self.failure = Some(e);
        }
        result
    }
    fn insert(
        &mut self,
        arena: &mut text::Builder<'_>,
        name: &str,
        input: Input<'_>,
        at: Location,
    ) -> Result<(), Error> {
        values::profile_name(name).map_err(|_| err(Code::Profile, Some(Field::Name), Some(at)))?;
        let view = arena.borrowed_view().map_err(|_| invariant())?;
        let used = self.slots.get(..self.count).ok_or_else(invariant)?;
        for old in used {
            if read(view, self.owner, old.name)? == name {
                return Err(conflict(
                    Code::Duplicate,
                    Field::Name,
                    at,
                    old.location.ok_or_else(invariant)?,
                ));
            }
        }
        if self.count >= self.slots.len() {
            return Err(err(Code::Capacity, None, Some(at)));
        }
        let bind = endpoint::numeric_endpoint(input.bind)
            .map_err(|_| err(Code::Endpoint, Some(Field::Bind), Some(at)))?;
        let named_smtp = matches!(input.kind, Kind::DirectSmtp | Kind::GatewaySmtp);
        let has_certificate = named_smtp || input.kind == Kind::Https;
        required(
            input.server_name.is_some(),
            named_smtp,
            Field::ServerName,
            at,
        )?;
        required(
            input.certificate.is_some(),
            has_certificate,
            Field::Certificate,
            at,
        )?;
        required(
            input.gateway.is_some(),
            input.kind == Kind::GatewaySmtp,
            Field::Gateway,
            at,
        )?;
        required(
            input.session_limit.is_some(),
            input.kind.is_smtp(),
            Field::SessionLimit,
            at,
        )?;
        required(
            input.per_peer_limit.is_some(),
            input.kind.is_smtp(),
            Field::PerPeerLimit,
            at,
        )?;
        if let Some(host) = input.server_name {
            values::dns_name(host)
                .map_err(|_| err(Code::Hostname, Some(Field::ServerName), Some(at)))?;
        }
        for (field, value) in [
            (Field::Certificate, input.certificate),
            (Field::Gateway, input.gateway),
        ] {
            if let Some(value) = value {
                values::profile_name(value)
                    .map_err(|_| err(Code::Profile, Some(field), Some(at)))?;
            }
        }
        let sessions = count(input.session_limit, Field::SessionLimit, at)?;
        let per_peer = count(input.per_peer_limit, Field::PerPeerLimit, at)?;
        if per_peer > sessions {
            return Err(err(Code::Range, Some(Field::PerPeerLimit), Some(at)));
        }
        if input.kind == Kind::LoopbackSmtpFixture && !bind.ip().is_loopback() {
            return Err(err(Code::Loopback, Some(Field::Bind), Some(at)));
        }
        if input.kind == Kind::Http01 && bind.port() != 80 {
            return Err(err(Code::Http01Port, Some(Field::Bind), Some(at)));
        }
        for old in used {
            let old_bind = old.bind.ok_or_else(invariant)?;
            if binds_overlap(old_bind, bind) {
                return Err(conflict(
                    Code::BindConflict,
                    Field::Bind,
                    at,
                    old.location.ok_or_else(invariant)?,
                ));
            }
        }
        let name = store(arena, name, Field::Name, at)?;
        let server = match input.server_name {
            Some(host) => {
                let h = arena
                    .append_dns(host)
                    .map_err(|_| err(Code::Text, Some(Field::ServerName), Some(at)))?;
                arena.compact(h).map_err(|_| invariant())?
            }
            None => text::Span::EMPTY,
        };
        let certificate = optional(arena, input.certificate, Field::Certificate, at)?;
        let gateway = optional(arena, input.gateway, Field::Gateway, at)?;
        *self.slots.get_mut(self.count).ok_or_else(invariant)? = Slot {
            name,
            server,
            certificate,
            gateway,
            bind: Some(bind),
            kind: input.kind,
            sessions,
            per_peer,
            location: Some(at),
        };
        self.count = self.count.checked_add(1).ok_or_else(invariant)?;
        Ok(())
    }
    pub fn finish(self, plan: &ResourcePlan) -> Result<Records<'a>, Error> {
        if let Some(e) = self.failure {
            return Err(e);
        }
        let slots = self.slots.get(..self.count).ok_or_else(invariant)?;
        let mut total = 0usize;
        let mut has_smtp = false;
        let mut has_https = false;
        for slot in slots {
            has_https |= slot.kind == Kind::Https;
            if slot.kind.is_smtp() {
                has_smtp = true;
                if slot.sessions > plan.limits().smtp_sessions {
                    return Err(err(Code::Range, Some(Field::SessionLimit), slot.location));
                }
                if slot.per_peer > plan.limits().smtp_per_peer {
                    return Err(err(Code::Range, Some(Field::PerPeerLimit), slot.location));
                }
                total = total.checked_add(slot.sessions).ok_or_else(|| {
                    err(
                        Code::SessionBudget,
                        Some(Field::SessionLimit),
                        slot.location,
                    )
                })?;
                if total > plan.limits().smtp_sessions {
                    return Err(err(
                        Code::SessionBudget,
                        Some(Field::SessionLimit),
                        slot.location,
                    ));
                }
            }
        }
        if !has_smtp {
            return Err(err(Code::MissingSmtp, None, None));
        }
        if !has_https {
            return Err(err(Code::MissingHttps, None, None));
        }
        Ok(Records {
            slots,
            owner: self.owner,
        })
    }
}
fn required(present: bool, wanted: bool, field: Field, at: Location) -> Result<(), Error> {
    match (present, wanted) {
        (false, true) => Err(err(Code::Required, Some(field), Some(at))),
        (true, false) => Err(err(Code::Forbidden, Some(field), Some(at))),
        _ => Ok(()),
    }
}
fn count(value: Option<u64>, field: Field, at: Location) -> Result<usize, Error> {
    match value {
        None => Ok(0),
        Some(value) => usize::try_from(value)
            .ok()
            .filter(|n| *n != 0)
            .ok_or_else(|| err(Code::Range, Some(field), Some(at))),
    }
}
fn binds_overlap(a: SocketAddr, b: SocketAddr) -> bool {
    a.is_ipv4() == b.is_ipv4()
        && a.port() == b.port()
        && (a.ip() == b.ip() || a.ip().is_unspecified() || b.ip().is_unspecified())
}
fn optional(
    arena: &mut text::Builder<'_>,
    value: Option<&str>,
    field: Field,
    at: Location,
) -> Result<text::Span, Error> {
    match value {
        Some(value) => store(arena, value, field, at),
        None => Ok(text::Span::EMPTY),
    }
}
fn store(
    arena: &mut text::Builder<'_>,
    value: &str,
    field: Field,
    at: Location,
) -> Result<text::Span, Error> {
    let h = arena
        .append(value.as_bytes())
        .map_err(|_| err(Code::Text, Some(field), Some(at)))?;
    arena.compact(h).map_err(|_| invariant())
}
fn read(text: text::View<'_>, owner: NonZeroU64, span: text::Span) -> Result<&str, Error> {
    std::str::from_utf8(text.read_span(owner, span).map_err(|_| invariant())?)
        .map_err(|_| invariant())
}
pub struct Records<'a> {
    slots: &'a [Slot],
    owner: NonZeroU64,
}
const _: [(); 1] = [(); (std::mem::size_of::<Records<'_>>() <= 128) as usize];
impl fmt::Debug for Records<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ListenerRecords(<redacted>)")
    }
}
impl Records<'_> {
    pub fn view_live<'s, 't>(&'s self, text: &'t text::Builder<'_>) -> Result<View<'s, 't>, Error> {
        self.view(text.borrowed_view().map_err(|_| invariant())?)
    }
    pub fn view<'s, 't>(&'s self, text: text::View<'t>) -> Result<View<'s, 't>, Error> {
        if self.owner != text.owner() {
            return Err(err(Code::ForeignArena, None, None));
        }
        Ok(View {
            slots: self.slots,
            owner: self.owner,
            text,
        })
    }
}
#[derive(Clone, Copy)]
pub struct View<'s, 't> {
    slots: &'s [Slot],
    owner: NonZeroU64,
    text: text::View<'t>,
}
impl fmt::Debug for View<'_, '_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ListenerView(<redacted>)")
    }
}
#[derive(Clone, Copy)]
pub struct Listener<'t> {
    pub name: &'t str,
    pub kind: Kind,
    pub bind: SocketAddr,
    pub server_name: Option<&'t str>,
    pub certificate: Option<&'t str>,
    pub gateway: Option<&'t str>,
    pub session_limit: Option<usize>,
    pub per_peer_limit: Option<usize>,
    pub location: Location,
}
impl fmt::Debug for Listener<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Listener(<redacted>)")
    }
}
impl<'t> View<'_, 't> {
    pub fn len(self) -> usize {
        self.slots.len()
    }
    pub fn is_empty(self) -> bool {
        self.slots.is_empty()
    }
    pub fn listener(self, index: usize) -> Result<Option<Listener<'t>>, Error> {
        let Some(s) = self.slots.get(index) else {
            return Ok(None);
        };
        let named_smtp = matches!(s.kind, Kind::DirectSmtp | Kind::GatewaySmtp);
        Ok(Some(Listener {
            name: read(self.text, self.owner, s.name)?,
            kind: s.kind,
            bind: s.bind.ok_or_else(invariant)?,
            server_name: if named_smtp {
                Some(read(self.text, self.owner, s.server)?)
            } else {
                None
            },
            certificate: if named_smtp || s.kind == Kind::Https {
                Some(read(self.text, self.owner, s.certificate)?)
            } else {
                None
            },
            gateway: if s.kind == Kind::GatewaySmtp {
                Some(read(self.text, self.owner, s.gateway)?)
            } else {
                None
            },
            session_limit: s.kind.is_smtp().then_some(s.sessions),
            per_peer_limit: s.kind.is_smtp().then_some(s.per_peer),
            location: s.location.ok_or_else(invariant)?,
        }))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::limits::Limits;
    use std::num::NonZeroU32;
    fn lock() -> std::sync::MutexGuard<'static, ()> {
        text::TEST_CONSTRUCTION_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
    fn at(line: u32) -> Location {
        Location {
            line: NonZeroU32::new(line).unwrap(),
            column: 3,
        }
    }
    fn input(kind: Kind, bind: &str) -> Input<'_> {
        let named = matches!(kind, Kind::DirectSmtp | Kind::GatewaySmtp);
        Input {
            kind,
            bind,
            server_name: named.then_some("Mail.Example.test"),
            certificate: (named || kind == Kind::Https).then_some("cert"),
            gateway: (kind == Kind::GatewaySmtp).then_some("upstream"),
            session_limit: kind.is_smtp().then_some(1),
            per_peer_limit: kind.is_smtp().then_some(1),
        }
    }
    struct Storage {
        bytes: Vec<u8>,
        slots: Vec<Slot>,
    }
    impl Storage {
        fn new() -> Self {
            Self {
                bytes: vec![0; text::MAX_BYTES],
                slots: vec![Slot::EMPTY; MAX_LISTENERS],
            }
        }
    }
    #[test]
    fn all_roles_retain_literal_settings_in_live_and_frozen_views() {
        let _lock = lock();
        let mut s = Storage::new();
        let mut arena = text::Builder::new(&mut s.bytes).unwrap();
        let mut b = Builder::new(&arena, &mut s.slots).unwrap();
        let rows = [
            ("direct", Kind::DirectSmtp, "0.0.0.0:25"),
            ("gateway", Kind::GatewaySmtp, "[::]:2525"),
            ("web", Kind::Https, "[::]:443"),
            ("challenge", Kind::Http01, "0.0.0.0:80"),
            ("fixture", Kind::LoopbackSmtpFixture, "127.1.2.3:2526"),
        ];
        for (i, (name, kind, bind)) in rows.iter().enumerate() {
            b.listener(&mut arena, name, input(*kind, bind), at(i as u32 + 1))
                .unwrap();
        }
        let plan = Limits::default().plan().unwrap();
        let records = b.finish(&plan).unwrap();
        {
            let view = records.view_live(&arena).unwrap();
            assert_eq!(view.len(), 5);
            assert!(!view.is_empty());
            assert!(view.listener(5).unwrap().is_none());
            assert!(view.listener(usize::MAX).unwrap().is_none());
            for (i, (name, kind, bind)) in rows.iter().enumerate() {
                let l = view.listener(i).unwrap().unwrap();
                assert_eq!(l.name, *name);
                assert_eq!(l.kind, *kind);
                assert_eq!(
                    l.kind.name(),
                    [
                        "direct_smtp",
                        "gateway_smtp",
                        "https",
                        "http01",
                        "loopback_smtp_fixture"
                    ][i]
                );
                assert_eq!(l.bind, bind.parse().unwrap());
                assert_eq!(l.location, at(i as u32 + 1));
                let named = matches!(kind, Kind::DirectSmtp | Kind::GatewaySmtp);
                assert_eq!(l.server_name, named.then_some("mail.example.test"));
                assert_eq!(
                    l.certificate,
                    (named || *kind == Kind::Https).then_some("cert")
                );
                assert_eq!(
                    l.gateway,
                    (*kind == Kind::GatewaySmtp).then_some("upstream")
                );
                assert_eq!(l.session_limit, kind.is_smtp().then_some(1));
                assert_eq!(l.per_peer_limit, kind.is_smtp().then_some(1));
            }
        }
        arena.append(b"later input").unwrap();
        assert_eq!(
            records
                .view(arena.freeze().unwrap())
                .unwrap()
                .listener(1)
                .unwrap()
                .unwrap()
                .gateway,
            Some("upstream")
        );
    }
    #[test]
    fn role_required_and_forbidden_fields_are_typed_and_sticky() {
        let _lock = lock();
        let fields = [
            Field::ServerName,
            Field::Certificate,
            Field::Gateway,
            Field::SessionLimit,
            Field::PerPeerLimit,
        ];
        let matrix = [
            (Kind::DirectSmtp, [true, true, false, true, true]),
            (Kind::GatewaySmtp, [true, true, true, true, true]),
            (Kind::Https, [false, true, false, false, false]),
            (Kind::Http01, [false, false, false, false, false]),
            (Kind::LoopbackSmtpFixture, [false, false, false, true, true]),
        ];
        let cases = matrix.into_iter().flat_map(|(kind, required)| {
            fields
                .into_iter()
                .zip(required)
                .map(move |(field, required)| {
                    (
                        kind,
                        field,
                        if required {
                            Code::Required
                        } else {
                            Code::Forbidden
                        },
                    )
                })
        });
        for (kind, field, code) in cases {
            let mut s = Storage::new();
            let mut arena = text::Builder::new(&mut s.bytes).unwrap();
            let mut b = Builder::new(&arena, &mut s.slots).unwrap();
            let mut i = input(kind, "127.0.0.1:80");
            let present = code == Code::Forbidden;
            match field {
                Field::ServerName => i.server_name = present.then_some("mail.example.test"),
                Field::Certificate => i.certificate = present.then_some("cert"),
                Field::Gateway => i.gateway = present.then_some("gw"),
                Field::SessionLimit => i.session_limit = present.then_some(1),
                Field::PerPeerLimit => i.per_peer_limit = present.then_some(1),
                _ => {}
            }
            let e = b.listener(&mut arena, "listener", i, at(3)).unwrap_err();
            assert_eq!((e.code, e.field), (code, Some(field)));
            assert_eq!(arena.used(), 0);
            assert_eq!(
                b.listener(
                    &mut arena,
                    "valid",
                    input(Kind::Https, "127.0.0.1:443"),
                    at(4)
                )
                .unwrap_err(),
                e
            );
            assert_eq!(b.finish(&Limits::default().plan().unwrap()).unwrap_err(), e);
        }
    }
    #[test]
    fn fixture_loopback_http01_port_and_numeric_bind_rules_refuse_invalid_inputs() {
        let _lock = lock();
        for (kind, bind, code) in [
            (Kind::LoopbackSmtpFixture, "0.0.0.0:2525", Code::Loopback),
            (Kind::LoopbackSmtpFixture, "[::]:2525", Code::Loopback),
            (Kind::LoopbackSmtpFixture, "192.0.2.1:2525", Code::Loopback),
            (Kind::Http01, "127.0.0.1:8080", Code::Http01Port),
            (Kind::Https, "[::ffff:127.0.0.1]:443", Code::Endpoint),
            (Kind::Https, "[fe80::1%1]:443", Code::Endpoint),
            (Kind::Https, "127.0.0.1:0", Code::Endpoint),
            (Kind::Https, "name.example.test:443", Code::Endpoint),
        ] {
            let mut s = Storage::new();
            let mut arena = text::Builder::new(&mut s.bytes).unwrap();
            let mut b = Builder::new(&arena, &mut s.slots).unwrap();
            let e = b
                .listener(&mut arena, "l", input(kind, bind), at(1))
                .unwrap_err();
            assert_eq!((e.code, e.field), (code, Some(Field::Bind)));
            assert_eq!(arena.used(), 0);
        }
        let mut s = Storage::new();
        let mut arena = text::Builder::new(&mut s.bytes).unwrap();
        let mut b = Builder::new(&arena, &mut s.slots).unwrap();
        b.listener(
            &mut arena,
            "fixture",
            input(Kind::LoopbackSmtpFixture, "[::1]:2525"),
            at(1),
        )
        .unwrap();
        b.listener(
            &mut arena,
            "https",
            input(Kind::Https, "127.0.0.1:443"),
            at(2),
        )
        .unwrap();
        b.finish(&Limits::default().plan().unwrap()).unwrap();
    }
    #[test]
    fn duplicate_names_and_bind_conflicts_keep_previous_coordinates() {
        let _lock = lock();
        for (a, b, conflicting) in [
            ("0.0.0.0:443", "192.0.2.1:443", true),
            ("192.0.2.1:443", "0.0.0.0:443", true),
            ("[::]:443", "[2001:db8::1]:443", true),
            ("[::1]:443", "[0:0:0:0:0:0:0:1]:443", true),
            ("192.0.2.1:443", "192.0.2.1:443", true),
            ("192.0.2.1:443", "192.0.2.2:443", false),
            ("0.0.0.0:443", "[::]:443", false),
            ("0.0.0.0:443", "0.0.0.0:444", false),
        ] {
            let mut s = Storage::new();
            let mut arena = text::Builder::new(&mut s.bytes).unwrap();
            let mut build = Builder::new(&arena, &mut s.slots).unwrap();
            build
                .listener(&mut arena, "first", input(Kind::Https, a), at(1))
                .unwrap();
            let result = build.listener(&mut arena, "second", input(Kind::Https, b), at(3));
            if conflicting {
                let e = result.unwrap_err();
                assert_eq!((e.code, e.previous), (Code::BindConflict, Some(at(1))));
            } else {
                result.unwrap();
            }
        }
        let mut s = Storage::new();
        let mut arena = text::Builder::new(&mut s.bytes).unwrap();
        let mut b = Builder::new(&arena, &mut s.slots).unwrap();
        b.listener(
            &mut arena,
            "same",
            input(Kind::Https, "127.0.0.1:443"),
            at(2),
        )
        .unwrap();
        let e = b
            .listener(&mut arena, "same", input(Kind::Http01, "invalid"), at(4))
            .unwrap_err();
        assert_eq!(e.to_string(),"config_listener_duplicate for name at line 4, byte column 3 (previously configured at line 2, byte column 3)");
    }
    #[test]
    fn mandatory_roles_and_global_pool_budgets_are_checked_at_finish() {
        let _lock = lock();
        let plan = Limits::default().plan().unwrap();
        for case in 0..5 {
            let mut s = Storage::new();
            let mut arena = text::Builder::new(&mut s.bytes).unwrap();
            let mut b = Builder::new(&arena, &mut s.slots).unwrap();
            if case != 1 {
                b.listener(
                    &mut arena,
                    "https",
                    input(Kind::Https, "127.0.0.1:443"),
                    at(1),
                )
                .unwrap();
            }
            if case != 0 {
                let mut smtp = input(Kind::DirectSmtp, "127.0.0.1:25");
                smtp.session_limit = Some(if case == 2 {
                    9
                } else if case == 3 {
                    3
                } else {
                    5
                });
                smtp.per_peer_limit = Some(if case == 3 { 3 } else { 1 });
                b.listener(&mut arena, "smtp", smtp, at(2)).unwrap();
            }
            if case == 4 {
                let mut smtp = input(Kind::GatewaySmtp, "127.0.0.1:2525");
                smtp.session_limit = Some(4);
                b.listener(&mut arena, "gateway", smtp, at(3)).unwrap();
            }
            let e = b.finish(&plan).unwrap_err();
            let expected = match case {
                0 => Code::MissingSmtp,
                1 => Code::MissingHttps,
                2 | 3 => Code::Range,
                _ => Code::SessionBudget,
            };
            assert_eq!(e.code, expected);
            if case == 2 {
                assert_eq!(e.field, Some(Field::SessionLimit));
            }
            if case == 3 {
                assert_eq!(e.field, Some(Field::PerPeerLimit));
            }
        }
        for (sessions, per_peer, field) in [
            (0, 1, Field::SessionLimit),
            (1, 0, Field::PerPeerLimit),
            (1, 2, Field::PerPeerLimit),
        ] {
            let mut s = Storage::new();
            let mut arena = text::Builder::new(&mut s.bytes).unwrap();
            let mut b = Builder::new(&arena, &mut s.slots).unwrap();
            let mut i = input(Kind::DirectSmtp, "127.0.0.1:25");
            i.session_limit = Some(sessions);
            i.per_peer_limit = Some(per_peer);
            let e = b.listener(&mut arena, "smtp", i, at(1)).unwrap_err();
            assert_eq!((e.code, e.field), (Code::Range, Some(field)));
        }
    }
    #[test]
    fn inclusive_pool_bounds_fixture_accounting_and_http01_role_are_pinned() {
        let _lock = lock();
        let plan = Limits::default().plan().unwrap();
        for split in [false, true] {
            let mut s = Storage::new();
            let mut arena = text::Builder::new(&mut s.bytes).unwrap();
            let mut b = Builder::new(&arena, &mut s.slots).unwrap();
            let mut smtp = input(Kind::DirectSmtp, "127.0.0.1:25");
            smtp.session_limit = Some(if split { 5 } else { 8 });
            smtp.per_peer_limit = Some(2);
            b.listener(&mut arena, "smtp", smtp, at(1)).unwrap();
            b.listener(
                &mut arena,
                "web",
                input(Kind::Https, "127.0.0.1:443"),
                at(2),
            )
            .unwrap();
            if split {
                let mut fixture = input(Kind::LoopbackSmtpFixture, "127.0.0.1:2525");
                fixture.session_limit = Some(3);
                b.listener(&mut arena, "fixture", fixture, at(3)).unwrap();
            }
            b.finish(&plan).unwrap();
        }
        {
            let mut s = Storage::new();
            let mut arena = text::Builder::new(&mut s.bytes).unwrap();
            let mut b = Builder::new(&arena, &mut s.slots).unwrap();
            let mut smtp = input(Kind::DirectSmtp, "127.0.0.1:25");
            smtp.session_limit = Some(5);
            b.listener(&mut arena, "smtp", smtp, at(1)).unwrap();
            b.listener(
                &mut arena,
                "web",
                input(Kind::Https, "127.0.0.1:443"),
                at(2),
            )
            .unwrap();
            let mut fixture = input(Kind::LoopbackSmtpFixture, "127.0.0.1:2525");
            fixture.session_limit = Some(4);
            b.listener(&mut arena, "fixture", fixture, at(3)).unwrap();
            let e = b.finish(&plan).unwrap_err();
            assert_eq!(
                (e.code, e.field, e.location),
                (Code::SessionBudget, Some(Field::SessionLimit), Some(at(3)))
            );
            assert_eq!(
                e.to_string(),
                "config_listener_session_budget for session_limit at line 3, byte column 3"
            );
        }
        {
            let mut s = Storage::new();
            let mut arena = text::Builder::new(&mut s.bytes).unwrap();
            let mut b = Builder::new(&arena, &mut s.slots).unwrap();
            b.listener(
                &mut arena,
                "smtp",
                input(Kind::DirectSmtp, "127.0.0.1:25"),
                at(1),
            )
            .unwrap();
            b.listener(
                &mut arena,
                "challenge",
                input(Kind::Http01, "127.0.0.1:80"),
                at(2),
            )
            .unwrap();
            assert_eq!(b.finish(&plan).unwrap_err().code, Code::MissingHttps);
        }
        for (field, expected) in [
            (Field::Name, "name"),
            (Field::Bind, "bind"),
            (Field::ServerName, "server_name"),
            (Field::Certificate, "certificate"),
            (Field::Gateway, "gateway"),
            (Field::SessionLimit, "session_limit"),
            (Field::PerPeerLimit, "per_peer_limit"),
        ] {
            assert_eq!(field.name(), expected);
        }
    }
    #[test]
    fn full_and_small_regions_and_reuse_respect_cell_and_text_bounds() {
        let _lock = lock();
        let mut s = Storage::new();
        let mut arena = text::Builder::new(&mut s.bytes).unwrap();
        let plan = Limits::default().plan().unwrap();
        assert_eq!(
            Builder::new(&arena, &mut []).unwrap_err().code,
            Code::Capacity
        );
        let mut excess = [Slot::EMPTY; 17];
        assert_eq!(
            Builder::new(&arena, &mut excess).unwrap_err().code,
            Code::Capacity
        );
        {
            let mut b = Builder::new(&arena, &mut s.slots).unwrap();
            b.listener(
                &mut arena,
                "smtp",
                input(Kind::DirectSmtp, "127.0.0.1:25"),
                at(1),
            )
            .unwrap();
            for i in 1..16 {
                b.listener(
                    &mut arena,
                    &format!("l{i}"),
                    input(Kind::Https, &format!("127.0.0.1:{}", 4000 + i)),
                    at(2),
                )
                .unwrap();
            }
            let records = b.finish(&plan).unwrap();
            assert_eq!(records.view_live(&arena).unwrap().len(), 16);
        }
        {
            let mut b = Builder::new(&arena, &mut s.slots).unwrap();
            b.listener(
                &mut arena,
                "fixture",
                input(Kind::LoopbackSmtpFixture, "127.0.0.1:2525"),
                at(4),
            )
            .unwrap();
            b.listener(
                &mut arena,
                "web",
                input(Kind::Https, "127.0.0.1:443"),
                at(5),
            )
            .unwrap();
            let records = b.finish(&plan).unwrap();
            let v = records.view_live(&arena).unwrap();
            assert_eq!(v.len(), 2);
            let l = v.listener(0).unwrap().unwrap();
            assert_eq!(l.kind, Kind::LoopbackSmtpFixture);
            assert!(l.server_name.is_none());
            assert!(l.certificate.is_none());
            assert!(l.gateway.is_none());
            assert!(v.listener(2).unwrap().is_none());
        }
        {
            let mut small = [Slot::EMPTY; 1];
            let mut b = Builder::new(&arena, &mut small).unwrap();
            b.listener(&mut arena, "a", input(Kind::Https, "127.0.0.1:443"), at(1))
                .unwrap();
            assert_eq!(
                b.listener(&mut arena, "b", input(Kind::Https, "127.0.0.1:444"), at(2))
                    .unwrap_err()
                    .code,
                Code::Capacity
            );
        }
        let mut bytes = [0; 8];
        let mut small_arena = text::Builder::new(&mut bytes).unwrap();
        let mut slots = [Slot::EMPTY; 1];
        let mut b = Builder::new(&small_arena, &mut slots).unwrap();
        let e = b
            .listener(
                &mut small_arena,
                "smtp",
                input(Kind::DirectSmtp, "127.0.0.1:25"),
                at(3),
            )
            .unwrap_err();
        assert_eq!(e.code, Code::Text);
        assert_eq!(small_arena.used(), 4);
        assert_eq!(b.finish(&plan).unwrap_err(), e);
        {
            let mut one = [Slot::EMPTY; 1];
            let mut b = Builder::new(&arena, &mut one).unwrap();
            b.listener(&mut arena, "a", input(Kind::Https, "127.0.0.1:443"), at(1))
                .unwrap();
            assert_eq!(
                b.listener(&mut arena, "b", input(Kind::Https, "invalid"), at(2))
                    .unwrap_err()
                    .code,
                Code::Capacity
            );
        }
        assert!(std::mem::size_of::<Slot>() <= 128);
        assert!(std::mem::size_of::<Builder<'_>>() <= 128);
    }
    #[test]
    fn exact_name_bounds_and_field_validation_precede_text_writes() {
        let _lock = lock();
        let name = format!("l{}", "n".repeat(63));
        let host = [
            "a".repeat(63),
            "b".repeat(63),
            "c".repeat(63),
            "d".repeat(51),
        ]
        .join(".");
        assert_eq!(host.len(), 243);
        let mut s = Storage::new();
        let mut arena = text::Builder::new(&mut s.bytes).unwrap();
        let mut b = Builder::new(&arena, &mut s.slots).unwrap();
        let mut i = input(Kind::GatewaySmtp, "127.0.0.1:25");
        i.server_name = Some(&host);
        i.certificate = Some(&name);
        i.gateway = Some(&name);
        b.listener(&mut arena, &name, i, at(1)).unwrap();
        b.listener(
            &mut arena,
            "web",
            input(Kind::Https, "127.0.0.1:443"),
            at(2),
        )
        .unwrap();
        let records = b.finish(&Limits::default().plan().unwrap()).unwrap();
        let l = records
            .view_live(&arena)
            .unwrap()
            .listener(0)
            .unwrap()
            .unwrap();
        assert_eq!(l.name, name);
        assert_eq!(l.server_name, Some(host.as_str()));
        assert_eq!(l.gateway, Some(name.as_str()));
        for (field, value, code) in [
            (Field::Name, format!("{name}a"), Code::Profile),
            (Field::ServerName, format!("{host}d"), Code::Hostname),
            (Field::Certificate, "Bad".into(), Code::Profile),
            (Field::Gateway, "bad.name".into(), Code::Profile),
        ] {
            let mut s = Storage::new();
            let mut arena = text::Builder::new(&mut s.bytes).unwrap();
            let mut b = Builder::new(&arena, &mut s.slots).unwrap();
            let mut i = input(Kind::GatewaySmtp, "127.0.0.1:25");
            let mut name = "l";
            match field {
                Field::Name => name = &value,
                Field::ServerName => i.server_name = Some(&value),
                Field::Certificate => i.certificate = Some(&value),
                Field::Gateway => i.gateway = Some(&value),
                _ => {}
            }
            let e = b.listener(&mut arena, name, i, at(1)).unwrap_err();
            assert_eq!((e.code, e.field), (code, Some(field)));
            assert_eq!(arena.used(), 0);
        }
    }
    #[test]
    fn foreign_arenas_and_debug_output_do_not_expose_configuration() {
        let _lock = lock();
        let mut s = Storage::new();
        let mut other = [0; 4096];
        let mut arena = text::Builder::new(&mut s.bytes).unwrap();
        let mut foreign = text::Builder::new(&mut other).unwrap();
        {
            let mut b = Builder::new(&arena, &mut s.slots).unwrap();
            let e = b
                .listener(
                    &mut foreign,
                    "private",
                    input(Kind::Https, "127.0.0.1:443"),
                    at(1),
                )
                .unwrap_err();
            assert_eq!(e.code, Code::ForeignArena);
            assert_eq!(
                b.listener(
                    &mut arena,
                    "valid",
                    input(Kind::Https, "127.0.0.1:443"),
                    at(2)
                )
                .unwrap_err(),
                e
            );
            assert_eq!(b.finish(&Limits::default().plan().unwrap()).unwrap_err(), e);
            assert!(!format!("{e:?} {e}").contains("private"));
            assert!(std::error::Error::source(&e).is_none());
        }
        let mut b = Builder::new(&arena, &mut s.slots).unwrap();
        assert_eq!(format!("{b:?}"), "ListenerBuilder(<redacted>)");
        let i = input(Kind::DirectSmtp, "127.0.0.1:25");
        assert_eq!(format!("{i:?}"), "ListenerInput(<redacted>)");
        b.listener(&mut arena, "smtp", i, at(1)).unwrap();
        b.listener(
            &mut arena,
            "web",
            input(Kind::Https, "127.0.0.1:443"),
            at(2),
        )
        .unwrap();
        let records = b.finish(&Limits::default().plan().unwrap()).unwrap();
        assert_eq!(
            records.view_live(&foreign).unwrap_err().code,
            Code::ForeignArena
        );
        assert_eq!(
            records.view(foreign.freeze().unwrap()).unwrap_err().code,
            Code::ForeignArena
        );
        let view = records.view_live(&arena).unwrap();
        assert_eq!(format!("{records:?}"), "ListenerRecords(<redacted>)");
        assert_eq!(format!("{view:?}"), "ListenerView(<redacted>)");
        assert_eq!(
            format!("{:?}", view.listener(0).unwrap().unwrap()),
            "Listener(<redacted>)"
        );
        let _ = arena.freeze().unwrap();
        let reused = text::Builder::new(&mut s.bytes).unwrap();
        assert_eq!(
            records.view_live(&reused).unwrap_err().code,
            Code::ForeignArena
        );
    }
}

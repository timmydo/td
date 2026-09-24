//! Structural domain policy bound to its routing table; no DNS/TLS publication.
use super::{routing, syntax::Location, text, values};
use crate::ids::AccountId;
use std::{
    fmt,
    num::{NonZeroU32, NonZeroU64},
};
const ORIGIN: Location = Location {
    line: NonZeroU32::MIN,
    column: 1,
};
pub const MAX_AGE: u64 = 31_557_600;
pub const DEFAULT_PREFERENCE: u16 = 10;
pub const DEFAULT_MAX_AGE_SECONDS: u32 = 86400;
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Mode {
    Off,
    Testing,
    Enforce,
    None,
}
impl Mode {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Testing => "testing",
            Self::Enforce => "enforce",
            Self::None => "none",
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Code {
    Capacity,
    ForeignArena,
    Routing,
    DnsName,
    Preference,
    Age,
    CertificateRequired,
    CertificateForbidden,
    Profile,
    Text,
    Invariant,
}
impl Code {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Capacity => "config_policy_capacity",
            Self::ForeignArena => "config_policy_foreign_arena",
            Self::Routing => "config_policy_routing",
            Self::DnsName => "config_policy_dns_name",
            Self::Preference => "config_policy_preference",
            Self::Age => "config_policy_age",
            Self::CertificateRequired => "config_policy_certificate_required",
            Self::CertificateForbidden => "config_policy_certificate_forbidden",
            Self::Profile => "config_policy_profile",
            Self::Text => "config_policy_text",
            Self::Invariant => "config_policy_invariant",
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Error {
    pub code: Code,
    pub location: Option<Location>,
    pub previous: Option<Location>,
    pub routing: Option<routing::Code>,
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.code.name())?;
        if let Some(code) = self.routing {
            write!(f, ": {}", code.name())?;
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
impl std::error::Error for Error {}
fn err(code: Code, at: Option<Location>) -> Error {
    Error {
        code,
        location: at,
        previous: None,
        routing: None,
    }
}
fn route_error(e: routing::Error) -> Error {
    Error {
        code: Code::Routing,
        location: e.location,
        previous: e.previous,
        routing: Some(e.code),
    }
}
fn invariant() -> Error {
    err(Code::Invariant, None)
}
#[derive(Clone, Copy)]
pub struct Slot {
    mx: text::Span,
    certificate: text::Span,
    age: u32,
    preference: u16,
    location: Location,
    mode: Mode,
    present: bool,
    explicit_mx: bool,
    has_certificate: bool,
}
impl Slot {
    pub const EMPTY: Self = Self {
        mx: text::Span::EMPTY,
        certificate: text::Span::EMPTY,
        age: DEFAULT_MAX_AGE_SECONDS,
        preference: DEFAULT_PREFERENCE,
        location: ORIGIN,
        mode: Mode::Off,
        present: false,
        explicit_mx: false,
        has_certificate: false,
    };
}
const _: [(); 1] = [(); (std::mem::size_of::<Slot>() <= 64) as usize];
pub struct Input<'a> {
    pub mx_host: Option<&'a str>,
    pub mx_preference: u64,
    pub mode: Mode,
    pub max_age_seconds: u64,
    pub certificate: Option<&'a str>,
}
impl Default for Input<'_> {
    fn default() -> Self {
        Self {
            mx_host: None,
            mx_preference: u64::from(DEFAULT_PREFERENCE),
            mode: Mode::Off,
            max_age_seconds: u64::from(DEFAULT_MAX_AGE_SECONDS),
            certificate: None,
        }
    }
}
impl fmt::Debug for Input<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("DomainPolicyInput(<redacted>)")
    }
}
pub struct Builder<'a> {
    routes: routing::Builder<'a>,
    slots: &'a mut [Slot],
    owner: NonZeroU64,
    failure: Option<Error>,
}
impl fmt::Debug for Builder<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("DomainPolicyBuilder(<redacted>)")
    }
}
impl<'a> Builder<'a> {
    pub fn new(
        arena: &text::Builder<'_>,
        route_text: &'a mut [u8],
        domains: &'a mut [routing::DomainSlot],
        aliases: &'a mut [routing::AliasSlot],
        slots: &'a mut [Slot],
    ) -> Result<Self, Error> {
        if slots.is_empty() || slots.len() > routing::MAX_DOMAINS {
            return Err(err(Code::Capacity, None));
        }
        let routes = routing::Builder::new(route_text, domains, aliases).map_err(route_error)?;
        for slot in slots.iter_mut() {
            slot.present = false;
        }
        Ok(Self {
            routes,
            slots,
            owner: arena.owner(),
            failure: None,
        })
    }
    fn record<T>(&mut self, result: Result<T, Error>) -> Result<T, Error> {
        if let Err(e) = result {
            self.failure = Some(e);
        }
        result
    }
    pub fn account(&mut self, id: AccountId, at: Location) -> Result<(), Error> {
        if let Some(e) = self.failure {
            return Err(e);
        }
        let result = self.routes.account(id, at).map_err(route_error);
        self.record(result)
    }
    pub fn alias(&mut self, address: &str, id: AccountId, at: Location) -> Result<(), Error> {
        if let Some(e) = self.failure {
            return Err(e);
        }
        let result = self.routes.alias(address, id, at).map_err(route_error);
        self.record(result)
    }
    pub fn domain(
        &mut self,
        arena: &mut text::Builder<'_>,
        name: &str,
        input: Input<'_>,
        at: Location,
    ) -> Result<(), Error> {
        if let Some(e) = self.failure {
            return Err(e);
        }
        let result = self.domain_inner(arena, name, input, at);
        self.record(result)
    }
    fn domain_inner(
        &mut self,
        arena: &mut text::Builder<'_>,
        name: &str,
        input: Input<'_>,
        at: Location,
    ) -> Result<(), Error> {
        if self.owner != arena.owner() {
            return Err(err(Code::ForeignArena, Some(at)));
        }
        let preference =
            u16::try_from(input.mx_preference).map_err(|_| err(Code::Preference, Some(at)))?;
        if input.max_age_seconds > MAX_AGE {
            return Err(err(Code::Age, Some(at)));
        }
        let age = u32::try_from(input.max_age_seconds).map_err(|_| err(Code::Age, Some(at)))?;
        if input.mode == Mode::Off && input.certificate.is_some() {
            return Err(err(Code::CertificateForbidden, Some(at)));
        }
        if input.mode != Mode::Off && input.certificate.is_none() {
            return Err(err(Code::CertificateRequired, Some(at)));
        }
        if let Some(certificate) = input.certificate {
            values::profile_name(certificate).map_err(|_| err(Code::Profile, Some(at)))?;
        }
        if let Some(host) = input.mx_host {
            values::dns_name(host).map_err(|_| err(Code::DnsName, Some(at)))?;
        }
        let index = usize::from(self.routes.domain_index(name, at).map_err(route_error)?);
        if index >= self.slots.len() {
            return Err(err(Code::Capacity, Some(at)));
        }
        let mx = match input.mx_host {
            Some(host) => dns(arena, host, at)?,
            None => text::Span::EMPTY,
        };
        let certificate = match input.certificate {
            Some(name) => {
                let handle = arena
                    .append(name.as_bytes())
                    .map_err(|_| err(Code::Text, Some(at)))?;
                arena.compact(handle).map_err(|_| invariant())?
            }
            None => text::Span::EMPTY,
        };
        *self.slots.get_mut(index).ok_or_else(invariant)? = Slot {
            mx,
            certificate,
            age,
            preference,
            location: at,
            mode: input.mode,
            present: true,
            explicit_mx: input.mx_host.is_some(),
            has_certificate: input.certificate.is_some(),
        };
        Ok(())
    }
    /// The dispatcher can stage this 243-byte global field in builder scratch;
    /// this handoff stores its single canonical copy for all default MX values.
    pub fn finish(
        self,
        arena: &mut text::Builder<'_>,
        server_hostname: &str,
        at: Location,
    ) -> Result<Records<'a>, Error> {
        if let Some(e) = self.failure {
            return Err(e);
        }
        if self.owner != arena.owner() {
            return Err(err(Code::ForeignArena, Some(at)));
        }
        let routes = self.routes.finish().map_err(route_error)?;
        let count = routes.domain_count();
        let slots = self
            .slots
            .get(..count)
            .ok_or_else(|| err(Code::Capacity, None))?;
        for index in 0..count {
            let original = routes.original_domain_index(index).map_err(route_error)?;
            if !slots.get(original).ok_or_else(invariant)?.present {
                return Err(invariant());
            }
        }
        let hostname = dns(arena, server_hostname, at)?;
        Ok(Records {
            routes,
            slots,
            hostname,
            owner: self.owner,
        })
    }
}
fn dns(arena: &mut text::Builder<'_>, input: &str, at: Location) -> Result<text::Span, Error> {
    let handle = arena.append_dns(input).map_err(|code| {
        err(
            if code == text::Code::DnsName {
                Code::DnsName
            } else {
                Code::Text
            },
            Some(at),
        )
    })?;
    arena.compact(handle).map_err(|_| invariant())
}
pub struct Records<'a> {
    routes: routing::Routing<'a>,
    slots: &'a [Slot],
    hostname: text::Span,
    owner: NonZeroU64,
}
impl fmt::Debug for Records<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("DomainPolicyRecords(<redacted>)")
    }
}
impl Records<'_> {
    pub fn routes(&self) -> &routing::Routing<'_> {
        &self.routes
    }
    pub fn view_live<'s, 't>(&'s self, text: &'t text::Builder<'_>) -> Result<View<'s, 't>, Error> {
        self.view(text.borrowed_view().map_err(|_| invariant())?)
    }
    pub fn view<'s, 't>(&'s self, text: text::View<'t>) -> Result<View<'s, 't>, Error> {
        if self.owner != text.owner() {
            return Err(err(Code::ForeignArena, None));
        }
        Ok(View {
            routes: &self.routes,
            slots: self.slots,
            hostname: self.hostname,
            owner: self.owner,
            text,
        })
    }
}
#[derive(Clone, Copy)]
pub struct View<'s, 't> {
    routes: &'s routing::Routing<'s>,
    slots: &'s [Slot],
    hostname: text::Span,
    text: text::View<'t>,
    owner: NonZeroU64,
}
impl fmt::Debug for View<'_, '_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("DomainPolicyView(<redacted>)")
    }
}
pub struct Domain<'s, 't> {
    pub name: &'s str,
    pub mx_host: &'t str,
    pub explicit_mx: bool,
    pub mx_preference: u16,
    pub mode: Mode,
    pub max_age_seconds: u32,
    pub certificate: Option<&'t str>,
    pub location: Location,
}
impl fmt::Debug for Domain<'_, '_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("DomainPolicy(<redacted>)")
    }
}
impl<'s, 't> View<'s, 't> {
    pub fn len(self) -> usize {
        self.routes.domain_count()
    }
    pub fn is_empty(self) -> bool {
        self.len() == 0
    }
    pub fn hostname(self) -> Result<&'t str, Error> {
        read(self.text, self.owner, self.hostname)
    }
    pub fn domain(self, index: usize) -> Result<Option<Domain<'s, 't>>, Error> {
        let Some(name) = self.routes.domain_name(index).map_err(route_error)? else {
            return Ok(None);
        };
        let original = self
            .routes
            .original_domain_index(index)
            .map_err(route_error)?;
        let slot = self.slots.get(original).ok_or_else(invariant)?;
        Ok(Some(Domain {
            name,
            mx_host: read(
                self.text,
                self.owner,
                if slot.explicit_mx {
                    slot.mx
                } else {
                    self.hostname
                },
            )?,
            explicit_mx: slot.explicit_mx,
            mx_preference: slot.preference,
            mode: slot.mode,
            max_age_seconds: slot.age,
            certificate: if slot.has_certificate {
                Some(read(self.text, self.owner, slot.certificate)?)
            } else {
                None
            },
            location: slot.location,
        }))
    }
}
fn read(text: text::View<'_>, owner: NonZeroU64, span: text::Span) -> Result<&str, Error> {
    std::str::from_utf8(text.read_span(owner, span).map_err(|_| invariant())?)
        .map_err(|_| invariant())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    const ACCOUNT: AccountId = AccountId::from_bytes([1; 16]);
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
    struct Storage {
        route: Vec<u8>,
        text: Vec<u8>,
        domains: Vec<routing::DomainSlot>,
        aliases: Vec<routing::AliasSlot>,
        slots: Vec<Slot>,
    }
    impl Storage {
        fn new(count: usize) -> Self {
            Self {
                route: vec![0; routing::MAX_TEXT_BYTES],
                text: vec![0; text::MAX_BYTES],
                domains: vec![routing::DomainSlot::EMPTY; count],
                aliases: vec![routing::AliasSlot::EMPTY; count],
                slots: vec![Slot::EMPTY; count],
            }
        }
    }
    #[test]
    fn forward_aliases_sorting_and_policy_association_share_domain_storage() {
        let _lock = lock();
        let mut s = Storage::new(3);
        let mut arena = text::Builder::new(&mut s.text).unwrap();
        let mut b = Builder::new(
            &arena,
            &mut s.route,
            &mut s.domains,
            &mut s.aliases,
            &mut s.slots,
        )
        .unwrap();
        b.alias("z@Z.test", ACCOUNT, at(1)).unwrap();
        b.alias("a@A.test", ACCOUNT, at(2)).unwrap();
        b.domain(&mut arena, "B.test", Input::default(), at(3))
            .unwrap();
        b.domain(
            &mut arena,
            "A.test",
            Input {
                mx_host: Some("MX.A.test"),
                mx_preference: 65535,
                mode: Mode::Enforce,
                max_age_seconds: 0,
                certificate: Some("cert-a"),
            },
            at(4),
        )
        .unwrap();
        b.domain(
            &mut arena,
            "Z.test",
            Input {
                mx_host: Some("MX.Z.test"),
                mx_preference: 0,
                mode: Mode::Testing,
                max_age_seconds: MAX_AGE,
                certificate: Some("cert-z"),
            },
            at(5),
        )
        .unwrap();
        b.account(ACCOUNT, at(6)).unwrap();
        let records = b.finish(&mut arena, "SERVER.test", at(7)).unwrap();
        assert_eq!(records.routes().text_bytes(), "a.testb.testz.testza".len());
        assert_eq!(records.routes().resolve("z@z.test").unwrap(), Some(ACCOUNT));
        assert_eq!(records.routes().resolve("a@a.test").unwrap(), Some(ACCOUNT));
        assert_eq!(
            arena.used(),
            "mx.a.testmx.z.testcert-acert-zserver.test".len()
        );
        let view = records.view_live(&arena).unwrap();
        assert_eq!(view.len(), 3);
        assert!(!view.is_empty());
        assert_eq!(view.hostname(), Ok("server.test"));
        let a = view.domain(0).unwrap().unwrap();
        assert_eq!(a.name, "a.test");
        assert_eq!(a.mx_host, "mx.a.test");
        assert!(a.explicit_mx);
        assert_eq!(a.mx_preference, 65535);
        assert_eq!(a.mode, Mode::Enforce);
        assert_eq!(a.max_age_seconds, 0);
        assert_eq!(a.certificate, Some("cert-a"));
        assert_eq!(a.location, at(4));
        let b = view.domain(1).unwrap().unwrap();
        assert_eq!(b.name, "b.test");
        assert_eq!(b.mx_host, "server.test");
        assert!(!b.explicit_mx);
        assert_eq!(b.mx_preference, 10);
        assert_eq!(b.mode, Mode::Off);
        assert_eq!(b.max_age_seconds, 86400);
        assert_eq!(b.certificate, None);
        let z = view.domain(2).unwrap().unwrap();
        assert_eq!(z.name, "z.test");
        assert_eq!(z.mx_host, "mx.z.test");
        assert_eq!(z.mode, Mode::Testing);
        assert_eq!(z.max_age_seconds, u32::try_from(MAX_AGE).unwrap());
        assert!(view.domain(3).unwrap().is_none());
        arena.append(b"later protected material").unwrap();
        let frozen = arena.freeze().unwrap();
        assert_eq!(records.view(frozen).unwrap().hostname(), Ok("server.test"));
    }
    #[test]
    fn certificate_presence_is_exactly_mode_dependent_and_off_retains_age() {
        let _lock = lock();
        for mode in [Mode::Off, Mode::Testing, Mode::Enforce, Mode::None] {
            for certificate in [None, Some("cert")] {
                let mut s = Storage::new(1);
                let mut arena = text::Builder::new(&mut s.text).unwrap();
                let mut b = Builder::new(
                    &arena,
                    &mut s.route,
                    &mut s.domains,
                    &mut s.aliases,
                    &mut s.slots,
                )
                .unwrap();
                b.account(ACCOUNT, at(1)).unwrap();
                let result = b.domain(
                    &mut arena,
                    "a.test",
                    Input {
                        mode,
                        certificate,
                        max_age_seconds: MAX_AGE,
                        ..Input::default()
                    },
                    at(2),
                );
                if (mode == Mode::Off) == certificate.is_none() {
                    result.unwrap();
                    let records = b.finish(&mut arena, "mx.test", at(3)).unwrap();
                    let view = records.view(arena.freeze().unwrap()).unwrap();
                    let domain = view.domain(0).unwrap().unwrap();
                    assert_eq!(domain.mode, mode);
                    assert_eq!(domain.max_age_seconds, u32::try_from(MAX_AGE).unwrap());
                    assert_eq!(domain.certificate, certificate);
                } else {
                    let e = result.unwrap_err();
                    assert_eq!(
                        e.code,
                        if mode == Mode::Off {
                            Code::CertificateForbidden
                        } else {
                            Code::CertificateRequired
                        }
                    );
                    assert_eq!(b.finish(&mut arena, "mx.test", at(3)).unwrap_err(), e);
                }
            }
        }
        for (mode, name) in [
            (Mode::Off, "off"),
            (Mode::Testing, "testing"),
            (Mode::Enforce, "enforce"),
            (Mode::None, "none"),
        ] {
            assert_eq!(mode.name(), name);
        }
    }
    #[test]
    fn field_bounds_profile_syntax_and_sticky_failure_refuse_candidates() {
        let _lock = lock();
        for (input, code) in [
            (
                Input {
                    mx_preference: 65536,
                    ..Input::default()
                },
                Code::Preference,
            ),
            (
                Input {
                    max_age_seconds: MAX_AGE + 1,
                    ..Input::default()
                },
                Code::Age,
            ),
            (
                Input {
                    mode: Mode::Testing,
                    certificate: Some("Bad Name"),
                    ..Input::default()
                },
                Code::Profile,
            ),
            (
                Input {
                    mx_host: Some("bad..test"),
                    ..Input::default()
                },
                Code::DnsName,
            ),
        ] {
            let mut s = Storage::new(1);
            let mut arena = text::Builder::new(&mut s.text).unwrap();
            let mut b = Builder::new(
                &arena,
                &mut s.route,
                &mut s.domains,
                &mut s.aliases,
                &mut s.slots,
            )
            .unwrap();
            let e = b.domain(&mut arena, "a.test", input, at(2)).unwrap_err();
            assert_eq!(e.code, code);
            assert_eq!(e.location, Some(at(2)));
            assert_eq!(b.account(ACCOUNT, at(3)), Err(e));
            assert_eq!(b.alias("x@a.test", ACCOUNT, at(4)), Err(e));
            assert_eq!(b.finish(&mut arena, "mx.test", at(5)).unwrap_err(), e);
        }
    }
    #[test]
    fn routing_errors_and_duplicate_coordinates_are_retained() {
        let _lock = lock();
        for scenario in 0..4 {
            let mut s = Storage::new(2);
            let mut arena = text::Builder::new(&mut s.text).unwrap();
            let mut b = Builder::new(
                &arena,
                &mut s.route,
                &mut s.domains,
                &mut s.aliases,
                &mut s.slots,
            )
            .unwrap();
            if scenario != 0 {
                b.account(ACCOUNT, at(1)).unwrap();
            }
            b.domain(&mut arena, "A.test", Input::default(), at(2))
                .unwrap();
            let code = match scenario {
                0 => routing::Code::MissingAccount,
                1 => {
                    b.alias("x@unknown.test", ACCOUNT, at(3)).unwrap();
                    routing::Code::UnknownDomain
                }
                2 => {
                    let e = b
                        .domain(&mut arena, "a.TEST", Input::default(), at(4))
                        .unwrap_err();
                    assert_eq!(e.previous, Some(at(2)));
                    assert_eq!(e.location, Some(at(4)));
                    assert!(e.to_string().contains("previously declared at line 2"));
                    routing::Code::DuplicateDomain
                }
                _ => {
                    b.alias("x@a.test", AccountId::from_bytes([2; 16]), at(3))
                        .unwrap();
                    routing::Code::UnknownAccount
                }
            };
            let e = b.finish(&mut arena, "mx.test", at(5)).unwrap_err();
            assert_eq!(e.code, Code::Routing);
            assert_eq!(e.routing, Some(code));
            assert!(std::error::Error::source(&e).is_none());
        }
    }
    #[test]
    fn foreign_arena_checks_cover_mutation_finish_and_both_views() {
        let _lock = lock();
        for scenario in 0..3 {
            let mut s = Storage::new(1);
            let mut arena = text::Builder::new(&mut s.text).unwrap();
            let mut other_bytes = [0; 64];
            let mut other = text::Builder::new(&mut other_bytes).unwrap();
            let mut b = Builder::new(
                &arena,
                &mut s.route,
                &mut s.domains,
                &mut s.aliases,
                &mut s.slots,
            )
            .unwrap();
            b.account(ACCOUNT, at(1)).unwrap();
            if scenario == 0 {
                let e = b
                    .domain(&mut other, "a.test", Input::default(), at(2))
                    .unwrap_err();
                assert_eq!(e.code, Code::ForeignArena);
                assert_eq!(b.finish(&mut arena, "mx.test", at(3)).unwrap_err(), e);
                continue;
            }
            b.domain(&mut arena, "a.test", Input::default(), at(2))
                .unwrap();
            if scenario == 1 {
                assert_eq!(
                    b.finish(&mut other, "mx.test", at(3)).unwrap_err().code,
                    Code::ForeignArena
                );
                continue;
            }
            let records = b.finish(&mut arena, "mx.test", at(3)).unwrap();
            assert_eq!(
                records.view_live(&other).unwrap_err().code,
                Code::ForeignArena
            );
            assert_eq!(
                records.view(other.freeze().unwrap()).unwrap_err().code,
                Code::ForeignArena
            );
        }
    }
    #[test]
    fn full_domain_count_uses_one_default_hostname_and_fits_policy_partition() {
        let _lock = lock();
        assert!(std::mem::size_of::<Slot>() <= 64);
        assert!(std::mem::size_of::<Builder<'_>>() <= 256);
        let mut s = Storage::new(routing::MAX_DOMAINS);
        let mut arena = text::Builder::new(&mut s.text).unwrap();
        let mut b = Builder::new(
            &arena,
            &mut s.route,
            &mut s.domains,
            &mut s.aliases,
            &mut s.slots,
        )
        .unwrap();
        b.account(ACCOUNT, at(1)).unwrap();
        for n in (0..routing::MAX_DOMAINS).rev() {
            b.domain(
                &mut arena,
                &format!("d{n:03}.test"),
                Input {
                    mx_preference: u64::try_from(n).unwrap(),
                    ..Input::default()
                },
                at(u32::try_from(n + 2).unwrap()),
            )
            .unwrap();
        }
        let records = b.finish(&mut arena, "MX.test", at(300)).unwrap();
        assert_eq!(arena.used(), "mx.test".len());
        let view = records.view(arena.freeze().unwrap()).unwrap();
        assert_eq!(view.len(), 256);
        for n in 0..256 {
            let row = view.domain(n).unwrap().unwrap();
            assert_eq!(row.name, format!("d{n:03}.test"));
            assert_eq!(row.mx_preference, u16::try_from(n).unwrap());
            assert_eq!(row.mx_host, "mx.test");
            assert!(!row.explicit_mx);
        }
    }
    #[test]
    fn text_and_cell_exhaustion_fail_without_partial_records() {
        let _lock = lock();
        for scenario in 0..3 {
            let mut s = Storage::new(2);
            if scenario == 0 {
                s.text.truncate(3);
            }
            if scenario == 1 {
                s.slots.truncate(1);
            }
            if scenario == 2 {
                s.text.clear();
            }
            let mut arena = text::Builder::new(&mut s.text).unwrap();
            let mut b = Builder::new(
                &arena,
                &mut s.route,
                &mut s.domains,
                &mut s.aliases,
                &mut s.slots,
            )
            .unwrap();
            b.account(ACCOUNT, at(1)).unwrap();
            let e = if scenario == 0 {
                b.domain(
                    &mut arena,
                    "a.test",
                    Input {
                        mx_host: Some("mx.test"),
                        ..Input::default()
                    },
                    at(2),
                )
                .unwrap_err()
            } else if scenario == 1 {
                b.alias("x@a.test", ACCOUNT, at(2)).unwrap();
                b.domain(&mut arena, "b.test", Input::default(), at(3))
                    .unwrap_err()
            } else {
                b.domain(&mut arena, "a.test", Input::default(), at(2))
                    .unwrap();
                let e = b.finish(&mut arena, "mx.test", at(3)).unwrap_err();
                assert_eq!(e.code, Code::Text);
                continue;
            };
            assert_eq!(
                e.code,
                if scenario == 1 {
                    Code::Capacity
                } else {
                    Code::Text
                }
            );
            assert_eq!(b.finish(&mut arena, "mx.test", at(4)).unwrap_err(), e);
        }
    }
    #[test]
    fn exact_host_and_profile_boundaries_and_constructor_ceilings() {
        let _lock = lock();
        let host = format!(
            "{}.{}.{}.{}",
            "A".repeat(63),
            "B".repeat(63),
            "C".repeat(63),
            "D".repeat(51)
        );
        assert_eq!(host.len(), values::MAX_DOMAIN_BYTES);
        let profile = "p".repeat(values::MAX_PROFILE_BYTES);
        for scenario in 0..4 {
            let mut s = Storage::new(1);
            let mut arena = text::Builder::new(&mut s.text).unwrap();
            let mut b = Builder::new(
                &arena,
                &mut s.route,
                &mut s.domains,
                &mut s.aliases,
                &mut s.slots,
            )
            .unwrap();
            b.account(ACCOUNT, at(1)).unwrap();
            let too_long_host = host.clone() + "x";
            let too_long_profile = profile.clone() + "x";
            let result = b.domain(
                &mut arena,
                "a.test",
                Input {
                    mx_host: Some(if scenario == 1 { &too_long_host } else { &host }),
                    mode: Mode::None,
                    certificate: Some(if scenario == 2 {
                        &too_long_profile
                    } else {
                        &profile
                    }),
                    ..Input::default()
                },
                at(2),
            );
            if scenario == 1 || scenario == 2 {
                assert_eq!(
                    result.unwrap_err().code,
                    if scenario == 1 {
                        Code::DnsName
                    } else {
                        Code::Profile
                    }
                );
                continue;
            }
            result.unwrap();
            let result = b.finish(
                &mut arena,
                if scenario == 3 { &too_long_host } else { &host },
                at(3),
            );
            if scenario == 3 {
                assert_eq!(result.unwrap_err().code, Code::DnsName);
                continue;
            }
            let records = result.unwrap();
            let view = records.view(arena.freeze().unwrap()).unwrap();
            assert_eq!(view.hostname().unwrap(), host.to_ascii_lowercase());
            let row = view.domain(0).unwrap().unwrap();
            assert_eq!(row.mx_host, view.hostname().unwrap());
            assert!(row.explicit_mx);
            assert_eq!(
                view.domain(0).unwrap().unwrap().certificate,
                Some(profile.as_str())
            );
        }
        for size in [0, routing::MAX_DOMAINS + 1] {
            let mut s = Storage::new(1);
            s.slots = vec![Slot::EMPTY; size];
            let arena = text::Builder::new(&mut s.text).unwrap();
            assert_eq!(
                Builder::new(
                    &arena,
                    &mut s.route,
                    &mut s.domains,
                    &mut s.aliases,
                    &mut s.slots
                )
                .unwrap_err()
                .code,
                Code::Capacity
            );
        }
    }
    #[test]
    fn reused_policy_cells_reset_admission_flags_and_never_publish_old_fields() {
        let _lock = lock();
        let mut s = Storage::new(2);
        {
            let mut arena = text::Builder::new(&mut s.text).unwrap();
            let mut b = Builder::new(
                &arena,
                &mut s.route,
                &mut s.domains,
                &mut s.aliases,
                &mut s.slots,
            )
            .unwrap();
            b.account(ACCOUNT, at(1)).unwrap();
            for name in ["old-a.test", "old-b.test"] {
                b.domain(
                    &mut arena,
                    name,
                    Input {
                        mx_host: Some("old-mx.test"),
                        mode: Mode::Enforce,
                        certificate: Some("old-cert"),
                        ..Input::default()
                    },
                    at(2),
                )
                .unwrap();
            }
            b.finish(&mut arena, "old.test", at(3)).unwrap();
        }
        assert!(s.slots.iter().all(|slot| slot.present));
        {
            let mut arena = text::Builder::new(&mut s.text).unwrap();
            let mut b = Builder::new(
                &arena,
                &mut s.route,
                &mut s.domains,
                &mut s.aliases,
                &mut s.slots,
            )
            .unwrap();
            assert!(b.slots.iter().all(|slot| !slot.present));
            b.account(ACCOUNT, at(4)).unwrap();
            b.alias("x@unknown.test", ACCOUNT, at(5)).unwrap();
            b.domain(&mut arena, "new.test", Input::default(), at(6))
                .unwrap();
            let e = b.finish(&mut arena, "new-mx.test", at(7)).unwrap_err();
            assert_eq!(e.routing, Some(routing::Code::UnknownDomain));
        }
        let mut arena = text::Builder::new(&mut s.text).unwrap();
        let mut b = Builder::new(
            &arena,
            &mut s.route,
            &mut s.domains,
            &mut s.aliases,
            &mut s.slots,
        )
        .unwrap();
        assert!(b.slots.iter().all(|slot| !slot.present));
        b.account(ACCOUNT, at(8)).unwrap();
        b.domain(&mut arena, "new.test", Input::default(), at(9))
            .unwrap();
        let records = b.finish(&mut arena, "new-mx.test", at(10)).unwrap();
        let view = records.view(arena.freeze().unwrap()).unwrap();
        let row = view.domain(0).unwrap().unwrap();
        assert_eq!(row.name, "new.test");
        assert_eq!(row.mx_host, "new-mx.test");
        assert!(!row.explicit_mx);
        assert_eq!(row.mode, Mode::Off);
        assert_eq!(row.certificate, None);
        assert!(view.domain(1).unwrap().is_none());
    }
    #[test]
    fn redaction_and_partial_certificate_append_refusal_are_explicit() {
        let _lock = lock();
        let mut s = Storage::new(1);
        let mut arena = text::Builder::new(&mut s.text).unwrap();
        let mut b = Builder::new(
            &arena,
            &mut s.route,
            &mut s.domains,
            &mut s.aliases,
            &mut s.slots,
        )
        .unwrap();
        let input = Input {
            mx_host: Some("private-mx.test"),
            ..Input::default()
        };
        assert_eq!(format!("{input:?}"), "DomainPolicyInput(<redacted>)");
        assert_eq!(format!("{b:?}"), "DomainPolicyBuilder(<redacted>)");
        b.account(ACCOUNT, at(1)).unwrap();
        b.domain(&mut arena, "private-domain.test", input, at(2))
            .unwrap();
        let records = b.finish(&mut arena, "private-server.test", at(3)).unwrap();
        let view = records.view_live(&arena).unwrap();
        let row = view.domain(0).unwrap().unwrap();
        assert_eq!(
            format!("{records:?} {view:?} {row:?}"),
            "DomainPolicyRecords(<redacted>) DomainPolicyView(<redacted>) DomainPolicy(<redacted>)"
        );
        let mut s = Storage::new(1);
        let mut arena = text::Builder::new(&mut s.text).unwrap();
        let mut b = Builder::new(
            &arena,
            &mut s.route,
            &mut s.domains,
            &mut s.aliases,
            &mut s.slots,
        )
        .unwrap();
        let e = b
            .domain(
                &mut arena,
                "a.test",
                Input {
                    mx_host: Some("private..invalid"),
                    ..Input::default()
                },
                at(2),
            )
            .unwrap_err();
        assert_eq!(
            e.to_string(),
            "config_policy_dns_name at line 2, byte column 3"
        );
        assert!(!format!("{e:?}").contains("private"));
        assert!(std::error::Error::source(&e).is_none());
        let mut s = Storage::new(1);
        s.text.truncate("mx.test".len());
        let mut arena = text::Builder::new(&mut s.text).unwrap();
        let mut b = Builder::new(
            &arena,
            &mut s.route,
            &mut s.domains,
            &mut s.aliases,
            &mut s.slots,
        )
        .unwrap();
        let e = b
            .domain(
                &mut arena,
                "a.test",
                Input {
                    mx_host: Some("mx.test"),
                    mode: Mode::Testing,
                    certificate: Some("cert"),
                    ..Input::default()
                },
                at(2),
            )
            .unwrap_err();
        assert_eq!(e.code, Code::Text);
        assert_eq!(arena.used(), "mx.test".len());
        assert_eq!(b.finish(&mut arena, "server.test", at(3)).unwrap_err(), e);
    }
}

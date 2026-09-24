//! Structural resolver/relay settings; no DNS, authentication or network authority.
use super::{endpoint, syntax::Location, text, values};
use std::{fmt, net::SocketAddr, num::NonZeroU64};
pub const MAX_RESOLVERS: usize = 4;
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Transport {
    #[default]
    ImplicitTls,
    RequiredStartTls,
}
impl Transport {
    pub const fn name(self) -> &'static str {
        match self {
            Self::ImplicitTls => "implicit_tls",
            Self::RequiredStartTls => "required_starttls",
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Code {
    ForeignArena,
    Capacity,
    Profile,
    Endpoint,
    DuplicateResolverName,
    DuplicateResolverEndpoint,
    DuplicateRelay,
    MissingResolvers,
    MissingRelay,
    Hostname,
    Port,
    Username,
    PasswordPath,
    CaPath,
    Text,
    Invariant,
}
impl Code {
    pub const fn name(self) -> &'static str {
        match self {
            Self::ForeignArena => "config_outbound_foreign_arena",
            Self::Capacity => "config_outbound_capacity",
            Self::Profile => "config_outbound_profile",
            Self::Endpoint => "config_outbound_endpoint",
            Self::DuplicateResolverName => "config_outbound_duplicate_resolver_name",
            Self::DuplicateResolverEndpoint => "config_outbound_duplicate_resolver_endpoint",
            Self::DuplicateRelay => "config_outbound_duplicate_relay",
            Self::MissingResolvers => "config_outbound_missing_resolvers",
            Self::MissingRelay => "config_outbound_missing_relay",
            Self::Hostname => "config_outbound_hostname",
            Self::Port => "config_outbound_port",
            Self::Username => "config_outbound_username",
            Self::PasswordPath => "config_outbound_password_path",
            Self::CaPath => "config_outbound_ca_path",
            Self::Text => "config_outbound_text",
            Self::Invariant => "config_outbound_invariant",
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Error {
    pub code: Code,
    pub location: Option<Location>,
    pub previous: Option<Location>,
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.code.name())?;
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
    }
}
fn duplicate(code: Code, at: Location, previous: Location) -> Error {
    Error {
        code,
        location: Some(at),
        previous: Some(previous),
    }
}
fn invariant() -> Error {
    err(Code::Invariant, None)
}
#[derive(Clone, Copy)]
struct ResolverSlot {
    name: text::Span,
    endpoint: SocketAddr,
    location: Location,
}
#[derive(Clone, Copy)]
struct RelaySlot {
    host: text::Span,
    username: text::Span,
    password: text::Span,
    ca: text::Span,
    location: Location,
    port: u16,
    transport: Transport,
    has_ca: bool,
}
#[derive(Clone, Copy)]
pub struct RelayInput<'a> {
    pub host: &'a str,
    pub port: u64,
    pub transport: Transport,
    pub username: &'a str,
    pub password_file: &'a str,
    pub ca_file: Option<&'a str>,
}
impl fmt::Debug for RelayInput<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("RelayInput(<redacted>)")
    }
}
pub struct Builder {
    resolvers: [Option<ResolverSlot>; MAX_RESOLVERS],
    count: usize,
    relay: Option<RelaySlot>,
    owner: NonZeroU64,
    failure: Option<Error>,
}
const _: [(); 1] = [(); (std::mem::size_of::<Builder>() <= 1024) as usize];
const _: [(); 1] = [(); (std::mem::size_of::<Records>() <= 1024) as usize];
impl fmt::Debug for Builder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("OutboundBuilder(<redacted>)")
    }
}
impl Builder {
    pub fn new(arena: &text::Builder<'_>) -> Self {
        Self {
            resolvers: [None; MAX_RESOLVERS],
            count: 0,
            relay: None,
            owner: arena.owner(),
            failure: None,
        }
    }
    fn operation<T>(
        &mut self,
        arena: &mut text::Builder<'_>,
        at: Location,
        action: impl FnOnce(&mut Self, &mut text::Builder<'_>) -> Result<T, Error>,
    ) -> Result<T, Error> {
        if let Some(e) = self.failure {
            return Err(e);
        }
        let result = if self.owner != arena.owner() {
            Err(err(Code::ForeignArena, Some(at)))
        } else {
            action(self, arena)
        };
        if let Err(e) = result {
            self.failure = Some(e);
        }
        result
    }
    pub fn resolver(
        &mut self,
        arena: &mut text::Builder<'_>,
        name: &str,
        address: &str,
        at: Location,
    ) -> Result<(), Error> {
        self.operation(arena, at, |this, arena| {
            values::profile_name(name).map_err(|_| err(Code::Profile, Some(at)))?;
            let address =
                endpoint::numeric_endpoint(address).map_err(|_| err(Code::Endpoint, Some(at)))?;
            let ip = address.ip();
            let invalid_destination = ip.is_unspecified()
                || ip.is_multicast()
                || match ip {
                    std::net::IpAddr::V4(ip) => ip.is_broadcast(),
                    std::net::IpAddr::V6(ip) => ip.to_ipv4().is_some() && !ip.is_loopback(),
                };
            if invalid_destination {
                return Err(err(Code::Endpoint, Some(at)));
            }
            let view = arena.borrowed_view().map_err(|_| invariant())?;
            for slot in this.resolvers.get(..this.count).ok_or_else(invariant)? {
                let slot = slot.as_ref().ok_or_else(invariant)?;
                let stored = read(view, this.owner, slot.name)?;
                if stored == name {
                    return Err(duplicate(Code::DuplicateResolverName, at, slot.location));
                }
                if slot.endpoint == address {
                    return Err(duplicate(
                        Code::DuplicateResolverEndpoint,
                        at,
                        slot.location,
                    ));
                }
            }
            if this.count >= MAX_RESOLVERS {
                return Err(err(Code::Capacity, Some(at)));
            }
            let next = this.count.checked_add(1).ok_or_else(invariant)?;
            let name = store(arena, name, at)?;
            *this.resolvers.get_mut(this.count).ok_or_else(invariant)? = Some(ResolverSlot {
                name,
                endpoint: address,
                location: at,
            });
            this.count = next;
            Ok(())
        })
    }
    pub fn relay(
        &mut self,
        arena: &mut text::Builder<'_>,
        input: RelayInput<'_>,
        at: Location,
    ) -> Result<(), Error> {
        self.operation(arena, at, |this, arena| {
            if let Some(old) = this.relay {
                return Err(duplicate(Code::DuplicateRelay, at, old.location));
            }
            values::dns_name(input.host).map_err(|_| err(Code::Hostname, Some(at)))?;
            let port = u16::try_from(input.port)
                .ok()
                .filter(|port| *port != 0)
                .ok_or_else(|| err(Code::Port, Some(at)))?;
            if input.username.is_empty()
                || input.username.len() > 254
                || !input.username.bytes().all(|b| (32..=126).contains(&b))
            {
                return Err(err(Code::Username, Some(at)));
            }
            values::absolute_path(input.password_file)
                .map_err(|_| err(Code::PasswordPath, Some(at)))?;
            if let Some(ca) = input.ca_file {
                values::absolute_path(ca).map_err(|_| err(Code::CaPath, Some(at)))?;
            }
            let host = arena
                .append_dns(input.host)
                .map_err(|_| err(Code::Text, Some(at)))?;
            let host = arena.compact(host).map_err(|_| invariant())?;
            let username = store(arena, input.username, at)?;
            let password = store(arena, input.password_file, at)?;
            let ca = match input.ca_file {
                Some(ca) => store(arena, ca, at)?,
                None => text::Span::EMPTY,
            };
            this.relay = Some(RelaySlot {
                host,
                username,
                password,
                ca,
                location: at,
                port,
                transport: input.transport,
                has_ca: input.ca_file.is_some(),
            });
            Ok(())
        })
    }
    pub fn finish(self) -> Result<Records, Error> {
        if let Some(e) = self.failure {
            return Err(e);
        }
        if self.count == 0 {
            return Err(err(Code::MissingResolvers, None));
        }
        let relay = self.relay.ok_or_else(|| err(Code::MissingRelay, None))?;
        Ok(Records {
            resolvers: self.resolvers,
            count: self.count,
            relay,
            owner: self.owner,
        })
    }
}
fn store(arena: &mut text::Builder<'_>, input: &str, at: Location) -> Result<text::Span, Error> {
    let handle = arena
        .append(input.as_bytes())
        .map_err(|_| err(Code::Text, Some(at)))?;
    arena.compact(handle).map_err(|_| invariant())
}
pub struct Records {
    resolvers: [Option<ResolverSlot>; MAX_RESOLVERS],
    count: usize,
    relay: RelaySlot,
    owner: NonZeroU64,
}
impl fmt::Debug for Records {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("OutboundRecords(<redacted>)")
    }
}
impl Records {
    pub fn view_live<'s, 't>(&'s self, text: &'t text::Builder<'_>) -> Result<View<'s, 't>, Error> {
        self.view(text.borrowed_view().map_err(|_| invariant())?)
    }
    pub fn view<'s, 't>(&'s self, text: text::View<'t>) -> Result<View<'s, 't>, Error> {
        if self.owner != text.owner() {
            return Err(err(Code::ForeignArena, None));
        }
        Ok(View {
            records: self,
            text,
        })
    }
}
#[derive(Clone, Copy)]
pub struct View<'s, 't> {
    records: &'s Records,
    text: text::View<'t>,
}
impl fmt::Debug for View<'_, '_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("OutboundView(<redacted>)")
    }
}
#[derive(Clone, Copy)]
pub struct Resolver<'t> {
    pub location: Location,
    pub name: &'t str,
    pub endpoint: SocketAddr,
}
impl fmt::Debug for Resolver<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Resolver(<redacted>)")
    }
}
#[derive(Clone, Copy)]
pub struct Relay<'t> {
    pub location: Location,
    pub host: &'t str,
    pub port: u16,
    pub transport: Transport,
    pub username: &'t str,
    pub password_file: &'t str,
    pub ca_file: Option<&'t str>,
}
impl fmt::Debug for Relay<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Relay(<redacted>)")
    }
}
impl<'t> View<'_, 't> {
    pub fn resolver_count(self) -> usize {
        self.records.count
    }
    pub fn resolver(self, index: usize) -> Result<Option<Resolver<'t>>, Error> {
        if index >= self.records.count {
            return Ok(None);
        }
        let slot = self
            .records
            .resolvers
            .get(index)
            .and_then(Option::as_ref)
            .ok_or_else(invariant)?;
        Ok(Some(Resolver {
            location: slot.location,
            name: read(self.text, self.records.owner, slot.name)?,
            endpoint: slot.endpoint,
        }))
    }
    pub fn relay(self) -> Result<Relay<'t>, Error> {
        let slot = &self.records.relay;
        let owner = self.records.owner;
        Ok(Relay {
            location: slot.location,
            host: read(self.text, owner, slot.host)?,
            port: slot.port,
            transport: slot.transport,
            username: read(self.text, owner, slot.username)?,
            password_file: read(self.text, owner, slot.password)?,
            ca_file: if slot.has_ca {
                Some(read(self.text, owner, slot.ca)?)
            } else {
                None
            },
        })
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
    fn relay() -> RelayInput<'static> {
        RelayInput {
            host: "Relay.Example.test",
            port: 465,
            transport: Transport::default(),
            username: " User:Case ",
            password_file: "/does-not-exist/Password",
            ca_file: None,
        }
    }
    fn complete(arena: &mut text::Builder<'_>) -> Records {
        let mut b = Builder::new(arena);
        b.resolver(arena, "local", "127.0.0.1:53", at(1)).unwrap();
        b.relay(arena, relay(), at(2)).unwrap();
        b.finish().unwrap()
    }
    #[test]
    fn order_case_transport_and_optional_paths_survive_live_and_frozen_views() {
        let _lock = lock();
        for transport in [Transport::ImplicitTls, Transport::RequiredStartTls] {
            let mut bytes = vec![0; text::MAX_BYTES];
            let mut arena = text::Builder::new(&mut bytes).unwrap();
            let mut b = Builder::new(&arena);
            for (name, address) in [
                ("second", "[::1]:53"),
                ("first", "127.0.0.1:5353"),
                ("third", "192.0.2.1:53"),
                ("fourth", "[2001:db8::1]:65535"),
            ] {
                b.resolver(&mut arena, name, address, at(1)).unwrap();
            }
            let mut input = relay();
            input.transport = transport;
            input.ca_file =
                (transport == Transport::RequiredStartTls).then_some("/does-not-exist/Trust");
            b.relay(&mut arena, input, at(2)).unwrap();
            let records = b.finish().unwrap();
            {
                let view = records.view_live(&arena).unwrap();
                assert_eq!(view.resolver_count(), 4);
                let names: Vec<_> = (0..4)
                    .map(|i| view.resolver(i).unwrap().unwrap().name)
                    .collect();
                assert_eq!(names, ["second", "first", "third", "fourth"]);
                for (i, expected) in [
                    "[::1]:53",
                    "127.0.0.1:5353",
                    "192.0.2.1:53",
                    "[2001:db8::1]:65535",
                ]
                .iter()
                .enumerate()
                {
                    let resolver = view.resolver(i).unwrap().unwrap();
                    assert_eq!(resolver.endpoint, expected.parse().unwrap());
                    assert_eq!(resolver.location, at(1));
                }
                assert_eq!(
                    view.resolver(0).unwrap().unwrap().endpoint,
                    "[::1]:53".parse().unwrap()
                );
                assert!(view.resolver(4).unwrap().is_none());
                assert!(view.resolver(usize::MAX).unwrap().is_none());
                let r = view.relay().unwrap();
                assert_eq!(r.location, at(2));
                assert_eq!(
                    r.transport.name(),
                    match transport {
                        Transport::ImplicitTls => "implicit_tls",
                        Transport::RequiredStartTls => "required_starttls",
                    }
                );
                assert_eq!(r.host, "relay.example.test");
                assert_eq!(r.port, 465);
                assert_eq!(r.transport, transport);
                assert_eq!(r.username, " User:Case ");
                assert_eq!(r.password_file, "/does-not-exist/Password");
                assert_eq!(
                    r.ca_file,
                    (transport == Transport::RequiredStartTls).then_some("/does-not-exist/Trust")
                );
            }
            arena.append(b"later protected file bytes").unwrap();
            assert_eq!(
                records
                    .view(arena.freeze().unwrap())
                    .unwrap()
                    .relay()
                    .unwrap()
                    .host,
                "relay.example.test"
            );
        }
    }
    #[test]
    fn singleton_and_binary_endpoint_duplicates_report_both_locations_and_poison() {
        let _lock = lock();
        for (name, endpoint, code) in [
            ("same", "127.0.0.1:54", Code::DuplicateResolverName),
            (
                "other",
                "[0:0:0:0:0:0:0:1]:53",
                Code::DuplicateResolverEndpoint,
            ),
        ] {
            let mut bytes = [0; 2048];
            let mut arena = text::Builder::new(&mut bytes).unwrap();
            let mut b = Builder::new(&arena);
            b.resolver(&mut arena, "same", "[::1]:53", at(1)).unwrap();
            let e = b.resolver(&mut arena, name, endpoint, at(3)).unwrap_err();
            assert_eq!(
                (e.code, e.location, e.previous),
                (code, Some(at(3)), Some(at(1)))
            );
            assert_eq!(b.relay(&mut arena, relay(), at(4)).unwrap_err(), e);
            assert_eq!(b.finish().unwrap_err(), e);
        }
        let mut bytes = [0; 2048];
        let mut arena = text::Builder::new(&mut bytes).unwrap();
        let mut b = Builder::new(&arena);
        b.relay(&mut arena, relay(), at(2)).unwrap();
        let e = b.relay(&mut arena, relay(), at(5)).unwrap_err();
        assert_eq!(
            (e.code, e.location, e.previous),
            (Code::DuplicateRelay, Some(at(5)), Some(at(2)))
        );
        assert_eq!(e.to_string(), "config_outbound_duplicate_relay at line 5, byte column 3 (previously declared at line 2, byte column 3)");
        assert_eq!(b.finish().unwrap_err(), e);
    }
    #[test]
    fn missing_sections_capacity_and_invalid_resolvers_never_produce_records() {
        let _lock = lock();
        let mut bytes = [0; 2048];
        let mut arena = text::Builder::new(&mut bytes).unwrap();
        assert_eq!(
            Builder::new(&arena).finish().unwrap_err().code,
            Code::MissingResolvers
        );
        let mut b = Builder::new(&arena);
        b.resolver(&mut arena, "first", "127.0.0.1:53", at(1))
            .unwrap();
        assert_eq!(b.finish().unwrap_err().code, Code::MissingRelay);
        let mut b = Builder::new(&arena);
        b.relay(&mut arena, relay(), at(1)).unwrap();
        assert_eq!(b.finish().unwrap_err().code, Code::MissingResolvers);
        let mut b = Builder::new(&arena);
        for n in 1..=4 {
            b.resolver(
                &mut arena,
                &format!("r{n}"),
                &format!("127.0.0.1:{n}"),
                at(n),
            )
            .unwrap();
        }
        assert_eq!(
            b.resolver(&mut arena, "fifth", "127.0.0.1:5", at(5))
                .unwrap_err()
                .code,
            Code::Capacity
        );
        assert_eq!(b.finish().unwrap_err().code, Code::Capacity);
        for (name, address, code) in [
            ("Upper", "127.0.0.1:53", Code::Profile),
            ("r", "dns.example.test:53", Code::Endpoint),
            ("r", "127.0.0.1:0", Code::Endpoint),
            ("r", "127.0.0.1:053", Code::Endpoint),
            ("r", "[::1%1]:53", Code::Endpoint),
            ("r", "[::ffff:192.0.2.1]:53", Code::Endpoint),
        ] {
            let mut b = Builder::new(&arena);
            let e = b.resolver(&mut arena, name, address, at(6)).unwrap_err();
            assert_eq!(e.code, code);
            assert_eq!(b.finish().unwrap_err(), e);
        }
    }
    #[test]
    fn destination_rules_and_later_duplicate_entries_are_checked() {
        let _lock = lock();
        for endpoint in [
            "0.0.0.0:53",
            "[::]:53",
            "255.255.255.255:53",
            "224.0.0.1:53",
            "[ff02::1]:53",
            "[::127.0.0.1]:53",
        ] {
            let mut bytes = [0; 2048];
            let mut arena = text::Builder::new(&mut bytes).unwrap();
            let mut b = Builder::new(&arena);
            let e = b.resolver(&mut arena, "r", endpoint, at(4)).unwrap_err();
            assert_eq!(e.code, Code::Endpoint);
            assert_eq!(arena.used(), 0);
            assert_eq!(b.finish().unwrap_err(), e);
        }
        for (name, endpoint, code) in [
            ("second", "127.0.0.1:55", Code::DuplicateResolverName),
            ("third", "127.0.0.1:54", Code::DuplicateResolverEndpoint),
        ] {
            let mut bytes = [0; 2048];
            let mut arena = text::Builder::new(&mut bytes).unwrap();
            let mut b = Builder::new(&arena);
            b.resolver(&mut arena, "first", "127.0.0.1:53", at(1))
                .unwrap();
            b.resolver(&mut arena, "second", "127.0.0.1:54", at(2))
                .unwrap();
            let e = b.resolver(&mut arena, name, endpoint, at(3)).unwrap_err();
            assert_eq!((e.code, e.previous), (code, Some(at(2))));
        }
    }
    #[test]
    fn relay_exact_bounds_and_printable_username_are_distinct_from_account_rules() {
        let _lock = lock();
        let host = [
            "a".repeat(63),
            "b".repeat(63),
            "c".repeat(63),
            "d".repeat(51),
        ]
        .join(".");
        assert_eq!(host.len(), 243);
        let user = format!(" :{}", "A".repeat(252));
        assert_eq!(user.len(), 254);
        let path = format!("/{}", "p".repeat(4094));
        for port in [1, 65535] {
            let mut bytes = vec![0; 16384];
            let mut arena = text::Builder::new(&mut bytes).unwrap();
            let mut b = Builder::new(&arena);
            b.resolver(&mut arena, "r", "127.0.0.1:53", at(1)).unwrap();
            b.relay(
                &mut arena,
                RelayInput {
                    host: &host,
                    port,
                    username: &user,
                    password_file: &path,
                    ca_file: Some("/"),
                    ..relay()
                },
                at(2),
            )
            .unwrap();
            let records = b.finish().unwrap();
            let r = records.view_live(&arena).unwrap().relay().unwrap();
            assert_eq!(r.host, host);
            assert_eq!(r.username, user);
            assert_eq!(r.password_file, path);
            assert_eq!(r.ca_file, Some("/"));
            assert_eq!(u64::from(r.port), port);
        }
        for (field, bad, code) in [
            ("host", format!("{host}d"), Code::Hostname),
            ("host", "1.2.3.4".into(), Code::Hostname),
            ("username", format!("{user}A"), Code::Username),
            ("username", "".into(), Code::Username),
            ("username", "x\ny".into(), Code::Username),
            ("username", "x\u{7f}".into(), Code::Username),
            ("username", "é".into(), Code::Username),
            ("password", format!("{path}p"), Code::PasswordPath),
            ("password", "/x/../y".into(), Code::PasswordPath),
            ("password", "relative".into(), Code::PasswordPath),
            ("ca", "/x//y".into(), Code::CaPath),
        ] {
            let mut bytes = vec![0; 16384];
            let mut arena = text::Builder::new(&mut bytes).unwrap();
            let mut b = Builder::new(&arena);
            let mut input = relay();
            match field {
                "host" => input.host = &bad,
                "username" => input.username = &bad,
                "password" => input.password_file = &bad,
                _ => input.ca_file = Some(&bad),
            }
            let used = arena.used();
            let e = b.relay(&mut arena, input, at(4)).unwrap_err();
            assert_eq!(arena.used(), used);
            assert_eq!(e.code, code);
            assert_eq!(b.finish().unwrap_err(), e);
        }
        for port in [0, 65536, u64::MAX] {
            let mut bytes = [0; 2048];
            let mut arena = text::Builder::new(&mut bytes).unwrap();
            let mut b = Builder::new(&arena);
            assert_eq!(
                b.relay(&mut arena, RelayInput { port, ..relay() }, at(1))
                    .unwrap_err()
                    .code,
                Code::Port
            );
        }
    }
    #[test]
    fn foreign_arenas_and_reused_backing_cannot_read_or_extend_records() {
        let _lock = lock();
        let mut bytes = [0; 2048];
        let mut other = [0; 2048];
        let mut arena = text::Builder::new(&mut bytes).unwrap();
        let mut foreign = text::Builder::new(&mut other).unwrap();
        let records = complete(&mut arena);
        assert_eq!(
            records.view_live(&foreign).unwrap_err().code,
            Code::ForeignArena
        );
        let mut b = Builder::new(&arena);
        let e = b
            .resolver(&mut foreign, "r", "127.0.0.1:53", at(8))
            .unwrap_err();
        assert_eq!(e.code, Code::ForeignArena);
        assert_eq!(b.finish().unwrap_err(), e);
        let mut b = Builder::new(&arena);
        let e = b.relay(&mut foreign, relay(), at(9)).unwrap_err();
        assert_eq!(e.code, Code::ForeignArena);
        assert_eq!(b.relay(&mut arena, relay(), at(10)).unwrap_err(), e);
        assert_eq!(
            records.view(foreign.freeze().unwrap()).unwrap_err().code,
            Code::ForeignArena
        );
        let _ = arena.freeze().unwrap();
        let reused = text::Builder::new(&mut bytes).unwrap();
        assert_eq!(
            records.view_live(&reused).unwrap_err().code,
            Code::ForeignArena
        );
    }
    #[test]
    fn partial_append_exhaustion_is_sticky_and_never_finalizes() {
        let _lock = lock();
        let mut bytes = [0; 30];
        let mut arena = text::Builder::new(&mut bytes).unwrap();
        let mut b = Builder::new(&arena);
        b.resolver(&mut arena, "r", "127.0.0.1:53", at(1)).unwrap();
        let e = b.relay(&mut arena, relay(), at(2)).unwrap_err();
        assert_eq!(e.code, Code::Text);
        assert!(arena.used() > 1);
        assert_eq!(
            b.resolver(&mut arena, "s", "127.0.0.1:54", at(3))
                .unwrap_err(),
            e
        );
        assert_eq!(b.finish().unwrap_err(), e);
    }
    #[test]
    fn layout_and_diagnostics_do_not_reveal_values() {
        let _lock = lock();
        assert!(std::mem::size_of::<Builder>() <= 1024);
        assert!(std::mem::size_of::<Records>() <= 1024);
        let mut bytes = [0; 2048];
        let mut arena = text::Builder::new(&mut bytes).unwrap();
        let b = Builder::new(&arena);
        assert_eq!(format!("{b:?}"), "OutboundBuilder(<redacted>)");
        assert_eq!(format!("{:?}", relay()), "RelayInput(<redacted>)");
        let records = complete(&mut arena);
        let view = records.view_live(&arena).unwrap();
        assert_eq!(format!("{records:?}"), "OutboundRecords(<redacted>)");
        assert_eq!(format!("{view:?}"), "OutboundView(<redacted>)");
        assert_eq!(format!("{:?}", view.relay().unwrap()), "Relay(<redacted>)");
        assert_eq!(
            format!("{:?}", view.resolver(0).unwrap().unwrap()),
            "Resolver(<redacted>)"
        );
        let mut b = Builder::new(&arena);
        let e = b
            .relay(
                &mut arena,
                RelayInput {
                    username: "private\nvalue",
                    ..relay()
                },
                at(4),
            )
            .unwrap_err();
        assert!(!format!("{e:?} {e}").contains("private"));
        assert!(std::error::Error::source(&e).is_none());
    }
}

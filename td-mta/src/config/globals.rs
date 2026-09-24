//! Global policy and lexical roots; no file, DNS or publication authority.
use super::{syntax::Location, text, values};
use crate::{admission::ViewMode, observability::Severity};
use std::{
    fmt,
    net::{Ipv4Addr, Ipv6Addr},
    num::NonZeroU64,
};

pub const DEFAULT_DATA: &str = "/var/lib/td-mta";
pub const DEFAULT_RUNTIME: &str = "/run/td-mta";
pub const DEFAULT_LOGS: &str = "/var/log/td-mta";
pub const DEFAULT_SEVERITY: Severity = Severity::Info;
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Field {
    PublicIpv4,
    PublicIpv6,
    Data,
    Runtime,
    Logs,
    MinimumSeverity,
}
impl Field {
    pub const fn name(self) -> &'static str {
        match self {
            Self::PublicIpv4 => "public_ipv4",
            Self::PublicIpv6 => "public_ipv6",
            Self::Data => "data",
            Self::Runtime => "runtime",
            Self::Logs => "logs",
            Self::MinimumSeverity => "minimum_severity",
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Code {
    ForeignArena,
    DuplicateServer,
    DuplicatePaths,
    DuplicateLogging,
    MissingServer,
    Address,
    Path,
    Overlap,
    Severity,
    Text,
    Invariant,
}
impl Code {
    pub const fn name(self) -> &'static str {
        match self {
            Self::ForeignArena => "config_globals_foreign_arena",
            Self::DuplicateServer => "config_globals_duplicate_server",
            Self::DuplicatePaths => "config_globals_duplicate_paths",
            Self::DuplicateLogging => "config_globals_duplicate_logging",
            Self::MissingServer => "config_globals_missing_server",
            Self::Address => "config_globals_address",
            Self::Path => "config_globals_path",
            Self::Overlap => "config_globals_overlap",
            Self::Severity => "config_globals_severity",
            Self::Text => "config_globals_text",
            Self::Invariant => "config_globals_invariant",
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Error {
    pub code: Code,
    pub field: Option<Field>,
    pub related_field: Option<Field>,
    pub location: Option<Location>,
    pub previous: Option<Location>,
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.code.name())?;
        if let Some(field) = self.field {
            write!(f, " for {}", field.name())?;
        }
        if let Some(field) = self.related_field {
            write!(f, " overlapping {}", field.name())?;
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
fn error(code: Code, field: Option<Field>, location: Option<Location>) -> Error {
    Error {
        code,
        field,
        related_field: None,
        location,
        previous: None,
    }
}
fn invariant() -> Error {
    error(Code::Invariant, None, None)
}
#[derive(Clone, Copy)]
pub struct ServerInput<'a> {
    pub online_background: bool,
    pub public_ipv4: Option<&'a str>,
    pub public_ipv6: Option<&'a str>,
}
impl Default for ServerInput<'_> {
    fn default() -> Self {
        Self {
            online_background: true,
            public_ipv4: None,
            public_ipv6: None,
        }
    }
}
impl fmt::Debug for ServerInput<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ServerOptionsInput(<redacted>)")
    }
}
#[derive(Clone, Copy, Default)]
pub struct PathsInput<'a> {
    pub data: Option<&'a str>,
    pub runtime: Option<&'a str>,
    pub logs: Option<&'a str>,
}
impl fmt::Debug for PathsInput<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PathsInput(<redacted>)")
    }
}
#[derive(Clone, Copy)]
pub struct Server {
    pub view_mode: ViewMode,
    pub public_ipv4: Option<Ipv4Addr>,
    pub public_ipv6: Option<Ipv6Addr>,
    pub location: Location,
}
impl fmt::Debug for Server {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ServerOptions(<redacted>)")
    }
}
#[derive(Clone, Copy)]
struct Roots {
    data: text::Span,
    runtime: text::Span,
    logs: text::Span,
    location: Option<Location>,
}
pub struct Builder {
    server: Option<Server>,
    roots: Option<Roots>,
    logging: Option<(Severity, Location)>,
    owner: NonZeroU64,
    failure: Option<Error>,
}
const _: [(); 1] = [(); (std::mem::size_of::<Builder>() <= 256) as usize];
impl fmt::Debug for Builder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("GlobalSettingsBuilder(<redacted>)")
    }
}
impl Builder {
    pub fn new(arena: &text::Builder<'_>) -> Self {
        Self {
            server: None,
            roots: None,
            logging: None,
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
            Err(error(Code::ForeignArena, None, Some(at)))
        } else {
            action(self, arena)
        };
        if let Err(e) = result {
            self.failure = Some(e);
        }
        result
    }
    /// Hostname/origin are separately staged for policy/graph canonical storage.
    pub fn server(
        &mut self,
        arena: &mut text::Builder<'_>,
        input: ServerInput<'_>,
        at: Location,
    ) -> Result<(), Error> {
        self.operation(arena, at, |this, _| {
            if let Some(old) = this.server {
                return Err(duplicate(Code::DuplicateServer, at, old.location));
            }
            let public_ipv4: Option<Ipv4Addr> = match input.public_ipv4 {
                Some(s) if s.len() <= 15 => Some(
                    s.parse()
                        .map_err(|_| error(Code::Address, Some(Field::PublicIpv4), Some(at)))?,
                ),
                Some(_) => return Err(error(Code::Address, Some(Field::PublicIpv4), Some(at))),
                None => None,
            };
            let public_ipv6: Option<Ipv6Addr> = match input.public_ipv6 {
                Some(s) if s.len() <= 45 => Some(
                    s.parse()
                        .map_err(|_| error(Code::Address, Some(Field::PublicIpv6), Some(at)))?,
                ),
                Some(_) => return Err(error(Code::Address, Some(Field::PublicIpv6), Some(at))),
                None => None,
            };
            if public_ipv4
                .is_some_and(|ip| ip.is_unspecified() || ip.is_multicast() || ip.is_broadcast())
            {
                return Err(error(Code::Address, Some(Field::PublicIpv4), Some(at)));
            }
            if public_ipv6.is_some_and(|ip| {
                ip.is_unspecified()
                    || ip.is_multicast()
                    || (ip.to_ipv4().is_some() && !ip.is_loopback())
            }) {
                return Err(error(Code::Address, Some(Field::PublicIpv6), Some(at)));
            }
            this.server = Some(Server {
                view_mode: if input.online_background {
                    ViewMode::OnlineBackground
                } else {
                    ViewMode::ForegroundOnly
                },
                public_ipv4,
                public_ipv6,
                location: at,
            });
            Ok(())
        })
    }
    pub fn paths(
        &mut self,
        arena: &mut text::Builder<'_>,
        input: PathsInput<'_>,
        at: Location,
    ) -> Result<(), Error> {
        self.operation(arena, at, |this, arena| {
            if let Some(old) = this.roots {
                return Err(duplicate(
                    Code::DuplicatePaths,
                    at,
                    old.location.ok_or_else(invariant)?,
                ));
            }
            this.roots = Some(roots(arena, input, Some(at))?);
            Ok(())
        })
    }
    pub fn logging(
        &mut self,
        arena: &mut text::Builder<'_>,
        severity: Option<&str>,
        at: Location,
    ) -> Result<(), Error> {
        self.operation(arena, at, |this, _| {
            if let Some((_, previous)) = this.logging {
                return Err(duplicate(Code::DuplicateLogging, at, previous));
            }
            let severity = match severity {
                None => DEFAULT_SEVERITY,
                Some("info") => Severity::Info,
                Some("warning") => Severity::Warning,
                Some("error") => Severity::Error,
                _ => {
                    return Err(error(
                        Code::Severity,
                        Some(Field::MinimumSeverity),
                        Some(at),
                    ))
                }
            };
            this.logging = Some((severity, at));
            Ok(())
        })
    }
    pub fn finish(self, arena: &mut text::Builder<'_>) -> Result<Records, Error> {
        if let Some(e) = self.failure {
            return Err(e);
        }
        if self.owner != arena.owner() {
            return Err(error(Code::ForeignArena, None, None));
        }
        let server = self
            .server
            .ok_or_else(|| error(Code::MissingServer, None, None))?;
        let roots = match self.roots {
            Some(r) => r,
            None => roots(arena, PathsInput::default(), None)?,
        };
        Ok(Records {
            server,
            roots,
            severity: self
                .logging
                .map_or(DEFAULT_SEVERITY, |(severity, _)| severity),
            logging_location: self.logging.map(|(_, at)| at),
            owner: self.owner,
        })
    }
}
fn duplicate(code: Code, at: Location, previous: Location) -> Error {
    Error {
        previous: Some(previous),
        ..error(code, None, Some(at))
    }
}
fn roots(
    arena: &mut text::Builder<'_>,
    input: PathsInput<'_>,
    at: Option<Location>,
) -> Result<Roots, Error> {
    let data = input.data.unwrap_or(DEFAULT_DATA);
    let runtime = input.runtime.unwrap_or(DEFAULT_RUNTIME);
    let logs = input.logs.unwrap_or(DEFAULT_LOGS);
    for (field, path) in [
        (Field::Data, data),
        (Field::Runtime, runtime),
        (Field::Logs, logs),
    ] {
        values::absolute_path(path).map_err(|_| error(Code::Path, Some(field), at))?;
    }
    for (field, first, supplied, related, second, related_supplied) in [
        (
            Field::Runtime,
            runtime,
            input.runtime.is_some(),
            Field::Data,
            data,
            input.data.is_some(),
        ),
        (
            Field::Logs,
            logs,
            input.logs.is_some(),
            Field::Data,
            data,
            input.data.is_some(),
        ),
        (
            Field::Logs,
            logs,
            input.logs.is_some(),
            Field::Runtime,
            runtime,
            input.runtime.is_some(),
        ),
    ] {
        if values::paths_overlap(first, second).map_err(|_| invariant())? {
            let (field, related) = if !supplied && related_supplied {
                (related, field)
            } else {
                (field, related)
            };
            return Err(Error {
                related_field: Some(related),
                ..error(Code::Overlap, Some(field), at)
            });
        }
    }
    let data = store(arena, data, Field::Data, at)?;
    let runtime = store(arena, runtime, Field::Runtime, at)?;
    let logs = store(arena, logs, Field::Logs, at)?;
    Ok(Roots {
        data,
        runtime,
        logs,
        location: at,
    })
}
fn store(
    arena: &mut text::Builder<'_>,
    value: &str,
    field: Field,
    at: Option<Location>,
) -> Result<text::Span, Error> {
    let handle = arena
        .append(value.as_bytes())
        .map_err(|_| error(Code::Text, Some(field), at))?;
    arena.compact(handle).map_err(|_| invariant())
}
pub struct Records {
    server: Server,
    roots: Roots,
    severity: Severity,
    logging_location: Option<Location>,
    owner: NonZeroU64,
}
const _: [(); 1] = [(); (std::mem::size_of::<Records>() <= 128) as usize];
impl fmt::Debug for Records {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("GlobalSettingsRecords(<redacted>)")
    }
}
impl Records {
    pub fn server(&self) -> Server {
        self.server
    }
    pub fn minimum_severity(&self) -> Severity {
        self.severity
    }
    pub fn logging_location(&self) -> Option<Location> {
        self.logging_location
    }
    pub fn view_live<'s, 't>(
        &'s self,
        arena: &'t text::Builder<'_>,
    ) -> Result<View<'s, 't>, Error> {
        self.view(arena.borrowed_view().map_err(|_| invariant())?)
    }
    pub fn view<'s, 't>(&'s self, text: text::View<'t>) -> Result<View<'s, 't>, Error> {
        if self.owner != text.owner() {
            return Err(error(Code::ForeignArena, None, None));
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
        f.write_str("GlobalSettingsView(<redacted>)")
    }
}
impl<'t> View<'_, 't> {
    fn read(self, span: text::Span) -> Result<&'t str, Error> {
        std::str::from_utf8(
            self.text
                .read_span(self.records.owner, span)
                .map_err(|_| invariant())?,
        )
        .map_err(|_| invariant())
    }
    pub fn data(self) -> Result<&'t str, Error> {
        self.read(self.records.roots.data)
    }
    pub fn runtime(self) -> Result<&'t str, Error> {
        self.read(self.records.roots.runtime)
    }
    pub fn logs(self) -> Result<&'t str, Error> {
        self.read(self.records.roots.logs)
    }
    pub fn paths_location(self) -> Option<Location> {
        self.records.roots.location
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use std::num::NonZeroU32;
    fn at(n: u32) -> Location {
        Location {
            line: NonZeroU32::new(n).unwrap(),
            column: 2,
        }
    }
    fn lock() -> std::sync::MutexGuard<'static, ()> {
        text::TEST_CONSTRUCTION_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }
    #[test]
    fn defaults_explicit_options_and_frozen_views() {
        let _lock = lock();
        for explicit in [false, true] {
            let mut bytes = [0; 256];
            let mut arena = text::Builder::new(&mut bytes).unwrap();
            let mut b = Builder::new(&arena);
            b.server(
                &mut arena,
                ServerInput {
                    online_background: !explicit,
                    public_ipv4: explicit.then_some("192.0.2.7"),
                    public_ipv6: explicit.then_some("2001:DB8::7"),
                },
                at(1),
            )
            .unwrap();
            if explicit {
                b.paths(
                    &mut arena,
                    PathsInput {
                        data: Some("/data"),
                        runtime: Some("/data-other"),
                        logs: Some("/logs"),
                    },
                    at(2),
                )
                .unwrap();
                b.logging(&mut arena, Some("warning"), at(3)).unwrap();
            }
            let records = b.finish(&mut arena).unwrap();
            assert_eq!(
                records.server().view_mode,
                if explicit {
                    ViewMode::ForegroundOnly
                } else {
                    ViewMode::OnlineBackground
                }
            );
            assert_eq!(
                records.server().public_ipv4,
                explicit.then_some(Ipv4Addr::new(192, 0, 2, 7))
            );
            assert_eq!(
                records.server().public_ipv6,
                explicit.then_some("2001:db8::7".parse().unwrap())
            );
            assert_eq!(
                records.minimum_severity(),
                if explicit {
                    Severity::Warning
                } else {
                    Severity::Info
                }
            );
            assert_eq!(records.logging_location(), explicit.then_some(at(3)));
            let live = records.view_live(&arena).unwrap();
            assert_eq!(
                live.data().unwrap(),
                if explicit { "/data" } else { "/var/lib/td-mta" }
            );
            assert_eq!(live.paths_location(), explicit.then_some(at(2)));
            let frozen = records.view(arena.freeze().unwrap()).unwrap();
            assert_eq!(
                frozen.runtime().unwrap(),
                if explicit {
                    "/data-other"
                } else {
                    "/run/td-mta"
                }
            );
            assert_eq!(
                frozen.logs().unwrap(),
                if explicit { "/logs" } else { "/var/log/td-mta" }
            );
        }
    }
    #[test]
    fn address_hints_have_fixed_family_and_bounded_numeric_grammar() {
        let _lock = lock();
        for (field, value) in [
            (Field::PublicIpv4, "2001:db8::1"),
            (Field::PublicIpv4, "127.000.0.1"),
            (Field::PublicIpv4, "localhost"),
            (Field::PublicIpv4, "192.0.2.1:25"),
            (Field::PublicIpv6, "192.0.2.1"),
            (Field::PublicIpv6, "fe80::1%eth0"),
            (Field::PublicIpv6, "[::1]"),
            (Field::PublicIpv6, "localhost"),
        ] {
            let mut bytes = [0; 256];
            let mut arena = text::Builder::new(&mut bytes).unwrap();
            let mut b = Builder::new(&arena);
            let input = if field == Field::PublicIpv4 {
                ServerInput {
                    public_ipv4: Some(value),
                    ..Default::default()
                }
            } else {
                ServerInput {
                    public_ipv6: Some(value),
                    ..Default::default()
                }
            };
            let e = b.server(&mut arena, input, at(1)).unwrap_err();
            assert_eq!(e, error(Code::Address, Some(field), Some(at(1))));
            assert_eq!(b.finish(&mut arena).unwrap_err(), e);
        }
        let mut bytes = [0; 256];
        let mut arena = text::Builder::new(&mut bytes).unwrap();
        let mut b = Builder::new(&arena);
        // Publication hints are lexical values, not proof of public reachability.
        b.server(
            &mut arena,
            ServerInput {
                public_ipv4: Some("127.0.0.1"),
                public_ipv6: Some("::1"),
                ..Default::default()
            },
            at(1),
        )
        .unwrap();
        let r = b.finish(&mut arena).unwrap();
        assert!(r.server().public_ipv4.unwrap().is_loopback());
        assert!(r.server().public_ipv6.unwrap().is_loopback());
    }
    #[test]
    fn roots_are_lexical_and_pairwise_disjoint_before_appending() {
        let _lock = lock();
        for (paths, field, related) in [
            (
                PathsInput {
                    data: Some("/a"),
                    runtime: Some("/a"),
                    logs: Some("/logs"),
                },
                Field::Runtime,
                Some(Field::Data),
            ),
            (
                PathsInput {
                    data: Some("/a/b"),
                    runtime: Some("/a"),
                    logs: Some("/logs"),
                },
                Field::Runtime,
                Some(Field::Data),
            ),
            (
                PathsInput {
                    data: Some("/a"),
                    runtime: Some("/r"),
                    logs: Some("/a/b"),
                },
                Field::Logs,
                Some(Field::Data),
            ),
            (
                PathsInput {
                    data: Some("/a"),
                    runtime: Some("/r"),
                    logs: Some("/r/b"),
                },
                Field::Logs,
                Some(Field::Runtime),
            ),
            (
                PathsInput {
                    data: Some("/"),
                    ..Default::default()
                },
                Field::Data,
                Some(Field::Runtime),
            ),
            (
                PathsInput {
                    runtime: Some("relative"),
                    ..Default::default()
                },
                Field::Runtime,
                None,
            ),
            (
                PathsInput {
                    data: Some("/a/../b"),
                    ..Default::default()
                },
                Field::Data,
                None,
            ),
            (
                PathsInput {
                    logs: Some("/a//b"),
                    ..Default::default()
                },
                Field::Logs,
                None,
            ),
        ] {
            let mut bytes = [0; 256];
            let mut arena = text::Builder::new(&mut bytes).unwrap();
            let mut b = Builder::new(&arena);
            let e = b.paths(&mut arena, paths, at(2)).unwrap_err();
            assert_eq!(
                e.code,
                if related.is_some() {
                    Code::Overlap
                } else {
                    Code::Path
                }
            );
            assert_eq!(e.field, Some(field));
            assert_eq!(e.related_field, related);
            assert_eq!(arena.used(), 0);
            assert_eq!(b.finish(&mut arena).unwrap_err(), e);
        }
        let path = format!("/{}", "x".repeat(values::MAX_PATH_BYTES - 1));
        let mut bytes = vec![0; 3 * values::MAX_PATH_BYTES];
        let mut arena = text::Builder::new(&mut bytes).unwrap();
        let mut b = Builder::new(&arena);
        b.server(&mut arena, ServerInput::default(), at(1)).unwrap();
        b.paths(
            &mut arena,
            PathsInput {
                data: Some(&path),
                ..Default::default()
            },
            at(2),
        )
        .unwrap();
        assert_eq!(
            b.finish(&mut arena)
                .unwrap()
                .view_live(&arena)
                .unwrap()
                .data()
                .unwrap(),
            path
        );
    }
    #[test]
    fn duplicate_sections_precede_fields_and_failures_stay_sticky() {
        let _lock = lock();
        for kind in 0..3 {
            let mut bytes = [0; 256];
            let mut arena = text::Builder::new(&mut bytes).unwrap();
            let mut b = Builder::new(&arena);
            let e = match kind {
                0 => {
                    b.server(&mut arena, ServerInput::default(), at(1)).unwrap();
                    b.server(
                        &mut arena,
                        ServerInput {
                            public_ipv4: Some("bad"),
                            ..Default::default()
                        },
                        at(2),
                    )
                    .unwrap_err()
                }
                1 => {
                    b.paths(&mut arena, PathsInput::default(), at(1)).unwrap();
                    b.paths(
                        &mut arena,
                        PathsInput {
                            data: Some("bad"),
                            ..Default::default()
                        },
                        at(2),
                    )
                    .unwrap_err()
                }
                _ => {
                    b.logging(&mut arena, Some("error"), at(1)).unwrap();
                    b.logging(&mut arena, Some("bad"), at(2)).unwrap_err()
                }
            };
            assert_eq!(
                e,
                duplicate(
                    [
                        Code::DuplicateServer,
                        Code::DuplicatePaths,
                        Code::DuplicateLogging
                    ][kind],
                    at(2),
                    at(1)
                )
            );
            assert_eq!(b.logging(&mut arena, Some("info"), at(3)).unwrap_err(), e);
            assert_eq!(b.finish(&mut arena).unwrap_err(), e);
        }
        let mut bytes = [0; 256];
        let mut arena = text::Builder::new(&mut bytes).unwrap();
        assert_eq!(
            Builder::new(&arena).finish(&mut arena).unwrap_err().code,
            Code::MissingServer
        );
        for value in ["Info", "debug", "", "warning "] {
            let mut b = Builder::new(&arena);
            assert_eq!(
                b.logging(&mut arena, Some(value), at(1)).unwrap_err(),
                error(Code::Severity, Some(Field::MinimumSeverity), Some(at(1)))
            );
        }
    }
    #[test]
    fn owner_checks_and_partial_text_failures_never_return_records() {
        let _lock = lock();
        let mut bytes = [0; 256];
        let mut arena = text::Builder::new(&mut bytes).unwrap();
        let mut other_bytes = [0; 256];
        let mut other = text::Builder::new(&mut other_bytes).unwrap();
        for kind in 0..4 {
            let mut b = Builder::new(&arena);
            b.server(&mut arena, ServerInput::default(), at(1)).unwrap();
            let e = match kind {
                0 => b
                    .server(&mut other, ServerInput::default(), at(2))
                    .unwrap_err(),
                1 => b
                    .paths(&mut other, PathsInput::default(), at(2))
                    .unwrap_err(),
                2 => b.logging(&mut other, Some("info"), at(2)).unwrap_err(),
                _ => b.finish(&mut other).unwrap_err(),
            };
            assert_eq!(e.code, Code::ForeignArena);
            assert_eq!(arena.used(), 0);
            assert_eq!(other.used(), 0);
        }
        let mut b = Builder::new(&arena);
        b.server(&mut arena, ServerInput::default(), at(1)).unwrap();
        let r = b.finish(&mut arena).unwrap();
        assert_eq!(r.view_live(&other).unwrap_err().code, Code::ForeignArena);
        assert_eq!(
            r.view(other.freeze().unwrap()).unwrap_err().code,
            Code::ForeignArena
        );
        let mut small = [0; DEFAULT_DATA.len()];
        let mut arena = text::Builder::new(&mut small).unwrap();
        let mut b = Builder::new(&arena);
        b.server(&mut arena, ServerInput::default(), at(1)).unwrap();
        let e = b
            .paths(&mut arena, PathsInput::default(), at(2))
            .unwrap_err();
        assert_eq!(e, error(Code::Text, Some(Field::Runtime), Some(at(2))));
        assert_eq!(arena.used(), DEFAULT_DATA.len());
        assert_eq!(b.finish(&mut arena).unwrap_err(), e);
    }
    #[test]
    fn diagnostics_and_debug_disclose_only_static_context() {
        let _lock = lock();
        let mut bytes = [0; 256];
        let mut arena = text::Builder::new(&mut bytes).unwrap();
        let mut b = Builder::new(&arena);
        assert_eq!(format!("{b:?}"), "GlobalSettingsBuilder(<redacted>)");
        assert!(!format!(
            "{:?}",
            PathsInput {
                data: Some("/private-path"),
                ..Default::default()
            }
        )
        .contains("private"));
        assert!(!format!(
            "{:?}",
            ServerInput {
                public_ipv4: Some("192.0.2.1"),
                ..Default::default()
            }
        )
        .contains("192"));
        b.server(&mut arena, ServerInput::default(), at(1)).unwrap();
        let r = b.finish(&mut arena).unwrap();
        assert_eq!(format!("{r:?}"), "GlobalSettingsRecords(<redacted>)");
        assert_eq!(format!("{:?}", r.server()), "ServerOptions(<redacted>)");
        assert_eq!(
            format!("{:?}", r.view_live(&arena).unwrap()),
            "GlobalSettingsView(<redacted>)"
        );
        let e = Error {
            related_field: Some(Field::Data),
            ..error(Code::Overlap, Some(Field::Logs), Some(at(2)))
        };
        assert_eq!(
            e.to_string(),
            "config_globals_overlap for logs overlapping data at line 2, byte column 2"
        );
        assert!(std::error::Error::source(&e).is_none());
    }
    #[test]
    fn maximum_root_text_and_explicit_defaults_have_exact_capacity() {
        let _lock = lock();
        let roots: Vec<_> = ['a', 'b', 'c']
            .into_iter()
            .map(|c| format!("/{}", c.to_string().repeat(values::MAX_PATH_BYTES - 1)))
            .collect();
        let mut bytes = vec![0; 3 * values::MAX_PATH_BYTES];
        let mut arena = text::Builder::new(&mut bytes).unwrap();
        let mut b = Builder::new(&arena);
        b.server(&mut arena, ServerInput::default(), at(1)).unwrap();
        b.paths(
            &mut arena,
            PathsInput {
                data: Some(&roots[0]),
                runtime: Some(&roots[1]),
                logs: Some(&roots[2]),
            },
            at(2),
        )
        .unwrap();
        assert_eq!(arena.used(), 12285);
        let r = b.finish(&mut arena).unwrap();
        assert_eq!(r.server().view_mode, ViewMode::OnlineBackground);
        let view = r.view_live(&arena).unwrap();
        assert_eq!(view.data().unwrap(), roots[0]);
        assert_eq!(view.runtime().unwrap(), roots[1]);
        assert_eq!(view.logs().unwrap(), roots[2]);
        for severity in ["info", "warning", "error"] {
            let mut bytes = [0; 256];
            let mut arena = text::Builder::new(&mut bytes).unwrap();
            let mut b = Builder::new(&arena);
            b.paths(&mut arena, PathsInput::default(), at(1)).unwrap();
            b.logging(&mut arena, Some(severity), at(2)).unwrap();
            b.server(&mut arena, ServerInput::default(), at(3)).unwrap();
            let used = arena.used();
            let r = b.finish(&mut arena).unwrap();
            assert_eq!(arena.used(), used);
            assert_eq!(r.minimum_severity().code(), severity);
            assert_eq!(r.view_live(&arena).unwrap().paths_location(), Some(at(1)));
        }
        let mut bytes = [];
        let mut arena = text::Builder::new(&mut bytes).unwrap();
        let mut b = Builder::new(&arena);
        b.server(&mut arena, ServerInput::default(), at(1)).unwrap();
        assert_eq!(
            b.finish(&mut arena).unwrap_err(),
            error(Code::Text, Some(Field::Data), None)
        );
    }
    #[test]
    fn empty_logging_retains_presence_and_duplicate_precedence() {
        let _lock = lock();
        let mut bytes = [0; 256];
        let mut arena = text::Builder::new(&mut bytes).unwrap();
        let mut b = Builder::new(&arena);
        b.server(&mut arena, ServerInput::default(), at(1)).unwrap();
        b.logging(&mut arena, None, at(5)).unwrap();
        let r = b.finish(&mut arena).unwrap();
        assert_eq!(r.minimum_severity(), Severity::Info);
        assert_eq!(r.logging_location(), Some(at(5)));
        let mut b = Builder::new(&arena);
        b.logging(&mut arena, None, at(5)).unwrap();
        assert_eq!(
            b.logging(&mut arena, Some("bad"), at(9)).unwrap_err(),
            duplicate(Code::DuplicateLogging, at(9), at(5))
        );
    }
    #[test]
    fn publication_hints_reject_non_host_addresses_and_accept_exact_text_bounds() {
        let _lock = lock();
        for (field, value) in [
            (Field::PublicIpv4, "0.0.0.0"),
            (Field::PublicIpv4, "224.0.0.1"),
            (Field::PublicIpv4, "255.255.255.255"),
            (Field::PublicIpv6, "::"),
            (Field::PublicIpv6, "ff02::1"),
            (Field::PublicIpv6, "::ffff:192.0.2.1"),
            (Field::PublicIpv6, "::192.0.2.1"),
        ] {
            let mut bytes = [0; 256];
            let mut arena = text::Builder::new(&mut bytes).unwrap();
            let mut b = Builder::new(&arena);
            let input = if field == Field::PublicIpv4 {
                ServerInput {
                    public_ipv4: Some(value),
                    ..Default::default()
                }
            } else {
                ServerInput {
                    public_ipv6: Some(value),
                    ..Default::default()
                }
            };
            assert_eq!(
                b.server(&mut arena, input, at(1)).unwrap_err(),
                error(Code::Address, Some(field), Some(at(1)))
            );
        }
        let v4 = "192.168.100.200";
        let v6 = "2001:0db8:0000:0000:0000:0001:192.168.100.200";
        assert_eq!(v4.len(), 15);
        assert_eq!(v6.len(), 45);
        let mut bytes = [0; 256];
        let mut arena = text::Builder::new(&mut bytes).unwrap();
        let mut b = Builder::new(&arena);
        b.server(
            &mut arena,
            ServerInput {
                public_ipv4: Some(v4),
                public_ipv6: Some(v6),
                ..Default::default()
            },
            at(1),
        )
        .unwrap();
        let r = b.finish(&mut arena).unwrap();
        assert_eq!(r.server().public_ipv4, Some(v4.parse().unwrap()));
        assert_eq!(r.server().public_ipv6, Some(v6.parse().unwrap()));
        for (field, value) in [
            (Field::PublicIpv4, format!("{v4}0")),
            (Field::PublicIpv6, format!("{v6}0")),
        ] {
            let mut b = Builder::new(&arena);
            let input = if field == Field::PublicIpv4 {
                ServerInput {
                    public_ipv4: Some(&value),
                    ..Default::default()
                }
            } else {
                ServerInput {
                    public_ipv6: Some(&value),
                    ..Default::default()
                }
            };
            assert_eq!(
                b.server(&mut arena, input, at(1)).unwrap_err().field,
                Some(field)
            );
        }
    }
    #[test]
    fn overlaps_with_defaults_report_the_supplied_root() {
        let _lock = lock();
        for (input, field, related) in [
            (
                PathsInput {
                    data: Some("/run"),
                    ..Default::default()
                },
                Field::Data,
                Field::Runtime,
            ),
            (
                PathsInput {
                    data: Some("/var/log"),
                    ..Default::default()
                },
                Field::Data,
                Field::Logs,
            ),
            (
                PathsInput {
                    runtime: Some("/var/log"),
                    ..Default::default()
                },
                Field::Runtime,
                Field::Logs,
            ),
            (
                PathsInput {
                    logs: Some("/run"),
                    ..Default::default()
                },
                Field::Logs,
                Field::Runtime,
            ),
        ] {
            let mut bytes = [0; 256];
            let mut arena = text::Builder::new(&mut bytes).unwrap();
            let mut b = Builder::new(&arena);
            let e = b.paths(&mut arena, input, at(1)).unwrap_err();
            assert_eq!(e.code, Code::Overlap);
            assert_eq!(e.field, Some(field));
            assert_eq!(e.related_field, Some(related));
            assert_eq!(arena.used(), 0);
        }
    }
}

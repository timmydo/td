//! Closed structural reference graph; provider verification remains separate.
use super::{
    certificate, endpoint::Origin, gateway, listener, policy, syntax::Location, text, values,
};
use crate::bounded::TextBuffer;
use std::{fmt, num::NonZeroU64};
pub const MAX_NAMES_PER_CERTIFICATE: usize = 32;
pub const MAX_BINDINGS: usize = certificate::MAX_PROFILES * MAX_NAMES_PER_CERTIFICATE;
pub const MAX_DERIVED_BINDINGS: usize = listener::MAX_LISTENERS + super::routing::MAX_DOMAINS;
const _: [(); 1] = [(); (MAX_DERIVED_BINDINGS <= MAX_BINDINGS) as usize];
pub const MAX_CANONICAL_ORIGIN_BYTES: usize = 8 + values::MAX_DOMAIN_BYTES + 1 + 5;
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Code {
    Capacity,
    ForeignArena,
    UnknownCertificate,
    UnknownGateway,
    GatewayPeers,
    UnusedCertificate,
    CertificateNames,
    OriginPort,
    Http01Required,
    StsPort,
    SniConflict,
    ExplicitMx,
    DirectMx,
    Text,
    Invariant,
}
impl Code {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Capacity => "config_graph_capacity",
            Self::ForeignArena => "config_graph_foreign_arena",
            Self::UnknownCertificate => "config_graph_unknown_certificate",
            Self::UnknownGateway => "config_graph_unknown_gateway",
            Self::GatewayPeers => "config_graph_gateway_peers",
            Self::UnusedCertificate => "config_graph_unused_certificate",
            Self::CertificateNames => "config_graph_certificate_names",
            Self::OriginPort => "config_graph_origin_port",
            Self::Http01Required => "config_graph_http01_required",
            Self::StsPort => "config_graph_sts_port",
            Self::SniConflict => "config_graph_sni_conflict",
            Self::ExplicitMx => "config_graph_explicit_mx",
            Self::DirectMx => "config_graph_direct_mx",
            Self::Text => "config_graph_text",
            Self::Invariant => "config_graph_invariant",
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Error {
    pub code: Code,
    pub location: Option<Location>,
    pub related: Option<Location>,
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.code.name())?;
        if let Some(at) = self.location {
            write!(f, " at line {}, byte column {}", at.line, at.column)?;
        }
        if let Some(at) = self.related {
            write!(
                f,
                " (related setting at line {}, byte column {})",
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
        related: None,
    }
}
fn invariant() -> Error {
    err(Code::Invariant, None)
}
fn conflict(code: Code, at: Location, related: Location) -> Error {
    Error {
        code,
        location: Some(at),
        related: Some(related),
    }
}
pub struct Inputs<'a> {
    pub listeners: listener::Records<'a>,
    pub certificates: certificate::Records<'a>,
    pub gateways: gateway::Records<'a>,
    pub domains: policy::Records<'a>,
}
impl fmt::Debug for Inputs<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("GraphInputs(<redacted>)")
    }
}
#[derive(Clone, Copy)]
pub struct BindingSlot {
    name: text::Span,
    profile: u8,
    location: Option<Location>,
}
impl BindingSlot {
    pub const EMPTY: Self = Self {
        name: text::Span::EMPTY,
        profile: 0,
        location: None,
    };
}
const _: [(); 1] = [(); (std::mem::size_of::<BindingSlot>() <= 32) as usize];
const _: [(); 1] = [(); (certificate::MAX_PROFILES <= u8::MAX as usize) as usize];
struct Names<'a> {
    slots: &'a mut [BindingSlot],
    count: usize,
    per_profile: [u8; certificate::MAX_PROFILES],
    owner: NonZeroU64,
}
impl Names<'_> {
    fn add(
        &mut self,
        arena: &mut text::Builder<'_>,
        profile: u8,
        name: &str,
        at: Location,
    ) -> Result<(), Error> {
        let view = arena.borrowed_view().map_err(|_| invariant())?;
        for slot in self.slots.get(..self.count).ok_or_else(invariant)? {
            if slot.profile == profile
                && read(view, self.owner, slot.name)?.eq_ignore_ascii_case(name)
            {
                return Ok(());
            }
        }
        let count = self
            .per_profile
            .get_mut(usize::from(profile))
            .ok_or_else(invariant)?;
        if usize::from(*count) >= MAX_NAMES_PER_CERTIFICATE {
            return Err(err(Code::CertificateNames, Some(at)));
        }
        if self.count >= self.slots.len() {
            return Err(err(Code::Capacity, Some(at)));
        }
        let handle = arena
            .append_certificate_name(name)
            .map_err(|_| err(Code::Text, Some(at)))?;
        let name = arena.compact(handle).map_err(|_| invariant())?;
        *self.slots.get_mut(self.count).ok_or_else(invariant)? = BindingSlot {
            name,
            profile,
            location: Some(at),
        };
        self.count = self.count.checked_add(1).ok_or_else(invariant)?;
        *count = count.checked_add(1).ok_or_else(invariant)?;
        Ok(())
    }
}
fn read(text: text::View<'_>, owner: NonZeroU64, span: text::Span) -> Result<&str, Error> {
    std::str::from_utf8(text.read_span(owner, span).map_err(|_| invariant())?)
        .map_err(|_| invariant())
}
fn certificate_index(
    view: certificate::View<'_, '_>,
    name: &str,
    at: Location,
) -> Result<u8, Error> {
    for index in 0..view.len() {
        let profile = view
            .profile(index)
            .map_err(|_| invariant())?
            .ok_or_else(invariant)?;
        if profile.name == name {
            return u8::try_from(index).map_err(|_| invariant());
        }
    }
    Err(err(Code::UnknownCertificate, Some(at)))
}
fn copy_name(
    buffer: &mut [u8; values::MAX_CERTIFICATE_NAME_BYTES],
    name: &str,
) -> Result<usize, Error> {
    let target = buffer.get_mut(..name.len()).ok_or_else(invariant)?;
    target.copy_from_slice(name.as_bytes());
    Ok(name.len())
}
fn policy_name(
    buffer: &mut [u8; values::MAX_CERTIFICATE_NAME_BYTES],
    domain: &str,
) -> Result<usize, Error> {
    let mut output = TextBuffer::new(buffer);
    output
        .format(format_args!("mta-sts.{domain}"))
        .map_err(|_| invariant())?;
    Ok(output.as_str().map_err(|_| invariant())?.len())
}
fn name_text(buffer: &[u8], len: usize) -> Result<&str, Error> {
    std::str::from_utf8(buffer.get(..len).ok_or_else(invariant)?).map_err(|_| invariant())
}
struct Views<'s, 't> {
    listeners: listener::View<'s, 't>,
    certificates: certificate::View<'s, 't>,
    gateways: gateway::View<'s, 't>,
    domains: policy::View<'s, 't>,
}
fn checked_views<'s, 't>(
    inputs: &'s Inputs<'_>,
    arena: &'t text::Builder<'_>,
) -> Result<Views<'s, 't>, Error> {
    let listeners = inputs.listeners.view_live(arena).map_err(|e| {
        err(
            if e.code == listener::Code::ForeignArena {
                Code::ForeignArena
            } else {
                Code::Invariant
            },
            None,
        )
    })?;
    let certificates = inputs.certificates.view_live(arena).map_err(|e| {
        err(
            if e.code == certificate::Code::ForeignArena {
                Code::ForeignArena
            } else {
                Code::Invariant
            },
            None,
        )
    })?;
    let gateways = inputs.gateways.view_live(arena).map_err(|e| {
        err(
            if e.code == gateway::Code::ForeignArena {
                Code::ForeignArena
            } else {
                Code::Invariant
            },
            None,
        )
    })?;
    let domains = inputs.domains.view_live(arena).map_err(|e| {
        err(
            if e.code == policy::Code::ForeignArena {
                Code::ForeignArena
            } else {
                Code::Invariant
            },
            None,
        )
    })?;
    Ok(Views {
        listeners,
        certificates,
        gateways,
        domains,
    })
}
fn advertised_name(
    views: &Views<'_, '_>,
    origin: Origin<'_>,
    name: &str,
    scratch: &mut [u8; values::MAX_CERTIFICATE_NAME_BYTES],
) -> Result<bool, Error> {
    if name.eq_ignore_ascii_case(origin.host()) {
        return Ok(true);
    }
    for index in 0..views.listeners.len() {
        let listener = views
            .listeners
            .listener(index)
            .map_err(|_| invariant())?
            .ok_or_else(invariant)?;
        if listener
            .server_name
            .is_some_and(|server| name.eq_ignore_ascii_case(server))
        {
            return Ok(true);
        }
    }
    for index in 0..views.domains.len() {
        let domain = views
            .domains
            .domain(index)
            .map_err(|_| invariant())?
            .ok_or_else(invariant)?;
        if domain.mode != policy::Mode::Off {
            let len = policy_name(scratch, domain.name)?;
            if name.eq_ignore_ascii_case(name_text(scratch, len)?) {
                return Ok(true);
            }
        }
    }
    Ok(false)
}
fn validate_links(
    views: &Views<'_, '_>,
    origin: Origin<'_>,
    origin_at: Location,
    scratch: &mut [u8; values::MAX_CERTIFICATE_NAME_BYTES],
) -> Result<(), Error> {
    let mut has_direct = false;
    let mut used = [false; certificate::MAX_PROFILES];
    let mut has_http01 = false;
    for index in 0..views.listeners.len() {
        let l = views
            .listeners
            .listener(index)
            .map_err(|_| invariant())?
            .ok_or_else(invariant)?;
        has_direct |= l.kind == listener::Kind::DirectSmtp;
        has_http01 |= l.kind == listener::Kind::Http01;
        if let Some(profile) = l.certificate {
            let profile = certificate_index(views.certificates, profile, l.location)?;
            *used.get_mut(usize::from(profile)).ok_or_else(invariant)? = true;
        }
        if l.kind == listener::Kind::Https && l.bind.port() != origin.port() {
            return Err(conflict(Code::OriginPort, l.location, origin_at));
        }
        if let Some(name) = l.gateway {
            let mut found = false;
            for gateway_index in 0..views.gateways.len() {
                let g = views
                    .gateways
                    .gateway(gateway_index)
                    .map_err(|_| invariant())?
                    .ok_or_else(invariant)?;
                if g.name == name {
                    found = true;
                    if g.peer_count() == 0 {
                        return Err(conflict(Code::GatewayPeers, l.location, g.location));
                    }
                    break;
                }
            }
            if !found {
                return Err(err(Code::UnknownGateway, Some(l.location)));
            }
        }
    }
    if let Some(acme) = views.certificates.acme().map_err(|_| invariant())? {
        if !has_http01 {
            return Err(err(Code::Http01Required, Some(acme.location)));
        }
    }
    let hostname = views.domains.hostname().map_err(|_| invariant())?;
    for index in 0..views.domains.len() {
        let d = views
            .domains
            .domain(index)
            .map_err(|_| invariant())?
            .ok_or_else(invariant)?;
        if !has_direct && !d.explicit_mx {
            return Err(err(Code::ExplicitMx, Some(d.location)));
        }
        let mut matching_direct = false;
        for listener_index in 0..views.listeners.len() {
            let l = views
                .listeners
                .listener(listener_index)
                .map_err(|_| invariant())?
                .ok_or_else(invariant)?;
            if l.kind == listener::Kind::DirectSmtp
                && l.server_name
                    .ok_or_else(invariant)?
                    .eq_ignore_ascii_case(d.mx_host)
            {
                matching_direct = true;
            }
        }
        let local = d.mx_host.eq_ignore_ascii_case(hostname)
            || advertised_name(views, origin, d.mx_host, scratch)?;
        if local && !matching_direct {
            return Err(err(Code::DirectMx, Some(d.location)));
        }
        if d.mode != policy::Mode::Off {
            if origin.port() != 443 {
                return Err(conflict(Code::StsPort, d.location, origin_at));
            }
            let profile = certificate_index(
                views.certificates,
                d.certificate.ok_or_else(invariant)?,
                d.location,
            )?;
            *used.get_mut(usize::from(profile)).ok_or_else(invariant)? = true;
            let len = policy_name(scratch, d.name)?;
            // HTTPS hosts are exactly the JMAP origin and enabled policy names.
            // Canonical unique domains make policy-to-policy collisions impossible.
            if name_text(scratch, len)?.eq_ignore_ascii_case(origin.host()) {
                for listener_index in 0..views.listeners.len() {
                    let l = views
                        .listeners
                        .listener(listener_index)
                        .map_err(|_| invariant())?
                        .ok_or_else(invariant)?;
                    if l.kind == listener::Kind::Https
                        && certificate_index(
                            views.certificates,
                            l.certificate.ok_or_else(invariant)?,
                            l.location,
                        )? != profile
                    {
                        return Err(conflict(Code::SniConflict, d.location, l.location));
                    }
                }
            }
        }
    }
    for (index, consumed) in used.iter().take(views.certificates.len()).enumerate() {
        if !consumed {
            let profile = views
                .certificates
                .profile(index)
                .map_err(|_| invariant())?
                .ok_or_else(invariant)?;
            return Err(err(Code::UnusedCertificate, Some(profile.location)));
        }
    }
    Ok(())
}
/// Consumes the four structural tables. On failure discard the candidate:
/// earlier text and binding writes may remain, but no records are returned.
pub fn bind<'a>(
    arena: &mut text::Builder<'_>,
    bindings: &'a mut [BindingSlot],
    origin: Origin<'_>,
    origin_at: Location,
    inputs: Inputs<'a>,
) -> Result<Records<'a>, Error> {
    if bindings.is_empty() || bindings.len() > MAX_BINDINGS {
        return Err(err(Code::Capacity, None));
    }
    let mut scratch = [0; values::MAX_CERTIFICATE_NAME_BYTES];
    validate_links(
        &checked_views(&inputs, arena)?,
        origin,
        origin_at,
        &mut scratch,
    )?;
    let mut origin_bytes = [0; MAX_CANONICAL_ORIGIN_BYTES];
    let mut output = TextBuffer::new(&mut origin_bytes);
    origin
        .write_canonical(&mut output)
        .map_err(|_| invariant())?;
    let origin_handle = arena
        .append(output.as_str().map_err(|_| invariant())?.as_bytes())
        .map_err(|_| err(Code::Text, Some(origin_at)))?;
    let origin_span = arena.compact(origin_handle).map_err(|_| invariant())?;
    let mut names = Names {
        slots: bindings,
        count: 0,
        per_profile: [0; certificate::MAX_PROFILES],
        owner: arena.owner(),
    };
    let listener_count = inputs
        .listeners
        .view_live(arena)
        .map_err(|_| invariant())?
        .len();
    for index in 0..listener_count {
        let entry = {
            let certificates = inputs
                .certificates
                .view_live(arena)
                .map_err(|_| invariant())?;
            let listeners = inputs.listeners.view_live(arena).map_err(|_| invariant())?;
            let l = listeners
                .listener(index)
                .map_err(|_| invariant())?
                .ok_or_else(invariant)?;
            if let Some(reference) = l.certificate {
                let profile = certificate_index(certificates, reference, l.location)?;
                let name = if l.kind == listener::Kind::Https {
                    origin.host()
                } else {
                    l.server_name.ok_or_else(invariant)?
                };
                Some((profile, copy_name(&mut scratch, name)?, l.location))
            } else {
                None
            }
        };
        if let Some((profile, len, at)) = entry {
            names.add(arena, profile, name_text(&scratch, len)?, at)?;
        }
    }
    let domain_count = inputs
        .domains
        .view_live(arena)
        .map_err(|_| invariant())?
        .len();
    for index in 0..domain_count {
        let entry = {
            let certificates = inputs
                .certificates
                .view_live(arena)
                .map_err(|_| invariant())?;
            let domains = inputs.domains.view_live(arena).map_err(|_| invariant())?;
            let d = domains
                .domain(index)
                .map_err(|_| invariant())?
                .ok_or_else(invariant)?;
            if d.mode != policy::Mode::Off {
                Some((
                    certificate_index(
                        certificates,
                        d.certificate.ok_or_else(invariant)?,
                        d.location,
                    )?,
                    policy_name(&mut scratch, d.name)?,
                    d.location,
                ))
            } else {
                None
            }
        };
        if let Some((profile, len, at)) = entry {
            names.add(arena, profile, name_text(&scratch, len)?, at)?;
        }
    }
    Ok(Records {
        inputs,
        bindings: names.slots.get(..names.count).ok_or_else(invariant)?,
        origin: origin_span,
        owner: names.owner,
    })
}
pub struct Records<'a> {
    inputs: Inputs<'a>,
    bindings: &'a [BindingSlot],
    origin: text::Span,
    owner: NonZeroU64,
}
const _: [(); 1] = [(); (std::mem::size_of::<Records<'_>>() <= 1024) as usize];
impl fmt::Debug for Records<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("GraphRecords(<redacted>)")
    }
}
impl Records<'_> {
    pub fn routes(&self) -> &super::routing::Routing<'_> {
        self.inputs.domains.routes()
    }
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
    records: &'s Records<'s>,
    text: text::View<'t>,
}
impl fmt::Debug for View<'_, '_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("GraphView(<redacted>)")
    }
}
pub struct RequiredName<'t> {
    pub certificate: certificate::Profile<'t>,
    pub name: &'t str,
    pub location: Location,
}
impl fmt::Debug for RequiredName<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("RequiredName(<redacted>)")
    }
}
impl<'s, 't> View<'s, 't> {
    pub fn origin(self) -> Result<Origin<'t>, Error> {
        Origin::parse(read(self.text, self.records.owner, self.records.origin)?)
            .map_err(|_| invariant())
    }
    pub fn binding_count(self) -> usize {
        self.records.bindings.len()
    }
    pub fn binding(self, index: usize) -> Result<Option<RequiredName<'t>>, Error> {
        let Some(slot) = self.records.bindings.get(index) else {
            return Ok(None);
        };
        let certificate = self
            .records
            .inputs
            .certificates
            .view(self.text)
            .map_err(|_| invariant())?
            .profile(usize::from(slot.profile))
            .map_err(|_| invariant())?
            .ok_or_else(invariant)?;
        Ok(Some(RequiredName {
            certificate,
            name: read(self.text, self.records.owner, slot.name)?,
            location: slot.location.ok_or_else(invariant)?,
        }))
    }
    pub fn listeners(self) -> Result<listener::View<'s, 't>, Error> {
        self.records
            .inputs
            .listeners
            .view(self.text)
            .map_err(|_| invariant())
    }
    pub fn certificates(self) -> Result<certificate::View<'s, 't>, Error> {
        self.records
            .inputs
            .certificates
            .view(self.text)
            .map_err(|_| invariant())
    }
    pub fn gateways(self) -> Result<gateway::View<'s, 't>, Error> {
        self.records
            .inputs
            .gateways
            .view(self.text)
            .map_err(|_| invariant())
    }
    pub fn domains(self) -> Result<policy::View<'s, 't>, Error> {
        self.records
            .inputs
            .domains
            .view(self.text)
            .map_err(|_| invariant())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
mod tests {
    use super::*;
    use crate::{config::routing, ids::AccountId, limits::Limits};
    use std::num::NonZeroU32;

    fn at(line: u32) -> Location {
        Location {
            line: NonZeroU32::new(line).unwrap(),
            column: 1,
        }
    }
    fn lock() -> std::sync::MutexGuard<'static, ()> {
        text::TEST_CONSTRUCTION_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }
    struct Tables {
        route_text: Vec<u8>,
        domains: Vec<routing::DomainSlot>,
        aliases: Vec<routing::AliasSlot>,
        policies: Vec<policy::Slot>,
        certificates: Vec<certificate::Slot>,
        gateways: Vec<gateway::Slot>,
        peers: Vec<gateway::PeerSlot>,
        listeners: Vec<listener::Slot>,
    }
    impl Tables {
        fn new() -> Self {
            Self {
                route_text: vec![0; routing::MAX_TEXT_BYTES],
                domains: vec![routing::DomainSlot::EMPTY; routing::MAX_DOMAINS],
                aliases: vec![],
                policies: vec![policy::Slot::EMPTY; routing::MAX_DOMAINS],
                certificates: vec![certificate::Slot::EMPTY; certificate::MAX_PROFILES],
                gateways: vec![gateway::Slot::EMPTY; gateway::MAX_GATEWAYS],
                peers: vec![gateway::PeerSlot::EMPTY; gateway::MAX_PEERS],
                listeners: vec![listener::Slot::EMPTY; listener::MAX_LISTENERS],
            }
        }
    }
    struct Config<'a> {
        listeners: Vec<(&'a str, listener::Input<'a>)>,
        certificates: Vec<(&'a str, certificate::ProfileInput<'a>)>,
        gateways: Vec<(&'a str, bool)>,
        domains: Vec<(&'a str, policy::Input<'a>)>,
        hostname: &'a str,
        acme: bool,
    }
    fn files() -> certificate::ProfileInput<'static> {
        certificate::ProfileInput::Files {
            chain_file: "/chain",
            key_file: "/key",
        }
    }
    fn https(bind: &str) -> listener::Input<'_> {
        listener::Input {
            kind: listener::Kind::Https,
            bind,
            server_name: None,
            certificate: Some("shared"),
            gateway: None,
            session_limit: None,
            per_peer_limit: None,
        }
    }
    impl<'a> Config<'a> {
        fn base() -> Self {
            Self {
                listeners: vec![
                    (
                        "smtp",
                        listener::Input {
                            kind: listener::Kind::DirectSmtp,
                            bind: "127.0.0.1:25",
                            server_name: Some("Mail.Example.test"),
                            certificate: Some("shared"),
                            gateway: None,
                            session_limit: Some(1),
                            per_peer_limit: Some(1),
                        },
                    ),
                    ("web", https("127.0.0.1:443")),
                ],
                certificates: vec![("shared", files())],
                gateways: vec![],
                domains: vec![("example.test", policy::Input::default())],
                hostname: "mail.example.test",
                acme: false,
            }
        }
        fn gateway() -> Self {
            let mut c = Self::base();
            c.listeners[0].1.kind = listener::Kind::GatewaySmtp;
            c.listeners[0].1.gateway = Some("upstream");
            c.gateways.push(("upstream", true));
            c.domains[0].1.mx_host = Some("mx.upstream.test");
            c
        }
        fn fixture() -> Self {
            let mut c = Self::base();
            c.listeners[0].1.kind = listener::Kind::LoopbackSmtpFixture;
            c.listeners[0].1.server_name = None;
            c.listeners[0].1.certificate = None;
            c.domains[0].1.mx_host = Some("mx.upstream.test");
            c
        }
        fn sts(&mut self, profile: &'a str) {
            self.domains[0].1.mode = policy::Mode::Enforce;
            self.domains[0].1.certificate = Some(profile);
        }
    }
    fn build<'a>(
        tables: &'a mut Tables,
        arena: &mut text::Builder<'_>,
        c: Config<'_>,
    ) -> Inputs<'a> {
        let mut ls = listener::Builder::new(arena, &mut tables.listeners).unwrap();
        for (i, (name, input)) in c.listeners.into_iter().enumerate() {
            ls.listener(arena, name, input, at(1 + i as u32)).unwrap();
        }
        let listeners = ls.finish(&Limits::default().plan().unwrap()).unwrap();
        let mut cs = certificate::Builder::new(arena, &mut tables.certificates).unwrap();
        for (i, (name, input)) in c.certificates.into_iter().enumerate() {
            cs.profile(arena, name, input, at(100 + i as u32)).unwrap();
        }
        if c.acme {
            cs.acme(
                arena,
                certificate::AcmeInput {
                    directory: "https://ca.example.test/directory",
                    contact: "operator@example.test",
                    terms_accepted: true,
                    ca_file: None,
                },
                at(150),
            )
            .unwrap();
        }
        let certificates = cs.finish().unwrap();
        let mut gs = gateway::Builder::new(arena, &mut tables.gateways, &mut tables.peers).unwrap();
        for (i, (name, peers)) in c.gateways.into_iter().enumerate() {
            gs.gateway(
                arena,
                name,
                gateway::Input {
                    ca_file: "/ca",
                    client_cert_sha256:
                        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
                    next_client_cert_sha256: None,
                },
                at(200 + i as u32),
            )
            .unwrap();
            if peers {
                gs.peer(arena, name, "192.0.2.0/24", at(250 + i as u32))
                    .unwrap();
            }
        }
        let gateways = gs.finish().unwrap();
        let mut ds = policy::Builder::new(
            arena,
            &mut tables.route_text,
            &mut tables.domains,
            &mut tables.aliases,
            &mut tables.policies,
        )
        .unwrap();
        ds.account(AccountId::from_bytes([1; 16]), at(299)).unwrap();
        for (i, (name, input)) in c.domains.into_iter().enumerate() {
            ds.domain(arena, name, input, at(300 + i as u32)).unwrap();
        }
        let domains = ds.finish(arena, c.hostname, at(900)).unwrap();
        Inputs {
            listeners,
            certificates,
            gateways,
            domains,
        }
    }
    fn run(c: Config<'_>, origin: &str) -> Result<Vec<(String, String)>, Error> {
        let mut bytes = vec![0; text::MAX_BYTES];
        let mut arena = text::Builder::new(&mut bytes).unwrap();
        let mut tables = Tables::new();
        let inputs = build(&mut tables, &mut arena, c);
        let mut bindings = vec![BindingSlot::EMPTY; MAX_BINDINGS];
        let records = bind(
            &mut arena,
            &mut bindings,
            Origin::parse(origin).unwrap(),
            at(901),
            inputs,
        )?;
        let view = records.view_live(&arena)?;
        (0..view.binding_count())
            .map(|i| {
                let b = view.binding(i)?.unwrap();
                Ok((b.certificate.name.to_owned(), b.name.to_owned()))
            })
            .collect()
    }
    const ORIGIN: &str = "https://jmap.example.test";

    #[test]
    fn direct_gateway_fixture_and_sts_names_are_closed() {
        let _lock = lock();
        let expected = vec![
            ("shared".into(), "mail.example.test".into()),
            ("shared".into(), "jmap.example.test".into()),
        ];
        assert_eq!(run(Config::base(), ORIGIN).unwrap(), expected);
        assert_eq!(run(Config::gateway(), ORIGIN).unwrap(), expected);
        let mut combined = Config::gateway();
        let mut direct = Config::base().listeners.remove(0).1;
        direct.bind = "127.0.0.2:25";
        combined.listeners.push(("direct", direct));
        combined.domains[0].1.mx_host = None;
        assert_eq!(run(combined, ORIGIN).unwrap(), expected);
        assert_eq!(
            run(Config::fixture(), ORIGIN).unwrap(),
            vec![expected[1].clone()]
        );
        let mut c = Config::base();
        c.sts("shared");
        let mut sts = expected;
        sts.push(("shared".into(), "mta-sts.example.test".into()));
        assert_eq!(run(c, ORIGIN).unwrap(), sts);
    }
    #[test]
    fn reference_errors_retain_only_fixed_codes_and_coordinates() {
        let _lock = lock();
        let mut c = Config::base();
        c.listeners[0].1.certificate = Some("secret-profile");
        let e = run(c, ORIGIN).unwrap_err();
        assert_eq!(e, err(Code::UnknownCertificate, Some(at(1))));
        assert_eq!(
            e.to_string(),
            "config_graph_unknown_certificate at line 1, byte column 1"
        );
        assert!(!format!("{e:?}").contains("secret"));
        assert!(std::error::Error::source(&e).is_none());
        let mut c = Config::base();
        c.sts("missing");
        assert_eq!(
            run(c, ORIGIN).unwrap_err(),
            err(Code::UnknownCertificate, Some(at(300)))
        );
        let mut c = Config::gateway();
        c.gateways.clear();
        assert_eq!(
            run(c, ORIGIN).unwrap_err(),
            err(Code::UnknownGateway, Some(at(1)))
        );
        let mut c = Config::gateway();
        c.gateways[0].1 = false;
        let e = run(c, ORIGIN).unwrap_err();
        assert_eq!(e, conflict(Code::GatewayPeers, at(1), at(200)));
        assert_eq!(e.to_string(), "config_graph_gateway_peers at line 1, byte column 1 (related setting at line 200, byte column 1)");
        let mut c = Config::base();
        c.gateways.push(("staged", false));
        assert!(run(c, ORIGIN).is_ok());
        let mut c = Config::base();
        c.certificates.push(("unused", files()));
        assert_eq!(
            run(c, ORIGIN).unwrap_err(),
            err(Code::UnusedCertificate, Some(at(101)))
        );
    }
    #[test]
    fn origin_ports_and_acme_http01_are_mandatory() {
        let _lock = lock();
        assert_eq!(
            run(Config::base(), "https://jmap.example.test:8443").unwrap_err(),
            conflict(Code::OriginPort, at(2), at(901))
        );
        let mut c = Config::base();
        c.listeners[1].1.bind = "127.0.0.1:8443";
        assert!(run(c, "https://jmap.example.test:8443").is_ok());
        let mut c = Config::base();
        c.listeners[1].1.bind = "127.0.0.1:8443";
        c.sts("shared");
        assert_eq!(
            run(c, "https://jmap.example.test:8443").unwrap_err(),
            conflict(Code::StsPort, at(300), at(901))
        );
        let mut c = Config::base();
        c.acme = true;
        c.certificates[0].1 = certificate::ProfileInput::Acme;
        assert_eq!(
            run(c, ORIGIN).unwrap_err(),
            err(Code::Http01Required, Some(at(150)))
        );
        let mut c = Config::base();
        c.acme = true;
        c.certificates[0].1 = certificate::ProfileInput::Acme;
        c.listeners.push((
            "challenge",
            listener::Input {
                kind: listener::Kind::Http01,
                bind: "127.0.0.1:80",
                server_name: None,
                certificate: None,
                gateway: None,
                session_limit: None,
                per_peer_limit: None,
            },
        ));
        assert!(run(c, ORIGIN).is_ok());
    }
    #[test]
    fn mx_defaults_and_explicit_upstream_targets_obey_ingress_roles() {
        let _lock = lock();
        let mut c = Config::fixture();
        c.domains[0].1.mx_host = None;
        assert_eq!(
            run(c, ORIGIN).unwrap_err(),
            err(Code::ExplicitMx, Some(at(300)))
        );
        let mut c = Config::gateway();
        c.domains[0].1.mx_host = None;
        assert_eq!(run(c, ORIGIN).unwrap_err().code, Code::ExplicitMx);
        let mut c = Config::base();
        c.hostname = "unserved.example.test";
        assert_eq!(
            run(c, ORIGIN).unwrap_err(),
            err(Code::DirectMx, Some(at(300)))
        );
        let mut c = Config::gateway();
        c.domains[0].1.mx_host = Some("MAIL.EXAMPLE.TEST");
        assert_eq!(run(c, ORIGIN).unwrap_err().code, Code::DirectMx);
        let mut c = Config::base();
        c.domains[0].1.mx_host = Some("external.example.test");
        assert!(run(c, ORIGIN).is_ok());
        let mut c = Config::base();
        c.domains[0].1.mx_host = Some("MAIL.EXAMPLE.TEST");
        assert!(run(c, ORIGIN).is_ok());
        let mut c = Config::base();
        c.hostname = "different.example.test";
        c.domains[0].1.mx_host = Some("mail.example.test");
        assert!(run(c, ORIGIN).is_ok());
    }
    #[test]
    fn sni_conflicts_are_per_listener_and_identifiers_deduplicate_per_profile() {
        let _lock = lock();
        let mut c = Config::base();
        c.sts("shared");
        let names = run(c, "https://MTA-STS.example.test:443/").unwrap();
        assert_eq!(names.len(), 2);
        assert_eq!(names[1].1, "mta-sts.example.test");
        let mut c = Config::base();
        c.certificates.push(("policy", files()));
        c.sts("policy");
        assert_eq!(
            run(c, "https://mta-sts.example.test").unwrap_err(),
            conflict(Code::SniConflict, at(300), at(2))
        );
        let mut c = Config::base();
        c.certificates.push(("second", files()));
        let mut web = https("127.0.0.2:443");
        web.certificate = Some("second");
        c.listeners.push(("other", web));
        assert_eq!(run(c, ORIGIN).unwrap().len(), 3);
        let mut c = Config::base();
        c.listeners.push(("other", https("127.0.0.2:443")));
        assert_eq!(run(c, ORIGIN).unwrap().len(), 2);
    }
    #[test]
    fn per_profile_name_limit_is_inclusive_and_global_storage_is_bounded() {
        let _lock = lock();
        let names: Vec<_> = (0..31).map(|i| format!("domain{i}.test")).collect();
        for (count, expected) in [(30, None), (31, Some(Code::CertificateNames))] {
            let mut c = Config::base();
            c.domains.clear();
            for name in &names[..count] {
                c.domains.push((
                    name,
                    policy::Input {
                        mode: policy::Mode::Testing,
                        certificate: Some("shared"),
                        ..Default::default()
                    },
                ));
            }
            let result = run(c, ORIGIN);
            if let Some(code) = expected {
                assert_eq!(result.unwrap_err().code, code);
            } else {
                assert_eq!(result.unwrap().len(), 32);
            }
        }
        for capacity in [0, 1, 2, MAX_BINDINGS, MAX_BINDINGS + 1] {
            let mut bytes = vec![0; text::MAX_BYTES];
            let mut arena = text::Builder::new(&mut bytes).unwrap();
            let mut tables = Tables::new();
            let inputs = build(&mut tables, &mut arena, Config::base());
            let mut slots = vec![BindingSlot::EMPTY; capacity];
            let result = bind(
                &mut arena,
                &mut slots,
                Origin::parse(ORIGIN).unwrap(),
                at(901),
                inputs,
            );
            if capacity == 2 || capacity == MAX_BINDINGS {
                assert!(result.is_ok());
            } else {
                assert_eq!(result.unwrap_err().code, Code::Capacity);
            }
        }
    }
    #[test]
    fn maximum_origin_and_derived_name_fit_without_truncation() {
        let _lock = lock();
        let domain = format!(
            "{}.{}.{}.{}.test",
            "a".repeat(63),
            "b".repeat(63),
            "c".repeat(63),
            "d".repeat(46)
        );
        assert_eq!(domain.len(), values::MAX_DOMAIN_BYTES);
        let raw = format!("https://{domain}:65535/");
        let mut c = Config::base();
        c.listeners[1].1.bind = "127.0.0.1:65535";
        assert_eq!(run(c, &raw).unwrap()[1].1, domain);
        let mut c = Config::base();
        c.domains = vec![(
            &domain,
            policy::Input {
                mode: policy::Mode::None,
                certificate: Some("shared"),
                ..Default::default()
            },
        )];
        let names = run(c, ORIGIN).unwrap();
        assert_eq!(names[2].1, format!("mta-sts.{domain}"));
        assert_eq!(names[2].1.len(), 251);
    }
    #[test]
    fn every_input_owner_is_checked_and_frozen_views_preserve_routes() {
        let _lock = lock();
        for foreign in 0..4 {
            let mut bytes = vec![0; text::MAX_BYTES];
            let mut arena = text::Builder::new(&mut bytes).unwrap();
            let mut tables = Tables::new();
            let mut inputs = build(&mut tables, &mut arena, Config::base());
            let mut other_bytes = vec![0; text::MAX_BYTES];
            let mut other = text::Builder::new(&mut other_bytes).unwrap();
            let mut other_tables = Tables::new();
            let other_inputs = build(&mut other_tables, &mut other, Config::base());
            match foreign {
                0 => inputs.listeners = other_inputs.listeners,
                1 => inputs.certificates = other_inputs.certificates,
                2 => inputs.gateways = other_inputs.gateways,
                _ => inputs.domains = other_inputs.domains,
            }
            let mut slots = [BindingSlot::EMPTY; 2];
            assert_eq!(
                bind(
                    &mut arena,
                    &mut slots,
                    Origin::parse(ORIGIN).unwrap(),
                    at(901),
                    inputs
                )
                .unwrap_err(),
                err(Code::ForeignArena, None)
            );
        }
        let mut bytes = vec![0; text::MAX_BYTES];
        let mut arena = text::Builder::new(&mut bytes).unwrap();
        let mut tables = Tables::new();
        let inputs = build(&mut tables, &mut arena, Config::base());
        assert_eq!(format!("{inputs:?}"), "GraphInputs(<redacted>)");
        let mut slots = [BindingSlot::EMPTY; 2];
        let records = bind(
            &mut arena,
            &mut slots,
            Origin::parse("https://JMAP.Example.test:443/").unwrap(),
            at(901),
            inputs,
        )
        .unwrap();
        assert_eq!(format!("{records:?}"), "GraphRecords(<redacted>)");
        assert_eq!(records.routes().domain_count(), 1);
        let mut other_bytes = [0; 1];
        let other = text::Builder::new(&mut other_bytes).unwrap();
        assert_eq!(
            records.view_live(&other).unwrap_err().code,
            Code::ForeignArena
        );
        assert_eq!(
            records.view(other.freeze().unwrap()).unwrap_err().code,
            Code::ForeignArena
        );
        let view = records.view(arena.freeze().unwrap()).unwrap();
        assert_eq!(format!("{view:?}"), "GraphView(<redacted>)");
        assert_eq!(view.origin().unwrap().host(), "jmap.example.test");
        assert_eq!(view.origin().unwrap().port(), 443);
        assert!(view.binding(usize::MAX).unwrap().is_none());
        assert_eq!(
            format!("{:?}", view.binding(0).unwrap().unwrap()),
            "RequiredName(<redacted>)"
        );
        assert_eq!(view.listeners().unwrap().len(), 2);
        assert_eq!(view.certificates().unwrap().len(), 1);
        assert_eq!(view.gateways().unwrap().len(), 0);
        assert_eq!(view.domains().unwrap().len(), 1);
    }
    #[test]
    fn cell_reuse_and_text_exhaustion_never_expose_partial_records() {
        let _lock = lock();
        let mut slots = [BindingSlot::EMPTY; 4];
        let mut tables = Tables::new();
        for fixture in [false, true] {
            let mut bytes = vec![0; text::MAX_BYTES];
            let mut arena = text::Builder::new(&mut bytes).unwrap();
            let inputs = build(
                &mut tables,
                &mut arena,
                if fixture {
                    Config::fixture()
                } else {
                    Config::base()
                },
            );
            let records = bind(
                &mut arena,
                &mut slots,
                Origin::parse(ORIGIN).unwrap(),
                at(901),
                inputs,
            )
            .unwrap();
            let view = records.view_live(&arena).unwrap();
            assert_eq!(view.binding_count(), if fixture { 1 } else { 2 });
            assert!(view.binding(view.binding_count()).unwrap().is_none());
        }
        let mut bytes = vec![0; text::MAX_BYTES];
        let mut arena = text::Builder::new(&mut bytes).unwrap();
        let inputs = build(&mut tables, &mut arena, Config::base());
        let remaining = arena.capacity() - arena.used();
        arena
            .append(&vec![
                b'x';
                remaining - ORIGIN.len() - "mail.example.test".len()
            ])
            .unwrap();
        assert_eq!(
            bind(
                &mut arena,
                &mut slots,
                Origin::parse(ORIGIN).unwrap(),
                at(901),
                inputs
            )
            .unwrap_err(),
            err(Code::Text, Some(at(2)))
        );
    }
    #[test]
    fn every_advertised_local_mx_needs_a_matching_direct_listener() {
        let _lock = lock();
        let mut gateway = Config::gateway();
        gateway.hostname = "different.example.test";
        gateway.domains[0].1.mx_host = Some("mail.example.test");
        assert_eq!(run(gateway, ORIGIN).unwrap_err().code, Code::DirectMx);
        for name in ["jmap.example.test", "mta-sts.example.test"] {
            let mut c = Config::base();
            c.sts("shared");
            c.domains[0].1.mx_host = Some(name);
            assert_eq!(run(c, ORIGIN).unwrap_err().code, Code::DirectMx);
        }
        let mut c = Config::base();
        c.domains[0].1.mx_host = Some("mta-sts.other.test");
        c.domains.push((
            "other.test",
            policy::Input {
                mode: policy::Mode::None,
                certificate: Some("shared"),
                ..Default::default()
            },
        ));
        assert_eq!(run(c, ORIGIN).unwrap_err().code, Code::DirectMx);
        let mut c = Config::base();
        c.domains[0].1.mx_host = Some("mta-sts.example.test");
        assert!(run(c, ORIGIN).is_ok()); // Off publishes no policy host.
        let mut c = Config::base();
        c.listeners[0].1.server_name = Some("jmap.example.test");
        c.domains[0].1.mx_host = Some("JMAP.EXAMPLE.TEST");
        assert!(run(c, ORIGIN).is_ok());
    }
    #[test]
    fn every_https_table_checks_mixed_case_sni_and_deduplication() {
        let _lock = lock();
        let mut c = Config::base();
        c.sts("shared");
        c.certificates.push(("second", files()));
        let mut second = https("127.0.0.2:443");
        second.certificate = Some("second");
        c.listeners.push(("second", second));
        assert_eq!(
            run(c, "https://MTA-STS.Example.test").unwrap_err(),
            conflict(Code::SniConflict, at(300), at(3))
        );
        let mut c = Config::base();
        c.listeners.push(("second", https("127.0.0.2:443")));
        assert_eq!(run(c, "https://JMAP.Example.test").unwrap().len(), 2);
        let mut c = Config::base();
        c.sts("policy");
        c.certificates.push(("policy", files()));
        assert_eq!(
            run(c, "https://MTA-STS.Example.test").unwrap_err().code,
            Code::SniConflict
        );
    }
    #[test]
    fn first_consumer_coordinates_and_origin_exhaustion_are_preserved() {
        let _lock = lock();
        let mut bytes = vec![0; text::MAX_BYTES];
        let mut arena = text::Builder::new(&mut bytes).unwrap();
        let mut tables = Tables::new();
        let mut c = Config::base();
        c.sts("shared");
        c.listeners.push(("second", https("127.0.0.2:443")));
        let inputs = build(&mut tables, &mut arena, c);
        let mut slots = [BindingSlot::EMPTY; 2];
        let r = bind(
            &mut arena,
            &mut slots,
            Origin::parse("https://MTA-STS.Example.test").unwrap(),
            at(901),
            inputs,
        )
        .unwrap();
        let view = r.view_live(&arena).unwrap();
        assert_eq!(view.binding(0).unwrap().unwrap().location, at(1));
        assert_eq!(view.binding(1).unwrap().unwrap().location, at(2));
        let mut bytes = vec![0; text::MAX_BYTES];
        let mut arena = text::Builder::new(&mut bytes).unwrap();
        let mut tables = Tables::new();
        let inputs = build(&mut tables, &mut arena, Config::base());
        let remaining = arena.capacity() - arena.used();
        arena.append(&vec![b'x'; remaining]).unwrap();
        assert_eq!(
            bind(
                &mut arena,
                &mut slots,
                Origin::parse(ORIGIN).unwrap(),
                at(901),
                inputs
            )
            .unwrap_err(),
            err(Code::Text, Some(at(901)))
        );
    }
    #[test]
    fn unused_profiles_fail_before_any_graph_text_or_binding_writes() {
        let _lock = lock();
        let mut bytes = vec![0; text::MAX_BYTES];
        let mut arena = text::Builder::new(&mut bytes).unwrap();
        let mut tables = Tables::new();
        let mut c = Config::base();
        c.certificates.push(("unused", files()));
        let inputs = build(&mut tables, &mut arena, c);
        let used = arena.used();
        let mut slots = [BindingSlot::EMPTY; 2];
        assert_eq!(
            bind(
                &mut arena,
                &mut slots,
                Origin::parse(ORIGIN).unwrap(),
                at(901),
                inputs
            )
            .unwrap_err()
            .code,
            Code::UnusedCertificate
        );
        assert_eq!(arena.used(), used);
        assert!(slots.iter().all(|slot| slot.location.is_none()));
    }
    #[test]
    fn all_listener_and_domain_consumers_fit_the_derived_ceiling() {
        let _lock = lock();
        let profiles: Vec<_> = (0..16).map(|i| format!("profile{i}")).collect();
        let binds: Vec<_> = (0..16)
            .map(|i| format!("192.0.2.{}:{}", i + 1, if i == 0 { 25 } else { 443 }))
            .collect();
        let domains: Vec<_> = (0..256).map(|i| format!("domain{i}.test")).collect();
        for capacity in [MAX_DERIVED_BINDINGS - 1, MAX_DERIVED_BINDINGS, MAX_BINDINGS] {
            let mut c = Config::base();
            c.listeners.clear();
            c.certificates.clear();
            c.domains.clear();
            for i in 0..16 {
                let mut l = if i == 0 {
                    Config::base().listeners.remove(0).1
                } else {
                    https(&binds[i])
                };
                l.bind = &binds[i];
                l.certificate = Some(&profiles[i]);
                c.listeners.push((&profiles[i], l));
                c.certificates.push((&profiles[i], files()));
            }
            for (i, domain) in domains.iter().enumerate() {
                c.domains.push((
                    domain,
                    policy::Input {
                        mode: policy::Mode::Testing,
                        certificate: Some(&profiles[i % 16]),
                        ..Default::default()
                    },
                ));
            }
            let mut bytes = vec![0; text::MAX_BYTES];
            let mut arena = text::Builder::new(&mut bytes).unwrap();
            let mut tables = Tables::new();
            let inputs = build(&mut tables, &mut arena, c);
            let mut slots = vec![BindingSlot::EMPTY; capacity];
            let result = bind(
                &mut arena,
                &mut slots,
                Origin::parse(ORIGIN).unwrap(),
                at(901),
                inputs,
            );
            if capacity < MAX_DERIVED_BINDINGS {
                assert_eq!(result.unwrap_err().code, Code::Capacity);
            } else {
                assert_eq!(
                    result.unwrap().view_live(&arena).unwrap().binding_count(),
                    272
                );
            }
        }
    }
}

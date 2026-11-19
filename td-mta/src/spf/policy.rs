//! One SPF term, borrowed from its validated TXT record.
use super::{macros, Checked, Outcome};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

pub(super) enum Term<'a> {
    Directive(Outcome, Mechanism<'a>),
    Redirect(&'a str),
    Explanation,
    UnknownModifier,
}

#[derive(Clone, Copy)]
pub(super) enum Mechanism<'a> {
    All,
    Network(IpAddr, u8),
    Include(&'a str),
    Address(Option<&'a str>, u8, u8),
    Mx(Option<&'a str>, u8, u8),
    Ptr(Option<&'a str>),
    Exists(&'a str),
}

impl<'a> Mechanism<'a> {
    pub(super) fn target(self) -> Option<&'a str> {
        match self {
            Self::Include(spec) | Self::Exists(spec) => Some(spec),
            Self::Address(spec, _, _) | Self::Mx(spec, _, _) | Self::Ptr(spec) => spec,
            _ => None,
        }
    }
}

fn bits(digits: &str, max: u8) -> Checked<u8> {
    if digits.is_empty()
        || !digits.bytes().all(|b| b.is_ascii_digit())
        || (digits.starts_with('0') && digits.len() > 1)
    {
        return Err(Outcome::Permerror);
    }
    digits
        .parse::<u8>()
        .ok()
        .filter(|&v| v <= max)
        .ok_or(Outcome::Permerror)
}

fn domain(spec: &str) -> Checked<&str> {
    macros::syntax(spec, true)?;
    Ok(spec)
}

fn optional_domain(input: &str) -> Checked<Option<&str>> {
    if input.is_empty() {
        return Ok(None);
    }
    domain(input.strip_prefix(':').ok_or(Outcome::Permerror)?).map(Some)
}

// A slash can also be a macro delimiter or a literal part of a domain-spec.
// Consume only a trailing decimal prefix.
fn cidr_suffix(input: &str) -> Option<(&str, &str)> {
    input
        .rsplit_once('/')
        .filter(|(_, digits)| !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()))
}

fn dual(input: &str) -> Checked<(Option<&str>, u8, u8)> {
    let (input, v6) = if let Some((head, digits)) =
        cidr_suffix(input).and_then(|(head, digits)| Some((head.strip_suffix('/')?, digits)))
    {
        (head, bits(digits, 128)?)
    } else {
        (input, 128)
    };
    let (input, v4) = if let Some((head, digits)) = cidr_suffix(input) {
        (head, bits(digits, 32)?)
    } else {
        (input, 32)
    };
    Ok((optional_domain(input)?, v4, v6))
}

pub(super) fn parse(input: &str) -> Checked<Term<'_>> {
    let (qualifier, body) = match input.as_bytes().first() {
        Some(b'-') => (Outcome::Fail, input.strip_prefix('-')),
        Some(b'~') => (Outcome::Softfail, input.strip_prefix('~')),
        Some(b'?') => (Outcome::Neutral, input.strip_prefix('?')),
        Some(b'+') => (Outcome::Pass, input.strip_prefix('+')),
        _ => (Outcome::Pass, Some(input)),
    };
    let body = body.ok_or(Outcome::Permerror)?;
    let len = body.find([':', '/', '=']).unwrap_or(body.len());
    let (name, value) = body.split_at_checked(len).ok_or(Outcome::Permerror)?;
    if let Some(spec) = value.strip_prefix('=') {
        if body != input
            || !name.as_bytes().first().is_some_and(u8::is_ascii_alphabetic)
            || !name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
        {
            return Err(Outcome::Permerror);
        }
        return if name.eq_ignore_ascii_case("redirect") {
            Ok(Term::Redirect(domain(spec)?))
        } else if name.eq_ignore_ascii_case("exp") {
            domain(spec)?;
            Ok(Term::Explanation)
        } else {
            macros::syntax(spec, false)?;
            Ok(Term::UnknownModifier)
        };
    }
    let rule = if name.eq_ignore_ascii_case("all") && value.is_empty() {
        Mechanism::All
    } else if name.eq_ignore_ascii_case("include") {
        Mechanism::Include(optional_domain(value)?.ok_or(Outcome::Permerror)?)
    } else if name.eq_ignore_ascii_case("exists") {
        Mechanism::Exists(optional_domain(value)?.ok_or(Outcome::Permerror)?)
    } else if name.eq_ignore_ascii_case("ptr") {
        Mechanism::Ptr(optional_domain(value)?)
    } else if name.eq_ignore_ascii_case("a") || name.eq_ignore_ascii_case("mx") {
        let (spec, v4, v6) = dual(value)?;
        if name.eq_ignore_ascii_case("a") {
            Mechanism::Address(spec, v4, v6)
        } else {
            Mechanism::Mx(spec, v4, v6)
        }
    } else if name.eq_ignore_ascii_case("ip4") || name.eq_ignore_ascii_case("ip6") {
        let literal = value.strip_prefix(':').ok_or(Outcome::Permerror)?;
        let (address, prefix) =
            cidr_suffix(literal).map_or((literal, None), |(addr, prefix)| (addr, Some(prefix)));
        let (address, max) = if name.eq_ignore_ascii_case("ip4") {
            (
                IpAddr::V4(
                    address
                        .parse::<Ipv4Addr>()
                        .map_err(|_| Outcome::Permerror)?,
                ),
                32,
            )
        } else {
            (
                IpAddr::V6(
                    address
                        .parse::<Ipv6Addr>()
                        .map_err(|_| Outcome::Permerror)?,
                ),
                128,
            )
        };
        Mechanism::Network(
            address,
            prefix.map(|n| bits(n, max)).transpose()?.unwrap_or(max),
        )
    } else {
        return Err(Outcome::Permerror);
    };
    Ok(Term::Directive(qualifier, rule))
}

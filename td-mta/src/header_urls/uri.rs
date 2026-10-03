//! Generic RFC 3986 URI syntax; no scheme-specific interpretation.
use super::Error;
use crate::{admission::work::Charge, decode_work::Work, ports::Tick};
fn unreserved(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~')
}
fn subdelim(b: u8) -> bool {
    matches!(
        b,
        b'!' | b'$' | b'&' | b'\'' | b'(' | b')' | b'*' | b'+' | b',' | b';' | b'='
    )
}
fn pchar(b: u8) -> bool {
    unreserved(b) || subdelim(b) || matches!(b, b':' | b'@')
}
#[derive(Clone, Copy)]
enum Phase {
    SchemeStart,
    Scheme,
    Hierarchy,
    Slash,
    Authority,
    Path,
    Query,
    Fragment,
}
#[derive(Clone, Copy)]
enum Host {
    Name,
    Port,
    Literal,
    AfterLiteral,
}
#[derive(Clone, Copy)]
enum Literal {
    Start,
    V6,
    FutureVersion { any: bool },
    FutureBody { any: bool },
}
pub(super) struct Validator {
    phase: Phase,
    percent: u8,
    host: Host,
    host_valid: bool,
    host_start: bool,
    user_possible: bool,
    had_at: bool,
    literal: Literal,
    ipv6: [u8; 45],
    length: usize,
}
impl Validator {
    pub(super) const fn new() -> Self {
        Self {
            phase: Phase::SchemeStart,
            percent: 0,
            host: Host::Name,
            host_valid: true,
            host_start: true,
            user_possible: true,
            had_at: false,
            literal: Literal::Start,
            ipv6: [0; 45],
            length: 0,
        }
    }
    pub(super) fn push(&mut self, b: u8, now: Tick, work: &mut impl Work) -> Result<(), Error> {
        if self.percent != 0 {
            if !b.is_ascii_hexdigit() {
                return Err(Error::Malformed);
            }
            self.percent -= 1;
            return Ok(());
        }
        match self.phase {
            Phase::SchemeStart => {
                if !b.is_ascii_alphabetic() {
                    return Err(Error::Malformed);
                }
                self.phase = Phase::Scheme;
            }
            Phase::Scheme => {
                if b == b':' {
                    self.phase = Phase::Hierarchy;
                } else if !b.is_ascii_alphanumeric() && !matches!(b, b'+' | b'-' | b'.') {
                    return Err(Error::Malformed);
                }
            }
            Phase::Hierarchy => {
                if b == b'/' {
                    self.phase = Phase::Slash;
                } else {
                    self.phase = Phase::Path;
                    self.path(b)?;
                }
            }
            Phase::Slash => {
                if b == b'/' {
                    self.phase = Phase::Authority;
                } else {
                    self.phase = Phase::Path;
                    self.path(b)?;
                }
            }
            Phase::Authority => self.authority(b, now, work)?,
            Phase::Path => self.path(b)?,
            Phase::Query => {
                if b == b'#' {
                    self.phase = Phase::Fragment;
                } else {
                    self.query(b)?;
                }
            }
            Phase::Fragment => self.query(b)?,
        }
        Ok(())
    }
    fn path(&mut self, b: u8) -> Result<(), Error> {
        match b {
            b'?' => self.phase = Phase::Query,
            b'#' => self.phase = Phase::Fragment,
            b'%' => self.percent = 2,
            b'/' => {}
            _ if pchar(b) => {}
            _ => return Err(Error::Malformed),
        }
        Ok(())
    }
    fn query(&mut self, b: u8) -> Result<(), Error> {
        if b == b'%' {
            self.percent = 2;
        } else if !pchar(b) && !matches!(b, b'/' | b'?') {
            return Err(Error::Malformed);
        }
        Ok(())
    }
    fn authority(&mut self, b: u8, now: Tick, work: &mut impl Work) -> Result<(), Error> {
        if matches!(self.host, Host::Literal) {
            return self.literal(b, now, work);
        }
        if matches!(b, b'/' | b'?' | b'#') {
            if !self.host_valid {
                return Err(Error::Malformed);
            }
            self.phase = Phase::Path;
            return self.path(b);
        }
        if b == b'@' {
            if self.had_at || !self.user_possible {
                return Err(Error::Malformed);
            }
            self.had_at = true;
            self.host = Host::Name;
            self.host_start = true;
            self.host_valid = true;
            return Ok(());
        }
        if b == b'[' {
            if !self.host_start || !self.host_valid {
                return Err(Error::Malformed);
            }
            self.user_possible = false;
            self.host = Host::Literal;
            self.host_start = false;
            return Ok(());
        }
        self.user_possible &= unreserved(b) || subdelim(b) || matches!(b, b':' | b'%');
        match self.host {
            Host::Name => {
                if b == b':' {
                    self.host = Host::Port;
                } else if !unreserved(b) && !subdelim(b) && b != b'%' {
                    self.host_valid = false;
                }
            }
            Host::Port => self.host_valid &= b.is_ascii_digit(),
            Host::AfterLiteral => {
                if b == b':' {
                    self.host = Host::Port;
                } else {
                    self.host_valid = false;
                }
            }
            Host::Literal => return Err(Error::InvalidState),
        }
        self.host_start = false;
        if !self.host_valid && (self.had_at || !self.user_possible) {
            return Err(Error::Malformed);
        }
        if b == b'%' {
            self.percent = 2;
        }
        Ok(())
    }
    fn literal(&mut self, b: u8, now: Tick, work: &mut impl Work) -> Result<(), Error> {
        if b == b']' {
            match self.literal {
                Literal::V6 => {
                    // Bound the std parser's input and prepay its small local parse.
                    work.charge(
                        now,
                        Charge {
                            records: 64,
                            ..Charge::default()
                        },
                    )?;
                    let bytes = self.ipv6.get(..self.length).ok_or(Error::InvalidState)?;
                    let text = std::str::from_utf8(bytes).map_err(|_| Error::InvalidState)?;
                    text.parse::<std::net::Ipv6Addr>()
                        .map_err(|_| Error::Malformed)?;
                }
                Literal::FutureBody { any: true } => {}
                _ => return Err(Error::Malformed),
            }
            self.host = Host::AfterLiteral;
            return Ok(());
        }
        match self.literal {
            Literal::Start if matches!(b, b'v' | b'V') => {
                self.literal = Literal::FutureVersion { any: false }
            }
            Literal::Start | Literal::V6 => {
                if !b.is_ascii_hexdigit() && !matches!(b, b':' | b'.') {
                    return Err(Error::Malformed);
                }
                *self.ipv6.get_mut(self.length).ok_or(Error::Malformed)? = b;
                self.length = self.length.checked_add(1).ok_or(Error::InvalidState)?;
                self.literal = Literal::V6;
            }
            Literal::FutureVersion { any } => {
                if b == b'.' && any {
                    self.literal = Literal::FutureBody { any: false };
                } else if b.is_ascii_hexdigit() {
                    self.literal = Literal::FutureVersion { any: true };
                } else {
                    return Err(Error::Malformed);
                }
            }
            Literal::FutureBody { .. } => {
                if !unreserved(b) && !subdelim(b) && b != b':' {
                    return Err(Error::Malformed);
                }
                self.literal = Literal::FutureBody { any: true };
            }
        }
        Ok(())
    }
    pub(super) fn finish(&self) -> Result<(), Error> {
        if self.percent != 0
            || matches!(self.phase, Phase::SchemeStart | Phase::Scheme)
            || (matches!(self.phase, Phase::Authority)
                && (!self.host_valid || matches!(self.host, Host::Literal)))
        {
            return Err(Error::Malformed);
        }
        Ok(())
    }
}

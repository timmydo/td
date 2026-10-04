//! Generic RFC 3986 URI and reference syntax; no resolution or scheme policy.
pub mod unfold;
use crate::{Charge, Work};
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error<E> {
    Malformed,
    Work(E),
    InvalidState,
}
impl<E: std::fmt::Display> std::fmt::Display for Error<E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Malformed => f.write_str("malformed URI spelling"),
            Self::Work(error) => write!(f, "URI work: {error}"),
            Self::InvalidState => f.write_str("invalid URI validator state"),
        }
    }
}
impl<E: std::error::Error> std::error::Error for Error<E> {}
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
    ReferenceStart,
    ReferencePrefix { scheme: bool },
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
/// Caller admits each byte before feeding it; internal IPv6 parsing is prepaid.
/// Complete spelling is passive and grants no source, resolution or I/O authority.
/// ```compile_fail,E0277
/// fn copied<T: Copy>() {} copied::<td_header::uri::Validator<()>>();
/// ```
/// ```compile_fail,E0277
/// fn cloned<T: Clone>() {} cloned::<td_header::uri::Validator<()>>();
/// ```
pub struct Validator<E: Copy> {
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
    complete: bool,
    failure: Option<Error<E>>,
}
impl<E: Copy> Validator<E> {
    pub const fn new() -> Self {
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
            complete: false,
            failure: None,
        }
    }
    /// URI-reference spelling, including relative and empty references.
    /// The caller retains feed/EOF admission; no base or resolution is supplied.
    pub const fn reference() -> Self {
        let mut validator = Self::new();
        validator.phase = Phase::ReferenceStart;
        validator
    }
    pub fn is_complete(&self) -> bool {
        self.failure.is_none() && self.complete
    }
    fn outcome<T>(&mut self, result: Result<T, Error<E>>) -> Result<T, Error<E>> {
        if let Err(error) = result {
            self.failure = Some(error);
        }
        result
    }
    pub fn check_work(&mut self, work: &mut impl Work<Error = E>) -> Result<(), Error<E>> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        let result = work.charge(Charge::default()).map_err(Error::Work);
        self.outcome(result)
    }
    pub fn push(&mut self, b: u8, work: &mut impl Work<Error = E>) -> Result<(), Error<E>> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        let result = if self.complete {
            Err(Error::InvalidState)
        } else {
            self.step(b, work)
        };
        self.outcome(result)
    }
    fn step(&mut self, b: u8, work: &mut impl Work<Error = E>) -> Result<(), Error<E>> {
        if self.percent != 0 {
            if !b.is_ascii_hexdigit() {
                return Err(Error::Malformed);
            }
            self.percent -= 1;
            return Ok(());
        }
        match self.phase {
            Phase::ReferenceStart => match b {
                b'/' => self.phase = Phase::Slash,
                b'?' => self.phase = Phase::Query,
                b'#' => self.phase = Phase::Fragment,
                b':' => return Err(Error::Malformed),
                _ => {
                    self.phase = Phase::ReferencePrefix {
                        scheme: b.is_ascii_alphabetic(),
                    };
                    self.path(b)?;
                }
            },
            Phase::ReferencePrefix { scheme } => {
                if b == b':' {
                    if !scheme {
                        return Err(Error::Malformed);
                    }
                    self.phase = Phase::Hierarchy;
                } else if matches!(b, b'/' | b'?' | b'#') {
                    // Reuse path's query/fragment delimiter transitions.
                    self.phase = Phase::Path;
                    self.path(b)?;
                } else {
                    self.phase = Phase::ReferencePrefix {
                        scheme: scheme
                            && (b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.')),
                    };
                    self.path(b)?;
                }
            }
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
            Phase::Authority => self.authority(b, work)?,
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
    fn path(&mut self, b: u8) -> Result<(), Error<E>> {
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
    fn query(&mut self, b: u8) -> Result<(), Error<E>> {
        if b == b'%' {
            self.percent = 2;
        } else if !pchar(b) && !matches!(b, b'/' | b'?') {
            return Err(Error::Malformed);
        }
        Ok(())
    }
    fn authority(&mut self, b: u8, work: &mut impl Work<Error = E>) -> Result<(), Error<E>> {
        if matches!(self.host, Host::Literal) {
            return self.literal(b, work);
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
    fn literal(&mut self, b: u8, work: &mut impl Work<Error = E>) -> Result<(), Error<E>> {
        if b == b']' {
            match self.literal {
                Literal::V6 => {
                    // Bound the std parser's input and prepay its small local parse.
                    work.charge(Charge {
                        records: 64,
                        ..Charge::default()
                    })
                    .map_err(Error::Work)?;
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
    pub fn finish(&mut self) -> Result<(), Error<E>> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        if self.complete {
            return Ok(());
        }
        let result = self.validate_end();
        self.outcome(result)?;
        self.complete = true;
        Ok(())
    }
    fn validate_end(&self) -> Result<(), Error<E>> {
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

impl<E: Copy> Default for Validator<E> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    #[derive(Default)]
    struct Counter {
        calls: u64,
        records: u64,
        refuse: bool,
    }
    impl Work for Counter {
        type Error = u8;
        fn charge(&mut self, charge: Charge) -> Result<(), u8> {
            self.calls += 1;
            assert_eq!(charge.visits, 0);
            if self.refuse {
                return Err(77);
            }
            self.records += charge.records;
            Ok(())
        }
    }
    fn validate(source: &[u8]) -> (Result<(), Error<u8>>, Counter) {
        let mut work = Counter::default();
        let mut validator = Validator::new();
        for byte in source {
            if let Err(error) = validator.push(*byte, &mut work) {
                return (Err(error), work);
            }
        }
        (validator.finish(), work)
    }
    fn validate_reference(source: &[u8]) -> (Result<(), Error<u8>>, Counter) {
        let mut work = Counter::default();
        let mut validator = Validator::reference();
        for byte in source {
            if let Err(error) = validator.push(*byte, &mut work) {
                return (Err(error), work);
            }
        }
        (validator.finish(), work)
    }
    #[test]
    fn literal_rfc_relative_references_and_scheme_disambiguation() {
        for source in [
            "",
            "g:h",
            "g",
            "./g",
            "g/",
            "/g",
            "//g",
            "?y",
            "g?y",
            "#s",
            "g#s",
            "g?y#s",
            ";x",
            "g;x",
            "g;x?y#s",
            ".",
            "./",
            "..",
            "../",
            "../g",
            "../..",
            "../../",
            "../../g",
            "../../../g",
            "../../../../g",
            "/./g",
            "/../g",
            "g.",
            ".g",
            "g..",
            "..g",
            "./../g",
            "./g/.",
            "g/./h",
            "g/../h",
            "g;x=1/./y",
            "g;x=1/../y",
            "g?y/./x",
            "g?y/../x",
            "g#s/./x",
            "g#s/../x",
            "http:g",
            "http://user:pass@host:42/a",
            "http://[::1]/",
            "g:h/i",
            "A+1.-:x",
            "./g:h",
            "/g:h",
            "1g/h:i",
            "g%3Ah",
            "g?x:y",
            "g#x:y",
            "//user:pass@host:42/a",
            "//[::1]/",
            "//[v1.a:!]/",
            "//",
            "/",
            "///x",
        ] {
            let (result, work) = validate_reference(source.as_bytes());
            assert_eq!(result, Ok(()), "{source}");
            assert_eq!(work.records, if source.contains("[::") { 64 } else { 0 });
        }
        for source in ["", "g", "./g", "/g", "//g", "?y", "#s", ".", ".."] {
            assert_eq!(
                validate(source.as_bytes()).0,
                Err(Error::Malformed),
                "absolute {source}"
            );
        }
    }
    #[test]
    fn malformed_reference_tails_and_first_segment_colons_latch() {
        for source in [
            ":x",
            "1g:h",
            "+g:h",
            ".:x",
            "g_h:x",
            "%67:h",
            "g%20:h",
            "%",
            "g%a",
            "g%xx",
            "g h",
            "g\\h",
            "g#x#y",
            "//host:bad/",
            "//[1::2::3]/",
            "//[v.abc]/",
            "//[::1]x/",
            "[x]",
            "é",
        ] {
            assert_eq!(
                validate_reference(source.as_bytes()).0,
                Err(Error::Malformed),
                "{source}"
            );
        }
        let mut validator = Validator::reference();
        let mut work = Counter::default();
        for byte in b"1g" {
            validator.push(*byte, &mut work).unwrap();
        }
        assert_eq!(validator.push(b':', &mut work), Err(Error::Malformed));
        assert_eq!(
            validator.push(b'/', &mut Counter::default()),
            Err(Error::Malformed)
        );
        assert_eq!(validator.finish(), Err(Error::Malformed));
        assert!(!validator.is_complete());
    }
    #[test]
    fn reference_ipv6_and_empty_completion_refusals_are_sticky() {
        let mut work = Counter {
            refuse: true,
            ..Counter::default()
        };
        let mut validator = Validator::reference();
        for byte in b"//[::1" {
            validator.push(*byte, &mut work).unwrap();
        }
        assert_eq!(work.calls, 0);
        assert_eq!(validator.push(b']', &mut work), Err(Error::Work(77)));
        assert_eq!(work.calls, 1);
        assert!(!validator.is_complete());
        assert_eq!(validator.finish(), Err(Error::Work(77)));
        assert_eq!(
            validator.check_work(&mut Counter::default()),
            Err(Error::Work(77))
        );
        let mut empty = Validator::reference();
        empty.finish().unwrap();
        assert!(empty.is_complete());
        empty.finish().unwrap();
        assert_eq!(empty.check_work(&mut work), Err(Error::Work(77)));
        assert!(!empty.is_complete());
        assert_eq!(empty.finish(), Err(Error::Work(77)));
        let mut fresh = Counter::default();
        assert_eq!(empty.push(b'x', &mut fresh), Err(Error::Work(77)));
        assert_eq!(empty.check_work(&mut fresh), Err(Error::Work(77)));
        assert_eq!(fresh.calls, 0);
        let mut finished = Validator::reference();
        finished.finish().unwrap();
        assert_eq!(finished.push(b'x', &mut fresh), Err(Error::InvalidState));
        assert!(!finished.is_complete());
        assert_eq!(finished.finish(), Err(Error::InvalidState));
        assert_eq!(finished.check_work(&mut fresh), Err(Error::InvalidState));
        assert_eq!(fresh.calls, 0);
        assert!(std::mem::size_of::<Validator<u8>>() <= 128);
    }
    #[test]
    fn literal_uri_spelling_with_required_scheme() {
        for source in [
            "a:",
            "http://",
            "http://@/",
            "https://example.com/a%2Fb?x=y#z",
            "ftp://user:pass@example.com:21/",
            "mailto:John.Doe@example.com",
            "urn:example:animal:ferret:nose",
            "http://[2001:db8::1]:80/a",
            "http://[::ffff:192.0.2.1]/",
            "http://[vF.a:!]/",
            "http://[VF.a:!]/",
            "a:x?%5Bx%5D#%5by%5d",
            "a:/a/b",
            "a:a:b/c",
            "a://foo:bar@host/",
            "a:?#",
            "a:%ff",
        ] {
            let (result, work) = validate(source.as_bytes());
            assert_eq!(result, Ok(()), "{source}");
            assert_eq!(
                work.records,
                if source.contains('[') && !(source.contains("[v") || source.contains("[V")) {
                    64
                } else {
                    0
                }
            );
        }
        assert!(std::mem::size_of::<Validator<u8>>() <= 128);
    }
    #[test]
    fn malformed_prefix_tail_and_authority_are_not_complete() {
        for source in [
            "",
            "relative/path",
            "//example.com/a",
            "1:x",
            "a",
            "a+",
            "a:%",
            "a:%a",
            "a:%x1",
            "a:space here",
            "a:\n",
            "a:é",
            "a://host:port/",
            "a://a@b@c/",
            "a://[::1",
            "a://[xyz]/",
            "a://[1::2::3]/",
            "a://[]/",
            "a://[v.abc]/",
            "a://[v1.]/",
            "a://[::1]tail/",
            "a:/[x]",
            "a:x#x#y",
            "a:x?[x]",
            "a:x#[x]",
        ] {
            assert_eq!(
                validate(source.as_bytes()).0,
                Err(Error::Malformed),
                "{source}"
            );
        }
        let long = format!("a://[{}]/", "1".repeat(46));
        assert_eq!(validate(long.as_bytes()).0, Err(Error::Malformed));
    }
    #[test]
    fn ipv6_parse_refusal_is_prepaid_and_sticky() {
        let mut validator = Validator::new();
        let mut work = Counter {
            refuse: true,
            ..Counter::default()
        };
        for byte in b"http://[::1" {
            validator.push(*byte, &mut work).unwrap();
        }
        assert_eq!(work.calls, 0);
        assert_eq!(validator.push(b']', &mut work), Err(Error::Work(77)));
        assert_eq!(work.calls, 1);
        assert_eq!(work.records, 0);
        let mut fresh = Counter::default();
        assert_eq!(validator.push(b']', &mut fresh), Err(Error::Work(77)));
        assert_eq!(validator.finish(), Err(Error::Work(77)));
        assert_eq!(validator.check_work(&mut fresh), Err(Error::Work(77)));
        assert!(!validator.is_complete());
        assert_eq!(fresh.calls, 0);
    }
    #[test]
    fn final_syntax_and_fresh_admission_never_revive_failure() {
        let mut work = Counter::default();
        let mut bad = Validator::new();
        bad.push(b'1', &mut work).unwrap_err();
        assert_eq!(bad.push(b'a', &mut work), Err(Error::Malformed));
        assert_eq!(bad.finish(), Err(Error::Malformed));
        assert_eq!(bad.check_work(&mut work), Err(Error::Malformed));
        assert_eq!(work.calls, 0);
        let mut tail = Validator::new();
        for byte in b"a:%" {
            tail.push(*byte, &mut work).unwrap();
        }
        assert_eq!(tail.finish(), Err(Error::Malformed));
        assert_eq!(
            tail.push(b'4', &mut Counter::default()),
            Err(Error::Malformed)
        );
        assert_eq!(tail.finish(), Err(Error::Malformed));
        assert_eq!(tail.check_work(&mut work), Err(Error::Malformed));
        assert!(!tail.is_complete());
        assert_eq!(work.calls, 0);
        let mut good = Validator::new();
        for byte in b"a:" {
            good.push(*byte, &mut work).unwrap();
        }
        assert!(!good.is_complete());
        good.finish().unwrap();
        assert!(good.is_complete());
        good.finish().unwrap();
        assert_eq!(work.calls, 0);
        good.check_work(&mut work).unwrap();
        assert_eq!(work.calls, 1);
        assert_eq!(work.records, 0);
        work.refuse = true;
        assert_eq!(good.check_work(&mut work), Err(Error::Work(77)));
        assert!(!good.is_complete());
        assert_eq!(good.finish(), Err(Error::Work(77)));
        assert_eq!(
            good.push(b'x', &mut Counter::default()),
            Err(Error::Work(77))
        );
        let mut finished = Validator::new();
        for byte in b"a:" {
            finished.push(*byte, &mut Counter::default()).unwrap();
        }
        finished.finish().unwrap();
        assert_eq!(
            finished.push(b'x', &mut Counter::default()),
            Err(Error::InvalidState)
        );
        assert!(!finished.is_complete());
        assert_eq!(finished.finish(), Err(Error::InvalidState));
    }
}

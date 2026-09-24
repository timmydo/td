//! Bounded local routing candidate. No SMTP grammar/authorization or store I/O.
use super::syntax::Location;
use crate::{format::row::MAX_ADDRESS, ids::AccountId};
use std::{cmp::Ordering, fmt, num::NonZeroU32};

pub const MAX_DOMAINS: usize = 256;
pub const MAX_ALIASES: usize = 4096;
pub const MAX_TEXT_BYTES: usize = 320 * 1024;
pub const MAX_DOMAIN_BYTES: usize = 243; // The mandatory postmaster address fits 254.
const MAX_LOCAL_BYTES: usize = 64;
const ORIGIN: Location = Location {
    line: NonZeroU32::MIN,
    column: 1,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Code {
    Capacity,
    InvalidDomain,
    InvalidAddress,
    SecondAccount,
    MissingAccount,
    MissingDomain,
    DuplicateDomain,
    DuplicateAlias,
    UnknownDomain,
    UnknownAccount,
    Invariant,
}
impl Code {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Capacity => "config_route_capacity",
            Self::InvalidDomain => "config_route_invalid_domain",
            Self::InvalidAddress => "config_route_invalid_address",
            Self::SecondAccount => "config_route_second_account",
            Self::MissingAccount => "config_route_missing_account",
            Self::MissingDomain => "config_route_missing_domain",
            Self::DuplicateDomain => "config_route_duplicate_domain",
            Self::DuplicateAlias => "config_route_duplicate_alias",
            Self::UnknownDomain => "config_route_unknown_domain",
            Self::UnknownAccount => "config_route_unknown_account",
            Self::Invariant => "config_route_invariant",
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
        Ok(())
    }
}
impl std::error::Error for Error {}
fn error(code: Code, location: Option<Location>) -> Error {
    Error {
        code,
        location,
        previous: None,
    }
}
fn invariant() -> Error {
    error(Code::Invariant, None)
}
#[derive(Clone, Copy)]
struct TextRef {
    offset: u32,
    length: u32,
}
impl TextRef {
    const EMPTY: Self = Self {
        offset: 0,
        length: 0,
    };
    fn get(self, text: &[u8]) -> Result<&[u8], Error> {
        let start = usize::try_from(self.offset).map_err(|_| invariant())?;
        let length = usize::try_from(self.length).map_err(|_| invariant())?;
        let end = start.checked_add(length).ok_or_else(invariant)?;
        text.get(start..end).ok_or_else(invariant)
    }
}
/// Opaque caller-owned cell. Only the builder's used prefix is live.
#[derive(Clone, Copy)]
pub struct AliasSlot {
    local: TextRef,
    account: AccountId,
    line: NonZeroU32,
    column: u16,
    domain: u8,
}
impl AliasSlot {
    pub const EMPTY: Self = Self {
        local: TextRef::EMPTY,
        account: AccountId::from_bytes([0; 16]),
        line: ORIGIN.line,
        column: ORIGIN.column,
        domain: 0,
    };
    fn location(self) -> Location {
        Location {
            line: self.line,
            column: self.column,
        }
    }
}
#[derive(Clone, Copy)]
pub struct DomainSlot {
    key: TextRef,
    line: NonZeroU32,
    column: u16,
    declared: bool,
    original: u8,
}
impl DomainSlot {
    pub const EMPTY: Self = Self {
        key: TextRef::EMPTY,
        line: ORIGIN.line,
        column: ORIGIN.column,
        declared: false,
        original: 0,
    };
    fn location(self) -> Location {
        Location {
            line: self.line,
            column: self.column,
        }
    }
}
fn domain_valid(input: &str) -> bool {
    !input.is_empty()
        && input.len() <= MAX_DOMAIN_BYTES
        && input.is_ascii()
        && input
            .rsplit('.')
            .next()
            .is_some_and(|label| !label.bytes().all(|b| b.is_ascii_digit()))
        && input.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && label
                    .as_bytes()
                    .first()
                    .is_some_and(u8::is_ascii_alphanumeric)
                && label
                    .as_bytes()
                    .last()
                    .is_some_and(u8::is_ascii_alphanumeric)
                && label
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        })
}
fn atext(b: u8) -> bool {
    b.is_ascii_alphanumeric()
        || matches!(
            b,
            b'!' | b'#'
                | b'$'
                | b'%'
                | b'&'
                | b'\''
                | b'*'
                | b'+'
                | b'-'
                | b'/'
                | b'='
                | b'?'
                | b'^'
                | b'_'
                | b'`'
                | b'{'
                | b'|'
                | b'}'
                | b'~'
        )
}
fn put(output: &mut [u8], length: &mut usize, byte: u8) -> Result<(), Code> {
    *output.get_mut(*length).ok_or(Code::InvalidAddress)? = byte;
    *length = length.checked_add(1).ok_or(Code::InvalidAddress)?;
    Ok(())
}
/// Internal canonical key: decoded local part, NUL separator, folded domain.
/// This is not a message-header or complete SMTP path parser.
fn address_key<'a>(input: &str, output: &'a mut [u8; MAX_ADDRESS]) -> Result<&'a [u8], Code> {
    if input.len() > MAX_ADDRESS || !input.is_ascii() {
        return Err(Code::InvalidAddress);
    }
    let (local, domain) = input.rsplit_once('@').ok_or(Code::InvalidAddress)?;
    if !domain_valid(domain) || local.is_empty() || local.len() > MAX_LOCAL_BYTES {
        return Err(Code::InvalidAddress);
    }
    let mut length = 0;
    if let Some(body) = local.strip_prefix('"') {
        let body = body.strip_suffix('"').ok_or(Code::InvalidAddress)?;
        let mut bytes = body.bytes();
        while let Some(byte) = bytes.next() {
            let byte = if byte == b'\\' {
                let escaped = bytes.next().ok_or(Code::InvalidAddress)?;
                if !(32..=126).contains(&escaped) {
                    return Err(Code::InvalidAddress);
                }
                escaped
            } else {
                if !(32..=126).contains(&byte) || byte == b'"' {
                    return Err(Code::InvalidAddress);
                }
                byte
            };
            put(output, &mut length, byte)?;
        }
        if length == 0 {
            return Err(Code::InvalidAddress);
        }
    } else {
        if !local
            .split('.')
            .all(|atom| !atom.is_empty() && atom.bytes().all(atext))
        {
            return Err(Code::InvalidAddress);
        }
        for byte in local.bytes() {
            put(output, &mut length, byte)?;
        }
    }
    let decoded = output.get_mut(..length).ok_or(Code::InvalidAddress)?;
    if decoded.eq_ignore_ascii_case(b"postmaster") {
        decoded.make_ascii_lowercase();
    }
    put(output, &mut length, 0)?;
    for byte in domain.bytes() {
        put(output, &mut length, byte.to_ascii_lowercase())?;
    }
    output.get(..length).ok_or(Code::InvalidAddress)
}
fn key_parts(key: &[u8]) -> Result<(&[u8], &[u8]), Error> {
    let split = key.iter().position(|b| *b == 0).ok_or_else(invariant)?;
    let after = split.checked_add(1).ok_or_else(invariant)?;
    Ok((
        key.get(..split).ok_or_else(invariant)?,
        key.get(after..).ok_or_else(invariant)?,
    ))
}

pub struct Builder<'a> {
    text: &'a mut [u8],
    text_length: usize,
    domains: &'a mut [DomainSlot],
    domain_count: usize,
    aliases: &'a mut [AliasSlot],
    alias_count: usize,
    account: Option<(AccountId, Location)>,
    failure: Option<Error>,
}
impl<'a> Builder<'a> {
    pub fn new(
        text: &'a mut [u8],
        domains: &'a mut [DomainSlot],
        aliases: &'a mut [AliasSlot],
    ) -> Result<Self, Error> {
        if text.is_empty()
            || text.len() > MAX_TEXT_BYTES
            || domains.is_empty()
            || domains.len() > MAX_DOMAINS
            || aliases.len() > MAX_ALIASES
            || std::mem::size_of::<AliasSlot>() > 32
            || std::mem::size_of::<DomainSlot>() > 16
        {
            return Err(error(Code::Capacity, None));
        }
        Ok(Self {
            text,
            text_length: 0,
            domains,
            domain_count: 0,
            aliases,
            alias_count: 0,
            account: None,
            failure: None,
        })
    }
    fn record<T>(&mut self, result: Result<T, Error>, at: Location) -> Result<T, Error> {
        let result = result.map_err(|mut e| {
            if e.location.is_none() {
                e.location = Some(at);
            }
            e
        });
        if let Err(e) = result {
            self.failure = Some(e);
        }
        result
    }
    fn store(&mut self, bytes: &[u8], at: Location) -> Result<TextRef, Error> {
        let end = self
            .text_length
            .checked_add(bytes.len())
            .ok_or(error(Code::Capacity, Some(at)))?;
        let reference = TextRef {
            offset: u32::try_from(self.text_length).map_err(|_| invariant())?,
            length: u32::try_from(bytes.len()).map_err(|_| invariant())?,
        };
        self.text
            .get_mut(self.text_length..end)
            .ok_or(error(Code::Capacity, Some(at)))?
            .copy_from_slice(bytes);
        self.text_length = end;
        Ok(reference)
    }
    pub fn account(&mut self, account: AccountId, at: Location) -> Result<(), Error> {
        if let Some(e) = self.failure {
            return Err(e);
        }
        if let Some((_, previous)) = self.account {
            let mut e = error(Code::SecondAccount, Some(at));
            e.previous = Some(previous);
            return self.record(Err(e), at);
        }
        self.account = Some((account, at));
        Ok(())
    }
    pub fn domain(&mut self, name: &str, at: Location) -> Result<(), Error> {
        if let Some(e) = self.failure {
            return Err(e);
        }
        let result = self.domain_inner(name, at);
        self.record(result, at)
    }
    fn intern_domain(&mut self, bytes: &[u8], at: Location) -> Result<u8, Error> {
        let used = self
            .domains
            .get(..self.domain_count)
            .ok_or_else(invariant)?;
        for (index, row) in used.iter().enumerate() {
            if row.key.get(self.text)? == bytes {
                return u8::try_from(index).map_err(|_| invariant());
            }
        }
        if self.domain_count >= self.domains.len() {
            return Err(error(Code::Capacity, Some(at)));
        }
        let index = u8::try_from(self.domain_count).map_err(|_| invariant())?;
        let key = self.store(bytes, at)?;
        *self
            .domains
            .get_mut(self.domain_count)
            .ok_or_else(invariant)? = DomainSlot {
            key,
            line: at.line,
            column: at.column,
            declared: false,
            original: index,
        };
        self.domain_count = self.domain_count.checked_add(1).ok_or_else(invariant)?;
        Ok(index)
    }
    fn domain_inner(&mut self, name: &str, at: Location) -> Result<(), Error> {
        if !domain_valid(name) {
            return Err(error(Code::InvalidDomain, Some(at)));
        }
        let mut lower = [0; MAX_DOMAIN_BYTES];
        let bytes = lower.get_mut(..name.len()).ok_or_else(invariant)?;
        for (cell, byte) in bytes.iter_mut().zip(name.bytes()) {
            *cell = byte.to_ascii_lowercase();
        }
        let index = self.intern_domain(bytes, at)?;
        let row = self
            .domains
            .get_mut(usize::from(index))
            .ok_or_else(invariant)?;
        if row.declared {
            let mut e = error(Code::DuplicateDomain, Some(at));
            e.previous = Some(row.location());
            return Err(e);
        }
        row.declared = true;
        row.line = at.line;
        row.column = at.column;
        Ok(())
    }

    pub fn alias(&mut self, address: &str, account: AccountId, at: Location) -> Result<(), Error> {
        if let Some(e) = self.failure {
            return Err(e);
        }
        let result = self.alias_inner(address, account, at);
        self.record(result, at)
    }
    fn alias_inner(
        &mut self,
        address: &str,
        account: AccountId,
        at: Location,
    ) -> Result<(), Error> {
        let mut buffer = [0; MAX_ADDRESS];
        let bytes = address_key(address, &mut buffer).map_err(|code| error(code, Some(at)))?;
        if self.alias_count >= self.aliases.len() {
            return Err(error(Code::Capacity, Some(at)));
        }
        let (local, domain) = key_parts(bytes)?;
        let domain = self.intern_domain(domain, at)?;
        let local = self.store(local, at)?;
        *self
            .aliases
            .get_mut(self.alias_count)
            .ok_or_else(invariant)? = AliasSlot {
            local,
            account,
            line: at.line,
            column: at.column,
            domain,
        };
        self.alias_count = self.alias_count.checked_add(1).ok_or_else(invariant)?;
        Ok(())
    }

    pub fn finish(self) -> Result<Routing<'a>, Error> {
        if let Some(e) = self.failure {
            return Err(e);
        }
        let (account, _) = self.account.ok_or(error(Code::MissingAccount, None))?;
        let text = self.text.get(..self.text_length).ok_or_else(invariant)?;
        let domains = self
            .domains
            .get_mut(..self.domain_count)
            .ok_or_else(invariant)?;
        let aliases = self
            .aliases
            .get_mut(..self.alias_count)
            .ok_or_else(invariant)?;
        if !domains.iter().any(|row| row.declared) {
            return Err(error(Code::MissingDomain, None));
        }
        for row in domains.iter() {
            row.key.get(text)?;
        }
        for row in aliases.iter() {
            row.local.get(text)?;
            if row.account != account {
                return Err(error(Code::UnknownAccount, Some(row.location())));
            }
            if !domains
                .get(usize::from(row.domain))
                .ok_or_else(invariant)?
                .declared
            {
                return Err(error(Code::UnknownDomain, Some(row.location())));
            }
        }
        // Private refs were checked above; offsets provide a total tie-break.
        domains.sort_unstable_by(|a, b| {
            a.key
                .get(text)
                .ok()
                .cmp(&b.key.get(text).ok())
                .then_with(|| a.key.offset.cmp(&b.key.offset))
        });
        let mut remap = [0u8; MAX_DOMAINS];
        for (index, row) in domains.iter().enumerate() {
            *remap
                .get_mut(usize::from(row.original))
                .ok_or_else(invariant)? = u8::try_from(index).map_err(|_| invariant())?;
        }
        for row in aliases.iter_mut() {
            row.domain = *remap.get(usize::from(row.domain)).ok_or_else(invariant)?;
        }
        aliases.sort_unstable_by(|a, b| {
            a.domain
                .cmp(&b.domain)
                .then_with(|| a.local.get(text).ok().cmp(&b.local.get(text).ok()))
                .then_with(|| a.line.cmp(&b.line))
                .then_with(|| a.column.cmp(&b.column))
                .then_with(|| a.local.offset.cmp(&b.local.offset))
        });
        for pair in aliases.windows(2) {
            if let [a, b] = pair {
                if a.domain == b.domain && a.local.get(text)? == b.local.get(text)? {
                    let mut e = error(Code::DuplicateAlias, Some(b.location()));
                    e.previous = Some(a.location());
                    return Err(e);
                }
            }
        }
        Ok(Routing {
            text,
            domains,
            aliases,
            account,
        })
    }
}
fn search<T>(
    rows: &[T],
    compare: impl Fn(&T) -> Result<Ordering, Error>,
) -> Result<Option<usize>, Error> {
    let mut low = 0usize;
    let mut high = rows.len();
    while low < high {
        let half = high.checked_sub(low).ok_or_else(invariant)? / 2;
        let middle = low.checked_add(half).ok_or_else(invariant)?;
        let row = rows.get(middle).ok_or_else(invariant)?;
        match compare(row)? {
            Ordering::Less => low = middle.checked_add(1).ok_or_else(invariant)?,
            Ordering::Equal => return Ok(Some(middle)),
            Ordering::Greater => high = middle,
        }
    }
    Ok(None)
}

pub struct Routing<'a> {
    text: &'a [u8],
    domains: &'a [DomainSlot],
    aliases: &'a [AliasSlot],
    account: AccountId,
}
impl fmt::Debug for Routing<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Routing(<redacted>)")
    }
}
impl Routing<'_> {
    pub fn account(&self) -> AccountId {
        self.account
    }
    pub fn domain_count(&self) -> usize {
        self.domains.len()
    }
    pub fn alias_count(&self) -> usize {
        self.aliases.len()
    }
    pub fn text_bytes(&self) -> usize {
        self.text.len()
    }
    /// Unsupported/malformed/unconfigured spellings have no route. The protocol
    /// parser separately owns SMTP command/path errors and envelope preservation.
    pub fn resolve(&self, input: &str) -> Result<Option<AccountId>, Error> {
        if input.eq_ignore_ascii_case("postmaster") {
            return Ok(Some(self.account));
        }
        let mut buffer = [0; MAX_ADDRESS];
        let Ok(key) = address_key(input, &mut buffer) else {
            return Ok(None);
        };
        let (local, domain) = key_parts(key)?;
        let Some(domain_index) = search(self.domains, |r| Ok(r.key.get(self.text)?.cmp(domain)))?
        else {
            return Ok(None);
        };
        if local == b"postmaster" {
            return Ok(Some(self.account));
        }
        let found = search(self.aliases, |r| {
            Ok(usize::from(r.domain)
                .cmp(&domain_index)
                .then(r.local.get(self.text)?.cmp(local)))
        })?;
        Ok(found.map(|_| self.account))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    const ACCOUNT: AccountId = AccountId::from_bytes([1; 16]);
    fn at(line: u32) -> Location {
        Location {
            line: NonZeroU32::new(line).unwrap(),
            column: 1,
        }
    }
    #[test]
    fn quoted_equivalence_domain_folding_and_reserved_routes() {
        let mut text = [0; 4096];
        let mut domains = [DomainSlot::EMPTY; 4];
        let mut aliases = [AliasSlot::EMPTY; 8];
        let mut b = Builder::new(&mut text, &mut domains, &mut aliases).unwrap();
        b.alias("User@Example.TEST", ACCOUNT, at(1)).unwrap();
        b.alias(r#""a@b"@EXAMPLE.TEST"#, ACCOUNT, at(2)).unwrap();
        b.alias(r#""a\\\"b"@other.test"#, ACCOUNT, at(3)).unwrap();
        b.alias("literal+tag@example.test", ACCOUNT, at(4)).unwrap();
        b.domain("OTHER.test", at(5)).unwrap();
        b.domain("example.test", at(6)).unwrap();
        b.account(ACCOUNT, at(7)).unwrap();
        let routes = b.finish().unwrap();
        assert_eq!(routes.account(), ACCOUNT);
        assert_eq!(routes.domain_count(), 2);
        assert_eq!(routes.alias_count(), 4);
        for address in [
            "User@example.test",
            r#""U\ser"@Example.Test"#,
            r#""a@b"@example.test"#,
            r#""a\\\"b"@OTHER.test"#,
            "literal+tag@example.test",
            "POSTMASTER",
            "postmaster@example.test",
            r#""PoStMaStEr"@OTHER.TEST"#,
        ] {
            assert_eq!(routes.resolve(address), Ok(Some(ACCOUNT)), "{address}");
        }
        for address in [
            "user@example.test",
            "User+tag@example.test",
            "literal@example.test",
            "nobody@example.test",
            "postmaster@foreign.test",
            "a@foreign.test",
            "",
            "<postmaster>",
            "\"postmaster\"",
            "\"PoStMaStEr\"",
            "\r\npostmaster",
            "User@example.test.",
            "User@[127.0.0.1]",
        ] {
            assert_eq!(routes.resolve(address), Ok(None), "{address}");
        }
        assert_eq!(format!("{routes:?}"), "Routing(<redacted>)");
    }
    #[test]
    fn duplicates_are_canonical_and_keep_original_locations() {
        for (first, second, duplicate_line) in [
            ("User@example.test", r#""User"@EXAMPLE.TEST"#, 4),
            ("postmaster@example.test", "POSTMASTER@EXAMPLE.TEST", 4),
            ("User@example.test", r#""User"@EXAMPLE.TEST"#, 3),
        ] {
            let mut text = [0; 512];
            let mut domains = [DomainSlot::EMPTY; 1];
            let mut aliases = [AliasSlot::EMPTY; 2];
            let mut b = Builder::new(&mut text, &mut domains, &mut aliases).unwrap();
            b.account(ACCOUNT, at(1)).unwrap();
            b.domain("example.test", at(2)).unwrap();
            b.alias(second, ACCOUNT, at(duplicate_line)).unwrap();
            b.alias(first, ACCOUNT, at(3)).unwrap();
            let e = b.finish().unwrap_err();
            assert_eq!(e.code, Code::DuplicateAlias);
            assert_eq!(e.location, Some(at(duplicate_line)));
            assert_eq!(e.previous, Some(at(3)));
        }
        let mut text = [0; 512];
        let mut domains = [DomainSlot::EMPTY; 2];
        let mut aliases = [];
        let mut b = Builder::new(&mut text, &mut domains, &mut aliases).unwrap();
        b.account(ACCOUNT, at(1)).unwrap();
        b.domain("EXAMPLE.test", at(2)).unwrap();
        let e = b.domain("example.TEST", at(3)).unwrap_err();
        assert_eq!(b.finish().unwrap_err(), e);
        assert_eq!(e.code, Code::DuplicateDomain);
        assert_eq!(e.previous, Some(at(2)));
    }
    #[test]
    fn absent_account_domain_and_dangling_targets_refuse_publication() {
        for (set_account, set_domain, address, target, expected) in [
            (false, true, None, ACCOUNT, Code::MissingAccount),
            (true, false, None, ACCOUNT, Code::MissingDomain),
            (
                true,
                true,
                Some("a@other.test"),
                ACCOUNT,
                Code::UnknownDomain,
            ),
            (
                true,
                true,
                Some("postmaster@other.test"),
                ACCOUNT,
                Code::UnknownDomain,
            ),
            (
                true,
                true,
                Some("a@example.test"),
                AccountId::from_bytes([2; 16]),
                Code::UnknownAccount,
            ),
            (
                true,
                true,
                Some("postmaster@example.test"),
                AccountId::from_bytes([2; 16]),
                Code::UnknownAccount,
            ),
        ] {
            let mut text = [0; 512];
            let mut domains = [DomainSlot::EMPTY; 2];
            let mut aliases = [AliasSlot::EMPTY; 1];
            let mut b = Builder::new(&mut text, &mut domains, &mut aliases).unwrap();
            if set_account {
                b.account(ACCOUNT, at(1)).unwrap();
            }
            if set_domain {
                b.domain("example.test", at(2)).unwrap();
            }
            if let Some(address) = address {
                b.alias(address, target, at(3)).unwrap();
            }
            assert_eq!(b.finish().unwrap_err().code, expected);
        }
        let mut text = [0; 512];
        let mut domains = [DomainSlot::EMPTY; 2];
        let mut aliases = [];
        let mut b = Builder::new(&mut text, &mut domains, &mut aliases).unwrap();
        b.account(ACCOUNT, at(1)).unwrap();
        let e = b.account(ACCOUNT, at(2)).unwrap_err();
        assert_eq!(e.code, Code::SecondAccount);
        assert_eq!(e.previous, Some(at(1)));
        assert_eq!(b.domain("example.test", at(3)), Err(e));
        assert_eq!(b.finish().unwrap_err(), e);
    }
    #[test]
    fn address_literal_oracles_and_length_limits() {
        let mut output = [0; MAX_ADDRESS];
        for (address, expected) in [
            ("a.b+tag@EXAMPLE.test", b"a.b+tag\0example.test".as_slice()),
            (r#""a\ b"@example.test"#, b"a b\0example.test".as_slice()),
            (
                r#""a\"b\\c"@example.test"#,
                b"a\"b\\c\0example.test".as_slice(),
            ),
            (
                r#""postmaster"@EXAMPLE.TEST"#,
                b"postmaster\0example.test".as_slice(),
            ),
        ] {
            assert_eq!(address_key(address, &mut output).unwrap(), expected);
        }
        for address in [
            "@example.test",
            "a..b@example.test",
            ".a@example.test",
            "a.@example.test",
            "a b@example.test",
            r#"""@example.test"#,
            r#""a"b"@example.test"#,
            r#""a\"@example.test"#,
            "é@example.test",
            "a@é.test",
            "a@-example.test",
            "a@example-.test",
            "a@ex_ample.test",
            "a@example..test",
            "a@example.test.",
            "a@[127.0.0.1]",
            "a@x\n",
            "a\0b@example.test",
            "<a@example.test>",
        ] {
            assert_eq!(
                address_key(address, &mut output),
                Err(Code::InvalidAddress),
                "{address:?}"
            );
        }
        let local = "x".repeat(MAX_LOCAL_BYTES);
        assert!(address_key(&format!("{local}@example.test"), &mut output).is_ok());
        assert!(address_key(&format!("x{local}@example.test"), &mut output).is_err());
        let domain = format!(
            "{}.{}.{}.{}",
            "x".repeat(63),
            "x".repeat(63),
            "x".repeat(63),
            "x".repeat(51)
        );
        assert_eq!(domain.len(), MAX_DOMAIN_BYTES);
        assert!(domain_valid(&domain));
        assert!(!domain_valid(&format!("x{domain}")));
        assert_eq!(format!("postmaster@{domain}").len(), MAX_ADDRESS);
        assert!(address_key(&format!("postmaster@{domain}"), &mut output).is_ok());
        assert!(address_key(&format!("xpostmaster@{domain}"), &mut output).is_err());
        assert!(!domain_valid(&format!("{}.test", "x".repeat(64))));
    }
    #[test]
    fn bounded_storage_failures_poison_and_never_grow() {
        assert!(std::mem::size_of::<AliasSlot>() <= 32);
        assert!(std::mem::size_of::<DomainSlot>() <= 16);
        for text_len in [1, 12] {
            let mut text = vec![0xaa; text_len];
            let mut domains = [DomainSlot::EMPTY; 1];
            let mut aliases = [];
            let mut b = Builder::new(&mut text, &mut domains, &mut aliases).unwrap();
            b.account(ACCOUNT, at(1)).unwrap();
            if text_len == 1 {
                let e = b.domain("example.test", at(2)).unwrap_err();
                assert_eq!(e.code, Code::Capacity);
                assert_eq!(b.finish().unwrap_err(), e);
            } else {
                b.domain("example.test", at(2)).unwrap();
                let routes = b.finish().unwrap();
                assert_eq!(routes.text_bytes(), 12);
                assert_eq!(routes.resolve("postmaster"), Ok(Some(ACCOUNT)));
            }
        }
        let mut text = [0; 512];
        let mut domains = [DomainSlot::EMPTY; 1];
        let mut aliases = [];
        let mut b = Builder::new(&mut text, &mut domains, &mut aliases).unwrap();
        b.account(ACCOUNT, at(1)).unwrap();
        b.domain("example.test", at(2)).unwrap();
        let e = b.alias("a@example.test", ACCOUNT, at(3)).unwrap_err();
        assert_eq!(e.code, Code::Capacity);
        assert_eq!(b.domain("other.test", at(4)), Err(e));
        assert_eq!(b.finish().unwrap_err(), e);
        let mut text = [0; 512];
        let mut domains = [DomainSlot::EMPTY; 1];
        let mut aliases = [AliasSlot::EMPTY; 1];
        let mut b = Builder::new(&mut text, &mut domains, &mut aliases).unwrap();
        b.domain("example.test", at(1)).unwrap();
        let e = b.domain("other.test", at(2)).unwrap_err();
        assert_eq!(e.code, Code::Capacity);
        assert_eq!(b.account(ACCOUNT, at(3)), Err(e));
    }
    #[test]
    fn hostile_input_never_appears_in_errors() {
        let mut text = [0; 512];
        let mut domains = [DomainSlot::EMPTY; 1];
        let mut aliases = [AliasSlot::EMPTY; 1];
        let mut b = Builder::new(&mut text, &mut domains, &mut aliases).unwrap();
        let e = b
            .alias("private_fixture\n@example.test", ACCOUNT, at(19))
            .unwrap_err();
        assert_eq!(e.code, Code::InvalidAddress);
        assert_eq!(e.location, Some(at(19)));
        assert!(!format!("{e} {e:?}").contains("private_fixture"));
        assert_eq!(b.account(ACCOUNT, at(20)), Err(e));
        assert_eq!(b.finish().unwrap_err(), e);
    }
    #[test]
    fn full_tables_sort_search_and_enforce_constructor_ceilings() {
        let mut text = vec![0; MAX_TEXT_BYTES];
        let mut domains = vec![DomainSlot::EMPTY; MAX_DOMAINS];
        let mut aliases = vec![AliasSlot::EMPTY; MAX_ALIASES];
        let mut b = Builder::new(&mut text, &mut domains, &mut aliases).unwrap();
        b.account(ACCOUNT, at(1)).unwrap();
        for index in (0..MAX_DOMAINS).rev() {
            b.domain(
                &format!("d{index}.test"),
                at(u32::try_from(index + 2).unwrap()),
            )
            .unwrap();
        }
        for index in (0..MAX_ALIASES).rev() {
            b.alias(
                &format!("a{index}@d{}.test", index % MAX_DOMAINS),
                ACCOUNT,
                at(u32::try_from(index + 300).unwrap()),
            )
            .unwrap();
        }
        let routes = b.finish().unwrap();
        assert_eq!(routes.alias_count(), MAX_ALIASES);
        assert_eq!(routes.domain_count(), MAX_DOMAINS);
        for index in 0..MAX_ALIASES {
            assert_eq!(
                routes.resolve(&format!("a{index}@d{}.test", index % MAX_DOMAINS)),
                Ok(Some(ACCOUNT))
            );
        }
        assert_eq!(routes.resolve("a4096@d0.test"), Ok(None));
        for (text_len, domain_len, alias_len) in [
            (0, 1, 1),
            (MAX_TEXT_BYTES + 1, 1, 1),
            (1, 0, 1),
            (1, MAX_DOMAINS + 1, 1),
            (1, 1, MAX_ALIASES + 1),
        ] {
            let mut text = vec![0xaa; text_len];
            let mut domains = vec![DomainSlot::EMPTY; domain_len];
            let mut aliases = vec![AliasSlot::EMPTY; alias_len];
            assert_eq!(
                Builder::new(&mut text, &mut domains, &mut aliases)
                    .err()
                    .unwrap()
                    .code,
                Code::Capacity
            );
            assert!(text.iter().all(|byte| *byte == 0xaa));
        }
    }
    #[test]
    fn rejected_domains_poison_with_locations() {
        for domain in ["", "ex_ample.test", "a.", "-x", "127.0.0.1", "1", "x.123"] {
            let mut text = [0; 512];
            let mut domains = [DomainSlot::EMPTY; 2];
            let mut aliases = [];
            let mut b = Builder::new(&mut text, &mut domains, &mut aliases).unwrap();
            let e = b.domain(domain, at(9)).unwrap_err();
            assert_eq!(e.code, Code::InvalidDomain, "{domain}");
            assert_eq!(e.location, Some(at(9)));
            assert_eq!(b.domain("example.test", at(10)), Err(e));
            assert_eq!(b.finish().unwrap_err(), e);
        }
    }
    #[test]
    fn serialized_quoted_local_part_bounds() {
        let mut output = [0; MAX_ADDRESS];
        for body in ["x".repeat(62), format!("{}\\x", "x".repeat(60))] {
            let accepted = format!("\"{body}\"@example.test");
            let refused = format!("\"x{body}\"@example.test");
            assert!(address_key(&accepted, &mut output).is_ok());
            assert_eq!(
                address_key(&refused, &mut output),
                Err(Code::InvalidAddress)
            );
        }
    }
    #[test]
    fn alias_text_exhaustion_and_shared_domain_storage() {
        for capacity in [12, 14] {
            let mut text = vec![0; capacity];
            let mut domains = [DomainSlot::EMPTY; 1];
            let mut aliases = [AliasSlot::EMPTY; 2];
            let mut b = Builder::new(&mut text, &mut domains, &mut aliases).unwrap();
            b.account(ACCOUNT, at(1)).unwrap();
            b.domain("example.test", at(2)).unwrap();
            let result = b.alias("a@EXAMPLE.test", ACCOUNT, at(3));
            if capacity == 12 {
                let e = result.unwrap_err();
                assert_eq!(e.code, Code::Capacity);
                assert_eq!(e.location, Some(at(3)));
                assert_eq!(b.account(ACCOUNT, at(4)), Err(e));
                assert_eq!(b.finish().unwrap_err(), e);
            } else {
                result.unwrap();
                b.alias("b@example.test", ACCOUNT, at(4)).unwrap();
                let routes = b.finish().unwrap();
                assert_eq!(routes.text_bytes(), 14);
                assert_eq!(routes.domain_count(), 1);
                assert_eq!(routes.resolve("b@example.test"), Ok(Some(ACCOUNT)));
            }
        }
    }
    #[test]
    fn full_counts_with_long_names_fit_text_reservation() {
        const {
            assert!(
                MAX_DOMAINS * MAX_DOMAIN_BYTES + MAX_ALIASES * MAX_LOCAL_BYTES <= MAX_TEXT_BYTES
            )
        };
        let domain = |index| {
            format!(
                "d{index:03}{}.{}.{}",
                "x".repeat(59),
                "x".repeat(63),
                "x".repeat(57)
            )
        };
        let local = |index| format!("a{index:04}{}", "x".repeat(59));
        let mut text = vec![0; MAX_TEXT_BYTES];
        let mut domains = vec![DomainSlot::EMPTY; MAX_DOMAINS];
        let mut aliases = vec![AliasSlot::EMPTY; MAX_ALIASES];
        let mut b = Builder::new(&mut text, &mut domains, &mut aliases).unwrap();
        b.account(ACCOUNT, at(1)).unwrap();
        for index in (0..MAX_ALIASES).rev() {
            b.alias(
                &format!("{}@{}", local(index), domain(index % MAX_DOMAINS)),
                ACCOUNT,
                at(u32::try_from(index + 2).unwrap()),
            )
            .unwrap();
        }
        for index in 0..MAX_DOMAINS {
            b.domain(&domain(index), at(u32::try_from(index + 5000).unwrap()))
                .unwrap();
        }
        let routes = b.finish().unwrap();
        assert_eq!(routes.text_bytes(), 309504);
        assert_eq!(routes.domain_count(), MAX_DOMAINS);
        assert_eq!(routes.alias_count(), MAX_ALIASES);
        for index in 0..MAX_ALIASES {
            assert_eq!(
                routes.resolve(&format!("{}@{}", local(index), domain(index % MAX_DOMAINS))),
                Ok(Some(ACCOUNT))
            );
        }
    }
    #[test]
    fn route_diagnostic_names_are_fixed() {
        let cases = [
            (Code::Capacity, "config_route_capacity"),
            (Code::InvalidDomain, "config_route_invalid_domain"),
            (Code::InvalidAddress, "config_route_invalid_address"),
            (Code::SecondAccount, "config_route_second_account"),
            (Code::MissingAccount, "config_route_missing_account"),
            (Code::MissingDomain, "config_route_missing_domain"),
            (Code::DuplicateDomain, "config_route_duplicate_domain"),
            (Code::DuplicateAlias, "config_route_duplicate_alias"),
            (Code::UnknownDomain, "config_route_unknown_domain"),
            (Code::UnknownAccount, "config_route_unknown_account"),
            (Code::Invariant, "config_route_invariant"),
        ];
        for (code, name) in cases {
            assert_eq!(code.name(), name);
        }
    }
}

//! Numeric endpoints and bounded HTTPS URI syntax; no DNS, sockets or authority.
use super::values;
use crate::bounded;
use std::{
    fmt,
    net::{IpAddr, SocketAddr},
};

pub const MAX_URI_BYTES: usize = 4096;
pub const MAX_ENDPOINT_BYTES: usize = 53;
pub const MAX_PREFIX_BYTES: usize = 49;
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Code {
    Origin,
    Uri,
    Endpoint,
    Prefix,
}
impl Code {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Origin => "config_endpoint_origin",
            Self::Uri => "config_endpoint_uri",
            Self::Endpoint => "config_endpoint_address",
            Self::Prefix => "config_endpoint_prefix",
        }
    }
}
impl fmt::Display for Code {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}
impl std::error::Error for Code {}
fn nonzero_decimal_u16(input: &str) -> Option<u16> {
    if input.is_empty() || input.starts_with('0') || !input.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    input.parse::<u16>().ok().filter(|p| *p != 0)
}
fn unreserved(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~')
}
fn target_valid(input: &str) -> bool {
    let mut bytes = input.bytes();
    while let Some(b) = bytes.next() {
        if b == b'%' {
            if !bytes.next().is_some_and(|b| b.is_ascii_hexdigit())
                || !bytes.next().is_some_and(|b| b.is_ascii_hexdigit())
            {
                return false;
            }
        } else if !unreserved(b)
            && !matches!(
                b,
                b':' | b'@'
                    | b'!'
                    | b'$'
                    | b'&'
                    | b'\''
                    | b'('
                    | b')'
                    | b'*'
                    | b'+'
                    | b','
                    | b';'
                    | b'='
                    | b'/'
                    | b'?'
            )
        {
            return false;
        }
    }
    true
}
#[derive(Clone, Copy)]
/// Validated operator HTTPS URI. Borrowed spelling is retained exactly for JWS.
/// The caller must separately enforce endpoint origin and TLS/file trust.
pub struct HttpsUri<'a> {
    raw: &'a str,
    host: &'a str,
    port: u16,
    target: &'a str,
}
impl fmt::Debug for HttpsUri<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("HttpsUri(<redacted>)")
    }
}
impl<'a> HttpsUri<'a> {
    pub fn parse(input: &'a str) -> Result<Self, Code> {
        if input.len() > MAX_URI_BYTES || !input.is_ascii() {
            return Err(Code::Uri);
        }
        let rest = input.strip_prefix("https://").ok_or(Code::Uri)?;
        let at = rest.find(['/', '?', '#']).unwrap_or(rest.len());
        let authority = rest.get(..at).ok_or(Code::Uri)?;
        let target = rest.get(at..).ok_or(Code::Uri)?;
        let (host, port) = if let Some((host, p)) = authority.split_once(':') {
            (host, nonzero_decimal_u16(p).ok_or(Code::Uri)?)
        } else {
            (authority, 443)
        };
        values::dns_name(host).map_err(|_| Code::Uri)?;
        let last = host.rsplit('.').next().ok_or(Code::Uri)?;
        if last
            .get(..2)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("0x"))
            && last
                .get(2..)
                .is_some_and(|digits| digits.bytes().all(|b| b.is_ascii_hexdigit()))
        {
            return Err(Code::Uri);
        }
        if !target_valid(target) {
            return Err(Code::Uri);
        }
        Ok(Self {
            raw: input,
            host,
            port,
            target,
        })
    }
    pub fn raw(self) -> &'a str {
        self.raw
    }
    pub fn host(self) -> &'a str {
        self.host
    }
    pub fn port(self) -> u16 {
        self.port
    }
    pub fn origin(self) -> Origin<'a> {
        Origin {
            host: self.host,
            port: self.port,
        }
    }
    pub fn same_origin(self, other: Self) -> bool {
        self.origin().same_origin(other.origin())
    }
    /// Append the unchanged path/query, adding `/` only for an empty path.
    /// Failed bounded formatting restores the output length, not its tail bytes.
    pub fn write_request_target(
        self,
        output: &mut bounded::TextBuffer<'_>,
    ) -> Result<(), bounded::Error> {
        output.format(format_args!(
            "{}{}",
            if self.target.starts_with('/') {
                ""
            } else {
                "/"
            },
            self.target
        ))
    }
}
#[derive(Clone, Copy)]
/// Validated JMAP origin with a case-preserving borrowed DNS host.
/// Canonical output folds the host and omits port 443 and the terminal slash.
pub struct Origin<'a> {
    host: &'a str,
    port: u16,
}
impl fmt::Debug for Origin<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Origin(<redacted>)")
    }
}
impl<'a> Origin<'a> {
    pub fn parse(input: &'a str) -> Result<Self, Code> {
        let uri = HttpsUri::parse(input).map_err(|_| Code::Origin)?;
        if !matches!(uri.target, "" | "/") {
            return Err(Code::Origin);
        }
        Ok(Self {
            host: uri.host,
            port: uri.port,
        })
    }
    pub fn host(self) -> &'a str {
        self.host
    }
    pub fn port(self) -> u16 {
        self.port
    }
    pub fn same_origin(self, other: Self) -> bool {
        self.port == other.port && self.host.eq_ignore_ascii_case(other.host)
    }
    /// Atomic bounded append; a capacity failure exposes no partial origin.
    pub fn write_canonical(
        self,
        output: &mut bounded::TextBuffer<'_>,
    ) -> Result<(), bounded::Error> {
        output.format(format_args!("{}", OriginDisplay(self)))
    }
}
struct OriginDisplay<'a>(Origin<'a>);
impl fmt::Display for OriginDisplay<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        use fmt::Write;
        f.write_str("https://")?;
        for b in self.0.host.bytes() {
            f.write_char(char::from(b.to_ascii_lowercase()))?;
        }
        if self.0.port != 443 {
            write!(f, ":{}", self.0.port)?;
        }
        Ok(())
    }
}
/// Numeric IP and shortest decimal port only; no zone, mapped IPv6 or port zero.
/// This parses a requested endpoint, never opens a socket or proves reachability.
pub fn numeric_endpoint(input: &str) -> Result<SocketAddr, Code> {
    if input.len() > MAX_ENDPOINT_BYTES || input.contains('%') {
        return Err(Code::Endpoint);
    }
    let (_, port_text) = input.rsplit_once(':').ok_or(Code::Endpoint)?;
    nonzero_decimal_u16(port_text).ok_or(Code::Endpoint)?;
    let endpoint = input.parse::<SocketAddr>().map_err(|_| Code::Endpoint)?;
    if matches!(endpoint.ip(),IpAddr::V6(ip) if ip.to_ipv4_mapped().is_some()) {
        return Err(Code::Endpoint);
    }
    Ok(endpoint)
}
#[derive(Clone, Copy, Eq, PartialEq)]
/// Binary canonical peer prefix with zero host bits and nonzero prefix length.
pub struct Prefix {
    network: IpAddr,
    bits: u8,
}
impl fmt::Debug for Prefix {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Prefix(<redacted>)")
    }
}
impl Prefix {
    pub fn parse(input: &str) -> Result<Self, Code> {
        if input.len() > MAX_PREFIX_BYTES {
            return Err(Code::Prefix);
        }
        let (address, bits) = input.split_once('/').ok_or(Code::Prefix)?;
        let bits = u8::try_from(nonzero_decimal_u16(bits).ok_or(Code::Prefix)?)
            .map_err(|_| Code::Prefix)?;
        let network = address.parse::<IpAddr>().map_err(|_| Code::Prefix)?;
        let value = Self { network, bits };
        let host_bits = match network {
            IpAddr::V4(ip) => u128::from(u32::from(ip)) & u128::from(!mask4(bits)?),
            IpAddr::V6(ip) => {
                if ip.to_ipv4_mapped().is_some() {
                    return Err(Code::Prefix);
                }
                u128::from(ip) & !mask6(bits)?
            }
        };
        if host_bits != 0 {
            return Err(Code::Prefix);
        }
        Ok(value)
    }
    pub fn network(self) -> IpAddr {
        self.network
    }
    pub fn bits(self) -> u8 {
        self.bits
    }
    fn contains_raw(self, peer: IpAddr) -> Result<bool, Code> {
        match (self.network, peer) {
            (IpAddr::V4(net), IpAddr::V4(ip)) => {
                Ok((u32::from(ip) & mask4(self.bits)?) == u32::from(net))
            }
            (IpAddr::V6(net), IpAddr::V6(ip)) => {
                Ok((u128::from(ip) & mask6(self.bits)?) == u128::from(net))
            }
            _ => Ok(false),
        }
    }
    /// Mapped IPv6 socket peers are checked against IPv4 prefixes only.
    /// The caller retains the original socket address for receipt metadata.
    pub fn contains(self, peer: IpAddr) -> Result<bool, Code> {
        let peer = match peer {
            IpAddr::V6(ip) => ip.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(peer),
            _ => peer,
        };
        self.contains_raw(peer)
    }
}
fn mask4(bits: u8) -> Result<u32, Code> {
    if !(1..=32).contains(&bits) {
        return Err(Code::Prefix);
    }
    u32::MAX
        .checked_shl(32u32.checked_sub(u32::from(bits)).ok_or(Code::Prefix)?)
        .ok_or(Code::Prefix)
}
fn mask6(bits: u8) -> Result<u128, Code> {
    if !(1..=128).contains(&bits) {
        return Err(Code::Prefix);
    }
    u128::MAX
        .checked_shl(128u32.checked_sub(u32::from(bits)).ok_or(Code::Prefix)?)
        .ok_or(Code::Prefix)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    #[test]
    fn origin_canonicalization_and_rejected_authority_forms() {
        for (input, canonical) in [
            ("https://EXAMPLE.test/", "https://example.test"),
            ("https://example.test:443", "https://example.test"),
            ("https://a:8443/", "https://a:8443"),
            ("https://a:1", "https://a:1"),
            ("https://a:65535", "https://a:65535"),
        ] {
            let origin = Origin::parse(input).unwrap();
            let mut bytes = [0; 300];
            let mut out = bounded::TextBuffer::new(&mut bytes);
            origin.write_canonical(&mut out).unwrap();
            assert_eq!(out.as_str().unwrap(), canonical);
        }
        for input in [
            "",
            "http://a",
            "HTTPS://a",
            "https://",
            "https://a:0",
            "https://a:",
            "https://a:0443",
            "https://a:+443",
            "https://a:65536",
            "https://user@a",
            "https://u:p@a",
            "https://a#b",
            "https://a?b",
            "https://a/x",
            "https://a//",
            "https://a.",
            "https://127.0.0.1",
            "https://[::1]",
            "https://a\\b",
            "https://é.test",
        ] {
            assert_eq!(Origin::parse(input).err(), Some(Code::Origin), "{input:?}");
        }
    }
    #[test]
    fn uri_spelling_and_request_target_remain_distinct() {
        for (input, target) in [
            ("https://EXAMPLE.test", "/"),
            ("https://a.0xg/", "/"),
            ("https://a?foo=%2F", "/?foo=%2F"),
            ("https://a/x/../y", "/x/../y"),
            ("https://a/%2e%2e/x", "/%2e%2e/x"),
            ("https://a/a%2fb?x=1&y=@", "/a%2fb?x=1&y=@"),
        ] {
            let uri = HttpsUri::parse(input).unwrap();
            assert_eq!(uri.raw(), input);
            let mut bytes = [0; 300];
            let mut out = bounded::TextBuffer::new(&mut bytes);
            uri.write_request_target(&mut out).unwrap();
            assert_eq!(out.as_str().unwrap(), target);
        }
        for input in [
            "https://a/#x",
            "https://a/%",
            "https://a/%0",
            "https://a/%g0",
            "https://a/%0g",
            "https://a/a b",
            "https://a/a\r\nb",
            "https://a/a\\b",
            "https://a/é",
            "https://a/[x]",
            "https://0x7f.0x1",
            "https://a.0x1",
            "https://0x",
            "https://0XAB",
        ] {
            assert_eq!(HttpsUri::parse(input).err(), Some(Code::Uri), "{input:?}");
        }
        let a = HttpsUri::parse("https://A:443/x").unwrap();
        assert!(a.same_origin(HttpsUri::parse("https://a/y").unwrap()));
        assert!(!a.same_origin(HttpsUri::parse("https://a:444/y").unwrap()));
        assert!(!a.same_origin(HttpsUri::parse("https://b/x").unwrap()));
        assert!(a.origin().same_origin(Origin::parse("https://a/").unwrap()));
        assert!(!a
            .origin()
            .same_origin(Origin::parse("https://a:444").unwrap()));
        assert!(!a.origin().same_origin(Origin::parse("https://b").unwrap()));
    }
    #[test]
    fn uri_and_output_boundaries_are_atomic() {
        let host = format!(
            "{}.{}.{}.{}",
            "a".repeat(63),
            "b".repeat(63),
            "c".repeat(63),
            "d".repeat(51)
        );
        assert_eq!(host.len(), values::MAX_DOMAIN_BYTES);
        assert!(HttpsUri::parse(&format!("https://{host}")).is_ok());
        assert_eq!(
            HttpsUri::parse(&format!("https://{host}e")).err(),
            Some(Code::Uri)
        );
        let full = format!("https://a/{}", "x".repeat(MAX_URI_BYTES - 10));
        assert_eq!(full.len(), MAX_URI_BYTES);
        assert!(HttpsUri::parse(&full).is_ok());
        assert_eq!(HttpsUri::parse(&(full + "x")).err(), Some(Code::Uri));
        let mut bytes = [0; 8];
        let mut out = bounded::TextBuffer::new(&mut bytes);
        out.append("old").unwrap();
        assert_eq!(
            Origin::parse("https://a")
                .unwrap()
                .write_canonical(&mut out),
            Err(bounded::Error::Capacity)
        );
        assert_eq!(out.as_str().unwrap(), "old");
        assert_eq!(
            HttpsUri::parse("https://a?query=1")
                .unwrap()
                .write_request_target(&mut out),
            Err(bounded::Error::Capacity)
        );
        assert_eq!(out.as_str().unwrap(), "old");
    }
    #[test]
    fn endpoints_are_numeric_nonzero_and_unmapped() {
        let longest = "[ffff:ffff:ffff:ffff:ffff:ffff:255.255.255.255]:65535";
        assert_eq!(longest.len(), MAX_ENDPOINT_BYTES);
        for input in [
            "127.0.0.1:53",
            "0.0.0.0:25",
            "[::1]:443",
            "[::]:80",
            longest,
        ] {
            assert!(numeric_endpoint(input).is_ok());
        }
        for input in [
            "a:25",
            "127.0.0.1:0",
            "127.0.0.1:025",
            "127.0.0.1:+25",
            "[::1]:0",
            "127.0.0.1:65536",
            "[::ffff:127.0.0.1]:25",
            "[fe80::1%1]:25",
            "[::1%0]:25",
            "::1:25",
            "127.00.0.1:25",
            " 127.0.0.1:25",
        ] {
            assert_eq!(numeric_endpoint(input), Err(Code::Endpoint), "{input:?}");
        }
    }
    #[test]
    fn prefix_host_bits_lengths_and_mapped_peers() {
        let longest = "ffff:ffff:ffff:ffff:ffff:ffff:255.255.255.255/128";
        assert_eq!(longest.len(), MAX_PREFIX_BYTES);
        assert!(Prefix::parse(longest).is_ok());
        for input in [
            "0.0.0.0/1",
            "127.0.0.0/8",
            "192.0.2.1/32",
            "::/1",
            "2001:db8::/32",
            "::1/128",
        ] {
            assert!(Prefix::parse(input).is_ok(), "{input}");
        }
        for input in [
            "0.0.0.0/0",
            "::/0",
            "127.0.0.1/8",
            "2001:db8::1/32",
            "192.0.2.1/33",
            "::/129",
            "::/032",
            "[::]/32",
            "::ffff:192.0.2.0/120",
            "::ffff:c000:200/120",
            "::/32/1",
            "a/8",
            "fe80::%1/64",
        ] {
            assert_eq!(Prefix::parse(input).err(), Some(Code::Prefix), "{input:?}");
        }
        let prefix = Prefix::parse("192.0.2.0/24").unwrap();
        for (peer, allowed) in [
            ("192.0.2.0", true),
            ("192.0.2.255", true),
            ("192.0.3.0", false),
            ("::ffff:192.0.2.7", true),
            ("2001:db8::1", false),
        ] {
            assert_eq!(
                prefix.contains(peer.parse().unwrap()),
                Ok(allowed),
                "{peer}"
            );
        }
        assert_eq!(
            Prefix::parse("2001:db8::/32"),
            Prefix::parse("2001:0db8:0:0:0:0:0:0/32")
        );
    }
    #[test]
    fn ipv6_membership_and_ipv4_mapping_keep_family_boundaries() {
        let prefix = Prefix::parse("2001:db8::/32").unwrap();
        assert_eq!(prefix.network(), "2001:db8::".parse::<IpAddr>().unwrap());
        assert_eq!(prefix.bits(), 32);
        for (peer, allowed) in [
            ("2001:db8::", true),
            ("2001:db8:ffff:ffff:ffff:ffff:ffff:ffff", true),
            ("2001:db9::", false),
            ("2001:db7:ffff:ffff:ffff:ffff:ffff:ffff", false),
            ("192.0.2.1", false),
        ] {
            assert_eq!(prefix.contains(peer.parse().unwrap()), Ok(allowed));
        }
        let broad = Prefix::parse("::/1").unwrap();
        assert_eq!(
            broad.contains("::ffff:192.0.2.1".parse().unwrap()),
            Ok(false)
        );
        for input in ["1.2.3.4/32", "::1/128"] {
            let exact = Prefix::parse(input).unwrap();
            assert_eq!(exact.contains(exact.network()), Ok(true));
        }
    }
    #[test]
    fn public_debug_and_error_surfaces_redact_configuration() {
        let marker = "private-fixture.test";
        let input = format!("https://{marker}/private-query?secret=1");
        let uri = HttpsUri::parse(&input).unwrap();
        let origin_input = format!("https://{marker}");
        let origin = Origin::parse(&origin_input).unwrap();
        assert!(!format!("{uri:?} {origin:?}").contains(marker));
        let prefix = Prefix::parse("192.0.2.0/24").unwrap();
        assert_eq!(format!("{prefix:?}"), "Prefix(<redacted>)");
        for (code, name) in [
            (Code::Origin, "config_endpoint_origin"),
            (Code::Uri, "config_endpoint_uri"),
            (Code::Endpoint, "config_endpoint_address"),
            (Code::Prefix, "config_endpoint_prefix"),
        ] {
            assert_eq!(code.name(), name);
            assert_eq!(code.to_string(), name);
            assert!(std::error::Error::source(&code).is_none());
        }
    }
}

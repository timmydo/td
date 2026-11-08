//! URLs as td fetches them, std alone: the strict form td-agent's
//! `web_fetch` judges a destination by (`Url`, `Destination`), and the
//! browser's references on top of it (`resolve`, `address`), which take
//! what a document or a person wrote and either reduce it to that strict
//! form or say why not. A reference is never read one way here and
//! another by the fetch service: everything this crate hands back parses
//! again through `Url::parse`.
#![forbid(unsafe_code)]

/// The longest URL taken.
pub const MAX_URL: usize = 2048;

/// A destination on an allowlist: a host, a DNS name in lower case or
/// an IP address, and a port.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct Destination {
    pub host: String,
    pub port: u16,
}

impl Destination {
    /// `host`, `host:port`, `[v6]` or `[v6]:port`; 443 when no port is
    /// named.
    pub fn parse(text: &str) -> Result<Self, String> {
        let wrong = || format!("{text:?} is not a host with an optional port");
        let (host, port) = if let Some(rest) = text.strip_prefix('[') {
            let (inner, after) = rest.split_once(']').ok_or_else(wrong)?;
            let v6 = inner.parse::<std::net::Ipv6Addr>().map_err(|_| wrong())?;
            let port = match after {
                "" => None,
                after => Some(after.strip_prefix(':').ok_or_else(wrong)?),
            };
            (format!("[{v6}]"), port)
        } else {
            let (host, port) = match text.rsplit_once(':') {
                Some((host, port)) => (host, Some(port)),
                None => (text, None),
            };
            (dns_or_v4(host).ok_or_else(wrong)?, port)
        };
        let port = match port {
            None => 443,
            Some(port) => Some(port)
                .filter(|port| (1..=5).contains(&port.len()))
                .filter(|port| port.bytes().all(|b| b.is_ascii_digit()))
                .and_then(|port| port.parse::<u16>().ok())
                .filter(|port| *port != 0)
                .ok_or_else(wrong)?,
        };
        Ok(Self { host, port })
    }

    /// As configuration writes it: the port left off when it is 443.
    pub fn text(&self) -> String {
        if self.port == 443 {
            self.host.clone()
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }
}

/// `host` as a destination names it: an IPv4 address, or a DNS name of
/// letters, digits and hyphens, in lower case, a trailing dot dropped,
/// whose last label is not a number, decimal or `0x` hexadecimal, since
/// the resolver reads such a name (`10.1`, `0x7f.1`, `2130706433`) as an
/// address. The egress relay's `parse_host` (net/src/egress.rs) holds
/// the same rule.
pub fn dns_or_v4(host: &str) -> Option<String> {
    if host.parse::<std::net::Ipv4Addr>().is_ok() {
        return Some(host.to_string());
    }
    let name = host.strip_suffix('.').unwrap_or(host);
    let label_ok = |label: &str| {
        !label.is_empty()
            && label.len() <= 63
            && !label.starts_with('-')
            && !label.ends_with('-')
            && label
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
    };
    let numeric = name.rsplit('.').next().is_some_and(|last| {
        let hex = last
            .get(..2)
            .filter(|prefix| prefix.eq_ignore_ascii_case("0x"))
            .and_then(|_| last.get(2..));
        last.bytes().all(|b| b.is_ascii_digit())
            || hex.is_some_and(|digits| digits.bytes().all(|b| b.is_ascii_hexdigit()))
    });
    (!name.is_empty() && name.len() <= 253 && name.split('.').all(label_ok) && !numeric)
        .then(|| name.to_ascii_lowercase())
}

/// A URL as it is fetched: its origin, `scheme://host[:port]` with the
/// scheme's own port left off, and its path with any query, the
/// fragment dropped.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Url {
    pub https: bool,
    pub destination: Destination,
    origin: String,
    path: String,
}

impl Url {
    /// `text`, an `http` or `https` URL of printable ASCII whose host is a
    /// DNS name, an IPv4 address or a bracketed IPv6 address, with no user,
    /// password or percent-encoding in its authority; anything else is
    /// refused rather than read the way one parser might and another not.
    pub fn parse(text: &str) -> Result<Self, String> {
        if text.len() > MAX_URL {
            return Err(format!(
                "the URL is {} bytes; at most {MAX_URL}",
                text.len()
            ));
        }
        if !text.bytes().all(|b| b.is_ascii_graphic()) {
            return Err("the URL has a space, a control character or a character outside ASCII; percent-encode it".into());
        }
        if text.contains('\\') {
            return Err("the URL has a backslash".into());
        }
        let (scheme, rest) = text
            .split_once("://")
            .ok_or("the URL has no scheme; it begins http:// or https://")?;
        let https = if scheme.eq_ignore_ascii_case("https") {
            true
        } else if scheme.eq_ignore_ascii_case("http") {
            false
        } else {
            return Err(format!(
                "the URL's scheme is {scheme:?}; only http and https are fetched"
            ));
        };
        let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
        let (authority, tail) = rest
            .split_at_checked(end)
            .ok_or("the URL could not be split")?;
        if authority.contains('@') {
            return Err("the URL names a user or a password, which are not sent".into());
        }
        if authority.contains('%') {
            return Err("the URL's host is percent-encoded".into());
        }
        let default = if https { 443 } else { 80 };
        // The scheme's port where none is named; an unclosed bracket is
        // left for the destination to refuse.
        let with_port = match authority.strip_prefix('[') {
            Some(inner) => match inner.split_once(']').map(|(_, after)| after) {
                Some("") => format!("{authority}:{default}"),
                Some(after) if !after.starts_with(':') => {
                    return Err("the URL's host is not followed by a port".into())
                }
                _ => authority.to_string(),
            },
            None if authority.contains(':') => authority.to_string(),
            None => format!("{authority}:{default}"),
        };
        let destination = Destination::parse(&with_port)
            .map_err(|_| format!("{authority:?} is not a host with an optional port"))?;
        let port = if destination.port == default {
            String::new()
        } else {
            format!(":{}", destination.port)
        };
        let origin = format!(
            "{}://{}{port}",
            if https { "https" } else { "http" },
            destination.host
        );
        let tail = tail.split('#').next().unwrap_or_default();
        let (path, query) = match tail.split_once('?') {
            Some((path, query)) => (path, Some(query)),
            None => (tail, None),
        };
        let mut path = without_dots(path);
        if let Some(query) = query {
            path.push('?');
            path.push_str(query);
        }
        Ok(Self {
            https,
            destination,
            origin,
            path,
        })
    }

    /// The URL whole, as it is fetched.
    pub fn text(&self) -> String {
        format!("{}{}", self.origin, self.path)
    }

    /// Where a redirect from here to `location` goes: an absolute URL, one
    /// without its scheme, a path, a query or a path relative to this
    /// one's directory; then taken as `parse` takes any.
    pub fn join(&self, location: &str) -> Result<Self, String> {
        let location = location.trim();
        // `http:/x` or `https:x` is read differently by each parser; only
        // a scheme followed by `//` is taken as one.
        // A scheme begins with a letter (WHATWG), so `2024:notes` is a
        // path.
        let named = location.split_once(':').is_some_and(|(scheme, rest)| {
            scheme.starts_with(|c: char| c.is_ascii_alphabetic())
                && scheme
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.'))
                && !rest.starts_with("//")
        });
        if named {
            return Err("it names a scheme without `//`".into());
        }
        let scheme = location
            .split_once("://")
            .map(|(scheme, _)| scheme)
            .filter(|scheme| {
                !scheme.is_empty()
                    && scheme
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.'))
            });
        let joined = if scheme.is_some() {
            location.to_string()
        } else if let Some(rest) = location.strip_prefix("//") {
            format!("{}://{rest}", if self.https { "https" } else { "http" })
        } else if location.starts_with('/') {
            format!("{}{location}", self.origin)
        } else if location.starts_with('?') {
            let path = self.path.split('?').next().unwrap_or_default();
            format!("{}{path}{location}", self.origin)
        } else if location.is_empty() || location.starts_with('#') {
            self.text()
        } else {
            let path = self.path.split('?').next().unwrap_or_default();
            let directory = path.rsplit_once('/').map_or("", |(directory, _)| directory);
            format!("{}{directory}/{location}", self.origin)
        };
        Self::parse(&joined)
    }
}

/// `path` with its `.` and `..` segments, and their `%2e` spellings,
/// removed as the fetch service's parser removes them (RFC 3986 §5.2.4,
/// WHATWG's URL standard), so that the URL said and joined against is
/// the one fetched; it begins with `/`.
fn without_dots(path: &str) -> String {
    let segments: Vec<&str> = path.strip_prefix('/').unwrap_or(path).split('/').collect();
    let count = segments.len();
    let mut kept: Vec<&str> = Vec::with_capacity(count);
    for (at, segment) in segments.into_iter().enumerate() {
        let last = at + 1 == count;
        let lower = segment.to_ascii_lowercase();
        let up = matches!(lower.as_str(), ".." | ".%2e" | "%2e." | "%2e%2e");
        let here = matches!(lower.as_str(), "." | "%2e");
        if up {
            kept.pop();
        }
        if up || here {
            if last {
                kept.push("");
            }
        } else {
            kept.push(segment);
        }
    }
    format!("/{}", kept.join("/"))
}

impl Url {
    /// `scheme://host[:port]`, the scheme's own port left off.
    pub fn origin(&self) -> &str {
        &self.origin
    }

    /// Whether `other` names the same host, whatever its scheme or port:
    /// a device's `http` page moving to its own `https` is still that
    /// device.
    pub fn same_host(&self, other: &Url) -> bool {
        self.destination.host == other.destination.host
    }
}

/// What a reference a document or a person wrote names.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Reference {
    /// An `http` or `https` URL as it is fetched, and the fragment the
    /// reference named after its `#`, if any, which is never sent.
    Fetch { url: Url, fragment: Option<String> },
    /// A reference of another scheme, `mailto` or `javascript` say, named
    /// in lower case: nothing here fetches or opens it.
    Other { scheme: String },
}

/// The schemes an address typed without `//` is taken to name, rather
/// than a host with a port: `localhost:8080` is a host.
const NAMED_ONLY: &[&str] = &[
    "about",
    "data",
    "file",
    "javascript",
    "mailto",
    "tel",
    "view-source",
];

/// `href`, as a document wrote it, joined against `base`, the URL the
/// document came from. It follows WHATWG's URL standard for `http` and
/// `https` where it reads a form the same way whatever the base, and
/// refuses the rest rather than guess: surrounding spaces and controls
/// and every tab and line break dropped; a backslash before the query
/// read as a slash, so `\\host`, `/\host` and `https:\\host` name a host
/// as they do there; spaces, quotes, angle brackets and bytes outside
/// ASCII percent-encoded; the fragment set aside. Two departures, both
/// toward refusal or an equal request: a backslash in the query is sent
/// as `%5C`, since `Url::parse` takes no backslash and a server decodes
/// the two alike; and a host outside ASCII, which needs IDNA, an empty
/// port and `http:` or `https:` without `//`, which WHATWG reads by the
/// base, are refused.
pub fn resolve(base: &Url, href: &str) -> Result<Reference, String> {
    let cleaned = clean(href)?;
    if let Some(scheme) = scheme_of(&cleaned) {
        if !matches!(scheme.as_str(), "http" | "https") {
            return Ok(Reference::Other { scheme });
        }
    }
    let (reference, fragment) = split_fragment(&cleaned);
    let encoded = encode(&reference)?;
    Ok(Reference::Fetch {
        url: base.join(&encoded)?,
        fragment,
    })
}

/// What a person typed as an address, its fragment set aside first: a
/// scheme followed by two slashes is a URL; `http:` or `https:` with no
/// slash after it names the host that follows, as WHATWG reads it with
/// no base (one slash is refused); a scheme from `NAMED_ONLY` is that scheme; anything else, `localhost:8080`
/// or `example.org/?next=https://x` say, is a host and path taken as
/// `https://`. The result is encoded as `resolve` encodes a reference.
pub fn address(text: &str) -> Result<Reference, String> {
    let cleaned = clean(text)?;
    if cleaned.is_empty() {
        return Err("no address was given".into());
    }
    let (reference, fragment) = split_fragment(&cleaned);
    let absolute = match scheme_of(&reference) {
        Some(scheme) if slashed(after_colon(&reference)) => {
            if !matches!(scheme.as_str(), "http" | "https") {
                return Ok(Reference::Other { scheme });
            }
            reference
        }
        Some(scheme) if matches!(scheme.as_str(), "http" | "https") => {
            format!("{scheme}://{}", after_colon(&reference))
        }
        Some(scheme) if NAMED_ONLY.contains(&scheme.as_str()) => {
            return Ok(Reference::Other { scheme });
        }
        _ => format!("https://{reference}"),
    };
    let encoded = encode(&absolute)?;
    Ok(Reference::Fetch {
        url: Url::parse(&encoded)?,
        fragment,
    })
}

/// `text` without its leading and trailing spaces and C0 controls and
/// without any tab or line break, at most `MAX_URL` bytes.
fn clean(text: &str) -> Result<String, String> {
    let trimmed = text.trim_matches(|c: char| c <= ' ');
    let cleaned: String = trimmed
        .chars()
        .filter(|c| !matches!(c, '\t' | '\n' | '\r'))
        .collect();
    if cleaned.len() > MAX_URL {
        return Err(format!(
            "the reference is {} bytes; at most {MAX_URL}",
            cleaned.len()
        ));
    }
    Ok(cleaned)
}

/// The scheme `text` begins with, in lower case: a letter, then letters,
/// digits, `+`, `-` or `.`, then `:`.
fn scheme_of(text: &str) -> Option<String> {
    let (scheme, _) = text.split_once(':')?;
    let named = scheme.starts_with(|c: char| c.is_ascii_alphabetic())
        && scheme
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.'));
    named.then(|| scheme.to_ascii_lowercase())
}

/// What follows the first `:`, all of `text` when there is none.
fn after_colon(text: &str) -> &str {
    text.split_once(':').map_or(text, |(_, rest)| rest)
}

/// Whether `text` begins with two slashes, each `/` or `\`, as WHATWG
/// takes either before an `http` or `https` authority.
fn slashed(text: &str) -> bool {
    let mut bytes = text.bytes();
    matches!(
        (bytes.next(), bytes.next()),
        (Some(b'/' | b'\\'), Some(b'/' | b'\\'))
    )
}

fn split_fragment(text: &str) -> (String, Option<String>) {
    match text.split_once('#') {
        Some((reference, fragment)) => (reference.to_string(), Some(fragment.to_string())),
        None => (text.to_string(), None),
    }
}

const HEX: &[u8] = b"0123456789ABCDEF";

/// `reference`, its fragment already set aside, as `Url::parse` and
/// `Url::join` take it.
fn encode(reference: &str) -> Result<String, String> {
    // Where the authority begins, when the reference names one: right
    // after `scheme:` and two slashes, or after two slashes at the start.
    let authority_at = match scheme_of(reference) {
        Some(scheme) if slashed(after_colon(reference)) => Some(scheme.len() + 3),
        Some(_) => None,
        None if slashed(reference) => Some(2),
        None => None,
    };
    let mut out = String::with_capacity(reference.len());
    let mut rest = reference;
    if let Some(at) = authority_at {
        let (head, tail) = reference
            .split_at_checked(at)
            .ok_or("the reference could not be split")?;
        let end = tail.find(['/', '\\', '?']).unwrap_or(tail.len());
        let (authority, after) = tail
            .split_at_checked(end)
            .ok_or("the reference could not be split")?;
        if !authority.is_ascii() {
            return Err(
                "the host is written outside ASCII; internationalized names are not supported yet"
                    .into(),
            );
        }
        if !authority.bytes().all(|b| b.is_ascii_graphic()) {
            return Err("the host has a space or a control character".into());
        }
        out.push_str(&head.replace('\\', "/"));
        out.push_str(authority);
        rest = after;
    }
    let mut query = false;
    for byte in rest.bytes() {
        if byte == b'?' {
            query = true;
        }
        let byte = if byte == b'\\' && !query { b'/' } else { byte };
        let escaped = !byte.is_ascii_graphic()
            || matches!(byte, b'"' | b'<' | b'>')
            || (!query && matches!(byte, b'`' | b'{' | b'}'))
            || (query && matches!(byte, b'\'' | b'\\'));
        if escaped {
            let digit =
                |nibble: u8| char::from(HEX.get(usize::from(nibble)).copied().unwrap_or(b'0'));
            out.push('%');
            out.push(digit(byte >> 4));
            out.push(digit(byte & 0xf));
        } else {
            out.push(char::from(byte));
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic)]
    use super::*;

    #[test]
    fn a_url_is_taken_only_in_a_form_every_parser_reads_alike() {
        let url = Url::parse("HTTPS://Docs.Example.org/a/b?q=1#frag").unwrap();
        assert_eq!(url.text(), "https://docs.example.org/a/b?q=1");
        assert_eq!(url.destination.text(), "docs.example.org");
        assert!(url.https);
        let url = Url::parse("http://example.org").unwrap();
        assert_eq!(url.text(), "http://example.org/");
        assert_eq!(url.destination.text(), "example.org:80");
        assert_eq!(
            Url::parse("http://example.org:8080?x").unwrap().text(),
            "http://example.org:8080/?x"
        );
        assert_eq!(
            Url::parse("https://example.org:443/").unwrap().text(),
            "https://example.org/"
        );
        assert_eq!(
            Url::parse("http://[2001:db8::1]:81/").unwrap().text(),
            "http://[2001:db8::1]:81/"
        );
        assert_eq!(
            Url::parse("https://[2001:db8::1]/x")
                .unwrap()
                .destination
                .text(),
            "[2001:db8::1]"
        );
        assert_eq!(
            Url::parse("http://93.184.216.34/").unwrap().text(),
            "http://93.184.216.34/"
        );
        // Dot segments go as the service's parser takes them away.
        for (url, text) in [
            ("https://h/a/b/..", "https://h/a/"),
            ("https://h/a/./b", "https://h/a/b"),
            ("https://h/a/%2E%2e/b?x=/../", "https://h/b?x=/../"),
            ("https://h/..", "https://h/"),
            ("https://h/a/b/", "https://h/a/b/"),
            ("https://h/a//b", "https://h/a//b"),
        ] {
            assert_eq!(Url::parse(url).unwrap().text(), text, "{url}");
        }
        for (url, why) in [
            ("ftp://h/", "scheme"),
            ("file:///etc/passwd", "scheme"),
            ("example.org/x", "no scheme"),
            ("https://user:pw@example.org/", "user or a password"),
            ("https://allowed.org\\@evil.org/", "backslash"),
            ("https://ex%61mple.org/", "percent-encoded"),
            ("https://exa mple.org/", "space"),
            ("https://exämple.org/", "outside ASCII"),
            ("https://0x7f.1/", "not a host"),
            ("https://2130706433/", "not a host"),
            ("https://example.org:0/", "not a host"),
            ("https://example.org:99999/", "not a host"),
            ("https://[::1/", "not a host"),
            ("https://[::1]x/", "not followed by a port"),
            ("https:///x", "not a host"),
        ] {
            let err = Url::parse(url).unwrap_err();
            assert!(err.contains(why), "{url}: {err}");
        }
        let long = format!("https://example.org/{}", "a".repeat(MAX_URL));
        assert!(Url::parse(&long).unwrap_err().contains("at most"));
    }

    #[test]
    fn a_location_is_joined_against_the_url_it_came_from() {
        let base = Url::parse("https://example.org/docs/page?x=1").unwrap();
        for (location, joined) in [
            ("https://other.org/y", "https://other.org/y"),
            ("//other.org/y", "https://other.org/y"),
            ("/root", "https://example.org/root"),
            ("?y=2", "https://example.org/docs/page?y=2"),
            ("next", "https://example.org/docs/next"),
            ("#part", "https://example.org/docs/page?x=1"),
            ("", "https://example.org/docs/page?x=1"),
            ("http://example.org/", "http://example.org/"),
        ] {
            assert_eq!(base.join(location).unwrap().text(), joined, "{location}");
        }
        assert!(base.join("ftp://other.org/").is_err());
        // A scheme without `//` is refused, not read as a path.
        for location in ["https:evil.org", "http:/x", "mailto:a@b"] {
            assert!(base.join(location).is_err(), "{location}");
        }
        assert_eq!(
            base.join("a:b/c").unwrap_err(),
            "it names a scheme without `//`"
        );
        assert_eq!(
            base.join("2024:notes").unwrap().text(),
            "https://example.org/docs/2024:notes"
        );
        // A relative one after dots is joined to what was fetched.
        let dotted = Url::parse("https://example.org/a/b/..").unwrap();
        assert_eq!(
            dotted.join("next").unwrap().text(),
            "https://example.org/a/next"
        );
        assert!(base.join("//user@other.org/").is_err());
    }

    #[test]
    fn a_host_is_the_same_whatever_the_scheme() {
        let plain = Url::parse("http://192.0.2.1/").unwrap();
        let secure = Url::parse("https://192.0.2.1:8443/setup").unwrap();
        assert!(plain.same_host(&secure));
        assert!(!plain.same_host(&Url::parse("http://192.0.2.2/").unwrap()));
        assert_eq!(secure.origin(), "https://192.0.2.1:8443");
    }

    fn fetched(reference: Reference) -> (String, Option<String>) {
        match reference {
            Reference::Fetch { url, fragment } => (url.text(), fragment),
            Reference::Other { scheme } => panic!("{scheme} is not fetched"),
        }
    }

    #[test]
    fn a_reference_is_joined_encoded_and_its_fragment_kept() {
        let base = Url::parse("https://example.org/docs/page?x=1").unwrap();
        for (href, text, fragment) in [
            ("next", "https://example.org/docs/next", None),
            ("/a b", "https://example.org/a%20b", None),
            ("über", "https://example.org/docs/%C3%BCber", None),
            ("#top", "https://example.org/docs/page?x=1", Some("top")),
            ("other#x#y", "https://example.org/docs/other", Some("x#y")),
            (
                "?q=a b'c",
                "https://example.org/docs/page?q=a%20b%27c",
                None,
            ),
            ("\\root", "https://example.org/root", None),
            (" \n/a\tb\r ", "https://example.org/ab", None),
            ("//other.org/{x}", "https://other.org/%7Bx%7D", None),
            ("https://other.org\\@evil", "https://other.org/@evil", None),
            ("HTTP://Other.org", "http://other.org/", None),
            ("a\"<>", "https://example.org/docs/a%22%3C%3E", None),
            // WHATWG's special slashes name a host whichever way they lean.
            ("/\\evil.org/x", "https://evil.org/x", None),
            ("\\\\evil.org", "https://evil.org/", None),
            ("https:/\\evil.org", "https://evil.org/", None),
            ("https:\\\\evil.org", "https://evil.org/", None),
            // A URL in the query is the query's.
            (
                "http:\\\\example.org/a b?u=http://x",
                "http://example.org/a%20b?u=http://x",
                None,
            ),
            (
                "http://h/p?a=\\&u=http://x",
                "http://h/p?a=%5C&u=http://x",
                None,
            ),
            ("?a=b\\c", "https://example.org/docs/page?a=b%5Cc", None),
        ] {
            let (got, kept) = fetched(resolve(&base, href).unwrap());
            assert_eq!(got, text, "{href:?}");
            assert_eq!(kept.as_deref(), fragment, "{href:?}");
        }
        for (href, scheme) in [
            ("mailto:a@example.org", "mailto"),
            ("JavaScript:void(0)", "javascript"),
            ("data:text/plain,x", "data"),
            ("ftp://example.org/", "ftp"),
        ] {
            assert_eq!(
                resolve(&base, href).unwrap(),
                Reference::Other {
                    scheme: scheme.into()
                },
                "{href}"
            );
        }
        for (href, why) in [
            ("https:other", "without `//`"),
            ("//bücher.example/", "outside ASCII"),
            ("https:\\\\bücher.example/", "outside ASCII"),
            ("http:evil.org", "without `//`"),
            ("https://a.org:/x", "not a host"),
            ("https://exa mple.org/", "space"),
        ] {
            let err = resolve(&base, href).unwrap_err();
            assert!(err.contains(why), "{href}: {err}");
        }
        let long = format!("/{}", "a".repeat(MAX_URL));
        assert!(resolve(&base, &long).unwrap_err().contains("at most"));
    }

    #[test]
    fn an_address_without_a_scheme_is_https() {
        for (typed, text, fragment) in [
            ("example.org", "https://example.org/", None),
            (
                "  example.org/a b#c ",
                "https://example.org/a%20b",
                Some("c"),
            ),
            ("localhost:8080", "https://localhost:8080/", None),
            ("http://example.org/x", "http://example.org/x", None),
            (
                "example.org/r?to=https://x",
                "https://example.org/r?to=https://x",
                None,
            ),
            ("localhost:8080/a://b", "https://localhost:8080/a://b", None),
            ("a.org/#x://y", "https://a.org/", Some("x://y")),
            ("http:example.org", "http://example.org/", None),
            (
                "example.org\\@evil.org",
                "https://example.org/@evil.org",
                None,
            ),
        ] {
            let (got, kept) = fetched(address(typed).unwrap());
            assert_eq!(got, text, "{typed:?}");
            assert_eq!(kept.as_deref(), fragment, "{typed:?}");
        }
        for (typed, scheme) in [
            ("about:blank", "about"),
            ("mailto:a@example.org", "mailto"),
            ("file:///etc/passwd", "file"),
            ("gopher://example.org/", "gopher"),
        ] {
            assert_eq!(
                address(typed).unwrap(),
                Reference::Other {
                    scheme: scheme.into()
                }
            );
        }
        assert!(address(" \t").unwrap_err().contains("no address"));
        assert!(address("exa mple.org").is_err());
    }
}

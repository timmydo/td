//! `web_fetch`'s URLs and pages (DESIGN.md §12): a URL the model wrote,
//! taken only in a form whose host every parser reads alike, so the
//! destination judged is the one the fetch service connects to; a
//! redirect's `location` joined against it; and a response as text.

use crate::config::Destination;

/// The longest URL taken.
pub const MAX_URL: usize = 2048;
/// The most redirects one call follows.
pub const MAX_REDIRECTS: usize = 5;
/// The most bytes of a response taken; the service refuses one past it.
pub const MAX_BODY: u64 = 8 * 1024 * 1024;
/// The most bytes of a page rendered, and the width it is rendered at,
/// which bounds a table's columns: unbounded, a wide cell over many rows
/// would pad every row to it.
pub const MAX_HTML: usize = 2 * 1024 * 1024;
const HTML_WIDTH: usize = 100;
/// What a fetch asks for: a page, else text, else whatever there is.
pub const ACCEPT: &str = "text/html, text/plain;q=0.9, */*;q=0.1";

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

/// A response's body as text: an HTML page rendered as text with its links
/// listed at its end, other text as it is (UTF-8, anything else replaced),
/// and anything else refused by its type.
pub fn text(content_type: Option<&str>, body: &[u8]) -> Result<String, String> {
    let media = content_type
        .and_then(|value| value.split(';').next())
        .map(|media| media.trim().to_ascii_lowercase())
        .unwrap_or_default();
    let start: Vec<u8> = body
        .iter()
        .skip_while(|b| b.is_ascii_whitespace())
        .take(15)
        .map(u8::to_ascii_lowercase)
        .collect();
    let looks_html = start.starts_with(b"<!doctype html") || start.starts_with(b"<html");
    let html = matches!(media.as_str(), "text/html" | "application/xhtml+xml")
        || (media.is_empty() && looks_html);
    let raw = if html {
        let mut text = td_html::to_text(body.get(..MAX_HTML).unwrap_or(body), HTML_WIDTH);
        if body.len() > MAX_HTML {
            text.push_str(&format!(
                "\n[the page is {} bytes; only its first {MAX_HTML} were rendered]\n",
                body.len()
            ));
        }
        text
    } else {
        let textual = media.starts_with("text/")
            || media.ends_with("+json")
            || media.ends_with("+xml")
            || matches!(
                media.as_str(),
                "application/json"
                    | "application/xml"
                    | "application/javascript"
                    | "application/x-javascript"
                    | "application/ecmascript"
                    | "application/toml"
                    | "application/yaml"
                    | "application/x-yaml"
                    | "application/x-sh"
            )
            || (media.is_empty() && std::str::from_utf8(body).is_ok());
        if !textual {
            let kind = if media.is_empty() {
                "of no stated type".to_string()
            } else {
                media
            };
            return Err(format!(
                "the response is {kind}, {} bytes, not text; web_fetch returns only text",
                body.len()
            ));
        }
        String::from_utf8_lossy(body).into_owned()
    };
    // Lines as the page has them, without what a terminal or the window
    // would read as control.
    Ok(raw
        .replace("\r\n", "\n")
        .chars()
        .filter(|c| matches!(c, '\n' | '\t') || !c.is_control())
        .collect())
}

/// What the model is answered: where the text came from, its status and
/// type, how much there is and which part this is, then that part, at
/// most `max_bytes` from `offset`, both moved back to a character's
/// start.
pub fn page(
    url: &str,
    status: u16,
    content_type: Option<&str>,
    text: &str,
    offset: u64,
    max_bytes: usize,
) -> Result<String, String> {
    let total = text.len();
    let offset = usize::try_from(offset).unwrap_or(usize::MAX);
    if offset > total || (offset == total && total > 0) {
        return Err(format!(
            "`offset` {offset} is past the end of the text, which is {total} bytes"
        ));
    }
    let back = |mut at: usize| {
        while at > 0 && !text.is_char_boundary(at) {
            at -= 1;
        }
        at
    };
    let from = back(offset);
    // At least the character at `offset`, so that reading on moves.
    let mut to = back(from.saturating_add(max_bytes).min(total));
    if to == from && from < total {
        to = text
            .get(from..)
            .and_then(|rest| rest.chars().next())
            .map_or(total, |c| from + c.len_utf8());
    }
    let shown = text.get(from..to).unwrap_or_default();
    let kind = content_type
        .map(crate::tools::visible)
        .unwrap_or_else(|| "no stated type".into());
    let mut head = format!("{url}\nstatus {status}, {kind}, {total} bytes of text");
    if from > 0 || to < total {
        head.push_str(&format!("; bytes {from} to {to} shown"));
    }
    if to < total {
        head.push_str(&format!("; read on with offset {to}"));
    }
    Ok(format!("{head}\n\n{shown}"))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
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
    fn a_page_is_text_and_anything_else_refused_by_its_type() {
        let page = b"<!DOCTYPE html><html><body><h1>Title</h1><p>Hello <a href=\"/x\">there</a>.</p><script>no()</script></body></html>";
        let text = text(Some("text/html; charset=utf-8"), page).unwrap();
        assert!(text.contains("Title"), "{text}");
        assert!(text.contains("Hello"), "{text}");
        assert!(text.contains("/x"), "{text}");
        assert!(!text.contains('<'), "{text}");
        // A page sent with no type is still a page.
        assert_eq!(super::text(None, page).unwrap(), text);
        assert_eq!(
            super::text(Some("application/json"), b"{\"a\":1}\r\n").unwrap(),
            "{\"a\":1}\n"
        );
        assert_eq!(
            super::text(Some("text/plain"), b"a\x1b[31mb\tc").unwrap(),
            "a[31mb\tc"
        );
        assert_eq!(super::text(None, b"plain").unwrap(), "plain");
        // A table's wide cell over many rows is rendered within the width,
        // not padded on every row to it.
        let mut wide = format!("<table><tr><td>{}</td></tr>", "w ".repeat(32 * 1024));
        wide.push_str(&"<tr><td>x</td></tr>".repeat(4096));
        wide.push_str("</table>");
        let rendered = super::text(Some("text/html"), wide.as_bytes()).unwrap();
        // Each row costs the width and its rule, not the cell's 64 KiB.
        assert!(
            rendered.len() < 4096 * 2 * 4 * HTML_WIDTH,
            "{}",
            rendered.len()
        );
        // A page past the bound says it was cut.
        let long = format!("<p>{}</p>", "a ".repeat(MAX_HTML));
        let cut = super::text(Some("text/html"), long.as_bytes()).unwrap();
        assert!(
            cut.ends_with(&format!("only its first {MAX_HTML} were rendered]\n")),
            "{}",
            cut.len()
        );
        let err = super::text(Some("image/png"), b"\x89PNG").unwrap_err();
        assert!(err.contains("image/png, 4 bytes, not text"), "{err}");
        let err = super::text(None, b"\xff\xfe").unwrap_err();
        assert!(err.contains("of no stated type"), "{err}");
    }

    #[test]
    fn a_long_text_is_read_a_part_at_a_time() {
        let text = "ab€cd";
        let whole = page("https://h/", 200, Some("text/plain"), text, 0, 100).unwrap();
        assert_eq!(
            whole,
            "https://h/\nstatus 200, text/plain, 7 bytes of text\n\nab€cd"
        );
        // A part ends before a character it would split, and says where
        // to read on.
        let first = page("https://h/", 200, Some("text/plain"), text, 0, 3).unwrap();
        assert!(
            first.ends_with("bytes 0 to 2 shown; read on with offset 2\n\nab"),
            "{first}"
        );
        let next = page("https://h/", 200, None, text, 2, 3).unwrap();
        assert!(next.contains("no stated type"), "{next}");
        assert!(
            next.ends_with("bytes 2 to 5 shown; read on with offset 5\n\n€"),
            "{next}"
        );
        let last = page("https://h/", 404, None, text, 5, 100).unwrap();
        assert!(last.contains("status 404"), "{last}");
        assert!(last.ends_with("bytes 5 to 7 shown\n\ncd"), "{last}");
        // A part smaller than the character at its offset still holds it.
        let one = page("https://h/", 200, None, text, 2, 1).unwrap();
        assert!(
            one.ends_with("bytes 2 to 5 shown; read on with offset 5\n\n€"),
            "{one}"
        );
        assert!(page("https://h/", 200, None, text, 7, 1)
            .unwrap_err()
            .contains("past the end"));
        assert!(page("https://h/", 200, None, "", 0, 1).is_ok());
    }
}

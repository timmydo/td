//! A pull parser for the XML subset that RSS and Atom feeds use.
//!
//! Input is `&str`, so there is no encoding handling. Elements, attributes,
//! text and CDATA are reported; the XML declaration, comments, processing
//! instructions and DOCTYPE (including an internal subset) are skipped.
//! Feeds are sloppy, so an unknown entity stays literal instead of failing
//! the document, but nesting is still checked: a mismatched or unclosed tag
//! is an error carrying the byte offset.

use std::fmt;

/// Maximum element nesting depth.
pub const MAX_DEPTH: usize = 256;

/// Longest entity reference considered; a longer run stays literal.
const MAX_ENTITY: usize = 40;

/// A start or empty element. The name is verbatim, namespace prefix
/// included (`atom:link`); attribute values are unescaped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tag {
    pub name: String,
    pub attrs: Vec<(String, String)>,
}

impl Tag {
    /// Value of the first attribute with this name.
    pub fn attr(&self, name: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    Start(Tag),
    Empty(Tag),
    End(String),
    /// Character data, unescaped. Whitespace-only runs are reported too;
    /// the caller decides whether they matter.
    Text(String),
    /// CDATA content, verbatim (no unescaping).
    CData(String),
    Eof,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ErrorKind {
    /// An element was still open at end of input.
    Unclosed(String),
    /// `</b>` closing `<a>`.
    Mismatched { expected: String, found: String },
    /// `</a>` with nothing open.
    Unopened(String),
    /// A tag, comment, CDATA, PI or DOCTYPE ran to end of input.
    Unterminated(&'static str),
    /// `<`, `</` or an attribute with no name.
    EmptyName,
    /// Nesting past [`MAX_DEPTH`].
    TooDeep,
}

impl fmt::Display for ErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ErrorKind::Unclosed(n) => write!(f, "unclosed <{}>", n),
            ErrorKind::Mismatched { expected, found } => {
                write!(f, "</{}> closes <{}>", found, expected)
            }
            ErrorKind::Unopened(n) => write!(f, "</{}> with no open element", n),
            ErrorKind::Unterminated(what) => write!(f, "unterminated {}", what),
            ErrorKind::EmptyName => write!(f, "missing name"),
            ErrorKind::TooDeep => write!(f, "nesting deeper than {}", MAX_DEPTH),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error {
    /// Byte offset in the input where the offending construct starts.
    pub offset: usize,
    pub kind: ErrorKind,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} at byte {}", self.kind, self.offset)
    }
}

impl std::error::Error for Error {}

/// A borrowing pull parser. Call [`Reader::next`] until it yields
/// [`Event::Eof`] or an error.
pub struct Reader<'a> {
    input: &'a str,
    pos: usize,
    open: Vec<String>,
}

impl<'a> Reader<'a> {
    pub fn new(input: &'a str) -> Reader<'a> {
        Reader {
            input,
            pos: 0,
            open: Vec::new(),
        }
    }

    /// Byte offset of the next unread byte.
    pub fn offset(&self) -> usize {
        self.pos
    }

    /// Next event. `Eof` repeats once the input is exhausted.
    // Named for the caller's read loop, not the Iterator contract: events
    // are fallible and the reader is not an iterator.
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> Result<Event, Error> {
        loop {
            let Some(&b) = self.input.as_bytes().get(self.pos) else {
                return match self.open.last() {
                    Some(name) => Err(Error {
                        offset: self.input.len(),
                        kind: ErrorKind::Unclosed(name.clone()),
                    }),
                    None => Ok(Event::Eof),
                };
            };
            if b != b'<' {
                return Ok(self.read_text());
            }
            if self.at(self.pos, "<!--") {
                self.skip_delimited(4, "-->", "comment")?;
                continue;
            }
            if self.at(self.pos, "<![CDATA[") {
                return self.read_cdata();
            }
            if self.at(self.pos, "<!") {
                self.skip_doctype()?;
                continue;
            }
            if self.at(self.pos, "<?") {
                self.skip_delimited(2, "?>", "processing instruction")?;
                continue;
            }
            if self.at(self.pos, "</") {
                return self.read_end();
            }
            return self.read_start();
        }
    }

    fn err(&self, offset: usize, kind: ErrorKind) -> Error {
        Error { offset, kind }
    }

    fn at(&self, pos: usize, lit: &str) -> bool {
        matches!(self.input.get(pos..), Some(rest) if rest.starts_with(lit))
    }

    fn slice(&self, from: usize, to: usize) -> &'a str {
        self.input.get(from..to).unwrap_or("")
    }

    fn find_from(&self, from: usize, needle: &str) -> Option<usize> {
        self.input.get(from..)?.find(needle).map(|i| from + i)
    }

    fn skip_ws(&self, mut i: usize) -> usize {
        while matches!(self.input.as_bytes().get(i), Some(b) if b.is_ascii_whitespace()) {
            i += 1;
        }
        i
    }

    /// End of a name starting at `i`. Only ASCII delimiters stop the scan,
    /// so the result is always a char boundary.
    fn scan_name(&self, mut i: usize) -> usize {
        while let Some(&b) = self.input.as_bytes().get(i) {
            if b.is_ascii_whitespace() || matches!(b, b'/' | b'>' | b'=' | b'<') {
                break;
            }
            i += 1;
        }
        i
    }

    fn read_text(&mut self) -> Event {
        let start = self.pos;
        let end = self.find_from(start, "<").unwrap_or(self.input.len());
        self.pos = end;
        Event::Text(unescape(self.slice(start, end)))
    }

    fn skip_delimited(
        &mut self,
        open: usize,
        close: &str,
        what: &'static str,
    ) -> Result<(), Error> {
        let start = self.pos;
        let from = start.saturating_add(open);
        let end = self
            .find_from(from, close)
            .ok_or_else(|| self.err(start, ErrorKind::Unterminated(what)))?;
        self.pos = end + close.len();
        Ok(())
    }

    fn read_cdata(&mut self) -> Result<Event, Error> {
        let start = self.pos;
        let from = start + "<![CDATA[".len();
        let end = self
            .find_from(from, "]]>")
            .ok_or_else(|| self.err(start, ErrorKind::Unterminated("CDATA section")))?;
        self.pos = end + 3;
        Ok(Event::CData(self.slice(from, end).to_string()))
    }

    /// `<!DOCTYPE ...>` and friends: quotes and an internal subset may hide
    /// a `>`.
    fn skip_doctype(&mut self) -> Result<(), Error> {
        let start = self.pos;
        let mut i = start + 2;
        let mut quote = 0u8;
        let mut subset = false;
        while let Some(&b) = self.input.as_bytes().get(i) {
            if quote != 0 {
                if b == quote {
                    quote = 0;
                }
            } else {
                match b {
                    b'"' | b'\'' => quote = b,
                    b'[' => subset = true,
                    b']' => subset = false,
                    b'>' if !subset => {
                        self.pos = i + 1;
                        return Ok(());
                    }
                    _ => {}
                }
            }
            i += 1;
        }
        Err(self.err(start, ErrorKind::Unterminated("DOCTYPE")))
    }

    fn read_start(&mut self) -> Result<Event, Error> {
        let start = self.pos;
        let name_start = start + 1;
        let name_end = self.scan_name(name_start);
        if name_end == name_start {
            return Err(self.err(start, ErrorKind::EmptyName));
        }
        let name = self.slice(name_start, name_end).to_string();
        let mut attrs: Vec<(String, String)> = Vec::new();
        let mut i = name_end;
        loop {
            i = self.skip_ws(i);
            match self.input.as_bytes().get(i) {
                None => return Err(self.err(start, ErrorKind::Unterminated("tag"))),
                Some(b'>') => {
                    if self.open.len() >= MAX_DEPTH {
                        return Err(self.err(start, ErrorKind::TooDeep));
                    }
                    self.pos = i + 1;
                    self.open.push(name.clone());
                    return Ok(Event::Start(Tag { name, attrs }));
                }
                Some(b'/') if self.input.as_bytes().get(i + 1) == Some(&b'>') => {
                    self.pos = i + 2;
                    return Ok(Event::Empty(Tag { name, attrs }));
                }
                Some(_) => {
                    let (attr, next) = self.read_attr(i, start)?;
                    if next <= i {
                        return Err(self.err(i, ErrorKind::EmptyName));
                    }
                    attrs.push(attr);
                    i = next;
                }
            }
        }
    }

    /// One attribute starting at `i`; returns it and the next scan position.
    fn read_attr(&self, i: usize, tag: usize) -> Result<((String, String), usize), Error> {
        let name_end = self.scan_name(i);
        if name_end == i {
            return Err(self.err(i, ErrorKind::EmptyName));
        }
        let name = self.slice(i, name_end).to_string();
        let eq = self.skip_ws(name_end);
        if self.input.as_bytes().get(eq) != Some(&b'=') {
            // Sloppy feeds emit valueless attributes; keep the name.
            return Ok(((name, String::new()), name_end));
        }
        let val = self.skip_ws(eq + 1);
        match self.input.as_bytes().get(val) {
            Some(&q) if q == b'"' || q == b'\'' => {
                let from = val + 1;
                let end = self
                    .input
                    .as_bytes()
                    .get(from..)
                    .and_then(|rest| rest.iter().position(|&b| b == q))
                    .map(|p| from + p)
                    .ok_or_else(|| self.err(tag, ErrorKind::Unterminated("attribute value")))?;
                Ok(((name, unescape(self.slice(from, end))), end + 1))
            }
            Some(_) => {
                // Unquoted value: tolerated, ends at whitespace or the close.
                // A bare `/` stays in the value so URLs survive.
                let mut end = val;
                while let Some(&b) = self.input.as_bytes().get(end) {
                    let closing = b == b'/' && self.input.as_bytes().get(end + 1) == Some(&b'>');
                    if b.is_ascii_whitespace() || b == b'>' || closing {
                        break;
                    }
                    end += 1;
                }
                Ok(((name, unescape(self.slice(val, end))), end))
            }
            None => Err(self.err(tag, ErrorKind::Unterminated("tag"))),
        }
    }

    fn read_end(&mut self) -> Result<Event, Error> {
        let start = self.pos;
        let name_start = start + 2;
        let name_end = self.scan_name(name_start);
        if name_end == name_start {
            return Err(self.err(start, ErrorKind::EmptyName));
        }
        let name = self.slice(name_start, name_end).to_string();
        let close = self.skip_ws(name_end);
        if self.input.as_bytes().get(close) != Some(&b'>') {
            return Err(self.err(start, ErrorKind::Unterminated("tag")));
        }
        self.pos = close + 1;
        match self.open.pop() {
            None => Err(self.err(start, ErrorKind::Unopened(name))),
            Some(expected) if expected != name => Err(self.err(
                start,
                ErrorKind::Mismatched {
                    expected,
                    found: name,
                },
            )),
            Some(_) => Ok(Event::End(name)),
        }
    }
}

/// Expand the predefined and numeric entities. An unknown named entity is
/// kept literally, `&` included.
pub fn unescape(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(amp) = rest.find('&') {
        out.push_str(rest.get(..amp).unwrap_or(""));
        let tail = rest.get(amp..).unwrap_or("");
        match entity(tail) {
            Some((ch, len)) => {
                out.push(ch);
                rest = tail.get(len..).unwrap_or("");
            }
            None => {
                out.push('&');
                rest = tail.get(1..).unwrap_or("");
            }
        }
    }
    out.push_str(rest);
    out
}

/// `s` starts with `&`; returns the character and the reference's length.
fn entity(s: &str) -> Option<(char, usize)> {
    let semi = s
        .as_bytes()
        .iter()
        .take(MAX_ENTITY)
        .position(|&b| b == b';')?;
    let body = s.get(1..semi)?;
    let ch = match body {
        "lt" => '<',
        "gt" => '>',
        "amp" => '&',
        "quot" => '"',
        "apos" => '\'',
        _ => {
            let num = body.strip_prefix('#')?;
            let (radix, digits) = match num.strip_prefix(['x', 'X']) {
                Some(hex) => (16, hex),
                None => (10, num),
            };
            if digits.is_empty() {
                return None;
            }
            char::from_u32(u32::from_str_radix(digits, radix).ok()?)?
        }
    };
    Some((ch, semi + 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn events(xml: &str) -> Result<Vec<Event>, Error> {
        let mut r = Reader::new(xml);
        let mut out = Vec::new();
        loop {
            match r.next()? {
                Event::Eof => return Ok(out),
                ev => out.push(ev),
            }
        }
    }

    fn fail(xml: &str) -> Error {
        let mut r = Reader::new(xml);
        loop {
            match r.next() {
                Ok(Event::Eof) => panic!("expected an error"),
                Ok(_) => continue,
                Err(e) => return e,
            }
        }
    }

    /// Text of every element with this name, in document order.
    fn texts(xml: &str, tag: &str) -> Vec<String> {
        let mut r = Reader::new(xml);
        let mut out = Vec::new();
        let mut current: Option<String> = None;
        loop {
            match r.next() {
                Ok(Event::Start(t)) => current = Some(t.name),
                Ok(Event::End(_)) => current = None,
                Ok(Event::Text(t)) | Ok(Event::CData(t)) => {
                    if current.as_deref() == Some(tag) {
                        out.push(t);
                    }
                }
                Ok(Event::Empty(_)) => {}
                Ok(Event::Eof) => return out,
                Err(e) => panic!("{}", e),
            }
        }
    }

    const RSS: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<rss version="2.0" xmlns:content="http://purl.org/rss/1.0/modules/content/">
  <channel>
    <title>Test Feed</title>
    <link>https://example.com</link>
    <item>
      <title>First Post</title>
      <link>https://example.com/1</link>
      <description>Hello &amp; world</description>
      <content:encoded><![CDATA[<p>HTML content</p>]]></content:encoded>
      <pubDate>Mon, 01 Jan 2024 00:00:00 GMT</pubDate>
    </item>
    <item>
      <title>Second Post</title>
      <link>https://example.com/2</link>
      <description>Another post</description>
    </item>
  </channel>
</rss>"#;

    const ATOM: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<feed xmlns="http://www.w3.org/2005/Atom">
  <title>Atom Feed</title>
  <entry>
    <title>Atom Entry</title>
    <link href="https://example.com/atom/1" rel="alternate" />
    <link rel='self' href='https://example.com/atom/1.xml'/>
    <summary>An atom summary</summary>
    <content>Full content here</content>
    <published>2024-01-01T00:00:00Z</published>
  </entry>
</feed>"#;

    #[test]
    fn rss_sample_yields_the_fields_a_feed_reader_wants() {
        assert_eq!(
            texts(RSS, "title"),
            ["Test Feed", "First Post", "Second Post"]
        );
        assert_eq!(
            texts(RSS, "link"),
            [
                "https://example.com",
                "https://example.com/1",
                "https://example.com/2"
            ]
        );
        assert_eq!(texts(RSS, "description"), ["Hello & world", "Another post"]);
        assert_eq!(texts(RSS, "content:encoded"), ["<p>HTML content</p>"]);
        assert_eq!(texts(RSS, "pubDate"), ["Mon, 01 Jan 2024 00:00:00 GMT"]);
    }

    #[test]
    fn rss_start_and_end_names_keep_their_namespace_prefix() {
        let evs = events(RSS).expect("rss parses");
        assert!(evs.contains(&Event::End("content:encoded".to_string())));
        let starts: Vec<&str> = evs
            .iter()
            .filter_map(|e| match e {
                Event::Start(t) => Some(t.name.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(starts.first().copied(), Some("rss"));
        assert!(starts.contains(&"content:encoded"));
    }

    #[test]
    fn rss_root_attributes_are_reported() {
        let evs = events(RSS).expect("rss parses");
        let root = evs
            .iter()
            .find_map(|e| match e {
                Event::Start(t) if t.name == "rss" => Some(t),
                _ => None,
            })
            .expect("expected a root start tag");
        assert_eq!(root.attr("version"), Some("2.0"));
        assert_eq!(
            root.attr("xmlns:content"),
            Some("http://purl.org/rss/1.0/modules/content/")
        );
        assert_eq!(root.attr("nope"), None);
    }

    #[test]
    fn atom_link_attributes_come_through_both_quote_styles() {
        let links: Vec<Tag> = events(ATOM)
            .expect("atom parses")
            .into_iter()
            .filter_map(|e| match e {
                Event::Empty(t) if t.name == "link" => Some(t),
                _ => None,
            })
            .collect();
        assert_eq!(links.len(), 2);
        assert_eq!(
            links.first().and_then(|t| t.attr("href")),
            Some("https://example.com/atom/1")
        );
        assert_eq!(links.first().and_then(|t| t.attr("rel")), Some("alternate"));
        assert_eq!(links.get(1).and_then(|t| t.attr("rel")), Some("self"));
        assert_eq!(
            links.get(1).and_then(|t| t.attr("href")),
            Some("https://example.com/atom/1.xml")
        );
    }

    #[test]
    fn atom_text_children_are_reported() {
        assert_eq!(texts(ATOM, "title"), ["Atom Feed", "Atom Entry"]);
        assert_eq!(texts(ATOM, "summary"), ["An atom summary"]);
        assert_eq!(texts(ATOM, "published"), ["2024-01-01T00:00:00Z"]);
    }

    #[test]
    fn cdata_keeps_markup_verbatim() {
        let xml = "<t><![CDATA[Post with <special> & \"chars\" ]]]></t>";
        let evs = events(xml).expect("cdata parses");
        assert_eq!(
            evs.get(1),
            Some(&Event::CData(
                "Post with <special> & \"chars\" ]".to_string()
            ))
        );
    }

    #[test]
    fn entities_named_numeric_and_unknown() {
        let xml = "<t>&lt;a&gt; &amp; &quot;b&quot; &apos;c&apos; \
                   &#65;&#x42;&#x2603; &nbsp; &bogus &amp</t>";
        assert_eq!(
            texts(xml, "t"),
            ["<a> & \"b\" 'c' AB\u{2603} &nbsp; &bogus &amp"]
        );
    }

    #[test]
    fn numeric_entity_out_of_range_stays_literal() {
        assert_eq!(
            texts("<t>&#xD800; &#99999999;</t>", "t"),
            ["&#xD800; &#99999999;"]
        );
    }

    #[test]
    fn self_closing_tags_with_attributes_in_both_quote_styles() {
        let xml = r#"<a><b x="1" y='2'/><c d="a>b" e='f/g'  /><d/></a>"#;
        let evs = events(xml).expect("parses");
        let Some(Event::Empty(b)) = evs.get(1) else {
            panic!("expected <b/>");
        };
        assert_eq!(
            b.attrs,
            vec![
                ("x".to_string(), "1".to_string()),
                ("y".to_string(), "2".to_string())
            ]
        );
        let Some(Event::Empty(c)) = evs.get(2) else {
            panic!("expected <c/>");
        };
        assert_eq!(c.attr("d"), Some("a>b"));
        assert_eq!(c.attr("e"), Some("f/g"));
        assert_eq!(
            evs.get(3),
            Some(&Event::Empty(Tag {
                name: "d".to_string(),
                attrs: Vec::new()
            }))
        );
    }

    #[test]
    fn attribute_values_are_unescaped() {
        let xml = r#"<a href="x?a=1&amp;b=2&#38;c=3"/>"#;
        let evs = events(xml).expect("parses");
        let Some(Event::Empty(a)) = evs.first() else {
            panic!("expected <a/>");
        };
        assert_eq!(a.attr("href"), Some("x?a=1&b=2&c=3"));
    }

    #[test]
    fn sloppy_attributes_are_tolerated() {
        let xml = "<a disabled href=http://example.com/x b=1>t</a>";
        let evs = events(xml).expect("parses");
        let Some(Event::Start(a)) = evs.first() else {
            panic!("expected <a>");
        };
        assert_eq!(a.attr("disabled"), Some(""));
        assert_eq!(a.attr("href"), Some("http://example.com/x"));
        assert_eq!(a.attr("b"), Some("1"));
    }

    #[test]
    fn declaration_comment_pi_and_doctype_are_skipped() {
        let xml = concat!(
            "<?xml version=\"1.0\"?>",
            "<!-- a > comment with -- dashes -->",
            "<?php echo \">\"; ?>",
            "<!DOCTYPE rss PUBLIC \"-//x>y//EN\" \"http://e/x.dtd\" [\n",
            "  <!ENTITY foo \"bar>baz\">\n",
            "]>",
            "<r>ok</r>",
            "<!-- trailing -->"
        );
        assert_eq!(
            events(xml).expect("parses"),
            vec![
                Event::Start(Tag {
                    name: "r".to_string(),
                    attrs: Vec::new()
                }),
                Event::Text("ok".to_string()),
                Event::End("r".to_string()),
            ]
        );
    }

    #[test]
    fn whitespace_only_text_is_still_reported() {
        let evs = events("<a>\n  <b/>\n</a>").expect("parses");
        assert_eq!(evs.get(1), Some(&Event::Text("\n  ".to_string())));
        assert_eq!(evs.get(3), Some(&Event::Text("\n".to_string())));
    }

    #[test]
    fn eof_repeats_and_offset_tracks_the_input() {
        let mut r = Reader::new("<a/>");
        assert!(matches!(r.next(), Ok(Event::Empty(_))));
        assert_eq!(r.offset(), 4);
        assert_eq!(r.next(), Ok(Event::Eof));
        assert_eq!(r.next(), Ok(Event::Eof));
    }

    #[test]
    fn multibyte_text_and_names_survive() {
        let xml = "<é attr=\"héllo\">naïve — text</é>";
        let evs = events(xml).expect("parses");
        let Some(Event::Start(t)) = evs.first() else {
            panic!("expected a start tag");
        };
        assert_eq!(t.name, "é");
        assert_eq!(t.attr("attr"), Some("héllo"));
        assert_eq!(evs.get(1), Some(&Event::Text("naïve — text".to_string())));
    }

    #[test]
    fn mismatched_end_tag_is_an_error_with_an_offset() {
        let err = fail("<a><b></c></a>");
        assert_eq!(err.offset, 6);
        assert_eq!(
            err.kind,
            ErrorKind::Mismatched {
                expected: "b".to_string(),
                found: "c".to_string()
            }
        );
        assert!(err.to_string().contains("byte 6"));
    }

    #[test]
    fn unclosed_element_at_eof_is_an_error() {
        let err = fail("<a><b>text");
        assert_eq!(err.kind, ErrorKind::Unclosed("b".to_string()));
        assert_eq!(err.offset, 10);
    }

    #[test]
    fn end_tag_without_a_start_is_an_error() {
        let err = fail("<a></a></a>");
        assert_eq!(err.kind, ErrorKind::Unopened("a".to_string()));
        assert_eq!(err.offset, 7);
    }

    #[test]
    fn deep_nesting_is_bounded() {
        let deep: String = "<a>".repeat(MAX_DEPTH + 1);
        let err = fail(&deep);
        assert_eq!(err.kind, ErrorKind::TooDeep);
        assert_eq!(err.offset, MAX_DEPTH * 3);
        let ok = format!("{}{}", "<a>".repeat(MAX_DEPTH), "</a>".repeat(MAX_DEPTH));
        assert!(events(&ok).is_ok());
    }

    #[test]
    fn unterminated_constructs_are_errors() {
        assert_eq!(
            fail("<a><!-- oops").kind,
            ErrorKind::Unterminated("comment")
        );
        assert_eq!(
            fail("<a><![CDATA[oops").kind,
            ErrorKind::Unterminated("CDATA section")
        );
        assert_eq!(
            fail("<?pi oops").kind,
            ErrorKind::Unterminated("processing instruction")
        );
        assert_eq!(
            fail("<!DOCTYPE r [").kind,
            ErrorKind::Unterminated("DOCTYPE")
        );
        assert_eq!(
            fail("<a b=\"c").kind,
            ErrorKind::Unterminated("attribute value")
        );
        assert_eq!(fail("<a b").kind, ErrorKind::Unterminated("tag"));
        assert_eq!(fail("</a").kind, ErrorKind::Unterminated("tag"));
    }

    #[test]
    fn a_bare_angle_bracket_in_text_is_an_error() {
        let err = fail("<t>a < b</t>");
        assert_eq!(err.kind, ErrorKind::EmptyName);
        assert_eq!(err.offset, 5);
    }
}

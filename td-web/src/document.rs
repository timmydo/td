//! A response as td-web shows it (DESIGN.md §5): HTML rendered through
//! td-html's rich output, text as it is, a PNG or JPEG decoded, anything
//! else named and not shown; and the rendering, lines of styled runs
//! with link targets, made again for each width the page is shown at.

use td_ui::picture::{Picture, MAX_PICTURE_AXIS, MAX_PICTURE_PIXELS};
use td_url::{Reference, Url};

/// The most bytes of markup rendered; the rest is not shown. td-html
/// builds a page's whole rendering before td-web keeps `MAX_LINES` of it,
/// so this bounds that work: a hostile page of empty paragraphs this size
/// costs about a second and some 270 MB at each width (DESIGN.md §10).
pub const MAX_MARKUP: usize = 2 << 20;
/// The most bytes of text rendered; its wrapping stops at `MAX_LINES`.
pub const MAX_TEXT: usize = 4 << 20;
/// The most characters one line of text is expanded to.
const MAX_LINE_CHARS: usize = 1 << 20;
/// The most lines a rendering keeps; the rest are not shown.
pub const MAX_LINES: usize = 100_000;
/// The most bytes searched for a title, from the start of the page.
const TITLE_WINDOW: usize = 64 << 10;
/// The longest title kept, in characters.
const MAX_TITLE: usize = 200;
/// A tab's stop in plain text.
const TAB: usize = 8;
/// What td-html's rich output puts after each struck character, which
/// td-web draws as a line of its own.
const STROKE: char = '\u{336}';

/// How a run of text looks.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Style {
    pub bold: bool,
    pub italic: bool,
    pub strike: bool,
    pub code: bool,
}

/// Text of one look, and the link it belongs to.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Run {
    pub text: String,
    pub style: Style,
    pub link: Option<usize>,
}

/// A link as the document wrote it, joined against the document's URL
/// when it is shown or followed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Link {
    pub written: String,
}

/// A document rendered at one width.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Rendered {
    pub width: usize,
    pub lines: Vec<Vec<Run>>,
    pub links: Vec<Link>,
    /// The first line each link is on, by link.
    pub link_lines: Vec<usize>,
    /// Whether lines past `MAX_LINES` were left out.
    pub cut: bool,
    base: Option<Url>,
}

impl Rendered {
    /// What link `at` names.
    pub fn target(&self, at: usize) -> Option<Result<Reference, String>> {
        let link = self.links.get(at)?;
        Some(match &self.base {
            Some(base) => td_url::resolve(base, &link.written),
            None => Err("the page has no address to join it against".into()),
        })
    }

    /// What the status row says link `at` goes to.
    pub fn shown(&self, at: usize) -> Option<String> {
        let written = &self.links.get(at)?.written;
        Some(match self.target(at)? {
            Ok(Reference::Fetch {
                url,
                fragment: Some(fragment),
            }) => format!("{}#{}", url.text(), visible(&fragment)),
            Ok(Reference::Fetch {
                url,
                fragment: None,
            }) => url.text(),
            Ok(Reference::Other { .. }) | Err(_) => visible(written),
        })
    }
}

/// `text` with every control character and line or paragraph separator
/// percent-encoded, so a single-line row shows all of it.
pub fn visible(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut bytes = [0u8; 4];
    for c in text.chars() {
        if c.is_control() || matches!(c, '\u{2028}' | '\u{2029}') {
            for byte in c.encode_utf8(&mut bytes).bytes() {
                out.push_str(&format!("%{byte:02X}"));
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// What a document is shown as.
enum Body {
    Html(Vec<u8>),
    Text(String),
    Picture(Picture),
}

pub struct Document {
    /// The URL that answered; none for an address td-web did not take.
    pub url: Option<Url>,
    pub status: u16,
    pub title: Option<String>,
    /// What the status row says once it has nothing better to say: the
    /// page was cut short, say.
    pub note: Option<String>,
    body: Body,
}

impl Document {
    /// `body`, answered with `status` from `url` as `content_type`.
    pub fn from_reply(url: Url, status: u16, content_type: Option<&str>, body: Vec<u8>) -> Self {
        if body.is_empty() && !(200..300).contains(&status) {
            let why = format!("The server answered {status} and sent nothing to show.");
            return Self {
                status,
                ..Self::error(Some(url), &why)
            };
        }
        let media = content_type
            .and_then(|value| value.split(';').next())
            .map(|media| media.trim().to_ascii_lowercase())
            .unwrap_or_default();
        let kind = match media.as_str() {
            "text/html" | "application/xhtml+xml" => Kind::Html,
            "image/png" => Kind::Png,
            "image/jpeg" | "image/pjpeg" => Kind::Jpeg,
            "" | "application/octet-stream" => sniff(&body),
            _ if media.starts_with("text/") => Kind::Text,
            _ => Kind::Other,
        };
        // td-html would show a byte order mark as text.
        let body = match body.strip_prefix(b"\xef\xbb\xbf") {
            Some(rest) if matches!(kind, Kind::Html | Kind::Text) => rest.to_vec(),
            _ => body,
        };
        let size = body.len();
        let cut = |most: usize| {
            (size > most)
                .then(|| format!("only the first {most} of the page's {size} bytes are shown"))
        };
        let (body, title, note) = match kind {
            Kind::Html => {
                let title = title(&body);
                let mut body = body;
                body.truncate(MAX_MARKUP);
                (Body::Html(body), title, cut(MAX_MARKUP))
            }
            Kind::Text => {
                let shown = body.get(..MAX_TEXT).unwrap_or(&body);
                (
                    Body::Text(String::from_utf8_lossy(shown).into_owned()),
                    None,
                    cut(MAX_TEXT),
                )
            }
            Kind::Png | Kind::Jpeg => match picture(kind, &body) {
                Ok(picture) => (Body::Picture(picture), None, None),
                Err(why) => (Body::Text(why), None, None),
            },
            Kind::Other => {
                let named = if media.is_empty() {
                    "of no stated type".to_string()
                } else {
                    visible(&media)
                };
                (
                    Body::Text(format!(
                        "This is {named}, {size} bytes.\n\ntd-web shows HTML, text, PNG and \
                         JPEG; Firefox opens the rest."
                    )),
                    None,
                    None,
                )
            }
        };
        Self {
            url: Some(url),
            status,
            title,
            note,
            body,
        }
    }

    /// A page saying why there is no document.
    pub fn error(url: Option<Url>, why: &str) -> Self {
        Self {
            url,
            status: 0,
            title: None,
            note: None,
            body: Body::Text(why.to_string()),
        }
    }

    /// The start page.
    pub fn blank() -> Self {
        Self::error(None, "")
    }

    pub fn picture(&self) -> Option<&Picture> {
        match &self.body {
            Body::Picture(picture) => Some(picture),
            _ => None,
        }
    }

    /// The document's lines at `width` columns (at least one), at most
    /// `MAX_LINES` of them.
    pub fn render(&self, width: usize) -> Rendered {
        let width = width.max(1);
        let mut rendered = match &self.body {
            Body::Html(source) => html(source, width),
            Body::Picture(_) => Rendered::default(),
            Body::Text(text) => wrap(text, width),
        };
        rendered.width = width;
        rendered.base = self.url.clone();
        rendered
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Kind {
    Html,
    Text,
    Png,
    Jpeg,
    Other,
}

/// What a body of no stated type is, by its first bytes.
fn sniff(body: &[u8]) -> Kind {
    let unmarked = body.strip_prefix(b"\xef\xbb\xbf").unwrap_or(body);
    let start: Vec<u8> = unmarked
        .iter()
        .skip_while(|b| b.is_ascii_whitespace())
        .take(15)
        .map(u8::to_ascii_lowercase)
        .collect();
    if start.starts_with(b"<!doctype html") || start.starts_with(b"<html") {
        Kind::Html
    } else if body.starts_with(b"\x89PNG\r\n\x1a\n") {
        Kind::Png
    } else if body.starts_with(&[0xff, 0xd8, 0xff]) {
        Kind::Jpeg
    } else if !body.contains(&0) && std::str::from_utf8(body).is_ok() {
        Kind::Text
    } else {
        Kind::Other
    }
}

/// The page's first `<title>`, its references decoded and its spaces
/// collapsed. A heuristic: a `<title>` inside a script or a comment is
/// taken as one.
fn title(html: &[u8]) -> Option<String> {
    let head = html.get(..html.len().min(TITLE_WINDOW))?;
    let lower = head.to_ascii_lowercase();
    let mut from = 0;
    let open = loop {
        let at = from + find(lower.get(from..)?, b"<title")?;
        let after = lower.get(at + 6).copied();
        if matches!(after, Some(b'>' | b'/' | b' ' | b'\t' | b'\n' | b'\r')) {
            break at + 6;
        }
        from = at + 6;
    };
    let start = open + 1 + lower.get(open..)?.iter().position(|&b| b == b'>')?;
    let end = start + find(lower.get(start..)?, b"</title")?;
    let text = td_html::to_text(head.get(start..end)?, 0);
    let collapsed: String = text
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .filter(|c| !c.is_control())
        .take(MAX_TITLE)
        .collect();
    (!collapsed.is_empty()).then_some(collapsed)
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// `source` through td-html's rich output at `width`. Runs of one href
/// next to each other, across a wrap too, are one link, so it is focused
/// and followed as one; a blank line between them ends it.
fn html(source: &[u8], width: usize) -> Rendered {
    let mut rendered = Rendered::default();
    let mut last: Option<(String, usize)> = None;
    for spans in td_html::to_rich(source, width) {
        if rendered.lines.len() == MAX_LINES {
            rendered.cut = true;
            break;
        }
        if spans.iter().all(|span| span.text.trim().is_empty()) {
            last = None;
        }
        let mut line = Vec::with_capacity(spans.len());
        for span in spans {
            let mut style = Style::default();
            let mut href = None;
            let mut image = None;
            for tag in &span.tags {
                match tag {
                    td_html::Tag::Strong => style.bold = true,
                    td_html::Tag::Emphasis => style.italic = true,
                    td_html::Tag::Strikeout => style.strike = true,
                    td_html::Tag::Code => style.code = true,
                    td_html::Tag::Link(target) => href = Some(target.as_str()),
                    td_html::Tag::Image(target) => image = Some(target.as_str()),
                    td_html::Tag::Preformat
                    | td_html::Tag::Colour(_)
                    | td_html::Tag::BgColour(_) => {}
                }
            }
            // An image's alt text is a link to the image unless it is
            // inside a link already.
            let link = match href.or(image) {
                Some(target) => match &last {
                    Some((previous, at)) if previous == target => Some(*at),
                    _ => {
                        let at = rendered.links.len();
                        rendered.links.push(Link {
                            written: target.to_string(),
                        });
                        rendered.link_lines.push(rendered.lines.len());
                        last = Some((target.to_string(), at));
                        Some(at)
                    }
                },
                None => {
                    if !span.text.trim().is_empty() {
                        last = None;
                    }
                    None
                }
            };
            let text = if style.strike {
                span.text.chars().filter(|&c| c != STROKE).collect()
            } else {
                span.text
            };
            line.push(Run { text, style, link });
        }
        rendered.lines.push(line);
    }
    rendered
}

/// `text`'s lines, tabs expanded, each wrapped at `width` columns at its
/// last space that fits, else at `width`.
fn wrap(text: &str, width: usize) -> Rendered {
    let mut rendered = Rendered::default();
    let mut chars: Vec<char> = Vec::new();
    'lines: for raw in text.lines() {
        chars.clear();
        for c in raw.chars() {
            if chars.len() >= MAX_LINE_CHARS {
                break;
            }
            if c == '\t' {
                let pad = TAB - chars.len() % TAB;
                chars.extend(std::iter::repeat_n(' ', pad));
            } else {
                chars.push(c);
            }
        }
        let mut rest: &[char] = &chars;
        loop {
            if rendered.lines.len() == MAX_LINES {
                rendered.cut = true;
                break 'lines;
            }
            if rest.len() <= width {
                rendered.lines.push(plain(rest.iter().collect()));
                break;
            }
            let window = rest.get(..=width).unwrap_or(rest);
            let cut = match window.iter().rposition(|&c| c == ' ') {
                Some(space) if space > 0 => space,
                _ => width,
            };
            let (head, tail) = rest.split_at(cut.min(rest.len()));
            rendered.lines.push(plain(head.iter().collect()));
            rest = match tail.first() {
                Some(' ') => tail.get(1..).unwrap_or_default(),
                _ => tail,
            };
        }
    }
    rendered
}

fn plain(text: String) -> Vec<Run> {
    vec![Run {
        text,
        style: Style::default(),
        link: None,
    }]
}

/// Whether a picture this size is within td-ui's bounds.
fn fits(width: usize, height: usize) -> bool {
    width <= MAX_PICTURE_AXIS
        && height <= MAX_PICTURE_AXIS
        && width.saturating_mul(height) <= MAX_PICTURE_PIXELS
}

/// `body` decoded as `kind`, a PNG or a JPEG, within td-ui's picture
/// bounds, judged from the header before anything is decoded: a JPEG too
/// large is decoded at the largest of td-jpeg's scales that fits; a PNG
/// too large is refused.
fn picture(kind: Kind, body: &[u8]) -> Result<Picture, String> {
    let (width, height, rgba) = if kind == Kind::Png {
        let header =
            td_png::header(body).map_err(|why| format!("The PNG could not be read: {why}"))?;
        if !fits(header.width, header.height) {
            return Err(format!(
                "The PNG is {} by {} pixels, too large to show.",
                header.width, header.height
            ));
        }
        let image =
            td_png::decode(body).map_err(|why| format!("The PNG could not be decoded: {why}"))?;
        (image.width, image.height, image.data)
    } else {
        let header =
            td_jpeg::header(body).map_err(|why| format!("The JPEG could not be read: {why}"))?;
        let scale = [
            td_jpeg::Scale::Full,
            td_jpeg::Scale::Half,
            td_jpeg::Scale::Quarter,
            td_jpeg::Scale::Eighth,
        ]
        .into_iter()
        .find(|&scale| {
            fits(
                td_jpeg::scaled(header.width, scale),
                td_jpeg::scaled(header.height, scale),
            )
        })
        .ok_or_else(|| {
            format!(
                "The JPEG is {} by {} pixels, too large to show.",
                header.width, header.height
            )
        })?;
        let image = td_jpeg::decode(body, scale)
            .map_err(|why| format!("The JPEG could not be decoded: {why}"))?;
        let mut rgba = Vec::new();
        rgba.try_reserve_exact(image.data.len() / 3 * 4)
            .map_err(|_| "There is not the memory to show the JPEG.".to_string())?;
        let (pixels, _) = image.data.as_chunks::<3>();
        for pixel in pixels {
            rgba.extend_from_slice(pixel);
            rgba.push(0xff);
        }
        (image.width, image.height, rgba)
    };
    Picture::new(width, height, rgba).map_err(|why| format!("The picture cannot be shown: {why}"))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]
    use super::*;

    fn base() -> Url {
        Url::parse("https://example.org/docs/page").unwrap()
    }

    fn text_of(rendered: &Rendered) -> Vec<String> {
        rendered
            .lines
            .iter()
            .map(|line| line.iter().map(|run| run.text.as_str()).collect())
            .collect()
    }

    fn html_page(body: &[u8]) -> Rendered {
        Document::from_reply(base(), 200, Some("text/html"), body.to_vec()).render(80)
    }

    #[test]
    fn html_keeps_its_links_joined_against_the_page_and_its_looks() {
        let page = b"<html><head><title> A &amp; B </title></head><body>\
            <p><b>bold</b> <a href=\"next\">next page</a> and \
            <a href=\"mailto:a@example.org\">mail</a> <code>c</code>\
            <img src=\"/p.png\" alt=\"Pic\"></p></body></html>";
        let doc =
            Document::from_reply(base(), 200, Some("text/html; charset=utf-8"), page.to_vec());
        assert_eq!(doc.title.as_deref(), Some("A & B"));
        let rendered = doc.render(80);
        assert_eq!(rendered.links.len(), 3);
        assert_eq!(rendered.shown(0).unwrap(), "https://example.org/docs/next");
        assert_eq!(rendered.shown(1).unwrap(), "mailto:a@example.org");
        assert_eq!(
            rendered.target(1).unwrap(),
            Ok(Reference::Other {
                scheme: "mailto".into()
            })
        );
        assert_eq!(rendered.shown(2).unwrap(), "https://example.org/p.png");
        assert!(rendered.target(3).is_none());
        let runs: Vec<&Run> = rendered.lines.iter().flatten().collect();
        let bold = runs.iter().find(|run| run.text == "bold").unwrap();
        assert!(bold.style.bold && bold.link.is_none());
        let code = runs.iter().find(|run| run.text == "c").unwrap();
        assert!(code.style.code);
        assert!(runs
            .iter()
            .any(|run| run.text == "Pic" && run.link == Some(2)));
    }

    #[test]
    fn a_link_wrapped_across_lines_is_one_link_and_a_paragraph_ends_it() {
        let page = b"<p><a href=\"/x\">one two three four five six</a> after</p>";
        let rendered =
            Document::from_reply(base(), 200, Some("text/html"), page.to_vec()).render(10);
        assert!(rendered.lines.len() > 1, "{:?}", text_of(&rendered));
        assert_eq!(rendered.links.len(), 1);
        assert_eq!(rendered.link_lines, [0]);
        let rendered = html_page(b"<p><a href=\"/x\">one</a></p><p><a href=\"/x\">two</a></p>");
        assert_eq!(rendered.links.len(), 2, "{:?}", text_of(&rendered));
    }

    #[test]
    fn struck_text_keeps_a_cell_a_character() {
        let rendered = html_page(b"<p>x <s>ab</s> y</p>");
        let struck = rendered.lines[0]
            .iter()
            .find(|run| run.style.strike)
            .unwrap();
        assert_eq!(struck.text, "ab");
    }

    #[test]
    fn a_page_of_no_stated_type_is_sniffed() {
        let doc = Document::from_reply(base(), 200, None, b"\n <!DOCTYPE html><p>x".to_vec());
        assert_eq!(text_of(&doc.render(20)), ["x"]);
        let doc = Document::from_reply(base(), 200, None, b"\xef\xbb\xbf<html><p>y".to_vec());
        assert_eq!(text_of(&doc.render(20)), ["y"], "past a byte order mark");
        let doc = Document::from_reply(base(), 200, None, b"plain\ttab".to_vec());
        assert_eq!(text_of(&doc.render(20)), ["plain   tab"]);
        let doc = Document::from_reply(base(), 200, None, vec![0, 1, 2]);
        assert!(text_of(&doc.render(80))[0].contains("of no stated type, 3 bytes"));
    }

    #[test]
    fn another_type_is_named_and_not_shown() {
        let doc = Document::from_reply(base(), 200, Some("application/pdf"), vec![1; 10]);
        let lines = text_of(&doc.render(200));
        assert_eq!(lines[0], "This is application/pdf, 10 bytes.");
        assert!(lines[2].contains("Firefox opens the rest"));
    }

    #[test]
    fn an_error_status_with_nothing_to_show_is_an_error_page() {
        let doc = Document::from_reply(base(), 404, Some("text/html"), Vec::new());
        assert_eq!(doc.status, 404);
        assert_eq!(
            text_of(&doc.render(80)),
            ["The server answered 404 and sent nothing to show."]
        );
        let doc = Document::from_reply(base(), 404, Some("text/html"), b"<p>gone".to_vec());
        assert_eq!(text_of(&doc.render(80)), ["gone"]);
    }

    #[test]
    fn text_wraps_at_its_last_space_else_at_the_width() {
        let doc = Document::from_reply(
            base(),
            200,
            Some("text/plain"),
            b"aaa bbb ccc\r\ndddddddddd".to_vec(),
        );
        assert_eq!(
            text_of(&doc.render(7)),
            ["aaa bbb", "ccc", "ddddddd", "ddd"]
        );
        assert_eq!(text_of(&doc.render(1)).len(), 19);
    }

    #[test]
    fn a_long_page_is_cut_and_says_so() {
        let mut page = b"<p>".to_vec();
        page.resize(MAX_MARKUP + 10, b'a');
        let doc = Document::from_reply(base(), 200, Some("text/html"), page);
        assert!(doc.note.unwrap().contains("only the first"));
        let doc = Document::from_reply(base(), 200, Some("text/plain"), vec![b'\n'; MAX_TEXT + 1]);
        assert!(doc.note.as_deref().unwrap().contains("only the first"));
        let rendered = doc.render(80);
        assert_eq!(rendered.lines.len(), MAX_LINES);
        assert!(rendered.cut);
    }

    #[test]
    fn a_title_is_found_only_as_a_title() {
        assert_eq!(
            title(b"<titlebar>x</titlebar><title>y</title>"),
            Some("y".into())
        );
        assert_eq!(
            title(b"<TITLE lang=en>\n Two\n words </TITLE>"),
            Some("Two words".into())
        );
        assert_eq!(title(b"<title></title>"), None);
        assert_eq!(title(b"<title>unclosed"), None);
    }

    #[test]
    fn a_link_shows_its_controls_encoded() {
        let rendered = html_page(b"<p><a href=\"/b#x\x01y\">b</a></p>");
        assert_eq!(rendered.shown(0).unwrap(), "https://example.org/b#x%01y");
    }

    #[test]
    fn a_png_is_a_picture_and_one_too_large_is_refused_from_its_header() {
        let png = td_png::encode_rgb8(2, 1, &[255, 0, 0, 0, 255, 0]).unwrap();
        let doc = Document::from_reply(base(), 200, Some("image/png"), png);
        let picture = doc.picture().unwrap();
        assert_eq!((picture.width(), picture.height()), (2, 1));
        let wide = td_png::encode_rgb8(
            MAX_PICTURE_AXIS + 1,
            1,
            &vec![0; (MAX_PICTURE_AXIS + 1) * 3],
        )
        .unwrap();
        let doc = Document::from_reply(base(), 200, Some("image/png"), wide);
        assert!(doc.picture().is_none());
        assert!(text_of(&doc.render(80))[0].contains("too large to show"));
        let doc = Document::from_reply(base(), 200, Some("image/png"), b"not a png".to_vec());
        assert!(doc.picture().is_none());
        assert!(text_of(&doc.render(80))[0].starts_with("The PNG could not be read"));
    }

    #[test]
    fn a_jpeg_is_a_picture_in_rgba() {
        let mut encoder = td_jpeg::Encoder::new(8, 8, 90).unwrap();
        encoder
            .encode_rows(&[200; 8 * 8 * 3], &td_jpeg::InOrder)
            .unwrap();
        let jpeg = encoder.finish(&td_jpeg::InOrder).unwrap();
        let doc = Document::from_reply(base(), 200, None, jpeg);
        let picture = doc.picture().unwrap();
        assert_eq!((picture.width(), picture.height()), (8, 8));
        assert_eq!(picture.bytes(), 8 * 8 * 4);
    }
}

//! A response as td-web shows it (DESIGN.md §5): HTML rendered once
//! through td-html's rich output into logical lines, text as it is, a
//! PNG or JPEG decoded, anything else named and not shown, all of it on
//! the fetch thread; and the rendering, those lines wrapped by pixel
//! width into lines of styled pieces with link targets, made again for
//! each width and body face the page is shown at, on the window's.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use td_ui::picture::{Picture, MAX_PICTURE_AXIS, MAX_PICTURE_PIXELS};
use td_ui::prose::{self, Metrics};
use td_ui::raster::{GlyphStyle, TextFamily, INK, PAPER};
use td_url::{Reference, Url};

/// The most bytes of markup rendered; the rest is not shown. td-html
/// builds a page's whole rendering before td-web keeps `MAX_LINES` of it,
/// so this bounds that work, which a fetch thread does: a hostile page of
/// empty paragraphs this size costs about 0.38 s and some 260 MB, and
/// td-html's output budget holds nested quotes or lists to about as much
/// in td-html (DESIGN.md §5).
pub const MAX_MARKUP: usize = 2 << 20;
/// The most bytes of text rendered.
pub const MAX_TEXT: usize = 4 << 20;
/// The most characters one line of text is expanded to.
const MAX_LINE_CHARS: usize = 1 << 20;
/// The most lines a page keeps, logical and wrapped alike; the rest are
/// not shown.
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
/// The tag of a piece that is no link.
pub const NO_LINK: usize = usize::MAX;
/// The most characters an unwrapped line keeps, preformatted or a table's:
/// the pane shows only its start, so the rest would cost its wrap at
/// each width and show nothing.
pub const MAX_PRE_CHARS: usize = 4096;
/// The most characters read as a line's leading structure.
const MAX_LEAD: usize = 256;

/// One rendering of markup or decoding of a picture at a time, whichever
/// fetch thread asks: a rendering can cost some 260 MB at `MAX_MARKUP`,
/// about as much in td-html when quotes or lists nest, and a picture
/// some 112 MB, and
/// `MAX_WORKERS` of them at once would not fit td-web's memory bound
/// (DESIGN.md §3).
static RENDERING: Mutex<()> = Mutex::new(());

/// How a run of text looks.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Style {
    pub bold: bool,
    pub italic: bool,
    pub strike: bool,
    /// Code: the monospace face, on a ground of its own.
    pub code: bool,
    /// The monospace face: preformatted text, and plain text.
    pub mono: bool,
}

impl Style {
    /// The glyph style a run of this look is measured and drawn in, its
    /// body text in `body`'s family.
    pub fn glyph(self, body: TextFamily) -> GlyphStyle {
        let family = if self.code || self.mono {
            TextFamily::Mono
        } else {
            body
        };
        let mut style = GlyphStyle::medium(INK, PAPER).with_family(family);
        if self.bold {
            style = style.bold();
        }
        if self.italic {
            style = style.italic();
        }
        style
    }
}

/// Text of one look, and the link it belongs to.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Run {
    pub text: String,
    pub style: Style,
    pub link: Option<usize>,
}

/// A line as the document has it, before it is wrapped: its runs, and
/// whether it may be wrapped, which a preformatted line may not.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Logical {
    pub runs: Vec<Run>,
    pub wrap: bool,
}

/// A link as the document wrote it, joined against the document's URL
/// when it is shown or followed, and the logical line it starts on.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Link {
    pub written: String,
    pub line: usize,
}

/// A document's text as logical lines, made once, and its links.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Page {
    pub lines: Vec<Logical>,
    pub links: Vec<Link>,
    /// Whether lines past `MAX_LINES` were left out.
    pub cut: bool,
    /// Whether td-html left the rest out, past its output budget.
    pub unrendered: bool,
    /// Whether a preformatted line was cut at `MAX_PRE_CHARS`.
    pub trimmed: bool,
    pub base: Option<Url>,
}

impl Page {
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

    /// The run a piece of a wrapped line shows.
    pub fn run(&self, wrapped: &Wrapped, piece: &prose::Piece) -> Option<&Run> {
        self.lines.get(wrapped.logical)?.runs.get(piece.run)
    }
}

/// Where a reader is in a page, kept across wraps: a logical line, and
/// the run and the byte in it that a wrapped line's own text starts at.
#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
pub struct Place {
    pub logical: usize,
    pub run: usize,
    pub at: usize,
}

/// One wrapped line: the logical line it is part of, where its own text
/// starts there (its logical line's start for its first line; past the
/// marks a continuation repeats for the rest), and its pieces and
/// glyphs, each piece tagged with its link, `NO_LINK` for none.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Wrapped {
    pub logical: usize,
    pub start: (usize, usize),
    pub line: prose::Line,
}

/// A page wrapped at one width in one body family.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Rendered {
    pub page: Arc<Page>,
    /// The width in pixels it was wrapped at.
    pub width: u32,
    pub lines: Vec<Wrapped>,
    /// The first line each link is on, by link.
    pub link_lines: Vec<usize>,
    /// Whether lines past `MAX_LINES` were left out, logical or wrapped.
    pub cut: bool,
}

impl Rendered {
    /// `page`'s logical lines laid into lines no wider than `width`
    /// pixels (at least one), measured with `metrics`, its body text in
    /// `body`; a line that may not wrap is one line however wide. At most
    /// `MAX_LINES` lines are laid out and kept, however narrow the width.
    pub fn wrap(page: &Arc<Page>, metrics: &mut Metrics, width: u32, body: TextFamily) -> Self {
        let width = width.max(1);
        let mut lines: Vec<Wrapped> = Vec::new();
        let mut cut = page.cut;
        // The first wrapped line of each logical line.
        let mut firsts = Vec::new();
        let mut runs: Vec<prose::Run> = Vec::new();
        'logical: for (index, logical) in page.lines.iter().enumerate() {
            if lines.len() >= MAX_LINES {
                cut = true;
                break;
            }
            firsts.push(lines.len());
            runs.clear();
            runs.extend(logical.runs.iter().map(|run| prose::Run {
                text: &run.text,
                style: run.style.glyph(body),
                tag: run.link.unwrap_or(NO_LINK),
            }));
            // One line past those kept says whether any were left out.
            let most = MAX_LINES - lines.len() + 1;
            for (mut line, start) in lay(metrics, logical, &runs, width, most) {
                if lines.len() >= MAX_LINES {
                    cut = true;
                    break 'logical;
                }
                // Kept for as long as the page is shown: no room to grow.
                line.pieces.shrink_to_fit();
                line.glyphs.shrink_to_fit();
                lines.push(Wrapped {
                    logical: index,
                    start,
                    line,
                });
            }
        }
        let mut found = vec![None; page.links.len()];
        for (at, wrapped) in lines.iter().enumerate() {
            for piece in &wrapped.line.pieces {
                if let Some(slot) = found.get_mut(piece.tag) {
                    slot.get_or_insert(at);
                }
            }
        }
        // A link no piece shows (its text all spaces, or cut) is on its
        // logical line's first wrapped line, else the last line.
        let last = lines.len().saturating_sub(1);
        let link_lines = found
            .into_iter()
            .zip(&page.links)
            .map(|(line, link)| {
                line.or_else(|| firsts.get(link.line).copied())
                    .unwrap_or(last)
            })
            .collect();
        Self {
            page: Arc::clone(page),
            width,
            lines,
            link_lines,
            cut,
        }
    }

    /// Where line `at` starts, as a place another wrap of the page finds.
    pub fn place(&self, at: usize) -> Place {
        self.lines
            .get(at)
            .map_or_else(Place::default, |wrapped| Place {
                logical: wrapped.logical,
                run: wrapped.start.0,
                at: wrapped.start.1,
            })
    }

    /// The line that shows `place`: the last that starts at or before it,
    /// else the first.
    pub fn line_at(&self, place: Place) -> usize {
        self.lines
            .partition_point(|wrapped| {
                (wrapped.logical, wrapped.start.0, wrapped.start.1)
                    <= (place.logical, place.run, place.at)
            })
            .saturating_sub(1)
    }
}

/// One character of a line's leading structure, from where it is in its
/// run, and whether it is a mark its continuation lines repeat: a
/// quote's `>` or a heading's `#`.
#[derive(Clone, Copy)]
struct Lead {
    run: usize,
    at: usize,
    scalar: char,
    mark: bool,
}

/// What a logical line starts with that its continuation lines hang
/// from, as td-html lays out a list, a nested list, a quote, a heading
/// or a definition at width 0, and the first run after it. It is whole
/// runs, at most `MAX_LEAD` characters in all, each holding only
/// structure (`structure`): td-html puts each prefix in a run of its own,
/// so marks sharing a run with text, as every line of plain text does,
/// do not hang. td-html's rich output also gives each word of a
/// paragraph a run of its own, so a paragraph of the document's own
/// that begins `> ` or `- ` hangs as td-html's structure does: nothing
/// in that output tells the two apart.
fn leading(runs: &[Run]) -> (Vec<Lead>, usize) {
    let mut lead = Vec::new();
    for (index, run) in runs.iter().enumerate() {
        let before = lead.len();
        if run.text.is_empty() || !structure(&run.text, index, &mut lead) || lead.len() > MAX_LEAD {
            lead.truncate(before);
            return (lead, index);
        }
    }
    (lead, runs.len())
}

/// Appends `text`, run `run`, to `lead` when all of it is structure, in
/// any order: spaces, a quote's `> `, a heading's one to six `#` and a
/// space, and a list's marker: `*`, `-`, `+`, `•`, `◦` or `▪` and a
/// space, or digits, after a `-` or not, then `.` or `)` and a space.
/// Whether it was.
fn structure(text: &str, run: usize, lead: &mut Vec<Lead>) -> bool {
    let chars: Vec<(usize, char)> = text.char_indices().take(MAX_LEAD + 1).collect();
    if chars.len() > MAX_LEAD {
        return false;
    }
    let c = |k: usize| chars.get(k).map(|&(_, c)| c);
    let digit = |k: usize| c(k).is_some_and(|d| d.is_ascii_digit());
    let mut i = 0;
    while i < chars.len() {
        let (taken, mark) = match c(i) {
            Some(' ') => (1, false),
            Some('>') if c(i + 1) == Some(' ') => (2, true),
            Some('#') => {
                let mut k = i;
                while c(k) == Some('#') {
                    k += 1;
                }
                if k - i <= 6 && c(k) == Some(' ') {
                    (k + 1 - i, true)
                } else {
                    (0, false)
                }
            }
            Some('*' | '-' | '+' | '•' | '◦' | '▪') if c(i + 1) == Some(' ') => (2, false),
            Some('-' | '0'..='9') if digit(i) || digit(i + 1) => {
                let mut k = i + 1;
                while digit(k) {
                    k += 1;
                }
                if matches!(c(k), Some('.' | ')')) && c(k + 1) == Some(' ') {
                    (k + 2 - i, false)
                } else {
                    (0, false)
                }
            }
            _ => (0, false),
        };
        if taken == 0 {
            return false;
        }
        for &(at, scalar) in chars.get(i..i + taken).unwrap_or_default() {
            lead.push(Lead {
                run,
                at,
                scalar,
                mark: mark && scalar != ' ',
            });
        }
        i += taken;
    }
    true
}

/// `units` of `prose::UNIT` rounded to the nearest pixel, as `prose`
/// rounds a pen.
fn pixels(units: u64) -> u32 {
    let unit = u64::from(prose::UNIT);
    u32::try_from(units.saturating_add(unit / 2) / unit).unwrap_or(u32::MAX)
}

/// `lines` with where each one's text starts: a logical line's first
/// line at its start, every other at its first piece.
fn started(lines: Vec<prose::Line>) -> Vec<(prose::Line, (usize, usize))> {
    lines
        .into_iter()
        .enumerate()
        .map(|(number, line)| {
            let start = match line.pieces.first() {
                Some(piece) if number > 0 => (piece.run, piece.text.start),
                _ => (0, 0),
            };
            (line, start)
        })
        .collect()
}

/// `logical` laid into at most `most` lines, `runs` being its runs as
/// prose measures them, each with where its own text starts. A line that
/// may not wrap is one line. One that starts with leading structure
/// (`leading`) hangs: its text after that structure is wrapped at
/// `width` less the structure's width, every line of it set that far in,
/// the first after the whole structure and each continuation after its
/// marks alone. A structure wider than half the width does not hang, so
/// the text keeps room.
fn lay(
    metrics: &mut Metrics,
    logical: &Logical,
    runs: &[prose::Run],
    width: u32,
    most: usize,
) -> Vec<(prose::Line, (usize, usize))> {
    if !logical.wrap {
        return started(prose::wrap_at_most(metrics, runs, u32::MAX, most));
    }
    let (lead, first) = leading(&logical.runs);
    let rest = runs.get(first..).unwrap_or_default();
    if lead.is_empty() || rest.iter().all(|run| run.text.is_empty()) {
        return started(prose::wrap_at_most(metrics, runs, width, most));
    }
    // Each leading character's pen and advance, in `prose::UNIT`s.
    let mut pen = 0u64;
    let mut placed = Vec::with_capacity(lead.len());
    for one in &lead {
        let Some(run) = runs.get(one.run) else {
            return started(prose::wrap_at_most(metrics, runs, width, most));
        };
        let shown = prose::shown(one.scalar);
        let advance = u64::from(metrics.advance(run.style.family, run.style.font_style, shown));
        placed.push((*one, pen, advance));
        pen = pen.saturating_add(advance);
    }
    let hang = pixels(pen);
    if hang.saturating_mul(2) > width {
        return started(prose::wrap_at_most(metrics, runs, width, most));
    }
    let lines = prose::wrap_at_most(metrics, rest, width - hang, most);
    let mut out = Vec::with_capacity(lines.len());
    for (number, line) in lines.into_iter().enumerate() {
        let mut laid = prose::Line::default();
        for &(one, pen, advance) in &placed {
            if number > 0 && !one.mark {
                continue;
            }
            let Some(run) = runs.get(one.run) else {
                continue;
            };
            let x = pixels(pen);
            let right = pixels(pen.saturating_add(advance));
            let at = laid.glyphs.len();
            let end = one.at + one.scalar.len_utf8();
            // A run's characters are one piece, on a continuation its
            // marks alone, the text between them left out.
            match laid.pieces.last_mut() {
                Some(piece) if piece.run == one.run => {
                    piece.text.end = end;
                    piece.right = piece.right.max(right);
                    piece.glyphs.end = at + 1;
                }
                _ => laid.pieces.push(prose::Piece {
                    run: one.run,
                    tag: run.tag,
                    style: run.style,
                    text: one.at..end,
                    left: x,
                    right,
                    glyphs: at..at + 1,
                }),
            }
            laid.glyphs.push(prose::Placed {
                x,
                scalar: prose::shown(one.scalar),
            });
            laid.width = laid.width.max(right);
        }
        let start = match line.pieces.first() {
            Some(piece) if number > 0 => (piece.run + first, piece.text.start),
            _ => (0, 0),
        };
        let base = laid.glyphs.len();
        for piece in line.pieces {
            laid.pieces.push(prose::Piece {
                run: piece.run + first,
                left: piece.left.saturating_add(hang),
                right: piece.right.saturating_add(hang),
                glyphs: piece.glyphs.start + base..piece.glyphs.end + base,
                ..piece
            });
        }
        laid.glyphs
            .extend(line.glyphs.iter().map(|glyph| prose::Placed {
                x: glyph.x.saturating_add(hang),
                ..*glyph
            }));
        if !line.glyphs.is_empty() {
            laid.width = laid.width.max(line.width.saturating_add(hang));
        }
        out.push((laid, start));
    }
    out
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
    Page(Arc<Page>),
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
    /// `from_reply_unless` for a navigation never stopped.
    #[cfg(test)]
    pub fn from_reply(url: Url, status: u16, content_type: Option<&str>, body: Vec<u8>) -> Self {
        Self::from_reply_unless(url, status, content_type, body, &AtomicBool::new(false))
            .unwrap_or_else(|| Self::error(None, "stopped"))
    }

    /// `body`, answered with `status` from `url` as `content_type`,
    /// rendered into logical lines or decoded; none once `stopped` is set
    /// before markup is rendered or a picture decoded. Those take
    /// `RENDERING`, and `stopped` is read both before and after it is
    /// held, so a navigation stopped or overtaken while it waited does no
    /// such work.
    pub fn from_reply_unless(
        url: Url,
        status: u16,
        content_type: Option<&str>,
        body: Vec<u8>,
        stopped: &AtomicBool,
    ) -> Option<Self> {
        if body.is_empty() && !(200..300).contains(&status) {
            let why = format!("The server answered {status} and sent nothing to show.");
            return Some(Self {
                status,
                ..Self::error(Some(url), &why)
            });
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
            Some(rest) if matches!(kind, Kind::Html | Kind::Text) => rest,
            _ => body.as_slice(),
        };
        let heavy = matches!(kind, Kind::Html | Kind::Png | Kind::Jpeg);
        if heavy && stopped.load(Ordering::SeqCst) {
            return None;
        }
        let _one = heavy.then(|| RENDERING.lock().unwrap_or_else(PoisonError::into_inner));
        if heavy && stopped.load(Ordering::SeqCst) {
            return None;
        }
        let size = body.len();
        let cut = |most: usize| {
            (size > most)
                .then(|| format!("only the first {most} of the page's {size} bytes are shown"))
        };
        let base = Some(url.clone());
        let (page, title, note) = match kind {
            Kind::Html => {
                let shown = body.get(..MAX_MARKUP).unwrap_or(body);
                (html(shown, base), title(body), cut(MAX_MARKUP))
            }
            Kind::Text => {
                let shown = body.get(..MAX_TEXT).unwrap_or(body);
                let text = String::from_utf8_lossy(shown);
                (plain(&text, true, base), None, cut(MAX_TEXT))
            }
            Kind::Png | Kind::Jpeg => match picture(kind, body) {
                Ok(picture) => {
                    return Some(Self {
                        url: Some(url),
                        status,
                        title: None,
                        note: None,
                        body: Body::Picture(picture),
                    });
                }
                Err(why) => (plain(&why, false, base), None, None),
            },
            Kind::Other => {
                let named = if media.is_empty() {
                    "of no stated type".to_string()
                } else {
                    visible(&media)
                };
                let said = format!(
                    "This is {named}, {size} bytes.\n\ntd-web shows HTML, text, PNG and \
                     JPEG; Firefox opens the rest."
                );
                (plain(&said, false, base), None, None)
            }
        };
        Some(Self {
            url: Some(url),
            status,
            title,
            note,
            body: Body::Page(Arc::new(page)),
        })
    }

    /// A page saying why there is no document.
    pub fn error(url: Option<Url>, why: &str) -> Self {
        let page = plain(why, false, url.clone());
        Self {
            url,
            status: 0,
            title: None,
            note: None,
            body: Body::Page(Arc::new(page)),
        }
    }

    /// The start page.
    pub fn blank() -> Self {
        Self::error(None, "")
    }

    pub fn picture(&self) -> Option<&Picture> {
        match &self.body {
            Body::Picture(picture) => Some(picture),
            Body::Page(_) => None,
        }
    }

    /// The document's lines wrapped at `width` pixels (at least one),
    /// measured with `metrics`, its body text in `body`; none for a
    /// picture.
    pub fn render(&self, metrics: &mut Metrics, width: u32, body: TextFamily) -> Rendered {
        match &self.body {
            Body::Page(page) => Rendered::wrap(page, metrics, width, body),
            Body::Picture(_) => Rendered::default(),
        }
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

/// `text` as prose shows it well: control characters but the tab
/// dropped (a CR before an LF among them, DEL and C1 too), which prose
/// would show as U+FFFD, and td-html's stroke marks when td-web strikes
/// the text itself.
fn clean(text: &str, struck: bool) -> String {
    text.chars()
        .filter(|&c| (c == '\t' || !c.is_control()) && !(struck && c == STROKE))
        .collect()
}

/// Whether `text` is one of td-html's table rules or column bars: only
/// box-drawing characters.
fn ruling(text: &str) -> bool {
    !text.is_empty() && text.chars().all(|c| ('\u{2500}'..='\u{257f}').contains(&c))
}

/// `source` through td-html's rich output, unwrapped, into logical
/// lines; the caller holds `RENDERING`. Runs of one href next to each
/// other, across lines too, are one link, so it is focused and followed
/// as one; a blank line between them ends it. A line holding
/// preformatted text, or one of a table's rules or rows, is not wrapped
/// and keeps its first `MAX_PRE_CHARS` characters; a link with no text
/// left is not one.
fn html(source: &[u8], base: Option<Url>) -> Page {
    let mut page = Page {
        base,
        ..Page::default()
    };
    let mut last: Option<(String, usize)> = None;
    let mut rendered = td_html::to_rich(source, 0).into_iter().peekable();
    while let Some(spans) = rendered.next() {
        // td-html ends a rendering it cut short with its notice, which the
        // status row says instead.
        if rendered.peek().is_none()
            && matches!(spans.as_slice(), [span]
                if span.text == td_html::CUT_NOTICE && span.tags.is_empty())
        {
            page.unrendered = true;
            break;
        }
        if page.lines.len() == MAX_LINES {
            page.cut = true;
            break;
        }
        if spans.iter().all(|span| span.text.trim().is_empty()) {
            last = None;
        }
        let unwrapped = spans
            .iter()
            .any(|span| ruling(&span.text) || span.tags.contains(&td_html::Tag::Preformat));
        let mut line = Logical {
            runs: Vec::with_capacity(spans.len()),
            wrap: !unwrapped,
        };
        // What an unwrapped line may still take.
        let mut left = MAX_PRE_CHARS;
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
                    td_html::Tag::Preformat => style.mono = true,
                    td_html::Tag::Link(target) => href = Some(target.as_str()),
                    td_html::Tag::Image(target) => image = Some(target.as_str()),
                    td_html::Tag::Colour(_) | td_html::Tag::BgColour(_) => {}
                }
            }
            let mut text = clean(&span.text, style.strike);
            if unwrapped {
                if left == 0 {
                    page.trimmed |= !text.is_empty();
                    // Text cut away still parts the links either side of it.
                    let target = href.or(image);
                    if !span.text.trim().is_empty()
                        && last.as_ref().map(|(previous, _)| previous.as_str()) != target
                    {
                        last = None;
                    }
                    continue;
                }
                match text.char_indices().nth(left) {
                    Some((end, _)) => {
                        text.truncate(end);
                        page.trimmed = true;
                        left = 0;
                    }
                    None => left -= text.chars().count(),
                }
            }
            // An image's alt text is a link to the image unless it is
            // inside a link already.
            let link = match href.or(image) {
                Some(target) => match &last {
                    Some((previous, at)) if previous == target => Some(*at),
                    _ => {
                        let at = page.links.len();
                        page.links.push(Link {
                            written: target.to_string(),
                            line: page.lines.len(),
                        });
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
            line.runs.push(Run { text, style, link });
        }
        let keep = if line.wrap { leading(&line.runs).1 } else { 0 };
        line.runs = merged(std::mem::take(&mut line.runs), keep);
        page.lines.push(line);
    }
    page
}

/// `runs` with each run joined to the one before it when they look alike
/// and link alike, but for run `keep`, the first after the leading
/// structure, so the line still hangs. td-html's rich output gives every
/// word a run of its own, and each level of a nested quote or list its
/// own prefix, which would cost a run and a piece each.
fn merged(runs: Vec<Run>, keep: usize) -> Vec<Run> {
    let mut out: Vec<Run> = Vec::with_capacity(runs.len().min(8));
    for (index, run) in runs.into_iter().enumerate() {
        match out.last_mut() {
            Some(last) if index != keep && last.style == run.style && last.link == run.link => {
                last.text.push_str(&run.text);
            }
            _ => out.push(run),
        }
    }
    out.shrink_to_fit();
    out
}

/// `text`'s lines, tabs expanded to every `TAB` characters and other
/// control characters dropped, as logical lines in the monospace face
/// when `mono`, else in the body's.
fn plain(text: &str, mono: bool, base: Option<Url>) -> Page {
    let mut page = Page {
        base,
        ..Page::default()
    };
    let style = Style {
        mono,
        ..Style::default()
    };
    for raw in text.lines() {
        if page.lines.len() == MAX_LINES {
            page.cut = true;
            break;
        }
        let mut line = String::new();
        let mut columns = 0;
        for c in raw.chars() {
            if columns >= MAX_LINE_CHARS {
                break;
            }
            if c == '\t' {
                let pad = TAB - columns % TAB;
                line.extend(std::iter::repeat_n(' ', pad));
                columns += pad;
            } else if !c.is_control() {
                line.push(c);
                columns += 1;
            }
        }
        page.lines.push(Logical {
            runs: vec![Run {
                text: line,
                style,
                link: None,
            }],
            wrap: true,
        });
    }
    page
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
    #![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
    use super::*;
    use td_ui::raster::Scale;
    use td_ui::CELL_WIDTH;

    fn base() -> Url {
        Url::parse("https://example.org/docs/page").unwrap()
    }

    /// The cell's metrics at scale one, as every in-process test has.
    fn cell() -> Metrics {
        Metrics::new(None, Scale::new(1).unwrap())
    }

    /// `doc` wrapped `columns` cells wide in Sans.
    fn cells(doc: &Document, columns: u32) -> Rendered {
        doc.render(&mut cell(), columns * CELL_WIDTH as u32, TextFamily::Sans)
    }

    /// Each wrapped line's text, from its pieces' runs.
    fn text_of(rendered: &Rendered) -> Vec<String> {
        rendered
            .lines
            .iter()
            .map(|wrapped| {
                wrapped
                    .line
                    .pieces
                    .iter()
                    .map(|piece| {
                        &rendered.page.run(wrapped, piece).unwrap().text[piece.text.clone()]
                    })
                    .collect()
            })
            .collect()
    }

    fn html_doc(body: &[u8]) -> Document {
        Document::from_reply(base(), 200, Some("text/html"), body.to_vec())
    }

    fn html_page(body: &[u8]) -> Rendered {
        cells(&html_doc(body), 80)
    }

    fn runs(rendered: &Rendered) -> Vec<&Run> {
        rendered
            .page
            .lines
            .iter()
            .flat_map(|line| &line.runs)
            .collect()
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
        let rendered = cells(&doc, 80);
        let page = &rendered.page;
        assert_eq!(page.links.len(), 3);
        assert_eq!(page.shown(0).unwrap(), "https://example.org/docs/next");
        assert_eq!(page.shown(1).unwrap(), "mailto:a@example.org");
        assert_eq!(
            page.target(1).unwrap(),
            Ok(Reference::Other {
                scheme: "mailto".into()
            })
        );
        assert_eq!(page.shown(2).unwrap(), "https://example.org/p.png");
        assert!(page.target(3).is_none());
        let runs = runs(&rendered);
        let bold = runs.iter().find(|run| run.text == "bold").unwrap();
        assert!(bold.style.bold && bold.link.is_none());
        let code = runs.iter().find(|run| run.text == "c").unwrap();
        assert!(code.style.code);
        assert!(runs
            .iter()
            .any(|run| run.text == "Pic" && run.link == Some(2)));
        // Each link's pieces carry its index as their tag.
        let mut tags: Vec<usize> = rendered.lines[0]
            .line
            .pieces
            .iter()
            .map(|piece| piece.tag)
            .filter(|&tag| tag != NO_LINK)
            .collect();
        tags.dedup();
        assert_eq!(tags, [0, 1, 2]);
    }

    #[test]
    fn a_link_wrapped_across_lines_is_one_link_and_a_paragraph_ends_it() {
        let page = b"<p><a href=\"/x\">one two three four five six</a> after</p>";
        let rendered = cells(&html_doc(page), 10);
        assert!(rendered.lines.len() > 1, "{:?}", text_of(&rendered));
        assert_eq!(rendered.page.links.len(), 1);
        assert_eq!(rendered.link_lines, [0]);
        assert!(rendered.lines[1]
            .line
            .pieces
            .iter()
            .any(|piece| piece.tag == 0));
        let rendered = html_page(b"<p><a href=\"/x\">one</a></p><p><a href=\"/x\">two</a></p>");
        assert_eq!(rendered.page.links.len(), 2, "{:?}", text_of(&rendered));
        assert_eq!(rendered.link_lines, [0, 2]);
    }

    #[test]
    fn markup_is_rendered_once_into_logical_lines_and_wrapped_at_each_width() {
        let doc = html_doc(b"<p>one two three four five six seven</p><p>end</p>");
        let Body::Page(page) = &doc.body else {
            panic!("a page");
        };
        assert_eq!(page.lines.len(), 3, "unwrapped, a blank line between");
        assert_eq!(
            text_of(&cells(&doc, 80)),
            ["one two three four five six seven", "", "end"]
        );
        assert_eq!(
            text_of(&cells(&doc, 14)),
            ["one two three", "four five six", "seven", "", "end"]
        );
        let narrow = cells(&doc, 14);
        assert_eq!(narrow.width, 14 * CELL_WIDTH as u32);
        // A place in the text is found again however the page is wrapped.
        let wide = cells(&doc, 80);
        let inside = narrow.place(1);
        assert_eq!(inside.logical, 0);
        assert_ne!(inside, Place::default(), "inside its logical line");
        assert_eq!(narrow.line_at(inside), 1);
        assert_eq!(wide.line_at(inside), 0, "the line holding it");
        assert_eq!(wide.line_at(narrow.place(2)), 0);
        assert_eq!(narrow.line_at(wide.place(2)), 4);
        let past = Place {
            logical: 9,
            ..Place::default()
        };
        assert_eq!(narrow.line_at(past), 4, "past the end, the last line");
        assert_eq!(narrow.line_at(Place::default()), 0);
    }

    #[test]
    fn struck_text_loses_td_htmls_stroke_marks() {
        let rendered = html_page(b"<p>x <s>ab</s> y</p>");
        let struck = runs(&rendered)
            .into_iter()
            .find(|run| run.style.strike)
            .unwrap();
        assert_eq!(struck.text, "ab");
        assert_eq!(text_of(&rendered), ["x ab y"]);
    }

    #[test]
    fn code_and_preformatted_text_are_monospace_and_a_preformatted_line_is_not_wrapped() {
        let doc = html_doc(b"<p>some <code>code</code> here</p><pre>a b c d e f g h i j</pre>");
        let rendered = cells(&doc, 6);
        let runs = runs(&rendered);
        let code = runs.iter().find(|run| run.text == "code").unwrap();
        assert_eq!(code.style.glyph(TextFamily::Serif).family, TextFamily::Mono);
        let prose = runs.iter().find(|run| run.text.contains("some")).unwrap();
        assert_eq!(
            prose.style.glyph(TextFamily::Serif).family,
            TextFamily::Serif
        );
        assert_eq!(prose.style.glyph(TextFamily::Sans).family, TextFamily::Sans);
        let lines = text_of(&rendered);
        assert_eq!(lines.last().unwrap(), "a b c d e f g h i j", "{lines:?}");
        let pre = rendered.lines.last().unwrap();
        assert!(!rendered.page.lines[pre.logical].wrap);
        assert_eq!(pre.line.pieces[0].style.family, TextFamily::Mono);
        assert!(pre.line.width > rendered.width);
    }

    /// Each wrapped line's glyphs as pens in cells and scalars.
    fn pens(rendered: &Rendered) -> Vec<Vec<(u32, char)>> {
        let cell = CELL_WIDTH as u32;
        rendered
            .lines
            .iter()
            .map(|wrapped| {
                wrapped
                    .line
                    .glyphs
                    .iter()
                    .map(|glyph| (glyph.x / cell, glyph.scalar))
                    .collect()
            })
            .collect()
    }

    /// The column each line's text starts at, past its marks, and what
    /// the line shows from there.
    fn hung(rendered: &Rendered) -> Vec<(u32, String)> {
        pens(rendered)
            .into_iter()
            .map(|line| {
                let start = line
                    .iter()
                    .position(|&(_, c)| !matches!(c, '>' | ' '))
                    .unwrap_or(0);
                let text = line.get(start..).unwrap_or_default();
                (
                    text.first().map_or(0, |&(x, _)| x),
                    text.iter().map(|&(_, c)| c).collect(),
                )
            })
            .collect()
    }

    #[test]
    fn a_list_items_lines_hang_from_its_marker() {
        let doc = html_doc(b"<ul><li>one two three four five six seven eight</li></ul>");
        assert_eq!(
            hung(&cells(&doc, 20)),
            [
                (0, "* one two three four".to_string()),
                (2, "five six seven".to_string()),
                (2, "eight".to_string())
            ]
        );
        let doc = html_doc(b"<ol><li>alpha beta gamma delta epsilon</li></ol>");
        let lines = hung(&cells(&doc, 20));
        assert_eq!(lines[0].1, "1. alpha beta gamma");
        assert_eq!(lines[1], (3, "delta epsilon".to_string()));
    }

    #[test]
    fn a_nested_lists_lines_hang_from_its_own_marker() {
        let doc =
            html_doc(b"<ul><li>outer<ul><li>inner one two three four five</li></ul></li></ul>");
        let lines = hung(&cells(&doc, 20));
        assert_eq!(lines[0], (0, "* outer".to_string()));
        let inner = pens(&cells(&doc, 20))[1].clone();
        assert_eq!(&inner[..4], [(0, ' '), (1, ' '), (2, '*'), (3, ' ')]);
        assert_eq!(lines[2], (4, "three four five".to_string()));
    }

    #[test]
    fn a_quotes_lines_repeat_its_marks_and_a_list_in_it_hangs_inside_them() {
        let doc = html_doc(
            b"<blockquote><blockquote>alpha beta gamma delta epsilon</blockquote></blockquote>",
        );
        let lines = pens(&cells(&doc, 20));
        assert_eq!(lines.len(), 2);
        assert_eq!(&lines[1][..3], [(0, '>'), (2, '>'), (4, 'd')]);
        // However deep, the repeated marks are one piece, the text
        // between them left out.
        let rendered = cells(&doc, 20);
        assert_eq!(rendered.lines[1].line.pieces[0].glyphs, 0..2);
        assert_eq!(text_of(&rendered)[1], "> >delta epsilon");
        let mut deep = b"<blockquote>".repeat(30);
        deep.extend(b"a b c d e f g h i j k l m n o p q r s t u v w x y z ".repeat(10));
        let rendered = cells(&html_doc(&deep), 200);
        assert!(rendered.lines.len() > 2);
        assert!(rendered
            .lines
            .iter()
            .all(|wrapped| wrapped.line.pieces.len() == 2));
        let doc =
            html_doc(b"<blockquote><ul><li>one two three four five six</li></ul></blockquote>");
        let rendered = cells(&doc, 20);
        let lines = pens(&rendered);
        assert_eq!(&lines[0][..4], [(0, '>'), (1, ' '), (2, '*'), (3, ' ')]);
        assert_eq!(&lines[1][..2], [(0, '>'), (4, 'f')], "no marker repeated");
        // Each piece names its own run's bytes.
        assert!(
            text_of(&rendered)[1].starts_with(">f"),
            "{:?}",
            text_of(&rendered)
        );
    }

    #[test]
    fn a_prefix_wider_than_half_the_line_does_not_hang() {
        let doc = html_doc(b"<ul><li>one two</li></ul>");
        assert_eq!(
            hung(&cells(&doc, 3)),
            [
                (0, "*".to_string()),
                (0, "one".to_string()),
                (0, "two".to_string())
            ]
        );
    }

    #[test]
    fn a_preformatted_line_is_cut_at_its_bound() {
        let mut page = b"<pre>".to_vec();
        page.extend(b"x".repeat(MAX_PRE_CHARS + 10));
        page.extend(b"\nshort</pre>");
        let rendered = cells(&html_doc(&page), 80);
        assert!(rendered.page.trimmed);
        assert_eq!(rendered.lines[0].line.glyphs.len(), MAX_PRE_CHARS);
        assert_eq!(text_of(&rendered)[1], "short");
        let rendered = html_page(b"<pre>short</pre>");
        assert!(!rendered.page.trimmed);
    }

    #[test]
    fn a_headings_lines_repeat_its_marks() {
        let doc = html_doc(b"<h2>alpha beta gamma delta epsilon</h2>");
        let lines = pens(&cells(&doc, 20));
        assert_eq!(&lines[0][..4], [(0, '#'), (1, '#'), (2, ' '), (3, 'a')]);
        assert_eq!(&lines[1][..3], [(0, '#'), (1, '#'), (3, 'd')]);
    }

    #[test]
    fn marks_sharing_a_run_with_text_do_not_hang() {
        for text in [
            "> alpha beta gamma delta epsilon",
            "# alpha beta gamma delta epsilon",
            "- alpha beta gamma delta epsilon",
        ] {
            let doc =
                Document::from_reply(base(), 200, Some("text/plain"), text.as_bytes().to_vec());
            let lines = pens(&cells(&doc, 20));
            assert!(lines.len() > 1);
            assert_eq!(lines[1][0], (0, 'd'), "{text}");
        }
    }

    #[test]
    fn a_tables_rules_and_rows_are_not_wrapped() {
        let doc = html_doc(
            b"<table><tr><td>a cell of many words to wrap</td><td>b</td></tr>\
              <tr><td>c</td><td>d</td></tr></table>",
        );
        let rendered = cells(&doc, 10);
        assert_eq!(rendered.lines.len(), rendered.page.lines.len());
        assert!(rendered.page.lines.iter().all(|line| !line.wrap));
        assert!(text_of(&rendered)[1].starts_with("a cell of many words to wrap"));
        // Text that only holds a rule character among others still wraps.
        let doc = html_doc(b"<p>a \xe2\x94\x80 rule among words that wrap</p>");
        assert!(cells(&doc, 10).lines.len() > 1);
    }

    #[test]
    fn a_link_cut_away_from_a_preformatted_line_is_no_link() {
        let mut page = b"<pre>".to_vec();
        page.extend(b"x".repeat(MAX_PRE_CHARS - 2));
        page.extend(b"<a href=\"/kept\">kept</a><a href=\"/gone\">gone</a></pre>");
        let rendered = cells(&html_doc(&page), 80);
        assert!(rendered.page.trimmed);
        let written: Vec<&str> = rendered
            .page
            .links
            .iter()
            .map(|link| link.written.as_str())
            .collect();
        assert_eq!(written, ["/kept"]);
        assert_eq!(rendered.lines[0].line.glyphs.len(), MAX_PRE_CHARS);
        // Text cut away parts links of one href either side of it.
        let mut page = b"<pre>".to_vec();
        page.extend(b"x".repeat(MAX_PRE_CHARS - 2));
        page.extend(b"<a href=\"/a\">ab</a>cut away\n<a href=\"/a\">cd</a></pre>");
        let rendered = cells(&html_doc(&page), 80);
        assert_eq!(rendered.page.links.len(), 2);
        // A link cut away keeps one going.
        let mut page = b"<pre>".to_vec();
        page.extend(b"x".repeat(MAX_PRE_CHARS - 2));
        page.extend(b"<a href=\"/a\">ab cut away\ncd</a></pre>");
        let rendered = cells(&html_doc(&page), 80);
        assert_eq!(rendered.page.links.len(), 1);
    }

    #[test]
    fn however_narrow_the_width_the_lines_laid_out_are_bounded() {
        let doc = Document::from_reply(
            base(),
            200,
            Some("text/plain"),
            "x".repeat(3 * MAX_LINES).into_bytes(),
        );
        let rendered = doc.render(&mut cell(), 1, TextFamily::Sans);
        assert_eq!(rendered.lines.len(), MAX_LINES);
        assert!(rendered.cut);
        let mut list = b"<ul>".to_vec();
        list.extend(b"<li>y".repeat(MAX_LINES));
        let doc = html_doc(&list);
        let rendered = doc.render(&mut cell(), 1, TextFamily::Sans);
        assert_eq!(rendered.lines.len(), MAX_LINES);
        assert!(rendered.cut);
    }

    #[test]
    fn a_stopped_navigation_renders_and_decodes_nothing() {
        let stopped = AtomicBool::new(true);
        for (kind, body) in [
            ("text/html", b"<p>x".to_vec()),
            ("image/png", td_png::encode_rgb8(1, 1, &[0, 0, 0]).unwrap()),
        ] {
            let doc = Document::from_reply_unless(base(), 200, Some(kind), body, &stopped);
            assert!(doc.is_none(), "{kind}");
        }
        let doc =
            Document::from_reply_unless(base(), 200, Some("text/plain"), b"x".to_vec(), &stopped);
        assert!(doc.is_some(), "text is not held back");
    }

    #[test]
    fn controls_but_the_tab_are_dropped_and_crlf_ends_a_line() {
        let doc = Document::from_reply(
            base(),
            200,
            Some("text/plain"),
            "a\x01b\rc\x1bd\x7fe\u{85}f\r\nnext\tx".as_bytes().to_vec(),
        );
        assert_eq!(text_of(&cells(&doc, 40)), ["abcdef", "next    x"]);
        let rendered = cells(&doc, 40);
        assert!(runs(&rendered).iter().all(|run| run.style.mono));
        let doc = html_doc(b"<pre>one\r\ntwo\x0cthree</pre>");
        assert_eq!(text_of(&cells(&doc, 40)), ["one", "twothree"]);
    }

    #[test]
    fn a_page_of_no_stated_type_is_sniffed() {
        let doc = Document::from_reply(base(), 200, None, b"\n <!DOCTYPE html><p>x".to_vec());
        assert_eq!(text_of(&cells(&doc, 20)), ["x"]);
        let doc = Document::from_reply(base(), 200, None, b"\xef\xbb\xbf<html><p>y".to_vec());
        assert_eq!(text_of(&cells(&doc, 20)), ["y"], "past a byte order mark");
        let doc = Document::from_reply(base(), 200, None, b"plain\ttab".to_vec());
        assert_eq!(text_of(&cells(&doc, 20)), ["plain   tab"]);
        let doc = Document::from_reply(base(), 200, None, vec![0, 1, 2]);
        assert!(text_of(&cells(&doc, 80))[0].contains("of no stated type, 3 bytes"));
    }

    #[test]
    fn another_type_is_named_and_not_shown() {
        let doc = Document::from_reply(base(), 200, Some("application/pdf"), vec![1; 10]);
        let lines = text_of(&cells(&doc, 200));
        assert_eq!(lines[0], "This is application/pdf, 10 bytes.");
        assert!(lines[2].contains("Firefox opens the rest"));
    }

    #[test]
    fn an_error_status_with_nothing_to_show_is_an_error_page() {
        let doc = Document::from_reply(base(), 404, Some("text/html"), Vec::new());
        assert_eq!(doc.status, 404);
        assert_eq!(
            text_of(&cells(&doc, 80)),
            ["The server answered 404 and sent nothing to show."]
        );
        let doc = Document::from_reply(base(), 404, Some("text/html"), b"<p>gone".to_vec());
        assert_eq!(text_of(&cells(&doc, 80)), ["gone"]);
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
            text_of(&cells(&doc, 7)),
            ["aaa bbb", "ccc", "ddddddd", "ddd"]
        );
        assert_eq!(text_of(&cells(&doc, 1)).len(), 19);
    }

    #[test]
    fn a_long_page_is_cut_and_says_so() {
        let mut page = b"<p>".to_vec();
        page.resize(MAX_MARKUP + 10, b'a');
        let doc = html_doc(&page);
        assert!(doc.note.unwrap().contains("only the first"));
        let doc = Document::from_reply(base(), 200, Some("text/plain"), vec![b'\n'; MAX_TEXT + 1]);
        assert!(doc.note.as_deref().unwrap().contains("only the first"));
        let rendered = cells(&doc, 80);
        assert_eq!(rendered.page.lines.len(), MAX_LINES);
        assert_eq!(rendered.lines.len(), MAX_LINES);
        assert!(rendered.page.cut && rendered.cut);
        assert!(!rendered.page.unrendered);
        // Markup td-html cuts short past its output budget says so in the
        // status row, not with td-html's notice as a line.
        let mut deep = b"<blockquote>".repeat(62);
        deep.extend(b"<p>x</p>".repeat(200_000));
        let page = html(&deep, None);
        assert!(page.unrendered && !page.cut);
        assert!(page.lines.len() > 10_000);
        assert!(page
            .lines
            .iter()
            .all(|line| line.runs.iter().all(|run| run.text != td_html::CUT_NOTICE)));
        // Markup's logical lines are bounded as text's are.
        let doc = html_doc("a<br>".repeat(MAX_LINES + 10).as_bytes());
        let rendered = cells(&doc, 80);
        assert_eq!(rendered.page.lines.len(), MAX_LINES);
        assert!(rendered.cut);
        // And so are the lines they wrap to.
        let doc = Document::from_reply(
            base(),
            200,
            Some("text/plain"),
            "ab".repeat(MAX_LINES).into_bytes(),
        );
        let rendered = cells(&doc, 1);
        assert!(!rendered.page.cut);
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
        assert_eq!(
            rendered.page.shown(0).unwrap(),
            "https://example.org/b#x%01y"
        );
    }

    #[test]
    fn a_png_is_a_picture_and_one_too_large_is_refused_from_its_header() {
        let png = td_png::encode_rgb8(2, 1, &[255, 0, 0, 0, 255, 0]).unwrap();
        let doc = Document::from_reply(base(), 200, Some("image/png"), png);
        let picture = doc.picture().unwrap();
        assert_eq!((picture.width(), picture.height()), (2, 1));
        assert!(cells(&doc, 80).lines.is_empty());
        let wide = td_png::encode_rgb8(
            MAX_PICTURE_AXIS + 1,
            1,
            &vec![0; (MAX_PICTURE_AXIS + 1) * 3],
        )
        .unwrap();
        let doc = Document::from_reply(base(), 200, Some("image/png"), wide);
        assert!(doc.picture().is_none());
        assert!(text_of(&cells(&doc, 80))[0].contains("too large to show"));
        let doc = Document::from_reply(base(), 200, Some("image/png"), b"not a png".to_vec());
        assert!(doc.picture().is_none());
        assert!(text_of(&cells(&doc, 80))[0].starts_with("The PNG could not be read"));
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

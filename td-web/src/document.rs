//! A response as td-web shows it (DESIGN.md §5): HTML rendered once
//! through td-html's rich output into logical lines, text as it is, a
//! PNG, JPEG, GIF, WebP or AVIF decoded, anything else named and not shown, all of it on
//! the render thread; and the rendering, those lines wrapped by pixel
//! width into lines of styled pieces with link targets, on the window's
//! thread, a few at a time near the ones shown, for each width and body
//! face the page is shown at.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use crate::worker::{SubmitError, Worker};

use crate::memory::{self, Resident};
use td_alloc::{Budget, BudgetError, BudgetShared, Charge};
use td_ui::picture::Picture;

use crate::charset::{self, Rules};
use crate::picture::{self, Format};
use td_charset::Encoding;
#[cfg(test)]
use td_ui::picture::{MAX_PICTURE_AXIS, MAX_PICTURE_PIXELS};
use td_ui::prose::{self, Metrics};
use td_ui::raster::{GlyphStyle, TextFamily, INK, PAPER};
use td_url::{Reference, Url};

/// The most bytes of markup rendered; the rest is not shown.
pub const MAX_MARKUP: usize = 2 << 20;
/// What td-html reads and builds of a page (DESIGN.md §3, §5): its first
/// `MAX_MARKUP` bytes, and an output budget of 16 bytes a byte of that.
/// Real pages measured at width 0 spend at most 11 bytes of budget a
/// byte, and a page of dense tables 16.4, which at its own 670 KiB needs
/// 11 MiB: every real page tried, cut to `MAX_MARKUP`, renders whole, and
/// hostile ones are cut short. td-html
/// builds a rendering whole before td-web keeps `MAX_LINES` of it, so
/// these bound that work: its peak is at most some 100 bytes a byte of
/// input and 1.25 a byte of budget (td-html/DESIGN.md, "Resident
/// rendering cost").
pub const HTML_LIMITS: td_html::Limits = td_html::Limits {
    input: MAX_MARKUP,
    output: 32 << 20,
};
/// The most bytes of text rendered.
pub const MAX_TEXT: usize = 4 << 20;
/// The most characters one line of text is expanded to.
const MAX_LINE_CHARS: usize = 1 << 20;
/// The most logical lines a page keeps; the rest are not shown.
pub const MAX_LINES: usize = 100_000;
/// The most text one wrap is handed, in bytes with one more for each
/// run: a logical line is wrapped a slice at a time, `FIRST_SLICE` and
/// twice that while a slice holds no whole line. A wrapped line whose
/// first `MAX_ROW_BYTES` hold no break ends there, the spaces after it
/// going with the break, and spaces a slice starts with that its wrap
/// drops start it again after them: only characters of no advance,
/// which never fill the width, make a line end otherwise than a wrap of
/// the whole line would.
const MAX_ROW_BYTES: usize = 64 << 10;
/// The first slice of a logical line handed to one wrap.
const FIRST_SLICE: usize = 4 << 10;
/// The wrapped lines laid at a time, and between the starts a walk along
/// a logical line remembers.
const STRIDE: usize = 64;
/// The logical lines whose walks' starts are remembered.
const WALKED: usize = 2;
/// The bytes of a body of no stated type MIME Sniffing reads, its
/// resource header.
const SNIFF_HEADER: usize = 1445;
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

impl Run {
    /// `text`, some of this run's, as prose measures it, its body text in
    /// `body`'s family and tagged with its link.
    fn prose<'t>(&self, body: TextFamily, text: &'t str) -> prose::Run<'t> {
        prose::Run {
            text,
            style: self.style.glyph(body),
            tag: self.link.unwrap_or(NO_LINK),
        }
    }
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

// Empty renderings need neither an allocation nor a budget control block.
static EMPTY_PAGE: Page = Page {
    lines: Vec::new(),
    links: Vec::new(),
    cut: false,
    unrendered: false,
    trimmed: false,
    base: None,
};
#[derive(Default)]
pub(crate) struct PageRef(Option<BudgetShared<Resident<Page>>>);
impl PageRef {
    fn new(page: Page, charge: Charge) -> Result<Self, BudgetError> {
        let retained = page.storage_bytes()?;
        let memory = charge.budget().try_clone()?;
        let resident = Resident::new(page, charge, retained)?;
        Ok(Self(Some(BudgetShared::new(&memory, resident)?)))
    }
    pub(crate) fn try_clone(&self) -> Result<Self, BudgetError> {
        Ok(Self(
            self.0.as_ref().map(BudgetShared::try_clone).transpose()?,
        ))
    }
    #[cfg(test)]
    #[allow(clippy::unwrap_used)]
    pub(crate) fn for_test(page: Page) -> Self {
        let memory = memory::root().unwrap();
        Self::new(page, memory.charge(memory::PAGES).unwrap()).unwrap()
    }
    fn budget(&self) -> Option<&Budget> {
        self.0.as_ref().map(|page| page.budget())
    }
}
impl std::ops::Deref for PageRef {
    type Target = Page;
    fn deref(&self) -> &Page {
        self.0.as_ref().map_or(&EMPTY_PAGE, |page| page)
    }
}
impl std::fmt::Debug for PageRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        (**self).fmt(f)
    }
}
impl PartialEq for PageRef {
    fn eq(&self, other: &Self) -> bool {
        **self == **other
    }
}
impl Eq for PageRef {}
#[cfg(test)]
impl Clone for PageRef {
    #[allow(clippy::unwrap_used)]
    fn clone(&self) -> Self {
        self.try_clone().unwrap()
    }
}

impl Page {
    fn storage_bytes(&self) -> Result<usize, BudgetError> {
        let mut total = 0usize;
        let mut add = |capacity: usize, size: usize| -> Result<(), BudgetError> {
            total = total
                .checked_add(capacity.checked_mul(size).ok_or(BudgetError::Overflow)?)
                .ok_or(BudgetError::Overflow)?;
            Ok(())
        };
        add(self.lines.capacity(), std::mem::size_of::<Logical>())?;
        add(self.links.capacity(), std::mem::size_of::<Link>())?;
        for line in &self.lines {
            add(line.runs.capacity(), std::mem::size_of::<Run>())?;
            for run in &line.runs {
                add(run.text.capacity(), 1)?;
            }
        }
        for link in &self.links {
            add(link.written.capacity(), 1)?;
        }
        // Url retains origin/path/host strings, each bounded by MAX_URL.
        if self.base.is_some() {
            add(3 * td_url::MAX_URL, 1)?;
        }
        Ok(total)
    }
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

    /// The first character from `start` up to `end`, in one logical line,
    /// that is no space, else `start`: where text from `start` is first
    /// drawn, since a wrapped line drops the spaces it ends with.
    pub fn drawn(&self, start: Place, end: Place) -> Place {
        self.lines
            .get(start.logical)
            .and_then(|line| past_spaces(line, (start.run, start.at)))
            .map(|(run, at)| Place { run, at, ..start })
            .filter(|&place| place < end)
            .unwrap_or(start)
    }

    /// Where link `at` starts: its first character other than a space on
    /// the logical line it starts on, else the start of its first run
    /// there. Links follow one another, so their places do too.
    pub fn link_place(&self, at: usize) -> Option<Place> {
        let link = self.links.get(at)?;
        let runs = self.lines.get(link.line).map_or(&[][..], |line| &line.runs);
        let mut first = None;
        for (index, run) in runs.iter().enumerate() {
            if run.link != Some(at) {
                continue;
            }
            let place = |at| Place {
                logical: link.line,
                run: index,
                at,
            };
            first.get_or_insert(place(0));
            if let Some((byte, _)) = run
                .text
                .char_indices()
                .find(|&(_, c)| prose::shown(c) != ' ')
            {
                return Some(place(byte));
            }
        }
        Some(first.unwrap_or(Place::line(link.line)))
    }

    /// Where logical line `logical` ends: after its last run's text.
    pub fn line_end(&self, logical: usize) -> Place {
        let runs = self.lines.get(logical).map_or(&[][..], |line| &line.runs);
        match runs.len().checked_sub(1) {
            Some(last) => Place {
                logical,
                run: last,
                at: runs.get(last).map_or(0, |run| run.text.len()),
            },
            None => Place::line(logical),
        }
    }

    /// The characters of `at`'s logical line from `at` on, each with where
    /// it is.
    fn chars_from(&self, at: Place) -> impl Iterator<Item = (Place, char)> + '_ {
        let runs = self
            .lines
            .get(at.logical)
            .map_or(&[][..], |line| &line.runs);
        runs.iter()
            .enumerate()
            .skip(at.run)
            .flat_map(move |(run, part)| {
                let from = if run == at.run { at.at } else { 0 };
                let text = part.text.get(from..).unwrap_or_default();
                text.char_indices().map(move |(byte, c)| {
                    let place = Place {
                        logical: at.logical,
                        run,
                        at: from + byte,
                    };
                    (place, c)
                })
            })
    }

    /// The characters of `at`'s logical line before `at`, the nearest
    /// first, each with where it is.
    fn chars_before(&self, at: Place) -> impl Iterator<Item = (Place, char)> + '_ {
        let runs = self
            .lines
            .get(at.logical)
            .map_or(&[][..], |line| &line.runs);
        runs.iter()
            .enumerate()
            .take(at.run.saturating_add(1))
            .rev()
            .flat_map(move |(run, part)| {
                let upto = if run == at.run {
                    at.at
                } else {
                    part.text.len()
                };
                let text = part.text.get(..upto).unwrap_or_default();
                text.char_indices().rev().map(move |(byte, c)| {
                    let place = Place {
                        logical: at.logical,
                        run,
                        at: byte,
                    };
                    (place, c)
                })
            })
    }

    /// Where the character after `at` ends; at a logical line's end, the
    /// next line's start, and at the page's end `at` itself.
    pub fn after_char(&self, at: Place) -> Place {
        match self.chars_from(at).next() {
            Some((place, c)) => place.past(c),
            None => self.next_start(at),
        }
    }

    /// Where the character before `at` starts; at a logical line's start,
    /// the line before's end, and at the page's start `at` itself.
    pub fn before_char(&self, at: Place) -> Place {
        match self.chars_before(at).next() {
            Some((place, _)) => place,
            None => self.previous_end(at),
        }
    }

    /// Where the next word from `at` ends, past what is no word before
    /// it; its logical line's end when no word is left there, and from a
    /// line's end the next line's start.
    pub fn word_after(&self, at: Place) -> Place {
        let mut chars = self.chars_from(at).peekable();
        if chars.peek().is_none() {
            return self.next_start(at);
        }
        let mut end = None;
        for (place, c) in chars {
            if word_char(c) {
                end = Some(place.past(c));
            } else if end.is_some() {
                break;
            }
        }
        end.unwrap_or_else(|| self.line_end(at.logical))
    }

    /// Where the word before `at` starts, past what is no word after it;
    /// its logical line's start when no word is left there, and from a
    /// line's start the line before's end.
    pub fn word_before(&self, at: Place) -> Place {
        let mut chars = self.chars_before(at).peekable();
        if chars.peek().is_none() {
            return self.previous_end(at);
        }
        let mut start = None;
        for (place, c) in chars {
            if word_char(c) {
                start = Some(place);
            } else if start.is_some() {
                break;
            }
        }
        start.unwrap_or(Place::line(at.logical))
    }

    /// The word of letters and digits the character at `at` is in, from
    /// where it starts to where it ends, within its logical line; none
    /// when that character is no word's.
    pub fn word(&self, at: Place) -> Option<(Place, Place)> {
        let (place, c) = self.chars_from(at).next()?;
        if !word_char(c) {
            return None;
        }
        let start = self
            .chars_before(place)
            .take_while(|&(_, c)| word_char(c))
            .last()
            .map_or(place, |(start, _)| start);
        let end = self
            .chars_from(place)
            .take_while(|&(_, c)| word_char(c))
            .last()
            .map_or(place, |(last, c)| last.past(c));
        Some((start, end))
    }

    /// Where the selection moves down a logical line from `at`: its line's
    /// end, or from there the next line's.
    pub fn line_down(&self, at: Place) -> Place {
        if self.chars_from(at).next().is_some() {
            return self.line_end(at.logical);
        }
        let next = at.logical.saturating_add(1);
        if next < self.lines.len() {
            self.line_end(next)
        } else {
            at
        }
    }

    /// Where the selection moves up a logical line from `at`: its line's
    /// start, or from there the line before's.
    pub fn line_up(&self, at: Place) -> Place {
        if self.chars_before(at).next().is_some() {
            return Place::line(at.logical);
        }
        at.logical.checked_sub(1).map_or(at, Place::line)
    }

    /// The next logical line's start after `at`'s; `at` on the last.
    fn next_start(&self, at: Place) -> Place {
        let next = at.logical.saturating_add(1);
        if next < self.lines.len() {
            Place::line(next)
        } else {
            at
        }
    }

    /// The logical line before `at`'s end; `at` on the first.
    fn previous_end(&self, at: Place) -> Place {
        at.logical
            .checked_sub(1)
            .map_or(at, |before| self.line_end(before))
    }

    /// The page's text from `start` to `end`, the runs of each logical
    /// line joined and the lines joined by a newline; none when it is
    /// longer than `most` bytes, which is found by measuring it before
    /// any of it is gathered, so what is made is never larger.
    pub fn text(&self, start: Place, end: Place, most: usize) -> Option<String> {
        let size = self.pieces(start, end).try_fold(0usize, |size, piece| {
            Some(size.saturating_add(piece.len())).filter(|&size| size <= most)
        })?;
        let mut out = String::with_capacity(size);
        self.pieces(start, end)
            .for_each(|piece| out.push_str(piece));
        Some(out)
    }

    /// The text from `start` to `end` in pieces: the part of each run
    /// between them, and a newline between two logical lines.
    fn pieces(&self, start: Place, end: Place) -> impl Iterator<Item = &str> + '_ {
        let last = end.logical.min(self.lines.len().saturating_sub(1));
        (start.logical..=last).flat_map(move |logical| {
            let runs = self.lines.get(logical).map_or(&[][..], |line| &line.runs);
            let on_first = logical == start.logical;
            let on_last = logical == end.logical;
            let parts = runs.iter().enumerate().filter_map(move |(run, part)| {
                if (on_first && run < start.run) || (on_last && run > end.run) {
                    return None;
                }
                let from = if on_first && run == start.run {
                    start.at
                } else {
                    0
                };
                let to = if on_last && run == end.run {
                    end.at
                } else {
                    part.text.len()
                };
                part.text.get(from..to)
            });
            (!on_first).then_some("\n").into_iter().chain(parts)
        })
    }
}

/// Whether `c` is part of a word as a double click and a word's move take
/// it: a letter or a digit.
fn word_char(c: char) -> bool {
    c.is_alphanumeric()
}

/// Where a reader is in a page, kept across wraps: a logical line, and
/// the run and the byte in it that a wrapped line's own text starts at.
#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
pub struct Place {
    pub logical: usize,
    pub run: usize,
    pub at: usize,
}

impl Place {
    /// Where logical line `logical` starts.
    pub fn line(logical: usize) -> Self {
        Self {
            logical,
            run: 0,
            at: 0,
        }
    }

    /// Whether this is where a logical line starts, as only its first
    /// wrapped line does.
    fn starts_line(self) -> bool {
        self.run == 0 && self.at == 0
    }

    /// The least place after this one.
    fn after(self) -> Self {
        Self {
            at: self.at.saturating_add(1),
            ..self
        }
    }

    /// Where `c`, the character at this place, ends.
    pub fn past(self, c: char) -> Self {
        Self {
            at: self.at.saturating_add(c.len_utf8()),
            ..self
        }
    }
}

include!("wrap.rs");

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
    Page(PageRef),
    Picture(Resident<Picture>),
    PictureResources,
}

pub struct Document {
    /// The URL that answered; none for an address td-web did not take.
    pub url: Option<Url>,
    pub status: u16,
    pub title: Option<String>,
    /// What the status row says once it has nothing better to say: the
    /// page was cut short, say.
    pub note: Option<String>,
    /// The reply's `content-disposition`, which names a saved file.
    pub disposition: Option<String>,
    body: Body,
    /// A reply td-web does not show, kept as it arrived for a save
    /// (DESIGN.md §5, "Saving"): at most `MAX_DOCUMENT` beside a page of
    /// a few lines.
    held: Option<Resident<Vec<u8>>>,
}

impl Document {
    /// `from_reply_unless` for a navigation never stopped.
    #[cfg(test)]
    pub fn from_reply(url: Url, status: u16, content_type: Option<&str>, body: Vec<u8>) -> Self {
        Self::from_reply_unless(url, status, content_type, body, &AtomicBool::new(false))
            .unwrap_or_else(|| Self::error(None, "stopped"))
    }

    /// `body`, answered with `status` from `url` as `content_type`,
    /// rendered into logical lines, markup and text decoded to UTF-8
    /// first (`charset`), or decoded as a picture; none once `stopped` is
    /// set before markup is decoded and rendered or a picture decoded.
    /// td-web calls this only on its `Renderer`'s thread, which takes
    /// each reply when the one before it is done, so a navigation stopped
    /// or overtaken while it waited does no such work.
    #[cfg(test)]
    pub fn from_reply_unless(
        url: Url,
        status: u16,
        content_type: Option<&str>,
        body: Vec<u8>,
        stopped: &AtomicBool,
    ) -> Option<Self> {
        match memory::root() {
            Ok(memory) => Self::from_reply_in(url, status, content_type, body, stopped, &memory),
            Err(_) => Some(Self::resources(Some(url), status)),
        }
    }
    fn from_reply_in(
        url: Url,
        status: u16,
        content_type: Option<&str>,
        body: Vec<u8>,
        stopped: &AtomicBool,
        root: &Budget,
    ) -> Option<Self> {
        if body.is_empty() && !(200..300).contains(&status) {
            let why = format!("The server answered {status} and sent nothing to show.");
            return Some(Self {
                status,
                ..Self::error_with_budget(Some(url), &why, root)
            });
        }
        let media = content_type
            .and_then(|value| value.split(';').next())
            .map(|media| media.trim().to_ascii_lowercase())
            .unwrap_or_default();
        let kind = match media.as_str() {
            "text/html" | "application/xhtml+xml" => Kind::Html,
            "" | "application/octet-stream" => sniff(&body),
            _ if media.starts_with("text/") => Kind::Text,
            _ => Format::from_mime(&media)
                .map(Kind::Picture)
                .unwrap_or(Kind::Other),
        };
        let heavy = matches!(kind, Kind::Html | Kind::Picture(_));
        if heavy && stopped.load(Ordering::SeqCst) {
            return None;
        }
        let Ok(memory) = root.child(root.limit()) else {
            return Some(Self::resources(Some(url), status));
        };
        let working = match kind {
            Kind::Html => memory::HTML_WORK,
            Kind::Text => memory::TEXT_WORK,
            // The picture decoder owns its admission. A notice is
            // admitted separately only if decoding fails.
            Kind::Picture(_) => 0,
            Kind::Other => body.capacity().checked_add(memory::SMALL_WORK)?,
        };
        let Ok(mut charge) = memory.charge(working) else {
            return Some(Self::resources(Some(url), status));
        };
        let size = body.len();
        let base = Some(url.clone());
        let (page, title, note, held) = match kind {
            Kind::Html | Kind::Text => {
                // Markup is cut here, not by td-html's input limit, so
                // td-html's notice means only its budget: the note says
                // how much of the page was read.
                // XML, as markup or shown as text, takes XML's rules.
                let xml = media == "text/xml" || media.ends_with("+xml");
                let (rules, most) = match kind {
                    Kind::Html if xml => (Rules::Xml, HTML_LIMITS.input),
                    Kind::Html => (Rules::Html, HTML_LIMITS.input),
                    _ if xml => (Rules::Xml, MAX_TEXT),
                    _ => (Rules::Text, MAX_TEXT),
                };
                let declared = content_type.and_then(charset::label);
                let declared = declared.as_ref().map(charset::Label::as_bytes);
                let Ok(decoded) = charset::decode(body, declared, rules, most) else {
                    return Some(Self {
                        status,
                        ..Self::resources(Some(url), status)
                    });
                };
                let note = noted(decoded.encoding, decoded.read, size);
                let text = decoded.text;
                if kind == Kind::Text {
                    (
                        match plain_checked(&text, true, base) {
                            Ok(page) => page,
                            Err(_) => return Some(Self::resources(Some(url), status)),
                        },
                        None,
                        note,
                        None,
                    )
                } else {
                    (
                        html(text.as_bytes(), base),
                        title(text.as_bytes()),
                        note,
                        None,
                    )
                }
            }
            Kind::Picture(format) => match picture::decode_with_budget(format, &body, &memory) {
                Ok(picture) => {
                    return Some(Self {
                        url: Some(url),
                        status,
                        title: None,
                        note: None,
                        disposition: None,
                        body: Body::Picture(picture),
                        held: None,
                    });
                }
                Err(why) if why.resources() => {
                    return Some(Self {
                        url: Some(url),
                        status,
                        title: None,
                        note: None,
                        disposition: None,
                        body: Body::PictureResources,
                        held: None,
                    });
                }
                Err(why) => {
                    charge = match memory.charge(memory::SMALL_WORK) {
                        Ok(charge) => charge,
                        Err(_) => return Some(Self::resources(Some(url), status)),
                    };
                    let Ok(message) = td_alloc::format(format_args!("{why}")) else {
                        return Some(Self::resources(Some(url), status));
                    };
                    (
                        match plain_checked(&message, false, base) {
                            Ok(page) => page,
                            Err(_) => return Some(Self::resources(Some(url), status)),
                        },
                        None,
                        None,
                        None,
                    )
                }
            },
            Kind::Other => {
                let named = if media.is_empty() {
                    "of no stated type".to_string()
                } else {
                    visible(media.get(..media.floor_char_boundary(4096)).unwrap_or(""))
                };
                // Only what the server answered with success is offered
                // for saving, and kept for it.
                let held = ((200..300).contains(&status) && !body.is_empty()).then_some(body);
                let offer = if held.is_some() {
                    "Ctrl+S saves it in Downloads.\n\n"
                } else {
                    ""
                };
                let said = format!(
                    "This is {named}, {size} bytes.\n\n{offer}td-web shows HTML, text, PNG, \
                     JPEG, GIF, WebP and AVIF; Firefox opens the rest."
                );
                (
                    match plain_checked(&said, false, base) {
                        Ok(page) => page,
                        Err(_) => return Some(Self::resources(Some(url), status)),
                    },
                    None,
                    None,
                    held,
                )
            }
        };
        let held = match held {
            Some(body) => {
                let bytes = body.capacity();
                let owned = memory
                    .charge(bytes)
                    .and_then(|charge| Resident::new(body, charge, bytes));
                match owned {
                    Ok(body) => Some(body),
                    Err(_) => return Some(Self::resources(Some(url), status)),
                }
            }
            None => None,
        };
        let page = match PageRef::new(page, charge) {
            Ok(page) => page,
            Err(_) => return Some(Self::resources(Some(url), status)),
        };
        Some(Self {
            url: Some(url),
            status,
            title,
            note,
            disposition: None,
            body: Body::Page(page),
            held,
        })
    }

    fn resources(url: Option<Url>, status: u16) -> Self {
        Self {
            url,
            status,
            title: None,
            note: None,
            disposition: None,
            body: Body::PictureResources,
            held: None,
        }
    }
    /// A bounded error page, or an allocation-free resource notice.
    pub fn error_with_budget(url: Option<Url>, why: &str, memory: &Budget) -> Self {
        let made = (|| {
            let charge = memory.charge(memory::SMALL_WORK)?;
            let why = why.get(..why.floor_char_boundary(4096)).unwrap_or("");
            let page = plain_checked(why, false, url.clone())?;
            PageRef::new(page, charge)
        })();
        match made {
            Ok(page) => Self {
                url,
                status: 0,
                title: None,
                note: None,
                disposition: None,
                body: Body::Page(page),
                held: None,
            },
            Err(_) => Self::resources(url, 0),
        }
    }
    #[cfg(test)]
    pub fn error(url: Option<Url>, why: &str) -> Self {
        match memory::root() {
            Ok(memory) => Self::error_with_budget(url, why, &memory),
            Err(_) => Self::resources(url, 0),
        }
    }
    pub fn blank() -> Self {
        Self {
            url: None,
            status: 0,
            title: None,
            note: None,
            disposition: None,
            body: Body::Page(PageRef::default()),
            held: None,
        }
    }
    pub fn fit_picture(
        &self,
        width: usize,
        height: usize,
    ) -> Option<Resident<td_ui::picture::Fitted>> {
        let Body::Picture(picture) = &self.body else {
            return None;
        };
        let charge = picture
            .budget()
            .charge(td_ui::picture::MAX_PICTURE_PIXELS * 4)
            .ok()?;
        let fitted = picture.fit(width, height)?;
        let bytes = fitted.bgra.capacity();
        Resident::new(fitted, charge, bytes).ok()
    }

    /// The body of a reply td-web does not show, given up to be saved;
    /// a later save fetches it again.
    pub fn take_held(&mut self) -> Option<Resident<Vec<u8>>> {
        self.held.take()
    }

    pub fn picture_resources(&self) -> bool {
        matches!(self.body, Body::PictureResources)
    }

    pub fn picture(&self) -> Option<&Picture> {
        match &self.body {
            Body::Picture(picture) => Some(picture),
            Body::Page(_) | Body::PictureResources => None,
        }
    }

    /// The document's lines to be wrapped at `width` pixels (at least
    /// one), its body text in `body`, as they are shown; none for a
    /// picture.
    pub fn render(&self, width: u32, body: TextFamily) -> Result<Rendered, BudgetError> {
        match &self.body {
            Body::Page(page) => Rendered::new(page, width, body),
            Body::Picture(_) | Body::PictureResources => Ok(Rendered::default()),
        }
    }
}

/// Common media types fit inside the charged request slot. Longer headers
/// retain exact UTF-8 bytes in a separately charged owner.
enum ContentType {
    Inline(td_alloc::SmallText<128>),
    Long(td_alloc::BudgetString),
}
impl ContentType {
    fn copy(memory: &Budget, text: &str) -> Result<Self, BudgetError> {
        match td_alloc::SmallText::copy(text) {
            Some(text) => Ok(Self::Inline(text)),
            None => td_alloc::BudgetString::copy(memory, text).map(Self::Long),
        }
    }
    fn as_str(&self) -> &str {
        match self {
            Self::Inline(text) => text.as_str(),
            Self::Long(text) => text,
        }
    }
}

/// A typed render request; its input body and URL retain their existing
/// fetch/metadata limits. The content type is a budget-owned handoff copy.
struct Reply {
    url: Url,
    status: u16,
    kind: Option<ContentType>,
    body: Vec<u8>,
    stopped: Arc<AtomicBool>,
}

fn render_reply(reply: Reply, memory: &Budget) -> Option<Document> {
    let Reply {
        url,
        status,
        kind,
        body,
        stopped,
    } = reply;
    Document::from_reply_in(
        url,
        status,
        kind.as_ref().map(ContentType::as_str),
        body,
        &stopped,
        memory,
    )
    .filter(|_| !stopped.load(Ordering::SeqCst))
}

/// One worker serializes rendering, with a fixed queue for fetch workers.
/// The reusable request/result slots are admitted on its page root at startup.
pub struct Renderer(BudgetShared<RenderState>);
struct RenderState {
    thread: Mutex<Worker<Reply, Option<Document>>>,
    memory: Budget,
}
#[cfg(test)]
impl Clone for Renderer {
    #[allow(clippy::unwrap_used)]
    fn clone(&self) -> Self {
        self.try_clone().unwrap()
    }
}

fn render_thread(memory: &Budget) -> Result<Worker<Reply, Option<Document>>, String> {
    Worker::start(memory, render_reply)
        .map_err(|_| td_alloc::string("the render thread could not start").unwrap_or_default())
}

impl Renderer {
    /// A renderer and its thread, or why the thread could not start.
    pub fn start() -> Result<Self, String> {
        let memory = memory::root()
            .map_err(|_| td_alloc::string("no memory for page budgets").unwrap_or_default())?;
        Self::with_budget(&memory)
    }
    pub fn with_budget(memory: &Budget) -> Result<Self, String> {
        let copied = memory
            .try_clone()
            .map_err(|_| td_alloc::string("no page budget handle").unwrap_or_default())?;
        let state = RenderState {
            thread: Mutex::new(render_thread(memory)?),
            memory: copied,
        };
        BudgetShared::new(memory, state)
            .map(Self)
            .map_err(|_| td_alloc::string("no memory for renderer ownership").unwrap_or_default())
    }
    pub fn try_clone(&self) -> Result<Self, BudgetError> {
        Ok(Self(self.0.try_clone()?))
    }
    pub fn budget(&self) -> &Budget {
        &self.0.memory
    }
    pub fn error(&self, url: Option<Url>, why: &str) -> Document {
        Document::error_with_budget(url, why, self.budget())
    }

    /// Claim a reusable slot, restarting a lost worker before retrying.
    /// Waiting for its answer does not hold the submission lock.
    fn run(&self, reply: Reply) -> Result<Option<Option<Document>>, BudgetError> {
        let ticket = {
            let mut thread = self.0.thread.lock().unwrap_or_else(PoisonError::into_inner);
            match thread.submit(reply) {
                Ok(ticket) => ticket,
                Err(SubmitError::Memory(error, _)) => return Err(error),
                Err(SubmitError::Closed(reply)) => {
                    let Ok(replacement) = Worker::start(self.budget(), render_reply) else {
                        return Ok(None);
                    };
                    *thread = replacement;
                    match thread.submit(reply) {
                        Ok(ticket) => ticket,
                        Err(SubmitError::Memory(error, _)) => return Err(error),
                        Err(SubmitError::Closed(_)) => return Ok(None),
                    }
                }
            }
        };
        Ok(ticket.wait())
    }

    /// `Document::from_reply_unless` on the render thread, waiting for it
    /// there behind the replies handed over before it; an error page when
    /// it could not be made there. A document whose navigation was stopped
    /// while it was made is dropped there too, before the next reply is
    /// taken, so it never stands beside that one's rendering.
    pub fn document(
        &self,
        url: Url,
        status: u16,
        content_type: Option<&str>,
        body: Vec<u8>,
        stopped: &Arc<AtomicBool>,
    ) -> Option<Document> {
        if stopped.load(Ordering::SeqCst) {
            return None;
        }
        let shown = url.clone();
        let made = (|| {
            let kind = content_type
                .map(|kind| ContentType::copy(self.budget(), kind))
                .transpose()?;
            let reply = Reply {
                url,
                status,
                kind,
                body,
                stopped: Arc::clone(stopped),
            };
            self.run(reply)
        })();
        match made {
            Ok(Some(document)) => document,
            Ok(None) => Some(self.error(Some(shown), "the page could not be rendered")),
            Err(_) => Some(Document::resources(Some(shown), status)),
        }
    }
}

// The worker catches a rendering's panic, which only an unwinding
// build can: a build that aborts on panic (`panic = "abort"` in a profile,
// `-C panic=abort` from a recipe) would end td-web on any rendering's
// panic, so it does not build.
#[cfg(not(panic = "unwind"))]
compile_error!("td-web's render thread catches a rendering's panic: its build must unwind");

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Kind {
    Html,
    Text,
    Picture(Format),
    Other,
}

/// What the status row notes of a page's text (DESIGN.md §8): the
/// encoding it was decoded from when that is not UTF-8, and how much of
/// it is shown when not all of it. Only a note, so none when there is no
/// memory for it.
fn noted(encoding: Encoding, read: usize, size: usize) -> Option<String> {
    let named = (encoding != Encoding::Utf8).then(|| encoding.name());
    if read >= size {
        return named.and_then(|name| td_alloc::string(name).ok());
    }
    let (name, between) = named.map_or(("", ""), |name| (name, " · "));
    td_alloc::format(format_args!(
        "{name}{between}only the first {read} of the page's {size} bytes are shown"
    ))
    .ok()
}

/// What a body of no stated type is: HTML by its start, a picture by its
/// signature, and text by a byte order mark or no byte of binary data in
/// its first `SNIFF_HEADER` bytes, the resource header (MIME Sniffing,
/// "Rules for distinguishing if a resource is text or binary").
fn sniff(body: &[u8]) -> Kind {
    let header = body.get(..SNIFF_HEADER).unwrap_or(body);
    let unmarked = body.strip_prefix(b"\xef\xbb\xbf").unwrap_or(body);
    let start: Vec<u8> = unmarked
        .iter()
        .skip_while(|b| b.is_ascii_whitespace())
        .take(15)
        .map(u8::to_ascii_lowercase)
        .collect();
    if start.starts_with(b"<!doctype html") || start.starts_with(b"<html") {
        Kind::Html
    } else if let Some(format) = Format::sniff(body) {
        Kind::Picture(format)
    } else if [b"\xfe\xff".as_slice(), b"\xff\xfe", b"\xef\xbb\xbf"]
        .iter()
        .any(|mark| body.starts_with(mark))
        || !header
            .iter()
            .any(|byte| matches!(byte, 0..=8 | 0x0b | 0x0e..=0x1a | 0x1c..=0x1f))
    {
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
    let text = td_html::to_text_within(head.get(start..end)?, 0, HTML_LIMITS);
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
fn clean(mut text: String, struck: bool) -> String {
    text.retain(|c| (c == '\t' || !c.is_control()) && !(struck && c == STROKE));
    text
}

/// Whether `text` is one of td-html's table rules or column bars: only
/// box-drawing characters.
fn ruling(text: &str) -> bool {
    !text.is_empty() && text.chars().all(|c| ('\u{2500}'..='\u{257f}').contains(&c))
}

/// `source` through td-html's rich output, unwrapped and within
/// `HTML_LIMITS`, into logical lines. Runs of one href next to each
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
    let mut rendered = td_html::to_rich_within(source, 0, HTML_LIMITS)
        .into_iter()
        .peekable();
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
            let has_text = !span.text.trim().is_empty();
            let mut text = clean(span.text, style.strike);
            if unwrapped {
                if left == 0 {
                    page.trimmed |= !text.is_empty();
                    // Text cut away still parts the links either side of it.
                    let target = href.or(image);
                    if has_text && last.as_ref().map(|(previous, _)| previous.as_str()) != target {
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
                    if has_text {
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
    page.lines.shrink_to_fit();
    page.links.shrink_to_fit();
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
    // Kept for as long as the page is shown: a run's text grown by
    // joining, or collected, holds up to twice its length.
    for run in &mut out {
        run.text.shrink_to_fit();
    }
    out.shrink_to_fit();
    out
}

/// `text`'s lines, tabs expanded to every `TAB` characters and other
/// control characters dropped, as logical lines in the monospace face
/// when `mono`, else in the body's.
#[cfg(test)]
#[allow(clippy::unwrap_used)]
fn plain(text: &str, mono: bool, base: Option<Url>) -> Page {
    plain_checked(text, mono, base).unwrap()
}
fn plain_checked(text: &str, mono: bool, base: Option<Url>) -> Result<Page, BudgetError> {
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
        // Unchanged text needs one validation scan and one copy, rather
        // than measuring and encoding each scalar again.
        let unchanged = if raw.is_ascii() {
            (!raw.bytes().any(|b| b.is_ascii_control())).then_some(raw.len().min(MAX_LINE_CHARS))
        } else {
            raw.char_indices()
                .take(MAX_LINE_CHARS)
                .try_fold(0, |_, (at, c)| {
                    (!c.is_control()).then_some(at + c.len_utf8())
                })
        };
        let line = if let Some(end) = unchanged {
            td_alloc::string(raw.get(..end).ok_or(BudgetError::Overflow)?)?
        } else {
            let mut bytes = 0usize;
            let mut columns = 0;
            for c in raw.chars() {
                if columns >= MAX_LINE_CHARS {
                    break;
                }
                let n = if c == '\t' {
                    let n = TAB - columns % TAB;
                    columns += n;
                    n
                } else if !c.is_control() {
                    columns += 1;
                    c.len_utf8()
                } else {
                    0
                };
                bytes = bytes.checked_add(n).ok_or(BudgetError::Overflow)?;
            }
            let mut line = String::new();
            td_alloc::reserve_string(&mut line, bytes)?;
            let mut columns = 0;
            for c in raw.chars() {
                if columns >= MAX_LINE_CHARS {
                    break;
                }
                if c == '\t' {
                    let pad = TAB - columns % TAB;
                    for _ in 0..pad {
                        line.push(' ');
                    }
                    columns += pad;
                } else if !c.is_control() {
                    line.push(c);
                    columns += 1;
                }
            }
            line
        };

        let mut runs = td_alloc::with_capacity(1)?;
        td_alloc::push(
            &mut runs,
            Run {
                text: line,
                style,
                link: None,
            },
        )?;
        td_alloc::push(&mut page.lines, Logical { runs, wrap: true })?;
    }
    Ok(page)
}

#[cfg(test)]
pub(crate) mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]

    #[test]
    fn page_and_wrap_owners_share_admission_until_the_last_drop() {
        let root = memory::root().unwrap();
        let base = root.used();
        let doc = Document::from_reply_in(
            base_url(),
            200,
            Some("text/plain"),
            b"alpha beta gamma".to_vec(),
            &AtomicBool::new(false),
            &root,
        )
        .unwrap();
        let page_bytes = root.used() - base;
        assert!(page_bytes > 0 && page_bytes < memory::TEXT_WORK);
        let first = doc.render(80, TextFamily::Sans).unwrap();
        let second = doc.render(40, TextFamily::Sans).unwrap();
        assert_eq!(
            root.used(),
            base + page_bytes + 2 * Budget::metadata_bytes()
        );
        drop(doc);
        assert_eq!(
            root.used(),
            base + page_bytes + 2 * Budget::metadata_bytes()
        );
        drop(first);
        assert_eq!(root.used(), base + page_bytes + Budget::metadata_bytes());
        drop(second);
        assert_eq!(root.used(), base);
    }

    #[test]
    fn fitted_pixels_outlive_their_document_and_refusals_release_credit() {
        // Decoding needs its working allowance and control blocks, not
        // an extra allowance for an error notice that success never uses.
        let root = Budget::new(memory::PICTURE_WORK + 2 * Budget::metadata_bytes()).unwrap();
        let baseline = root.used();
        let doc = Document::from_reply_in(
            base_url(),
            200,
            Some("image/gif"),
            picture::tests::GIF.to_vec(),
            &AtomicBool::new(false),
            &root,
        )
        .unwrap();
        let pixels = root.used();
        assert!(td_alloc::with_reservation_limit(0, || doc.fit_picture(4, 4)).is_none());
        assert_eq!(root.used(), pixels);
        let fit = doc.fit_picture(4, 4).unwrap();
        let bytes = fit.bgra.capacity();
        assert_eq!(root.used(), pixels + bytes);
        drop(doc);
        assert_eq!(root.used(), baseline + Budget::metadata_bytes() + bytes);
        assert!(!fit.bgra.is_empty());
        drop(fit);
        assert_eq!(root.used(), baseline);
    }

    #[test]
    fn page_root_refusal_preserves_the_displayed_page_and_held_save_charge() {
        let root = memory::root().unwrap();
        let baseline = root.used();
        let old = Document::from_reply_in(
            base_url(),
            200,
            Some("text/plain"),
            b"old page".to_vec(),
            &AtomicBool::new(false),
            &root,
        )
        .unwrap();
        let kept = root.used();
        let blocker = root.charge(root.limit() - root.used()).unwrap();
        assert!(old.render(40, TextFamily::Sans).is_err());
        let refused = Document::from_reply_in(
            base_url(),
            200,
            Some("text/html"),
            b"<p>new page".to_vec(),
            &AtomicBool::new(false),
            &root,
        )
        .unwrap();
        assert!(refused.picture_resources());
        drop((refused, blocker));
        assert_eq!(root.used(), kept);
        assert_eq!(
            old.render(80, TextFamily::Sans).unwrap().page.lines[0].runs[0].text,
            "old page"
        );
        drop(old);
        assert_eq!(root.used(), baseline);
        let mut held = Document::from_reply_in(
            base_url(),
            200,
            Some("application/pdf"),
            vec![9; 4096],
            &AtomicBool::new(false),
            &root,
        )
        .unwrap();
        let body = held.take_held().unwrap();
        drop(held);
        assert_eq!(
            root.used(),
            baseline + Budget::metadata_bytes() + body.capacity()
        );
        drop(body);
        assert_eq!(root.used(), baseline);
    }

    #[test]
    fn cancelled_render_jobs_return_their_page_admission() {
        let root = memory::root().unwrap();
        let renderer = Renderer::with_budget(&root).unwrap();
        let baseline = root.used();
        let stopped = Arc::new(AtomicBool::new(true));
        for kind in ["text/html", "text/plain", "image/gif"] {
            assert!(renderer
                .document(
                    base_url(),
                    200,
                    Some(kind),
                    b"<p>cancelled".to_vec(),
                    &stopped
                )
                .is_none());
            renderer.0.thread.lock().unwrap().idle();
            assert_eq!(root.used(), baseline);
        }
        stopped.store(false, Ordering::SeqCst);
        let doc = renderer
            .document(
                base_url(),
                200,
                Some("text/plain"),
                b"kept".to_vec(),
                &stopped,
            )
            .unwrap();
        drop(renderer);
        assert!(root.used() > Budget::metadata_bytes());
        drop(doc);
        assert_eq!(root.used(), Budget::metadata_bytes());
    }

    fn base_url() -> Url {
        base()
    }

    #[test]
    fn resource_failure_document_needs_no_error_page() {
        let doc = super::Document {
            url: None,
            status: 200,
            title: None,
            note: None,
            disposition: None,
            body: super::Body::PictureResources,
            held: None,
        };
        assert!(doc.picture_resources());
        assert!(doc.picture().is_none());
    }

    use super::*;
    use td_ui::raster::Scale;
    use td_ui::CELL_WIDTH;

    pub(crate) const STRIDE: usize = super::STRIDE;

    fn base() -> Url {
        Url::parse("https://example.org/docs/page").unwrap()
    }

    /// The cell's metrics at scale one, as every in-process test has.
    pub(crate) fn cell() -> Metrics {
        Metrics::new(None, Scale::new(1).unwrap())
    }

    /// A page's lines, every one laid, as reading down it a line at a
    /// time lays them.
    pub(crate) struct Laid {
        pub page: PageRef,
        pub width: u32,
        pub lines: Vec<Wrapped>,
    }

    impl Laid {
        /// Every line of `rendered`, from its first, a line at a time.
        pub(crate) fn of(mut rendered: Rendered, metrics: &mut Metrics) -> Self {
            let mut lines = Vec::new();
            let mut at = Place::default();
            while !rendered.page.lines.is_empty() {
                rendered.settle(metrics, at, 1);
                lines.extend(rendered.shown(at, 1).cloned());
                let (next, moved) = rendered.forward(metrics, at, 1);
                if moved == 0 {
                    break;
                }
                at = next;
            }
            Self {
                page: rendered.page,
                width: rendered.width,
                lines,
            }
        }

        /// The first line each link is on, by link: the first with a
        /// piece of it, else its logical line's first.
        pub(crate) fn link_lines(&self) -> Vec<usize> {
            (0..self.page.links.len())
                .map(|link| {
                    let tagged = self
                        .lines
                        .iter()
                        .position(|wrapped| wrapped.line.pieces.iter().any(|p| p.tag == link));
                    let logical = self.page.links[link].line;
                    tagged.unwrap_or_else(|| {
                        self.lines
                            .iter()
                            .position(|wrapped| wrapped.logical == logical)
                            .unwrap()
                    })
                })
                .collect()
        }
    }

    /// The laid lines `rendered` holds.
    pub(crate) fn laid(rendered: &mut Rendered) -> &mut Rows {
        &mut rendered.laid
    }

    /// `doc` wrapped `columns` cells wide in Sans, every line laid.
    fn cells(doc: &Document, columns: u32) -> Laid {
        Laid::of(
            doc.render(columns * CELL_WIDTH as u32, TextFamily::Sans)
                .unwrap(),
            &mut cell(),
        )
    }

    /// Each wrapped line's text, from its pieces' runs.
    fn text_of(rendered: &Laid) -> Vec<String> {
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

    fn html_page(body: &[u8]) -> Laid {
        cells(&html_doc(body), 80)
    }

    fn runs(rendered: &Laid) -> Vec<&Run> {
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
        assert_eq!(rendered.link_lines(), [0]);
        assert!(rendered.lines[1]
            .line
            .pieces
            .iter()
            .any(|piece| piece.tag == 0));
        let rendered = html_page(b"<p><a href=\"/x\">one</a></p><p><a href=\"/x\">two</a></p>");
        assert_eq!(rendered.page.links.len(), 2, "{:?}", text_of(&rendered));
        assert_eq!(rendered.link_lines(), [0, 2]);
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
        // A place in the text is found again however the page is wrapped,
        // by laying no more than its logical line.
        let wide = cells(&doc, 80);
        let line_at = |laid: &Laid, place| {
            let mut rendered = Rendered::new(&laid.page, laid.width, TextFamily::Sans).unwrap();
            let start = rendered.snap(&mut cell(), place);
            assert!(rendered.laid.len() <= STRIDE);
            laid.lines.iter().position(|w| w.place() == start).unwrap()
        };
        let inside = narrow.lines[1].place();
        assert_eq!(inside.logical, 0);
        assert_ne!(inside, Place::default(), "inside its logical line");
        assert_eq!(line_at(&narrow, inside), 1);
        assert_eq!(line_at(&wide, inside), 0, "the line holding it");
        assert_eq!(line_at(&wide, narrow.lines[2].place()), 0);
        assert_eq!(line_at(&narrow, wide.lines[2].place()), 4);
        let past = Place::line(9);
        assert_eq!(line_at(&narrow, past), 4, "past the end, the last line");
        assert_eq!(line_at(&narrow, Place::default()), 0);
        let middle = Place {
            at: inside.at + 1,
            ..inside
        };
        assert_eq!(line_at(&narrow, middle), 1, "inside a line");
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
        let shown = lines.last().unwrap();
        assert!(shown.starts_with("a b c d"), "{lines:?}");
        assert!("a b c d e f g h i j".starts_with(shown.as_str()));
        let pre = rendered.lines.last().unwrap();
        assert!(!rendered.page.lines[pre.logical].wrap);
        assert_eq!(pre.line.pieces[0].style.family, TextFamily::Mono);
        assert!(pre.line.width > rendered.width);
        let wide = text_of(&cells(&doc, 80));
        assert_eq!(wide.last().unwrap(), "a b c d e f g h i j");
    }

    /// An unwrapped line is laid only as far as a pane shows it, its
    /// edge and an overhang past it; the page keeps the rest, which a
    /// wider pane lays.
    #[test]
    fn an_unwrapped_line_is_laid_only_to_the_pane_s_edge() {
        let mut page = b"<pre>".to_vec();
        page.extend(b"x".repeat(MAX_PRE_CHARS));
        page.extend(b"</pre><table><tr><td>a</td><td>");
        page.extend(b"y ".repeat(1000));
        page.extend(b"</td></tr></table>");
        let doc = html_doc(&page);
        for columns in [10, 80, 300] {
            let rendered = cells(&doc, columns);
            let edge = rendered.width + prose::OVERHANG;
            for wrapped in &rendered.lines {
                let glyphs = &wrapped.line.glyphs;
                assert!(glyphs.iter().all(|glyph| glyph.x < edge));
                assert!(glyphs.len() as u32 <= edge / CELL_WIDTH as u32 + 1);
            }
            assert!(rendered.lines[0].line.width >= rendered.width);
        }
        let all: usize = cells(&doc, 10).page.lines[0]
            .runs
            .iter()
            .map(|run| run.text.chars().count())
            .sum();
        assert_eq!(all, MAX_PRE_CHARS);
    }

    /// Each wrapped line's glyphs as pens in cells and scalars.
    fn pens(rendered: &Laid) -> Vec<Vec<(u32, char)>> {
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
    fn hung(rendered: &Laid) -> Vec<(u32, String)> {
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
        assert_eq!(rendered.page.lines[0].runs[0].text.len(), MAX_PRE_CHARS);
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
        let wide = cells(&doc, 80);
        assert!(text_of(&wide)[1].starts_with("a cell of many words to wrap"));
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
        let kept: usize = rendered.page.lines[0]
            .runs
            .iter()
            .map(|run| run.text.chars().count())
            .sum();
        assert_eq!(kept, MAX_PRE_CHARS);
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
        // One logical line of 300,000 wrapped lines, read from its start,
        // its end, and back up from there.
        let rows = 40;
        let most = rows + 2 * STRIDE.max(rows) + STRIDE;
        let metrics = &mut cell();
        let mut rendered = doc.render(1, TextFamily::Sans).unwrap();
        rendered.settle(metrics, Place::default(), rows);
        assert!(rendered.laid.len() <= most);
        let last = rendered.last(metrics);
        assert!(rendered.laid.len() <= STRIDE);
        assert_eq!(last.at, 3 * MAX_LINES - 1, "a glyph a line");
        assert!(rendered.after.is_none());
        assert_eq!(rendered.stops[0].1.len(), (3 * MAX_LINES).div_ceil(STRIDE));
        let mut top = rendered.back(metrics, last, rows - 1).0;
        rendered.settle(metrics, top, rows);
        assert_eq!(rendered.shown(top, rows).count(), rows);
        for _ in 0..200 {
            top = rendered.back(metrics, top, rows).0;
            rendered.settle(metrics, top, rows);
            assert!(rendered.laid.len() <= most);
        }
        assert_eq!(top.at, 3 * MAX_LINES - rows - 200 * rows);
        // As many logical lines as a page keeps, each wrapped to three.
        let mut list = b"<ul>".to_vec();
        list.extend(b"<li>y".repeat(MAX_LINES + 1));
        let doc = html_doc(&list);
        let mut rendered = doc.render(1, TextFamily::Sans).unwrap();
        assert!(rendered.page.cut);
        let last = rendered.last(metrics);
        assert_eq!(last.logical, MAX_LINES - 1);
        let top = rendered.back(metrics, last, rows - 1).0;
        rendered.settle(metrics, top, rows);
        assert!(rendered.laid.len() <= most);
        let shown: Vec<usize> = rendered.shown(top, rows).map(|w| w.logical).collect();
        assert_eq!(shown.len(), rows);
        assert_eq!(shown.last(), Some(&(MAX_LINES - 1)));
    }

    /// A line of characters of no advance, which never fills its width,
    /// is wrapped a slice at a time and ends at `MAX_ROW_BYTES`; laid
    /// from any of its lines' starts, it lays the same lines.
    #[test]
    fn a_line_that_never_fills_its_width_ends_at_its_bound() {
        let marks = "\u{301}".repeat(3 * MAX_ROW_BYTES / 2);
        let page = PageRef::for_test(Page {
            lines: vec![Logical {
                runs: vec![Run {
                    text: format!("a{marks}b c"),
                    style: Style::default(),
                    link: None,
                }],
                wrap: true,
            }],
            ..Page::default()
        });
        // On the cell a mark is as wide as any glyph, so a line wider
        // than all of them stands for one they never fill.
        let rendered = Rendered::new(&page, u32::MAX / 2, TextFamily::Sans).unwrap();
        let laid = Laid::of(rendered.clone(), &mut cell());
        let bytes: Vec<usize> = laid
            .lines
            .iter()
            .map(|w| w.line.pieces.iter().map(|p| p.text.len()).sum())
            .collect();
        assert_eq!(bytes.len(), 4, "{bytes:?}");
        assert!(bytes[..3].iter().all(|&b| b < MAX_ROW_BYTES), "{bytes:?}");
        assert_eq!(
            bytes.iter().sum::<usize>(),
            page.lines[0].runs[0].text.len()
        );
        for line in &laid.lines {
            let (again, _) = rendered.lay(&mut cell(), line.place(), 1).unwrap();
            assert_eq!(again[0].line, line.line);
        }
    }

    /// Each line's glyphs as pens and scalars.
    fn glyphs_of(lines: &[prose::Line]) -> Vec<Vec<(u32, char)>> {
        lines
            .iter()
            .map(|line| line.glyphs.iter().map(|g| (g.x, g.scalar)).collect())
            .collect()
    }

    /// Laid a slice at a time from where the reader is, a page's lines
    /// are those a wrap of each whole logical line makes, slices ending
    /// inside words and inside runs of spaces too, which a line never
    /// breaks at: a long run of spaces after a word, before one or alone
    /// is no line of its own, and no line starts with one it drops.
    #[test]
    fn a_page_laid_a_slice_at_a_time_is_its_whole_lines_wrapped() {
        let spaces = " ".repeat(100_000);
        let words = "word ".repeat(20_000);
        let texts = [
            format!("word{spaces}"),
            format!("{spaces}word"),
            spaces.clone(),
            format!("a{spaces}b{spaces}c"),
            format!("{words}{spaces}{words}"),
            "x".repeat(200_000),
            format!("{}\nafter", "\t".repeat(20_000)),
            // Leading spaces, dropped, and a word across the slice's end.
            format!("{}abcdefghij more", "\t".repeat(8191)),
            format!("{}abcdefghij", " ".repeat(65530)),
        ];
        for text in texts {
            let doc = Document::from_reply(base(), 200, Some("text/plain"), text.into_bytes());
            let Body::Page(page) = &doc.body else {
                panic!("a page");
            };
            for columns in [10, 80] {
                let width = columns * CELL_WIDTH as u32;
                let metrics = &mut cell();
                let mut whole = Vec::new();
                for logical in &page.lines {
                    let runs: Vec<prose::Run> = logical
                        .runs
                        .iter()
                        .map(|run| run.prose(TextFamily::Sans, &run.text))
                        .collect();
                    whole.extend(
                        prose::wrap(metrics, &runs, width, &Budget::new(64 << 20).unwrap())
                            .unwrap(),
                    );
                }
                let laid = cells(&doc, columns);
                let lines: Vec<prose::Line> = laid
                    .lines
                    .iter()
                    .map(|w| w.line.try_clone().unwrap())
                    .collect();
                assert_eq!(glyphs_of(&lines), glyphs_of(&whole), "{columns}");
                for wrapped in laid.lines.iter().filter(|w| w.start != (0, 0)) {
                    let (run, at) = wrapped.start;
                    let text = &page.lines[wrapped.logical].runs[run].text[at..];
                    assert!(!text.starts_with(' '), "{:?}", wrapped.start);
                }
            }
        }
    }

    #[test]
    fn a_stopped_navigation_renders_and_decodes_nothing() {
        let stopped = AtomicBool::new(true);
        for (kind, body) in [
            ("text/html", b"<p>x".to_vec()),
            ("image/png", td_png::encode_rgb8(1, 1, &[0, 0, 0]).unwrap()),
            ("image/gif", picture::tests::GIF.to_vec()),
            ("image/webp", picture::tests::WEBP.to_vec()),
            ("image/avif", picture::tests::AVIF.to_vec()),
        ] {
            let doc = Document::from_reply_unless(base(), 200, Some(kind), body, &stopped);
            assert!(doc.is_none(), "{kind}");
        }
        let doc =
            Document::from_reply_unless(base(), 200, Some("text/plain"), b"x".to_vec(), &stopped);
        assert!(doc.is_some(), "text is not held back");
    }

    /// Replies handed over from several fetch threads at once each come
    /// back as their own document, made on the render thread, and one
    /// whose navigation was stopped is not rendered, or not kept.
    #[test]
    fn the_renderer_answers_each_reply_and_renders_no_stopped_one() {
        let renderer = Renderer::start().unwrap();
        let threads: Vec<_> = (0..4)
            .map(|n| {
                let renderer = renderer.clone();
                std::thread::spawn(move || {
                    let body = format!("<title>page {n}</title><p>text {n}").into_bytes();
                    let going = Arc::new(AtomicBool::new(false));
                    let doc = renderer.document(base(), 200, Some("text/html"), body, &going);
                    (n, doc.unwrap())
                })
            })
            .collect();
        for thread in threads {
            let (n, doc) = thread.join().unwrap();
            assert_eq!(doc.title, Some(format!("page {n}")));
            assert_eq!(text_of(&cells(&doc, 80)), [format!("text {n}")]);
        }
        let stopped = Arc::new(AtomicBool::new(true));
        let body = b"<p>x".to_vec();
        let doc = renderer.document(base(), 200, Some("text/html"), body, &stopped);
        assert!(doc.is_none());
        // Already stopped text is skipped before allocating a handoff too.
        let body = b"x".to_vec();
        let doc = renderer.document(base(), 200, Some("text/plain"), body, &stopped);
        assert!(doc.is_none());
    }

    #[test]
    fn renderer_handoff_refusal_keeps_the_old_page_and_returns_credit() {
        let root = memory::root().unwrap();
        let renderer = Renderer::with_budget(&root).unwrap();
        let going = Arc::new(AtomicBool::new(false));
        let old = renderer
            .document(base(), 200, Some("text/plain"), b"old".to_vec(), &going)
            .unwrap();
        renderer.0.thread.lock().unwrap().idle();
        let baseline = root.used();
        let long = format!("text/plain; x={}", "x".repeat(128));
        for (kind, needed) in [("text/plain", 0), (long.as_str(), 1)] {
            for allowance in 0..=needed {
                let url = base();
                let body = b"new".to_vec();
                let doc = td_alloc::with_reservation_limit(allowance, || {
                    renderer.document(url, 200, Some(kind), body, &going)
                })
                .unwrap();
                assert_eq!(doc.picture_resources(), allowance < needed);
                drop(doc);
                renderer.0.thread.lock().unwrap().idle();
                assert_eq!(root.used(), baseline);
                assert_eq!(text_of(&cells(&old, 80)), ["old"]);
            }
        }
        let blocker = root.charge(root.limit() - root.used()).unwrap();
        let refused = renderer
            .document(base(), 200, None, b"new".to_vec(), &going)
            .unwrap();
        assert!(refused.picture_resources());
        going.store(true, Ordering::SeqCst);
        assert!(renderer
            .document(base(), 200, Some("text/plain"), b"stopped".to_vec(), &going)
            .is_none());
        drop((blocker, refused, old, renderer));
        assert_eq!(root.used(), Budget::metadata_bytes());
    }

    #[test]
    fn handoff_headers_keep_every_byte_across_inline_boundary() {
        let root = memory::root().unwrap();
        let baseline = root.used();
        for len in [0, 127, 128, 129, 8192] {
            let text = "x".repeat(len);
            let kind = ContentType::copy(&root, &text).unwrap();
            assert_eq!(kind.as_str(), text);
            assert_eq!(root.used() == baseline, len <= 128);
            drop(kind);
            assert_eq!(root.used(), baseline);
        }
        let text = format!("{}é", "x".repeat(127));
        assert_eq!(ContentType::copy(&root, &text).unwrap().as_str(), text);
        assert_eq!(crate::worker::QUEUED, crate::app::MAX_WORKERS);
    }

    #[test]
    fn a_renderer_restarts_a_lost_worker() {
        let renderer = Renderer::start().unwrap();
        renderer.0.thread.lock().unwrap().stop();
        let going = Arc::new(AtomicBool::new(false));
        for _ in 0..2 {
            let doc = renderer.document(
                base(),
                200,
                Some("text/html"),
                b"<p>still here".to_vec(),
                &going,
            );
            assert_eq!(text_of(&cells(&doc.unwrap(), 80)), ["still here"]);
        }
    }

    /// `HTML_LIMITS` renders ordinary markup of `MAX_MARKUP` whole, here
    /// headed sections of paragraphs with links, emphasis and code, and
    /// cuts one-letter paragraphs in nested quotes short.
    #[test]
    fn ordinary_markup_at_the_bound_renders_whole() {
        let section = "<h2>A heading</h2><p>Some <a href=\"/docs/next#part\">ordinary \
                       text</a> with <em>emphasis</em>, <code>code</code> and words \
                       enough to make a paragraph of a manual.</p><ul><li>one item</li>\
                       <li>another <strong>item</strong></li></ul>";
        let page = section.repeat(MAX_MARKUP / section.len());
        assert!(page.len() > MAX_MARKUP - section.len());
        let page = html(page.as_bytes(), None);
        assert!(!page.unrendered && !page.cut);
        let mut deep = "<blockquote>".repeat(62);
        deep.push_str(&"<p>x".repeat((MAX_MARKUP - deep.len()) / 4));
        assert!(html(deep.as_bytes(), None).unrendered);
    }

    /// What glibc holds for `bytes` asked of it: a chunk of them and its
    /// 8-byte header, in 16-byte steps, at least 32; nothing for nothing.
    fn chunk(bytes: usize) -> usize {
        if bytes == 0 {
            0
        } else {
            (bytes + 8).div_ceil(16).max(2) * 16
        }
    }

    /// What `page` holds on the heap, by its vectors' and strings'
    /// capacities.
    fn page_bytes(page: &Page) -> usize {
        let mut total = chunk(page.lines.capacity() * std::mem::size_of::<Logical>());
        for line in &page.lines {
            total += chunk(line.runs.capacity() * std::mem::size_of::<Run>());
            total += line
                .runs
                .iter()
                .map(|run| chunk(run.text.capacity()))
                .sum::<usize>();
        }
        total += chunk(page.links.capacity() * std::mem::size_of::<Link>());
        total
            + page
                .links
                .iter()
                .map(|link| chunk(link.written.capacity()))
                .sum::<usize>()
    }

    /// A page keeps no more than td-html's output budget, however its
    /// markup is shaped: a run holds its text in its own length, joined
    /// from a word a run or not, and its lines and links no spare room.
    #[test]
    fn a_page_keeps_no_more_than_the_budget() {
        let fill = |head: &str, unit: &str| {
            let mut markup = head.to_string();
            while markup.len() + unit.len() <= MAX_MARKUP {
                markup.push_str(unit);
            }
            markup
        };
        let words = format!("<p>{}", "word ".repeat(200));
        let styles = format!("<p>{}", "<b>a</b> <i>b</i> ".repeat(100));
        let links = format!("<p>{}", "<a href=\"/x\">a</a> b ".repeat(100));
        for markup in [
            fill("", &words),
            fill("", &styles),
            fill("", &links),
            fill(&"<blockquote>".repeat(62), "<p>x"),
            fill("", "<p>x"),
        ] {
            let page = html(markup.as_bytes(), None);
            assert!(page.lines.iter().all(|line| line
                .runs
                .iter()
                .all(|run| run.text.capacity() == run.text.len())));
            assert!(page.lines.capacity() == page.lines.len());
            let held = page_bytes(&page);
            assert!(held <= HTML_LIMITS.output, "{held}: {}", &markup[..40]);
        }
    }

    /// The most a text page holds: its text, at most `MAX_TEXT` bytes of
    /// UTF-8 however it was sent, a tab of it eight spaces and any other
    /// character its own bytes, and for each of its lines its `Logical`, a
    /// one-run `Vec` and what a chunk rounds its text up by, 120 bytes in
    /// all.
    const TEXT_PAGE: usize = 8 * MAX_TEXT + MAX_LINES * 120;

    /// A text page of `MAX_TEXT` keeps no more than `TEXT_PAGE`, however
    /// its tabs are spread across its lines; past the budget's, since its
    /// tabs are expanded.
    #[test]
    fn a_text_page_keeps_no_more_than_its_bound() {
        let mut most = 0;
        for tabs in [1, 7, 40, 41, 100, 5000] {
            let mut text = String::with_capacity(MAX_TEXT);
            while text.len() + tabs < MAX_TEXT {
                text.extend(std::iter::repeat_n('\t', tabs));
                text.push('\n');
            }
            let page = plain(&text, true, None);
            most = most.max(page_bytes(&page));
        }
        assert!(most <= TEXT_PAGE, "{most}");
        assert!(most > HTML_LIMITS.output, "{most}");
    }

    /// The limits as measured against the jail (DESIGN.md §3): what
    /// td-web holds at the worst while a reply is made into a document
    /// beside the one shown stays 20 MB under `memory-high`. That is its own
    /// resident memory on the largest output td supports, 3840x2160, with
    /// td-ui's three window buffers (`BASELINE`); a body for each fetch
    /// out (`MAX_WORKERS` of `MAX_DOCUMENT`), a page's text decoded from
    /// another encoding taking its body's place (`charset::decode`, at
    /// most `MAX_MARKUP` or `MAX_TEXT`, the body dropped before it is
    /// rendered); on the render thread either
    /// a rendering at td-html's rule for its peak, 100 bytes a byte of
    /// input read and 1.25 a byte of budget (td-html/DESIGN.md, "Resident
    /// rendering cost"), or the costliest picture decoding, a 16-bit RGBA
    /// PNG of `MAX_PICTURE_PIXELS` inflated at 8 bytes a pixel beside its
    /// 4-byte RGBA (201 MB measured); and what is shown, either a page,
    /// which holds no more than td-html's output for it, so no more than
    /// its budget, or a text page (`TEXT_PAGE`), with the lines of it laid
    /// on the window's thread (`WRAP`: those near the ones shown, however
    /// long the page, at most 19.3 MB measured, with room), or a picture
    /// with its copy fitted to the pane, now the larger. `BASELINE`, `WRAP`
    /// and `MEMORY_HIGH`, the jail's in recipes/src/recipes/web.rs, are
    /// copied by hand and reviewed when any of them changes.
    #[test]
    fn page_admission_and_decoder_allowances_fit_the_jail() {
        let root = memory::root().unwrap();
        assert_eq!(root.limit(), memory::PAGES + 16 * Budget::metadata_bytes());
        let bodies = crate::app::MAX_WORKERS * crate::fetch::MAX_DOCUMENT as usize;
        assert!(145_000_000 + bodies + root.limit() + 20_000_000 < (640 << 20));
        const {
            assert!(memory::HTML_WORK >= 100 * HTML_LIMITS.input + HTML_LIMITS.output * 5 / 4);
            assert!(memory::TEXT_WORK >= TEXT_PAGE + MAX_TEXT + memory::SMALL_WORK);
            assert!(memory::PICTURE_WORK >= (192 << 20) + (1 << 20));
        }
        let row_overhead = 16 * MAX_PICTURE_AXIS;
        assert!(memory::PICTURE_WORK >= (16 << 20) + 4 * MAX_PICTURE_PIXELS + row_overhead);
        assert!(memory::PICTURE_WORK >= 12 * MAX_PICTURE_PIXELS + row_overhead + (1 << 20));
        assert!(
            memory::PICTURE_WORK
                >= 2 * crate::fetch::MAX_DOCUMENT as usize + 8 * MAX_PICTURE_PIXELS + row_overhead
        );
        assert!(memory::PICTURE_WORK >= 3 * crate::fetch::MAX_DOCUMENT as usize);
    }

    #[test]
    fn the_html_limits_fit_the_jail() {
        const BASELINE: usize = 145_000_000;
        const WRAP: usize = 24_000_000;
        const FITTED: usize = 3840 * 2160 * 4;
        const MEMORY_HIGH: usize = 640 << 20;
        let bodies = crate::app::MAX_WORKERS * crate::fetch::MAX_DOCUMENT as usize;
        // Decoded text in its body's place is no more than the body could be.
        assert!(MAX_MARKUP.max(MAX_TEXT) <= crate::fetch::MAX_DOCUMENT as usize);
        let rendering = 100 * HTML_LIMITS.input + HTML_LIMITS.output * 5 / 4;
        let decoding = MAX_PICTURE_PIXELS * (8 + 4);
        let page = HTML_LIMITS.output.max(TEXT_PAGE) + WRAP;
        let picture = MAX_PICTURE_PIXELS * 4 + FITTED;
        // A reply td-web does not show keeps its body for a save, beside a
        // page of a few lines; a save's own body is one of `bodies`.
        let held = crate::fetch::MAX_DOCUMENT as usize + 1_000_000;
        let most = BASELINE + bodies + rendering.max(decoding) + page.max(picture).max(held);
        assert!(most + 20_000_000 < MEMORY_HIGH, "{most}");
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
        // Text in another encoding has no byte of binary data; text in
        // UTF-16 says so by its byte order mark.
        let doc = Document::from_reply(base(), 200, None, b"caf\xe9\x1b".to_vec());
        assert_eq!(text_of(&cells(&doc, 20)), ["café"]);
        let doc = Document::from_reply(base(), 200, None, b"\xff\xfeo\x00k\x00".to_vec());
        assert_eq!(text_of(&cells(&doc, 20)), ["ok"]);
        let doc = Document::from_reply(base(), 200, None, b"a\x0bb".to_vec());
        assert!(text_of(&cells(&doc, 80))[0].contains("of no stated type"));
        // Only the resource header is read for binary data.
        let mut late = vec![b'a'; SNIFF_HEADER];
        late.push(0);
        let doc = Document::from_reply(base(), 200, None, late.clone());
        assert_eq!(text_of(&cells(&doc, 2000)), ["a".repeat(SNIFF_HEADER)]);
        late[SNIFF_HEADER - 1] = 0;
        let doc = Document::from_reply(base(), 200, None, late);
        assert!(text_of(&cells(&doc, 80))[0].contains("of no stated type"));
    }

    /// XML shown as text takes XML's rules: UTF-8, not windows-1252,
    /// when it has no label.
    #[test]
    fn xml_shown_as_text_defaults_to_utf8() {
        for media in ["text/xml", "text/vnd.example+xml"] {
            let (lines, note) = shown(media, b"<a>caf\xe9</a>");
            assert_eq!(
                (lines, note),
                (vec!["<a>caf\u{fffd}</a>".into()], None),
                "{media}"
            );
        }
        assert_eq!(
            shown("text/xml; charset=iso-8859-1", b"<a>caf\xe9</a>"),
            (vec!["<a>café</a>".into()], Some("windows-1252".into()))
        );
    }

    /// `body` answered as `content_type`: its lines' text and the note
    /// the status row shows.
    fn shown(content_type: &str, body: &[u8]) -> (Vec<String>, Option<String>) {
        let doc = Document::from_reply(base(), 200, Some(content_type), body.to_vec());
        (text_of(&cells(&doc, 80)), doc.note)
    }

    /// A page in another encoding is decoded by the Content-Type's
    /// charset, else a `<meta>` naming one, its `<title>` too, and the
    /// status row names the encoding.
    #[test]
    fn a_legacy_page_is_decoded_by_its_label_else_its_meta() {
        let page = b"<title>Caf\xe9</title><p>\x80 5, caf\xe9";
        let doc = Document::from_reply(
            base(),
            200,
            Some("text/html; charset=windows-1252"),
            page.to_vec(),
        );
        assert_eq!(doc.title.as_deref(), Some("Café"));
        assert_eq!(text_of(&cells(&doc, 80)), ["€ 5, café"]);
        assert_eq!(doc.note.as_deref(), Some("windows-1252"));
        assert_eq!(
            shown("text/html;charset=\"ISO-8859-1\"", page).0,
            ["€ 5, café"],
            "Latin-1 is windows-1252"
        );
        let (lines, note) = shown("text/html", b"<meta charset=windows-1252><p>caf\xe9 \x80");
        assert_eq!(
            (lines, note.as_deref()),
            (vec!["café €".into()], Some("windows-1252"))
        );
        let page = b"<head><meta http-equiv=\"Content-Type\" \
                     content=\"text/html; charset=iso-8859-15\">\
                     <title>\xa4uro</title></head><p>\xa4";
        let doc = Document::from_reply(base(), 200, Some("text/html"), page.to_vec());
        assert_eq!(doc.title.as_deref(), Some("€uro"));
        assert_eq!(text_of(&cells(&doc, 80)), ["€"]);
        assert_eq!(doc.note.as_deref(), Some("ISO-8859-15"));
    }

    #[test]
    fn shift_jis_and_gbk_pages_are_decoded() {
        let (lines, note) = shown(
            "text/html; charset=Shift_JIS",
            b"<p>\x93\xfa\x96{\x8c\xea \xb1",
        );
        assert_eq!(lines, ["日本語 ｱ"]);
        assert_eq!(note.as_deref(), Some("Shift_JIS"));
        let (lines, note) = shown("text/html", b"<meta charset=gb2312><p>\xd6\xd0\xce\xc4");
        assert_eq!(lines, ["中文"]);
        assert_eq!(note.as_deref(), Some("GBK"));
    }

    /// A byte order mark decides, over the label, and is not shown.
    #[test]
    fn a_byte_order_mark_wins_over_the_label() {
        let mut page = b"\xff\xfe".to_vec();
        for unit in "<p>é€</p>".encode_utf16() {
            page.extend(unit.to_le_bytes());
        }
        let (lines, note) = shown("text/html", &page);
        assert_eq!(
            (lines, note.as_deref()),
            (vec!["é€".into()], Some("UTF-16LE"))
        );
        let (lines, note) = shown(
            "text/html; charset=windows-1252",
            b"\xef\xbb\xbf<p>caf\xc3\xa9",
        );
        assert_eq!((lines, note), (vec!["café".into()], None));
    }

    /// Text in another encoding by its label; with none, UTF-8 when it is
    /// UTF-8, else windows-1252, as a page with no label or `<meta>`.
    #[test]
    fn text_is_decoded_by_its_label_else_falls_back() {
        let (lines, note) = shown(
            "text/plain; charset=iso-8859-2",
            b"Za\xbf\xf3\xb3\xe6 g\xea\xb6l\xb1 ja\xbc\xf1",
        );
        assert_eq!(lines, ["Zażółć gęślą jaźń"]);
        assert_eq!(note.as_deref(), Some("ISO-8859-2"));
        assert_eq!(
            shown("text/plain", b"caf\xc3\xa9"),
            (vec!["café".into()], None)
        );
        let windows = (vec!["café".into()], Some("windows-1252".into()));
        assert_eq!(shown("text/plain", b"caf\xe9"), windows);
        assert_eq!(shown("text/html", b"<p>caf\xe9"), windows);
        assert_eq!(shown("text/html", b"<p>caf\xc3\xa9").1, None);
        // XML's default is UTF-8, not windows-1252.
        assert_eq!(
            shown("application/xhtml+xml", b"<p>caf\xe9"),
            (vec!["caf\u{fffd}".into()], None)
        );
    }

    /// Malformed bytes are replacements and the page renders on.
    #[test]
    fn malformed_bytes_are_replaced_and_the_page_renders() {
        let (lines, _) = shown("text/html; charset=utf-8", b"<p>a\xffb</p><p>\xe2\x82 next");
        assert_eq!(lines, ["a\u{fffd}b", "", "\u{fffd} next"]);
        let (lines, note) = shown("text/html; charset=shift_jis", b"<p>x\x93\xff y\x93");
        assert_eq!(lines, ["x\u{fffd} y\u{fffd}"]);
        assert_eq!(note.as_deref(), Some("Shift_JIS"));
        let (lines, _) = shown("text/plain; charset=utf-16le", b"o\x00k\x00!");
        assert_eq!(lines, ["ok\u{fffd}"]);
    }

    /// The bounds are on the text decoded, in UTF-8: markup decoded three
    /// bytes a byte reaches `MAX_MARKUP` from a third of as many bytes,
    /// and td-html is handed no more, so its notice still means only its
    /// budget; text likewise at `MAX_TEXT`.
    #[test]
    fn a_page_that_expands_is_cut_at_its_bound_in_utf8() {
        let size = MAX_MARKUP / 2;
        let mut page = b"<pre>".to_vec();
        page.resize(size, 0x80);
        let doc = Document::from_reply(base(), 200, Some("text/html; charset=windows-1252"), page);
        let read = 5 + (MAX_MARKUP - 5) / 3;
        assert_eq!(
            doc.note.unwrap(),
            format!("windows-1252 · only the first {read} of the page's {size} bytes are shown")
        );
        let Body::Page(page) = &doc.body else {
            panic!("a page");
        };
        assert!(!page.unrendered, "td-html read it all");
        // Lines of 99 halfwidth katakana, each three bytes of UTF-8.
        let mut line = vec![0xb1; 99];
        line.push(b'\n');
        let text = line.repeat(MAX_TEXT / 100 + 1);
        let size = text.len();
        let doc = Document::from_reply(base(), 200, Some("text/plain; charset=shift_jis"), text);
        let (whole, rest) = (MAX_TEXT / 298, MAX_TEXT % 298 / 3);
        let read = whole * 100 + rest;
        assert_eq!(
            doc.note.unwrap(),
            format!("Shift_JIS · only the first {read} of the page's {size} bytes are shown")
        );
        let Body::Page(page) = &doc.body else {
            panic!("a page");
        };
        assert_eq!(page.lines.len(), whole + 1);
        assert_eq!(page.lines[whole].runs[0].text.chars().count(), rest);
    }

    #[test]
    fn another_type_is_named_and_not_shown() {
        let mut doc = Document::from_reply(base(), 200, Some("application/pdf"), vec![1; 10]);
        let lines = text_of(&cells(&doc, 200));
        assert_eq!(lines[0], "This is application/pdf, 10 bytes.");
        assert_eq!(lines[2], "Ctrl+S saves it in Downloads.");
        assert!(lines[4].contains("Firefox opens the rest"));
        assert_eq!(
            doc.take_held().as_deref().map(Vec::as_slice),
            Some(&[1u8; 10][..]),
            "kept for a save"
        );
        assert!(doc.take_held().is_none(), "given up once");
        // What the server answered without success is neither offered nor
        // kept; nor is a page td-web shows.
        let mut doc = Document::from_reply(base(), 404, Some("application/pdf"), vec![1; 10]);
        let lines = text_of(&cells(&doc, 200));
        assert!(lines[2].contains("Firefox opens the rest"), "{lines:?}");
        assert!(doc.take_held().is_none());
        let mut doc = Document::from_reply(base(), 200, Some("text/plain"), b"x".to_vec());
        assert!(doc.take_held().is_none());
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
        assert!(rendered.page.cut);
        assert!(!rendered.page.unrendered);
        // Markup td-html cuts short past its output budget says so in the
        // status row, not with td-html's notice as a line.
        let mut deep = b"<blockquote>".repeat(62);
        deep.extend(b"<p>x</p>".repeat(200_000));
        let page = html(&deep, None);
        assert!(page.unrendered && !page.cut);
        assert!(page.lines.len() > 5_000);
        assert!(page
            .lines
            .iter()
            .all(|line| line.runs.iter().all(|run| run.text != td_html::CUT_NOTICE)));
        // A page whose first block alone is past the budget is empty.
        let mut words = b"<p>".to_vec();
        words.extend(b"x ".repeat((MAX_MARKUP - 3) / 2));
        let page = html(&words, None);
        assert!(page.unrendered && page.lines.is_empty());
        // Markup's logical lines are bounded as text's are.
        let doc = html_doc("a<br>".repeat(MAX_LINES + 10).as_bytes());
        let Body::Page(page) = &doc.body else {
            panic!("a page");
        };
        assert_eq!(page.lines.len(), MAX_LINES);
        assert!(page.cut);
        // The lines they wrap to are not: each is laid when it is shown.
        let doc = Document::from_reply(
            base(),
            200,
            Some("text/plain"),
            "ab".repeat(MAX_LINES).into_bytes(),
        );
        let mut rendered = doc.render(1, TextFamily::Sans).unwrap();
        assert!(!rendered.page.cut);
        let last = rendered.last(&mut cell());
        assert_eq!(last.at, 2 * MAX_LINES - 1, "a glyph a line, all of them");
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

    /// A page of plain runs, a line of them each.
    fn of_runs(lines: &[&[&str]]) -> Page {
        let lines = lines
            .iter()
            .map(|runs| Logical {
                runs: runs
                    .iter()
                    .map(|text| Run {
                        text: text.to_string(),
                        style: Style::default(),
                        link: None,
                    })
                    .collect(),
                wrap: true,
            })
            .collect();
        Page {
            lines,
            ..Page::default()
        }
    }

    fn at(logical: usize, run: usize, at: usize) -> Place {
        Place { logical, run, at }
    }

    /// The text between two places is the logical lines', their runs
    /// joined and the lines by a newline, a line's own leading structure
    /// with it; none past the bound.
    #[test]
    fn the_text_between_two_places_joins_runs_and_lines() {
        let page = of_runs(&[&["> ", "alpha ", "beta"], &[], &["gamma"]]);
        let whole = "> alpha beta\n\ngamma";
        let end = page.line_end(2);
        assert_eq!(end, at(2, 0, 5));
        assert_eq!(page.line_end(1), at(1, 0, 0));
        assert_eq!(page.text(at(0, 0, 0), end, usize::MAX).unwrap(), whole);
        assert_eq!(page.text(at(0, 1, 2), at(0, 2, 2), 100).unwrap(), "pha be");
        assert_eq!(
            page.text(at(0, 2, 2), at(2, 0, 3), 100).unwrap(),
            "ta\n\ngam"
        );
        assert_eq!(page.text(at(0, 1, 6), at(0, 2, 0), 100).unwrap(), "");
        assert_eq!(page.text(at(0, 0, 0), end, whole.len()).unwrap(), whole);
        assert_eq!(page.text(at(0, 0, 0), end, whole.len() - 1), None);
        assert_eq!(page.text(at(0, 0, 0), end, 3), None);
        // A run longer than the bound is refused before it is copied.
        let long = "x".repeat(1 << 20);
        let page = of_runs(&[&["ab", &long]]);
        assert_eq!(page.text(at(0, 0, 0), page.line_end(0), 1000), None);
        let text = page
            .text(at(0, 0, 0), page.line_end(0), 2 + long.len())
            .unwrap();
        assert_eq!(
            (text.len(), text.capacity()),
            (2 + long.len(), 2 + long.len())
        );
        // Several runs, which a string grown run by run would hold with
        // room to spare (eight bytes, then sixteen).
        let page = of_runs(&[&["aaaaa", "bbbb"]]);
        let text = page.text(at(0, 0, 0), page.line_end(0), 100).unwrap();
        assert_eq!((text.as_str(), text.capacity()), ("aaaaabbbb", 9));
        // A quote's marks its continuations repeat are not its text.
        let doc = html_doc(b"<blockquote>alpha beta gamma delta epsilon</blockquote>");
        let laid = cells(&doc, 10);
        assert!(laid.lines.len() > 2, "{:?}", text_of(&laid));
        assert!(text_of(&laid)[1].starts_with('>'));
        let page = &laid.page;
        let last = page.lines.len() - 1;
        assert_eq!(
            page.text(Place::default(), page.line_end(last), 1000)
                .unwrap(),
            "> alpha beta gamma delta epsilon"
        );
    }

    /// Words, characters and logical lines a selection moves by, across
    /// runs, and at a line's ends across lines, an empty one included.
    #[test]
    fn words_characters_and_lines_cross_runs_and_lines() {
        let page = of_runs(&[&["foo", "-bar baz"], &[], &["qux"], &["ab", "cd"]]);
        assert_eq!(page.after_char(at(0, 0, 3)), at(0, 1, 1));
        assert_eq!(page.after_char(at(0, 1, 8)), at(1, 0, 0));
        assert_eq!(page.after_char(at(1, 0, 0)), at(2, 0, 0));
        assert_eq!(page.after_char(at(3, 1, 2)), at(3, 1, 2), "the page's end");
        assert_eq!(page.before_char(at(0, 1, 0)), at(0, 0, 2));
        assert_eq!(page.before_char(at(2, 0, 0)), at(1, 0, 0));
        assert_eq!(
            page.before_char(at(0, 0, 0)),
            at(0, 0, 0),
            "the page's start"
        );
        assert_eq!(page.word(at(0, 0, 1)), Some((at(0, 0, 0), at(0, 0, 3))));
        assert_eq!(page.word(at(0, 1, 0)), None, "no word's");
        assert_eq!(page.word(at(0, 1, 2)), Some((at(0, 1, 1), at(0, 1, 4))));
        assert_eq!(page.word(at(3, 1, 0)), Some((at(3, 0, 0), at(3, 1, 2))));
        assert_eq!(page.word(at(1, 0, 0)), None, "an empty line");
        assert_eq!(page.word_after(at(0, 0, 0)), at(0, 0, 3));
        assert_eq!(page.word_after(at(0, 0, 3)), at(0, 1, 4));
        assert_eq!(page.word_after(at(0, 1, 4)), at(0, 1, 8));
        assert_eq!(page.word_after(at(0, 1, 8)), at(1, 0, 0));
        assert_eq!(page.word_before(at(0, 1, 8)), at(0, 1, 5));
        assert_eq!(page.word_before(at(0, 1, 5)), at(0, 1, 1));
        assert_eq!(page.word_before(at(0, 1, 1)), at(0, 0, 0));
        assert_eq!(page.word_before(at(0, 0, 0)), at(0, 0, 0));
        assert_eq!(page.word_before(at(2, 0, 0)), at(1, 0, 0));
        assert_eq!(page.line_down(at(0, 0, 1)), at(0, 1, 8));
        assert_eq!(page.line_down(at(0, 1, 8)), at(1, 0, 0));
        assert_eq!(page.line_down(at(3, 1, 2)), at(3, 1, 2));
        assert_eq!(page.line_up(at(0, 1, 3)), at(0, 0, 0));
        assert_eq!(page.line_up(at(2, 0, 0)), at(1, 0, 0));
        assert_eq!(page.line_up(at(0, 0, 0)), at(0, 0, 0));
    }

    #[test]
    fn gif_mime_sniffing_and_errors_use_the_same_picture_path() {
        for version in [b"GIF87a", b"GIF89a"] {
            let mut gif = picture::tests::GIF.to_vec();
            gif[..6].copy_from_slice(version);
            for media in [
                None,
                Some("application/octet-stream"),
                Some("IMAGE/GIF; x=y"),
            ] {
                let doc = Document::from_reply(base(), 200, media, gif.clone());
                let fitted = doc.picture().unwrap().fit(2, 1).unwrap();
                assert_eq!(fitted.pixel(0, 0), Some([0, 0, 0, 0]));
                assert_eq!(fitted.pixel(1, 0), Some([0, 0, 255, 255]));
            }
        }
        let doc = Document::from_reply(
            base(),
            200,
            Some("application/pdf"),
            picture::tests::GIF.to_vec(),
        );
        assert!(doc.picture().is_none());
        let doc = Document::from_reply(base(), 200, Some("image/gif"), b"not GIF".to_vec());
        assert!(doc.picture().is_none());
        assert!(text_of(&cells(&doc, 80))[0].starts_with("The GIF could not be read"));
    }
    #[test]
    fn avif_navigation_uses_the_shared_decoder_and_admitted_still_subset() {
        let expected = Picture::new(6, 4, [232, 134, 109, 255].repeat(24)).unwrap();
        for mime in [
            Some("image/avif"),
            Some("IMAGE/AVIF; x=y"),
            None,
            Some("application/octet-stream"),
        ] {
            let doc = Document::from_reply(base(), 200, mime, picture::tests::AVIF.to_vec());
            assert_eq!(doc.picture().unwrap().fit(6, 4), expected.fit(6, 4));
        }
        let doc = Document::from_reply(
            base(),
            200,
            Some("application/pdf"),
            picture::tests::AVIF.to_vec(),
        );
        assert!(doc.picture().is_none());
        let doc = Document::from_reply(
            base(),
            200,
            Some("image/avif"),
            include_bytes!("../../td-avif/tests/fixtures/libavif.avif").to_vec(),
        );
        assert!(doc.picture().is_none());
        assert!(text_of(&cells(&doc, 80))[0].starts_with("The AVIF could not be decoded"));
    }

    #[test]
    fn webp_navigation_uses_mime_or_container_signature() {
        let expected = Picture::new(
            35,
            27,
            include_bytes!("../../td-webp/tests/fixtures/palette.rgba").to_vec(),
        )
        .unwrap();
        for mime in [
            Some("image/webp"),
            Some("IMAGE/WEBP; x=y"),
            None,
            Some("application/octet-stream"),
        ] {
            let doc = Document::from_reply(base(), 200, mime, picture::tests::WEBP.to_vec());
            assert_eq!(doc.picture().unwrap().fit(35, 27), expected.fit(35, 27));
        }
        let doc = Document::from_reply(
            base(),
            200,
            Some("application/pdf"),
            picture::tests::WEBP.to_vec(),
        );
        assert!(doc.picture().is_none());
    }
}
#[cfg(test)]
#[path = "../../td-ui/tests/fonts/mod.rs"]
#[allow(clippy::indexing_slicing)]
mod bench_fonts;
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod wrap_bench {
    use super::*;
    use crate::view::View;
    use std::time::Instant;
    use td_ui::prose::Faces;
    use td_ui::raster::Scale;
    fn faces() -> Faces {
        use super::bench_fonts::{Builder, Glyph, Segment};
        let mut b = Builder::new(vec![Glyph::Empty; 97]);
        b.format4 = vec![
            Segment::Delta(32, 126, 1u16.wrapping_sub(32)),
            Segment::Delta(0x301, 0x301, 96u16.wrapping_sub(0x301)),
        ];
        b.advances = (0..97)
            .map(|n| if n == 96 { 0 } else { 200 + (n % 7) * 100 })
            .collect();
        let bytes = b.font();
        Faces::new(
            std::array::from_fn(|_| bytes.clone()),
            std::array::from_fn(|_| bytes.clone()),
        )
        .unwrap()
    }
    #[test]
    #[ignore = "paired release performance measurement, TD_WRAP_BENCH selects fixture"]
    fn navigation() {
        let scenario = std::env::var("TD_WRAP_BENCH").unwrap_or_else(|_| "scroll".into());
        let faces = faces();
        let mut metrics = Metrics::new(Some(&faces), Scale::new(1).unwrap());
        let text = match scenario.as_str() {
            "marks" => format!(
                "<p>e{} word {}</p>",
                "\u{301}".repeat(50_000),
                "alphabet ".repeat(20_000)
            ),
            "runs" => format!(
                "<blockquote><ul>{}</ul></blockquote>",
                "<li>alpha <b>beta</b> <i>gamma</i> delta epsilon zeta</li>".repeat(3000)
            ),
            "unicode" => format!(
                "<p>{}</p>",
                (0x20..0x2800)
                    .filter_map(char::from_u32)
                    .collect::<String>()
                    .repeat(8)
            ),
            _ => format!(
                "<p>{}</p>",
                "alpha beta gamma delta epsilon zeta ".repeat(10_000)
            ),
        };
        let doc = Document::from_reply(
            Url::parse("https://bench.invalid/").unwrap(),
            200,
            Some("text/html"),
            text.into_bytes(),
        );
        let mut rendered = doc.render(640, TextFamily::Sans).unwrap();
        let mut view = View::default();
        let start = Instant::now();
        let mut checksum = 0u64;
        let mut retained = 0;
        for i in 0..if scenario == "resize" { 1200 } else { 12000 } {
            if scenario == "resize" {
                rendered = Rendered::default();
                std::hint::black_box(&rendered);
                rendered = doc.render(240 + (i % 31) * 17, TextFamily::Sans).unwrap();
                view.top = rendered.snap(&mut metrics, view.top);
            }
            view.scroll(
                if i % 200 < 100 { 7 } else { -7 },
                &mut rendered,
                &mut metrics,
                40,
            );
            retained = retained.max(rendered.page.budget().unwrap().used());
            for line in rendered.shown(view.top, 40) {
                checksum = checksum.wrapping_mul(31).wrapping_add(line.logical as u64);
                for glyph in &line.line.glyphs {
                    checksum = checksum
                        .wrapping_mul(31)
                        .wrapping_add(glyph.x as u64)
                        .wrapping_add(glyph.scalar as u64);
                }
            }
        }
        let elapsed = start.elapsed().as_nanos();
        let status = std::fs::read_to_string("/proc/self/status").unwrap();
        let peak = status
            .lines()
            .find_map(|l| l.strip_prefix("VmHWM:"))
            .unwrap()
            .trim();
        println!(
            "BENCH {scenario} ns={elapsed} checksum={checksum} retained={retained} peak={peak}"
        );
    }
}

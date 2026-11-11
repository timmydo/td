//! A response as td-web shows it (DESIGN.md §5): HTML rendered once
//! through td-html's rich output into logical lines, text as it is, a
//! PNG or JPEG decoded, anything else named and not shown, all of it on
//! the render thread; and the rendering, those lines wrapped by pixel
//! width into lines of styled pieces with link targets, on the window's
//! thread, a few at a time near the ones shown, for each width and body
//! face the page is shown at.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex, PoisonError};

use td_ui::picture::{Picture, MAX_PICTURE_AXIS, MAX_PICTURE_PIXELS};
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

impl Wrapped {
    /// Where it starts, as a place another wrap of the page finds.
    pub fn place(&self) -> Place {
        Place {
            logical: self.logical,
            run: self.start.0,
            at: self.start.1,
        }
    }

    /// Whether `piece` shows the page's text at the bytes it names,
    /// rather than marks its continuation repeats from the line's start.
    pub fn shows(&self, piece: &prose::Piece) -> bool {
        self.start == (0, 0) || piece.run >= self.start.0
    }
}

/// A page laid out at one width in one body family, as it is shown: only
/// the wrapped lines near those shown are laid and kept, a stretch of
/// consecutive ones from wherever the reader is, made again from the
/// logical lines when the reader moves past it (DESIGN.md §5). Every
/// place it answers is where one of its wrapped lines starts.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Rendered {
    pub page: Arc<Page>,
    /// The width in pixels it is wrapped at.
    pub width: u32,
    body: TextFamily,
    /// Consecutive wrapped lines of the page, in order.
    laid: VecDeque<Wrapped>,
    /// Where the wrapped line after the last laid starts; none past the
    /// page's end.
    after: Option<Place>,
    /// For the last `WALKED` logical lines walked, the last last, where
    /// every `STRIDE`th of its wrapped lines starts, from its first, as a
    /// walk along it found them: a walk back into it starts near where it
    /// is going, and one between two long lines walks neither again.
    stops: Vec<(usize, Vec<Place>)>,
}

impl Rendered {
    /// `page` to be laid into lines no wider than `width` pixels (at
    /// least one), its body text in `body`; a line that may not wrap is
    /// one line however wide. Nothing is laid yet.
    pub fn new(page: &Arc<Page>, width: u32, body: TextFamily) -> Self {
        Self {
            page: Arc::clone(page),
            width: width.max(1),
            body,
            ..Self::default()
        }
    }

    /// The laid line starting at `place`, by its index.
    fn index(&self, place: Place) -> Option<usize> {
        self.laid
            .binary_search_by(|wrapped| wrapped.place().cmp(&place))
            .ok()
    }

    /// Where the logical line after `logical` starts; none after the
    /// page's last.
    fn next_line(&self, logical: usize) -> Option<Place> {
        let next = logical.saturating_add(1);
        (next < self.page.lines.len()).then(|| Place::line(next))
    }

    /// The lines shown from `top`, at most `rows`, as laid: none when
    /// `top` is not (`settle` lays them).
    pub fn shown(&self, top: Place, rows: usize) -> impl Iterator<Item = &Wrapped> {
        let from = self.index(top).unwrap_or(self.laid.len());
        self.laid.range(from..).take(rows)
    }

    /// Where the reader is, as the status row says it, when `rows` lines
    /// from `top` do not show all of the page: the logical line the last
    /// of them is part of, counted from one. Read from the lines laid
    /// (`settle`), which reach a line past those shown where there is one.
    pub fn bottom(&self, top: Place, rows: usize) -> Option<usize> {
        let at = self.index(top)?;
        let rows = rows.max(1);
        let after = self.laid.len() - at;
        if top == Place::default() && self.after.is_none() && after <= rows {
            return None;
        }
        let last = self.laid.get(at + after.min(rows) - 1)?;
        Some(last.logical + 1)
    }

    /// The laid lines from `top` through the line after `rows` more,
    /// where the page has them, and none further than a screenful, or
    /// `STRIDE` lines, either side: the wrap held, however long the page.
    pub fn settle(&mut self, metrics: &mut Metrics, top: Place, rows: usize) {
        if self.page.lines.is_empty() {
            return;
        }
        let mut at = match self.index(top) {
            Some(at) => at,
            None => {
                self.reset(metrics, top);
                0
            }
        };
        while self.laid.len() <= at.saturating_add(rows) && self.grow(metrics) {}
        let margin = rows.max(STRIDE);
        while at > margin && self.laid.pop_front().is_some() {
            at -= 1;
        }
        while self.laid.len() > at.saturating_add(rows).saturating_add(margin) {
            if let Some(gone) = self.laid.pop_back() {
                self.after = Some(gone.place());
            }
        }
    }

    /// The laid lines from `top`, where a line starts, and no others.
    fn reset(&mut self, metrics: &mut Metrics, top: Place) {
        let (lines, next) = self.lay(metrics, top, STRIDE);
        self.laid = lines.into();
        self.after = next.or_else(|| self.next_line(top.logical));
    }

    /// Lays up to `STRIDE` lines after those laid; whether there were any.
    fn grow(&mut self, metrics: &mut Metrics) -> bool {
        let Some(from) = self.after.filter(|_| !self.laid.is_empty()) else {
            return false;
        };
        let (lines, next) = self.lay(metrics, from, STRIDE);
        if lines.is_empty() {
            self.after = None;
            return false;
        }
        self.laid.extend(lines);
        self.after = next.or_else(|| self.next_line(from.logical));
        true
    }

    /// Lays up to `STRIDE` lines before those laid; whether there were
    /// any.
    fn grow_back(&mut self, metrics: &mut Metrics) -> bool {
        let Some(first) = self.laid.front().map(Wrapped::place) else {
            return false;
        };
        let (lines, _) = if !first.starts_line() {
            self.walk(metrics, first.logical, Some(first), STRIDE)
        } else if first.logical > 0 {
            self.walk(metrics, first.logical - 1, None, STRIDE)
        } else {
            return false;
        };
        let grew = !lines.is_empty();
        for line in lines.into_iter().rev() {
            self.laid.push_front(line);
        }
        grew
    }

    /// Where the line `count` lines after the one starting at `top`
    /// starts, else the page's last line, and how many lines on that is.
    /// The lines passed are laid and kept until `settle`, so a caller
    /// moves a screenful or so at a time.
    pub fn forward(&mut self, metrics: &mut Metrics, top: Place, count: usize) -> (Place, usize) {
        let (mut at, mut moved) = (top, 0);
        if self.page.lines.is_empty() || count == 0 {
            return (at, moved);
        }
        loop {
            let index = match self.index(at) {
                Some(index) => index,
                None => {
                    self.reset(metrics, at);
                    0
                }
            };
            let step = (count - moved).min(self.laid.len().saturating_sub(index + 1));
            if let Some(line) = self.laid.get(index + step) {
                at = line.place();
            }
            moved += step;
            if moved == count || !self.grow(metrics) {
                return (at, moved);
            }
        }
    }

    /// Where the line `count` lines before the one starting at `top`
    /// starts, else the page's first, and how many lines back that is.
    pub fn back(&mut self, metrics: &mut Metrics, top: Place, count: usize) -> (Place, usize) {
        let (mut at, mut moved) = (top, 0);
        if self.page.lines.is_empty() || count == 0 {
            return (at, moved);
        }
        loop {
            let index = match self.index(at) {
                Some(index) => index,
                None => {
                    self.reset(metrics, at);
                    0
                }
            };
            let step = (count - moved).min(index);
            if let Some(line) = self.laid.get(index - step) {
                at = line.place();
            }
            moved += step;
            if moved == count || !self.grow_back(metrics) {
                return (at, moved);
            }
        }
    }

    /// Where the page's last line starts, laying it and those before it
    /// back from the last logical line, none of the rest.
    pub fn last(&mut self, metrics: &mut Metrics) -> Place {
        if self.after.is_none() {
            if let Some(last) = self.laid.back() {
                return last.place();
            }
        }
        let Some(logical) = self.page.lines.len().checked_sub(1) else {
            return Place::default();
        };
        let (lines, _) = self.walk(metrics, logical, None, STRIDE);
        self.laid = lines;
        self.after = None;
        self.laid
            .back()
            .map_or(Place::line(logical), Wrapped::place)
    }

    /// Where the line that shows `place` starts: the last that starts at
    /// or before it, else the first; a place past the page's end is on
    /// its last line. Found among those laid, else by laying its logical
    /// line up to it.
    pub fn snap(&mut self, metrics: &mut Metrics, place: Place) -> Place {
        if place.logical >= self.page.lines.len() {
            return self.last(metrics);
        }
        let at = self
            .laid
            .partition_point(|wrapped| wrapped.place() <= place);
        if let Some(line) = at.checked_sub(1).and_then(|at| self.laid.get(at)) {
            let next = self.laid.get(at).map(Wrapped::place).or(self.after);
            if next.is_none_or(|next| next > place) {
                return line.place();
            }
        }
        let (lines, _) = self.walk(metrics, place.logical, Some(place.after()), 1);
        lines
            .back()
            .map_or(Place::line(place.logical), Wrapped::place)
    }

    /// Lays logical line `logical` from its start, or from the nearest
    /// start a walk along it remembered, keeping the last `keep` lines
    /// that start before `before` (to its end when none) and remembering
    /// where every `STRIDE`th starts; and where the line after those kept
    /// starts, when it is in this logical line.
    fn walk(
        &mut self,
        metrics: &mut Metrics,
        logical: usize,
        before: Option<Place>,
        keep: usize,
    ) -> (VecDeque<Wrapped>, Option<Place>) {
        let mut starts = match self.stops.iter().position(|(line, _)| *line == logical) {
            Some(at) => self.stops.remove(at).1,
            None => Vec::new(),
        };
        if starts.is_empty() {
            starts.push(Place::line(logical));
        }
        let walked = self.walk_from(metrics, before, keep, &mut starts);
        if self.stops.len() == WALKED {
            self.stops.remove(0);
        }
        self.stops.push((logical, starts));
        walked
    }

    /// `walk` along the logical line whose remembered starts are
    /// `starts`, the first its own start, remembering more as it finds
    /// them.
    fn walk_from(
        &self,
        metrics: &mut Metrics,
        before: Option<Place>,
        keep: usize,
        starts: &mut Vec<Place>,
    ) -> (VecDeque<Wrapped>, Option<Place>) {
        // The last remembered start before where it is going.
        let mut stop = match before {
            Some(before) => starts.partition_point(|&start| start < before),
            None => starts.len(),
        }
        .saturating_sub(1);
        let mut kept = VecDeque::new();
        let Some(&first) = starts.get(stop) else {
            return (kept, None);
        };
        let mut from = first;
        loop {
            let (lines, next) = self.lay(metrics, from, STRIDE);
            for line in lines {
                if before.is_some_and(|before| line.place() >= before) {
                    return (kept, Some(line.place()));
                }
                if kept.len() == keep {
                    kept.pop_front();
                }
                kept.push_back(line);
            }
            let Some(next) = next else {
                return (kept, None);
            };
            stop += 1;
            if stop == starts.len() {
                starts.push(next);
            }
            from = next;
        }
    }

    /// Calls `found` with each character of logical line `logical` that
    /// find searches, as it is, and where it is, until `found` says to
    /// stop: all of a line that wraps, and only what is laid of one that
    /// does not, the characters whose pens fall before the pane's edge and
    /// `prose::OVERHANG` past it, as `within` lays them.
    pub fn searched(
        &self,
        metrics: &mut Metrics,
        logical: usize,
        mut found: impl FnMut(char, Place) -> bool,
    ) {
        let Some(line) = self.page.lines.get(logical) else {
            return;
        };
        let edge = (!line.wrap).then(|| self.width.saturating_add(prose::OVERHANG));
        let mut pen = 0u64;
        for (run, part) in line.runs.iter().enumerate() {
            let style = part.style.glyph(self.body);
            for (at, scalar) in part.text.char_indices() {
                if let Some(edge) = edge {
                    if pixels(pen) >= edge {
                        return;
                    }
                    let shown = prose::shown(scalar);
                    let advance = metrics.advance(style.family, style.font_style, shown);
                    pen = pen.saturating_add(u64::from(advance));
                }
                if !found(scalar, Place { logical, run, at }) {
                    return;
                }
            }
        }
    }

    /// The lines of logical line `from.logical` from `from`, where one of
    /// its lines starts, at most `most`, and where its next line starts,
    /// none after its last. A line that may not wrap is one line, laid
    /// only as far as the pane shows it. One that starts with leading
    /// structure (`leading`) hangs: its text after that structure is
    /// wrapped at the width less the structure's, every line of it set
    /// that far in, the first after the whole structure and each
    /// continuation after its marks alone; a structure wider than half the
    /// width does not hang, so the text keeps room. The text is handed to
    /// `prose` a slice at a time (`slice`): every line but a slice's last
    /// is as the whole line would wrap, and the wrap starts again at the
    /// last. A slice of `MAX_ROW_BYTES` holding no whole line is one line,
    /// and the next starts at the first character after it that is no
    /// space, as at any break: when spaces, which a line never breaks at,
    /// overflowed it, that is the line a wrap of the whole would make, and
    /// a line of spaces alone is none. When the slice starts with spaces
    /// its wrap drops, a line of their own, the wrap starts again after
    /// them, as the whole wrap does, so a word past the slice is not cut.
    /// Only characters of no advance end a line there that the whole wrap
    /// would not.
    fn lay(
        &self,
        metrics: &mut Metrics,
        from: Place,
        most: usize,
    ) -> (Vec<Wrapped>, Option<Place>) {
        let mut out = Vec::new();
        let Some(logical) = self.page.lines.get(from.logical) else {
            return (out, None);
        };
        let line = |line: prose::Line, start: (usize, usize)| Wrapped {
            logical: from.logical,
            start,
            line,
        };
        if !logical.wrap {
            let runs: Vec<prose::Run> = logical
                .runs
                .iter()
                .map(|run| run.prose(self.body, &run.text))
                .collect();
            let shown = within(metrics, &runs, self.width.saturating_add(prose::OVERHANG));
            let mut laid = prose::wrap_at_most(metrics, &shown, u32::MAX, 1)
                .pop()
                .unwrap_or_default();
            laid.pieces.shrink_to_fit();
            laid.glyphs.shrink_to_fit();
            out.push(line(laid, (0, 0)));
            return (out, None);
        }
        let hang = Hang::of(metrics, logical, self.body, self.width);
        let (base, width) = match &hang {
            Some(hang) => (hang.first, self.width - hang.width),
            None => (0, self.width),
        };
        let mut first = from.starts_line();
        let mut at = if first {
            (base, 0)
        } else {
            (from.run, from.at).max((base, 0))
        };
        let mut budget = FIRST_SLICE;
        while out.len() < most {
            let want = most - out.len();
            let (runs, cut) = slice(logical, self.body, at, budget);
            let lines = prose::wrap_at_most(metrics, &runs, width, want + 1);
            let whole = match cut {
                None => lines.len(),
                Some(_) if lines.len() > want => want + 1,
                Some(_) => lines.len().saturating_sub(1),
            };
            if whole == 0 {
                if budget < MAX_ROW_BYTES {
                    budget = budget.saturating_mul(2).min(MAX_ROW_BYTES);
                    continue;
                }
                // A slice's text that holds no break is one line, but for
                // spaces alone, which a break drops.
                let laid = lines.into_iter().next().unwrap_or_default();
                // Spaces a slice starts with that its wrap dropped, a line
                // of their own, leave a wrap starting afresh after them,
                // whose line may hold more than this slice.
                let dropped = first_scalar(logical, at).is_some_and(|c| prose::shown(c) == ' ')
                    && laid
                        .glyphs
                        .first()
                        .is_some_and(|glyph| prose::shown(glyph.scalar) != ' ');
                if let Some(past) = past_spaces(logical, at).filter(|_| dropped) {
                    at = past;
                    continue;
                }
                let next = cut.and_then(|cut| past_spaces(logical, cut));
                if !laid.glyphs.is_empty() || (next.is_none() && out.is_empty()) {
                    let start = if first { (0, 0) } else { at };
                    out.push(line(
                        hung(laid, at, hang.as_ref(), first, logical, self.body),
                        start,
                    ));
                    first = false;
                }
                match next {
                    Some(next) => at = next,
                    None => return (out, None),
                }
                continue;
            }
            for (number, laid) in lines.into_iter().enumerate() {
                let start = if number == 0 { at } else { started(&laid, at) };
                if number == whole {
                    // A slice's last line, which another slice lays.
                    at = start;
                    break;
                }
                if out.len() == most {
                    let next = Place {
                        logical: from.logical,
                        run: start.0,
                        at: start.1,
                    };
                    return (out, Some(next));
                }
                let shown = if first { (0, 0) } else { start };
                out.push(line(
                    hung(laid, at, hang.as_ref(), first, logical, self.body),
                    shown,
                ));
                first = false;
            }
            if cut.is_none() {
                return (out, None);
            }
        }
        let next = Place {
            logical: from.logical,
            run: at.0,
            at: at.1,
        };
        (out, Some(next))
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

/// The start of `runs` that an unwrapped line shows at `edge` pixels: the
/// characters whose pens, laid as prose lays them, fall before it. A pane
/// never shows the rest, so it is neither laid nor kept; each run keeps
/// its offsets, so pieces still name their run's text.
fn within<'r>(metrics: &mut Metrics, runs: &[prose::Run<'r>], edge: u32) -> Vec<prose::Run<'r>> {
    let mut pen = 0u64;
    let mut shown = Vec::new();
    for run in runs {
        let mut end = run.text.len();
        for (at, scalar) in run.text.char_indices() {
            if pixels(pen) >= edge {
                end = at;
                break;
            }
            let advance =
                metrics.advance(run.style.family, run.style.font_style, prose::shown(scalar));
            pen = pen.saturating_add(u64::from(advance));
        }
        shown.push(prose::Run {
            text: run.text.get(..end).unwrap_or_default(),
            ..*run
        });
        if end < run.text.len() {
            break;
        }
    }
    shown
}

/// Where a line of a slice's wrap starts, as the logical line has it: a
/// slice starting at `at` holds its run from that byte on, and the runs
/// after it whole. A line holding nothing starts where the slice does.
fn started(line: &prose::Line, at: (usize, usize)) -> (usize, usize) {
    match line.pieces.first() {
        Some(piece) if piece.run == 0 => (at.0, at.1 + piece.text.start),
        Some(piece) => (at.0 + piece.run, piece.text.start),
        None => at,
    }
}

/// The first character of `logical` at or after `at`, past empty runs.
fn first_scalar(logical: &Logical, at: (usize, usize)) -> Option<char> {
    logical
        .runs
        .iter()
        .enumerate()
        .skip(at.0)
        .find_map(|(index, run)| {
            let from = if index == at.0 { at.1 } else { 0 };
            run.text.get(from..).and_then(|text| text.chars().next())
        })
}

/// Where the first character of `logical` at or after `at` that is no
/// space is, as `prose` takes a tab for one; none when only spaces are
/// left.
fn past_spaces(logical: &Logical, at: (usize, usize)) -> Option<(usize, usize)> {
    for (index, run) in logical.runs.iter().enumerate().skip(at.0) {
        let from = if index == at.0 { at.1 } else { 0 };
        let text = run.text.get(from..).unwrap_or_default();
        if let Some((byte, _)) = text.char_indices().find(|&(_, c)| prose::shown(c) != ' ') {
            return Some((index, from + byte));
        }
    }
    None
}

/// The runs of `logical` from `at` as prose measures them, holding at most
/// `budget` bytes of text with one more for each run, and where they stop
/// when that is before the logical line's end: within a run, at a
/// character, so a slice holds at least one character of the first.
fn slice(
    logical: &Logical,
    body: TextFamily,
    at: (usize, usize),
    budget: usize,
) -> (Vec<prose::Run<'_>>, Option<(usize, usize)>) {
    let mut runs = Vec::new();
    let mut spent = 0usize;
    for (index, run) in logical.runs.iter().enumerate().skip(at.0) {
        let from = if index == at.0 { at.1 } else { 0 };
        let text = run.text.get(from..).unwrap_or_default();
        let room = budget.saturating_sub(spent.saturating_add(1));
        if spent >= budget || text.len() > room {
            let mut end = room.min(text.len());
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            if end > 0 {
                runs.push(run.prose(body, text.get(..end).unwrap_or_default()));
            }
            return (runs, Some((index, from + end)));
        }
        runs.push(run.prose(body, text));
        spent += text.len() + 1;
    }
    (runs, None)
}

/// What a logical line's lines hang from (`leading`): the first run after
/// its leading structure, that structure's width in pixels, and each of
/// its characters with its pen and advance in `prose::UNIT`s.
struct Hang {
    first: usize,
    width: u32,
    placed: Vec<(Lead, u64, u64)>,
}

impl Hang {
    /// How `logical`'s lines hang at `width` pixels, its body text in
    /// `body`; none when it starts with no structure, holds nothing after
    /// it, or the structure is wider than half the width.
    fn of(metrics: &mut Metrics, logical: &Logical, body: TextFamily, width: u32) -> Option<Self> {
        let (lead, first) = leading(&logical.runs);
        let rest = logical.runs.get(first..).unwrap_or_default();
        if lead.is_empty() || rest.iter().all(|run| run.text.is_empty()) {
            return None;
        }
        let mut pen = 0u64;
        let mut placed = Vec::with_capacity(lead.len());
        for one in lead {
            let style = logical.runs.get(one.run)?.style.glyph(body);
            let shown = prose::shown(one.scalar);
            let advance = u64::from(metrics.advance(style.family, style.font_style, shown));
            placed.push((one, pen, advance));
            pen = pen.saturating_add(advance);
        }
        let hang = pixels(pen);
        (hang.saturating_mul(2) <= width).then_some(Self {
            first,
            width: hang,
            placed,
        })
    }
}

/// `line`, laid from a slice of `logical` starting at `at`, as the logical
/// line has it: its pieces naming the line's runs and bytes, and, when it
/// hangs, set in by the hang after its structure, the first line's
/// whole, a continuation's marks alone, as one piece a run with the text
/// between them left out.
fn hung(
    line: prose::Line,
    at: (usize, usize),
    hang: Option<&Hang>,
    first: bool,
    logical: &Logical,
    body: TextFamily,
) -> prose::Line {
    let placed = hang.map_or(&[][..], |hang| &hang.placed);
    let shift = hang.map_or(0, |hang| hang.width);
    let marks = placed
        .iter()
        .filter(|(one, _, _)| first || one.mark)
        .count();
    let mut laid = prose::Line {
        width: 0,
        pieces: Vec::with_capacity(line.pieces.len() + marks),
        glyphs: Vec::with_capacity(line.glyphs.len() + marks),
    };
    for &(one, pen, advance) in placed {
        if !first && !one.mark {
            continue;
        }
        let Some(run) = logical.runs.get(one.run) else {
            continue;
        };
        let x = pixels(pen);
        let right = pixels(pen.saturating_add(advance));
        let glyph = laid.glyphs.len();
        let end = one.at + one.scalar.len_utf8();
        match laid.pieces.last_mut() {
            Some(piece) if piece.run == one.run => {
                piece.text.end = end;
                piece.right = piece.right.max(right);
                piece.glyphs.end = glyph + 1;
            }
            _ => laid.pieces.push(prose::Piece {
                run: one.run,
                tag: run.link.unwrap_or(NO_LINK),
                style: run.style.glyph(body),
                text: one.at..end,
                left: x,
                right,
                glyphs: glyph..glyph + 1,
            }),
        }
        laid.glyphs.push(prose::Placed {
            x,
            scalar: prose::shown(one.scalar),
        });
        laid.width = laid.width.max(right);
    }
    let base = laid.glyphs.len();
    for piece in line.pieces {
        let text = if piece.run == 0 {
            piece.text.start + at.1..piece.text.end + at.1
        } else {
            piece.text
        };
        laid.pieces.push(prose::Piece {
            run: piece.run + at.0,
            text,
            left: piece.left.saturating_add(shift),
            right: piece.right.saturating_add(shift),
            glyphs: piece.glyphs.start + base..piece.glyphs.end + base,
            ..piece
        });
    }
    laid.glyphs
        .extend(line.glyphs.iter().map(|glyph| prose::Placed {
            x: glyph.x.saturating_add(shift),
            ..*glyph
        }));
    if !line.glyphs.is_empty() || hang.is_none() {
        laid.width = laid.width.max(line.width.saturating_add(shift));
    }
    laid
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
    /// before markup is rendered or a picture decoded. td-web calls this
    /// only on its `Renderer`'s thread, which takes each reply when the
    /// one before it is done, so a navigation stopped or overtaken while
    /// it waited does no such work.
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
        let size = body.len();
        let cut = |most: usize| {
            (size > most)
                .then(|| format!("only the first {most} of the page's {size} bytes are shown"))
        };
        let base = Some(url.clone());
        let (page, title, note) = match kind {
            Kind::Html => {
                // Cut here, not by td-html's input limit, so td-html's
                // notice means only its budget: the note below says how
                // much of the page was read.
                let shown = body.get(..HTML_LIMITS.input).unwrap_or(body);
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

    /// The document's lines to be wrapped at `width` pixels (at least
    /// one), its body text in `body`, as they are shown; none for a
    /// picture.
    pub fn render(&self, width: u32, body: TextFamily) -> Rendered {
        match &self.body {
            Body::Page(page) => Rendered::new(page, width, body),
            Body::Picture(_) => Rendered::default(),
        }
    }
}

/// Work for the render thread, which sends its own answer.
type Job = Box<dyn FnOnce() + Send>;

/// The one thread every reply is made into a document on, one at a time,
/// whichever fetch thread hands it over (DESIGN.md §3). A rendering
/// within `HTML_LIMITS` can peak at some 240 MB, and the allocator keeps
/// much of that in the arena of the thread that rendered it, which a
/// thread alive beside it does not reuse: renderings on overlapping fetch
/// threads kept it once per thread. A job that panics answers nothing and
/// the thread goes on, td-web's build unwinding; a thread found gone is
/// started again for the next job.
#[derive(Clone)]
pub struct Renderer(Arc<Mutex<mpsc::Sender<Job>>>);

/// Starts a render thread, which runs what it is sent in turn until every
/// sender is gone.
fn render_thread() -> Result<mpsc::Sender<Job>, String> {
    let (send, jobs) = mpsc::channel::<Job>();
    std::thread::Builder::new()
        .name("td-web render".into())
        .spawn(move || {
            for job in jobs {
                job();
            }
        })
        .map_err(|why| format!("the render thread could not start: {why}"))?;
    Ok(send)
}

impl Renderer {
    /// A renderer and its thread, or why the thread could not start.
    pub fn start() -> Result<Self, String> {
        Ok(Self(Arc::new(Mutex::new(render_thread()?))))
    }

    /// `work`'s answer, made on the render thread behind the work handed
    /// over before it; none when it panicked or no thread would take it.
    fn run<T: Send + 'static>(&self, work: impl FnOnce() -> T + Send + 'static) -> Option<T> {
        let (answer, done) = mpsc::channel();
        let job: Job = Box::new(move || {
            let made = std::panic::catch_unwind(std::panic::AssertUnwindSafe(work));
            let _ = answer.send(made.ok());
        });
        {
            let mut thread = self.0.lock().unwrap_or_else(PoisonError::into_inner);
            if let Err(mpsc::SendError(job)) = thread.send(job) {
                *thread = render_thread().ok()?;
                thread.send(job).ok()?;
            }
        }
        done.recv().ok().flatten()
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
        let (shown, kind) = (url.clone(), content_type.map(str::to_string));
        let stopped = Arc::clone(stopped);
        self.run(move || {
            Document::from_reply_unless(url, status, kind.as_deref(), body, &stopped)
                .filter(|_| !stopped.load(Ordering::SeqCst))
        })
        .unwrap_or_else(|| {
            Some(Document::error(
                Some(shown),
                "the page could not be rendered",
            ))
        })
    }
}

// `Renderer::run` catches a rendering's panic, which only an unwinding
// build can: a build that aborts on panic (`panic = "abort"` in a profile,
// `-C panic=abort` from a recipe) would end td-web on any rendering's
// panic, so it does not build.
#[cfg(not(panic = "unwind"))]
compile_error!("td-web's render thread catches a rendering's panic: its build must unwind");

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
        line.shrink_to_fit();
        page.lines.push(Logical {
            runs: vec![Run {
                text: line,
                style,
                link: None,
            }],
            wrap: true,
        });
    }
    page.lines.shrink_to_fit();
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
pub(crate) mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
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
        pub page: Arc<Page>,
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
    pub(crate) fn laid(rendered: &mut Rendered) -> &mut VecDeque<Wrapped> {
        &mut rendered.laid
    }

    /// `doc` wrapped `columns` cells wide in Sans, every line laid.
    fn cells(doc: &Document, columns: u32) -> Laid {
        Laid::of(
            doc.render(columns * CELL_WIDTH as u32, TextFamily::Sans),
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
            let mut rendered = Rendered::new(&laid.page, laid.width, TextFamily::Sans);
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
        let mut rendered = doc.render(1, TextFamily::Sans);
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
        let mut rendered = doc.render(1, TextFamily::Sans);
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
        let page = Arc::new(Page {
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
        let rendered = Rendered::new(&page, u32::MAX / 2, TextFamily::Sans);
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
            let (again, _) = rendered.lay(&mut cell(), line.place(), 1);
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
                    whole.extend(prose::wrap(metrics, &runs, width));
                }
                let laid = cells(&doc, columns);
                let lines: Vec<prose::Line> = laid.lines.iter().map(|w| w.line.clone()).collect();
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
        // Text is made though its navigation stopped, then dropped there.
        let body = b"x".to_vec();
        let doc = renderer.document(base(), 200, Some("text/plain"), body, &stopped);
        assert!(doc.is_none());
    }

    /// All of a renderer's work runs on its one thread, whichever thread
    /// hands it over: not the caller's, and not a thread per job.
    #[test]
    fn a_renderer_s_work_runs_on_its_one_thread() {
        let renderer = Renderer::start().unwrap();
        let here = std::thread::current().id();
        let threads: Vec<_> = (0..4)
            .map(|_| {
                let renderer = renderer.clone();
                std::thread::spawn(move || {
                    (0..3)
                        .map(|_| renderer.run(|| std::thread::current().id()).unwrap())
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        let ids: Vec<_> = threads
            .into_iter()
            .flat_map(|thread| thread.join().unwrap())
            .collect();
        assert_eq!(ids.len(), 12);
        assert!(ids.iter().all(|&id| id == ids[0] && id != here));
    }

    /// A renderer whose thread is gone.
    fn gone() -> Renderer {
        let (send, jobs) = mpsc::channel::<Job>();
        drop(jobs);
        Renderer(Arc::new(Mutex::new(send)))
    }

    /// A job that panics answers nothing, a reply's an error page, and the
    /// same thread takes the next; a renderer whose thread is gone starts
    /// another rather than failing every page after. A test build always
    /// unwinds; what holds td-web's own build to it is the
    /// `compile_error!` beside `Renderer`, which a build that aborts on
    /// panic meets.
    #[test]
    fn a_renderer_outlives_a_panic_and_a_lost_thread() {
        let renderer = Renderer::start().unwrap();
        let before = renderer.run(|| std::thread::current().id()).unwrap();
        assert_eq!(
            renderer.run(|| -> u8 { panic!("a rendering panics") }),
            None
        );
        assert_eq!(renderer.run(|| std::thread::current().id()), Some(before));
        let going = Arc::new(AtomicBool::new(false));
        let gone = gone();
        let body = b"<p>still here".to_vec();
        let doc = gone.document(base(), 200, Some("text/html"), body, &going);
        assert_eq!(text_of(&cells(&doc.unwrap(), 80)), ["still here"]);
        assert!(gone.run(|| 1).is_some(), "the new thread stays");
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

    /// The most a text page holds: its text, a tab of it eight spaces and
    /// any other byte at most three (a byte not UTF-8 is U+FFFD), and for
    /// each of its lines its `Logical`, a one-run `Vec` and what a chunk
    /// rounds its text up by, 120 bytes in all.
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
    /// out (`MAX_WORKERS` of `MAX_DOCUMENT`); on the render thread either
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
    fn the_html_limits_fit_the_jail() {
        const BASELINE: usize = 145_000_000;
        const WRAP: usize = 24_000_000;
        const FITTED: usize = 3840 * 2160 * 4;
        const MEMORY_HIGH: usize = 640 << 20;
        let bodies = crate::app::MAX_WORKERS * crate::fetch::MAX_DOCUMENT as usize;
        let rendering = 100 * HTML_LIMITS.input + HTML_LIMITS.output * 5 / 4;
        let decoding = MAX_PICTURE_PIXELS * (8 + 4);
        let page = HTML_LIMITS.output.max(TEXT_PAGE) + WRAP;
        let picture = MAX_PICTURE_PIXELS * 4 + FITTED;
        let most = BASELINE + bodies + rendering.max(decoding) + page.max(picture);
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
        let mut rendered = doc.render(1, TextFamily::Sans);
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
}

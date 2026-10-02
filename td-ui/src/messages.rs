//! A chat transcript: a scrolling list of messages, each a header over
//! wrapped sections of caller-supplied text, with collapsing, text
//! selection across messages and whole-message copy (see DESIGN.md,
//! "Shared message list"). The controller owns the messages and their
//! layout; adding or changing a message allocates fallibly under fixed
//! budgets, and input and painting allocate nothing but the text a copy
//! hands the clipboard. Nothing here reads a clock, a file or the
//! environment: the caller passes the clock with each press.

use std::sync::Arc;

use crate::chrome::{Button, BUTTON_MARGIN, ROW, SELECTED_ROW};
use crate::raster::{
    text_run, Draw, GlyphStyle, Primitive, Rect, Scrollbar, Surface, BORDER, CHROME,
    INACTIVE_SELECTION, INK, LINE_NUMBER, MISSPELLED, PAPER, SELECTED,
};
use crate::window::{Clipboard, Refusal};
use crate::{CELL_HEIGHT, CELL_WIDTH};

/// The messages a list holds.
pub const MAX_MESSAGES: usize = 65_536;
/// The sections one message holds.
pub const MAX_SECTIONS: usize = 16;
/// A section's text, and a message's copy source, in bytes: the
/// clipboard's ceiling.
pub const MAX_TEXT_BYTES: usize = crate::clipboard::MAX_BYTES;
/// Every label, title and text a list holds together, in bytes.
pub const MAX_TOTAL_BYTES: usize = 16 * 1024 * 1024;
/// A header label, a status, a verdict or a section title, in bytes.
pub const MAX_LABEL_BYTES: usize = 128;
/// The rows a layout may hold, chrome rows included.
pub const MAX_LINES: usize = 1 << 20;
/// The text rows an excerpt section shows.
pub const EXCERPT_ROWS: usize = 8;
/// The text columns a layout needs, besides a cell each side.
pub const MIN_COLUMNS: usize = 16;
/// The space below each message, in font pixels.
pub const GAP: usize = 8;
/// The longest wait between a click's release and the next press that
/// makes the two a double click.
pub const MULTI_CLICK_MS: u64 = 500;
/// The copy button's caption.
pub const COPY_LABEL: &str = "Copy";
/// What an excerpt cut short shows below its last row.
pub const MORE: &str = "\u{2026} excerpt; Copy takes the whole";

/// The scrollbar gutter and its track, in font pixels, as `chrome::List`'s.
const GUTTER: usize = 16;
const TRACK: usize = 12;
/// A double click's second press lies within this many font pixels of the
/// first's release on each axis.
const NEAR: i64 = 4;
const OPEN: char = '\u{25be}';
const SHUT: char = '\u{25b8}';

/// Why a message, an edit or a geometry was refused.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    /// A label, status, verdict or title is empty.
    Empty,
    /// A label, status, verdict or title holds a control character.
    Control,
    /// A bound above was reached, or an allocation failed.
    Limit,
    /// No message at that index.
    NoMessage,
    /// No section at that index.
    NoSection,
    /// The surface is past the raster's ceilings.
    InvalidSurface,
    /// The rectangle lies outside the surface.
    InvalidRect,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Empty => "empty message label",
            Self::Control => "control character in a message label",
            Self::Limit => "message list limit",
            Self::NoMessage => "no such message",
            Self::NoSection => "no such message section",
            Self::InvalidSurface => "invalid message list surface",
            Self::InvalidRect => "message list rectangle lies outside the surface",
        })
    }
}

impl std::error::Error for Error {}

/// How a status or a verdict reads: its ink, and a verdict's mark.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Tone {
    Neutral,
    Good,
    Bad,
}

impl Tone {
    fn ink(self) -> u32 {
        match self {
            Self::Neutral => LINE_NUMBER,
            Self::Good => SELECTED,
            Self::Bad => MISSPELLED,
        }
    }
    fn mark(self) -> char {
        match self {
            Self::Neutral => '\u{2022}',
            Self::Good => '\u{2713}',
            Self::Bad => '\u{2717}',
        }
    }
}

#[derive(Clone, Debug)]
struct Section {
    title: Option<String>,
    text: String,
    collapsed: bool,
    excerpt: bool,
}

/// One message: a header label (its role or source), an optional status
/// and verdict shown in the header, and its sections in order. A section
/// is untitled body text, or titled, which collapses to its title row; an
/// excerpt section shows at most `EXCERPT_ROWS` rows. A tool block is a
/// message whose sections are its arguments and the excerpt of its
/// result, with the whole result as its `source`.
#[derive(Clone, Debug)]
pub struct Message {
    label: String,
    status: Option<(String, Tone)>,
    verdict: Option<(String, Tone)>,
    sections: Vec<Section>,
    source: Option<Arc<str>>,
    collapsed: bool,
}

fn copied(text: &str, bound: usize) -> Result<String, Error> {
    if text.len() > bound {
        return Err(Error::Limit);
    }
    let mut out = String::new();
    out.try_reserve_exact(text.len())
        .map_err(|_| Error::Limit)?;
    out.push_str(text);
    Ok(out)
}

fn label(text: &str) -> Result<String, Error> {
    if text.is_empty() {
        return Err(Error::Empty);
    }
    if text.chars().any(char::is_control) {
        return Err(Error::Control);
    }
    copied(text, MAX_LABEL_BYTES)
}

impl Message {
    /// A message under `header`, with no sections yet.
    pub fn new(header: &str) -> Result<Self, Error> {
        Ok(Self {
            label: label(header)?,
            status: None,
            verdict: None,
            sections: Vec::new(),
            source: None,
            collapsed: false,
        })
    }

    fn with(mut self, section: Section) -> Result<Self, Error> {
        if self.sections.len() >= MAX_SECTIONS {
            return Err(Error::Limit);
        }
        self.sections.try_reserve(1).map_err(|_| Error::Limit)?;
        self.sections.push(section);
        Ok(self)
    }

    /// Adds an untitled section of body text.
    pub fn text(self, text: &str) -> Result<Self, Error> {
        let text = copied(text, MAX_TEXT_BYTES)?;
        self.with(Section {
            title: None,
            text,
            collapsed: false,
            excerpt: false,
        })
    }

    /// Adds a titled section (reasoning, a tool's arguments), shown
    /// collapsed to its title row when `collapsed`.
    pub fn section(self, title: &str, text: &str, collapsed: bool) -> Result<Self, Error> {
        let title = label(title)?;
        let text = copied(text, MAX_TEXT_BYTES)?;
        self.with(Section {
            title: Some(title),
            text,
            collapsed,
            excerpt: false,
        })
    }

    /// Adds a titled excerpt section, which shows at most `EXCERPT_ROWS`
    /// rows of its text and `MORE` below them when there are more.
    pub fn excerpt(self, title: &str, text: &str) -> Result<Self, Error> {
        let title = label(title)?;
        let text = copied(text, MAX_TEXT_BYTES)?;
        self.with(Section {
            title: Some(title),
            text,
            collapsed: false,
            excerpt: true,
        })
    }

    /// The status the header shows after the label, in the tone's ink.
    pub fn status(mut self, text: &str, tone: Tone) -> Result<Self, Error> {
        self.status = Some((label(text)?, tone));
        Ok(self)
    }

    /// The verdict mark the header shows before its copy button.
    pub fn verdict(mut self, text: &str, tone: Tone) -> Result<Self, Error> {
        self.verdict = Some((label(text)?, tone));
        Ok(self)
    }

    /// The text the message's copy action copies in place of its
    /// sections' text: a tool block's whole result.
    pub fn source(mut self, source: Arc<str>) -> Result<Self, Error> {
        if source.len() > MAX_TEXT_BYTES {
            return Err(Error::Limit);
        }
        self.source = Some(source);
        Ok(self)
    }

    /// Shown collapsed to its header when `collapsed`.
    pub fn collapsed(mut self, collapsed: bool) -> Self {
        self.collapsed = collapsed;
        self
    }

    pub fn label(&self) -> &str {
        &self.label
    }

    pub fn sections(&self) -> usize {
        self.sections.len()
    }

    /// Section `index`'s text as supplied.
    pub fn section_text(&self, index: usize) -> Option<&str> {
        self.sections
            .get(index)
            .map(|section| section.text.as_str())
    }

    pub fn is_collapsed(&self) -> bool {
        self.collapsed
    }

    /// The bytes the message counts against `MAX_TOTAL_BYTES`.
    fn bytes(&self) -> usize {
        let marks = [&self.status, &self.verdict]
            .iter()
            .filter_map(|mark| mark.as_ref())
            .map(|(text, _)| text.len())
            .sum::<usize>();
        self.sections
            .iter()
            .map(|section| section.text.len() + section.title.as_ref().map_or(0, String::len))
            .sum::<usize>()
            + self.label.len()
            + marks
            + self.source.as_ref().map_or(0, |source| source.len())
    }

    /// Every section's text, collapsed or excerpted ones whole, a blank
    /// line between two: the message's source text when it has no
    /// `source`.
    fn copy_pieces(&self, f: &mut dyn FnMut(&str)) {
        for (index, section) in self.sections.iter().enumerate() {
            if index > 0 {
                f("\n\n");
            }
            f(&section.text);
        }
    }
}

/// A place in the text: a byte of a section of a message. Points order as
/// the text reads; a chrome row maps to the nearest place (a header to
/// its message's start, the space below a message to past its last
/// section), so a selection's ends are always points.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct Point {
    pub message: usize,
    pub section: usize,
    pub byte: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Kind {
    Header,
    Title,
    Text,
    More,
    Gap,
}

/// One laid-out row: a byte range of a section's text for `Text`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Line {
    message: u32,
    section: u16,
    kind: Kind,
    start: u32,
    end: u32,
}

impl Line {
    fn height(self) -> u64 {
        (match self.kind {
            Kind::Header => ROW,
            Kind::Gap => GAP,
            Kind::Title | Kind::Text | Kind::More => CELL_HEIGHT,
        }) as u64
    }
    /// The order rows read in, by which an anchor survives a relayout.
    fn key(self) -> (u32, u32, u32) {
        let section = 3 * u32::from(self.section);
        match self.kind {
            Kind::Header => (self.message, 0, 0),
            Kind::Title => (self.message, section + 1, 0),
            Kind::Text => (self.message, section + 2, self.start),
            Kind::More => (self.message, section + 3, 0),
            Kind::Gap => (self.message, u32::MAX, 0),
        }
    }
    fn point(self, byte: usize) -> Point {
        let message = self.message as usize;
        let section = usize::from(self.section);
        match self.kind {
            Kind::Header => Point {
                message,
                section: 0,
                byte: 0,
            },
            Kind::Title => Point {
                message,
                section,
                byte: 0,
            },
            Kind::Text => Point {
                message,
                section,
                byte,
            },
            Kind::More => Point {
                message,
                section,
                byte: usize::MAX,
            },
            Kind::Gap => Point {
                message,
                section: usize::MAX,
                byte: 0,
            },
        }
    }
}

/// What a shown row is, for a consumer reading the list back (its driven
/// text) and for tests.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Shown<'a> {
    Header {
        message: usize,
    },
    Title {
        message: usize,
        section: usize,
        collapsed: bool,
    },
    Text {
        message: usize,
        section: usize,
        text: &'a str,
    },
    More {
        message: usize,
        section: usize,
    },
    Gap {
        message: usize,
    },
}

/// The list's keys; `from_chord` is the default binding set.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Key {
    Up,
    Down,
    PageUp,
    PageDown,
    Home,
    End,
    PreviousMessage,
    NextMessage,
    /// Collapses or expands the focused message.
    Toggle,
    /// Copies the selection.
    Copy,
    /// Copies the focused message whole.
    CopyMessage,
    SelectAll,
}

impl Key {
    pub fn from_chord(chord: &str) -> Option<Self> {
        Some(match chord {
            "Up" => Self::Up,
            "Down" => Self::Down,
            "PageUp" => Self::PageUp,
            "PageDown" => Self::PageDown,
            "Home" | "C-Home" => Self::Home,
            "End" | "C-End" => Self::End,
            "M-Up" => Self::PreviousMessage,
            "M-Down" => Self::NextMessage,
            "Return" => Self::Toggle,
            "C-c" => Self::Copy,
            "C-S-c" => Self::CopyMessage,
            "C-a" => Self::SelectAll,
            _ => return None,
        })
    }
}

/// The input a list reads, in surface pixels; `at_ms` is the caller's
/// clock, which pairs a release with the next press as a double click.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Event {
    Key {
        key: Key,
        repeat: bool,
    },
    Press {
        x: i64,
        y: i64,
        extend: bool,
        at_ms: u64,
    },
    Move {
        x: i64,
        y: i64,
    },
    Release {
        x: i64,
        y: i64,
        at_ms: u64,
    },
    /// The button's drag ended without a release.
    Cancel,
    /// Wheel travel in rows, down positive.
    Wheel {
        rows: isize,
    },
    Focus(bool),
}

/// What an event did. Everything but `Ignored` and `Consumed` may have
/// changed what the list shows.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Outcome {
    /// Not the list's: off its rectangle, or nothing to act on.
    Ignored,
    /// The list's, changing nothing.
    Consumed,
    Changed,
    /// Text was offered to the clipboard.
    Copied,
    /// The clipboard refused the copy.
    Refused(Refusal),
    /// Nothing is selected or focused to copy.
    NothingToCopy,
}

/// The message list over one rectangle of a surface.
#[derive(Debug)]
pub struct Controller {
    surface: Surface,
    rect: Rect,
    laid: bool,
    messages: Vec<Message>,
    bytes: usize,
    lines: Vec<Line>,
    /// Each message's first row.
    first_line: Vec<usize>,
    scratch: Vec<Line>,
    top: usize,
    /// The font-pixel offset of `top` and of the whole layout's end.
    top_y: u64,
    total_y: u64,
    /// Whether the view stays at the end as the list grows.
    follow: bool,
    /// The first shown row's place while there is no layout, so a resize
    /// that gives the list room again shows it.
    kept: Option<(u32, u32, u32)>,
    focused: bool,
    focus: Option<usize>,
    /// The selection's anchor and head.
    selection: Option<(Point, Point)>,
    dragging: bool,
    /// A click's release time and place, for the next press to pair with.
    click: Option<(u64, i64, i64)>,
    /// Whether the held press selected a word, so its release is no click.
    word: bool,
}

impl Controller {
    /// An empty list over `rect`; a rectangle too small for its rows lays
    /// nothing out until a resize gives it room (`has_layout`).
    pub fn new(surface: Surface, rect: Rect) -> Result<Self, Error> {
        let mut list = Self {
            surface,
            rect,
            laid: false,
            messages: Vec::new(),
            bytes: 0,
            lines: Vec::new(),
            first_line: Vec::new(),
            scratch: Vec::new(),
            top: 0,
            top_y: 0,
            total_y: 0,
            follow: true,
            kept: None,
            focused: false,
            focus: None,
            selection: None,
            dragging: false,
            click: None,
            word: false,
        };
        list.resize(surface, rect)?;
        Ok(list)
    }

    /// Lays the list out over a new rectangle, keeping the first shown
    /// row's place in the text, or the end when the view was at it. A
    /// refusal leaves no layout, and so no hit targets.
    pub fn resize(&mut self, surface: Surface, rect: Rect) -> Result<(), Error> {
        let anchor = self.top_key();
        self.relayout_all(surface, rect, anchor)
    }

    /// The first shown row's place, laid out or kept.
    fn top_key(&self) -> Option<(u32, u32, u32)> {
        self.lines
            .get(self.top)
            .map(|line| line.key())
            .or(self.kept)
    }

    fn relayout_all(
        &mut self,
        surface: Surface,
        rect: Rect,
        anchor: Option<(u32, u32, u32)>,
    ) -> Result<(), Error> {
        self.dragging = false;
        self.click = None;
        self.laid = false;
        self.lines.clear();
        self.first_line.clear();
        // Until a layout holds again the place is kept, not a row.
        self.kept = anchor.filter(|_| !self.follow);
        self.top = 0;
        self.top_y = 0;
        self.total_y = 0;
        surface.check().map_err(|_| Error::InvalidSurface)?;
        let inside = rect.x >= 0
            && rect.y >= 0
            && i128::from(rect.x) + i128::from(rect.width) <= surface.width as i128
            && i128::from(rect.y) + i128::from(rect.height) <= surface.height as i128;
        if !inside {
            return Err(Error::InvalidRect);
        }
        self.surface = surface;
        self.rect = rect;
        if !self.roomy() {
            return Ok(());
        }
        self.first_line
            .try_reserve(self.messages.len())
            .map_err(|_| Error::Limit)?;
        let columns = self.columns();
        let mut lines = std::mem::take(&mut self.lines);
        let mut result = Ok(());
        for (index, message) in self.messages.iter().enumerate() {
            self.first_line.push(lines.len());
            let budget = MAX_LINES.saturating_sub(lines.len());
            if let Err(error) = layout(index, message, columns, budget, &mut lines) {
                result = Err(error);
                break;
            }
        }
        self.lines = lines;
        if let Err(error) = result {
            self.lines.clear();
            self.first_line.clear();
            return Err(error);
        }
        self.laid = true;
        self.kept = None;
        self.total_y = self.lines.iter().map(|line| line.height()).sum();
        self.settle(anchor);
        Ok(())
    }

    pub fn has_layout(&self) -> bool {
        self.laid
    }

    pub fn rect(&self) -> Rect {
        self.rect
    }

    pub fn len(&self) -> usize {
        self.messages.len()
    }

    pub fn is_empty(&self) -> bool {
        self.messages.is_empty()
    }

    pub fn message(&self, index: usize) -> Option<&Message> {
        self.messages.get(index)
    }

    /// The message the copy key and `Toggle` act on.
    pub fn focused_message(&self) -> Option<usize> {
        self.focus
    }

    /// The selection's anchor and head, in that order.
    pub fn selection(&self) -> Option<(Point, Point)> {
        self.selection
    }

    pub fn clear_selection(&mut self) -> bool {
        self.selection.take().is_some()
    }

    /// Whether the view is at the end and stays there as the list grows.
    pub fn following(&self) -> bool {
        self.follow
    }

    /// The bytes the messages hold against `MAX_TOTAL_BYTES`.
    pub fn storage_bytes(&self) -> usize {
        self.bytes
    }

    /// Appends a message and returns its index.
    pub fn push(&mut self, message: Message) -> Result<usize, Error> {
        let index = self.messages.len();
        if index >= MAX_MESSAGES {
            return Err(Error::Limit);
        }
        let bytes = self.admit(message.bytes(), 0)?;
        self.messages.try_reserve(1).map_err(|_| Error::Limit)?;
        self.first_line.try_reserve(1).map_err(|_| Error::Limit)?;
        if self.laid {
            let mut scratch = std::mem::take(&mut self.scratch);
            scratch.clear();
            let budget = MAX_LINES.saturating_sub(self.lines.len());
            let laid =
                layout(index, &message, self.columns(), budget, &mut scratch).and_then(|()| {
                    self.lines
                        .try_reserve(scratch.len())
                        .map_err(|_| Error::Limit)
                });
            if let Err(error) = laid {
                self.scratch = scratch;
                return Err(error);
            }
            let anchor = self.anchor();
            self.first_line.push(self.lines.len());
            self.total_y += scratch.iter().map(|line| line.height()).sum::<u64>();
            self.lines.extend_from_slice(&scratch);
            self.scratch = scratch;
            self.settle(anchor);
        }
        self.messages.push(message);
        self.bytes = bytes;
        Ok(index)
    }

    /// Appends `text` to a section of a message, as a reply streams in;
    /// the selection stays where it was.
    pub fn append(&mut self, index: usize, section: usize, text: &str) -> Result<(), Error> {
        let current = self
            .messages
            .get(index)
            .ok_or(Error::NoMessage)?
            .sections
            .get(section)
            .ok_or(Error::NoSection)?
            .text
            .len();
        if current.saturating_add(text.len()) > MAX_TEXT_BYTES {
            return Err(Error::Limit);
        }
        let bytes = self.admit(text.len(), 0)?;
        let target = self
            .messages
            .get_mut(index)
            .and_then(|message| message.sections.get_mut(section))
            .ok_or(Error::NoSection)?;
        target
            .text
            .try_reserve(text.len())
            .map_err(|_| Error::Limit)?;
        target.text.push_str(text);
        if let Err(error) = self.relayout_tail(index, section) {
            if let Some(target) = self
                .messages
                .get_mut(index)
                .and_then(|message| message.sections.get_mut(section))
            {
                target.text.truncate(current);
            }
            return Err(error);
        }
        self.bytes = bytes;
        Ok(())
    }

    /// Replaces a message whole; a selection reaching into it is cleared.
    pub fn replace(&mut self, index: usize, message: Message) -> Result<(), Error> {
        let old = self.messages.get(index).ok_or(Error::NoMessage)?.bytes();
        let bytes = self.admit(message.bytes(), old)?;
        let previous = std::mem::replace(
            self.messages.get_mut(index).ok_or(Error::NoMessage)?,
            message,
        );
        if let Err(error) = self.relayout(index) {
            if let Some(slot) = self.messages.get_mut(index) {
                *slot = previous;
            }
            return Err(error);
        }
        self.bytes = bytes;
        if self
            .ordered()
            .is_some_and(|(lo, hi)| lo.message <= index && index <= hi.message)
        {
            self.selection = None;
        }
        Ok(())
    }

    /// Sets or clears a message's status; the header alone changes.
    pub fn set_status(&mut self, index: usize, status: Option<(&str, Tone)>) -> Result<(), Error> {
        let status = status
            .map(|(text, tone)| label(text).map(|text| (text, tone)))
            .transpose()?;
        self.set_mark(index, status, false)
    }

    /// Sets or clears a message's verdict mark.
    pub fn set_verdict(
        &mut self,
        index: usize,
        verdict: Option<(&str, Tone)>,
    ) -> Result<(), Error> {
        let verdict = verdict
            .map(|(text, tone)| label(text).map(|text| (text, tone)))
            .transpose()?;
        self.set_mark(index, verdict, true)
    }

    fn set_mark(
        &mut self,
        index: usize,
        mark: Option<(String, Tone)>,
        verdict: bool,
    ) -> Result<(), Error> {
        let message = self.messages.get(index).ok_or(Error::NoMessage)?;
        let slot = if verdict {
            &message.verdict
        } else {
            &message.status
        };
        let old = slot.as_ref().map_or(0, |(text, _)| text.len());
        let new = mark.as_ref().map_or(0, |(text, _)| text.len());
        let bytes = self.admit(new, old)?;
        let message = self.messages.get_mut(index).ok_or(Error::NoMessage)?;
        if verdict {
            message.verdict = mark;
        } else {
            message.status = mark;
        }
        self.bytes = bytes;
        Ok(())
    }

    /// Sets the text a message's copy action copies (a tool block's whole
    /// result as it arrives).
    pub fn set_source(&mut self, index: usize, source: Option<Arc<str>>) -> Result<(), Error> {
        if source
            .as_ref()
            .is_some_and(|source| source.len() > MAX_TEXT_BYTES)
        {
            return Err(Error::Limit);
        }
        let message = self.messages.get(index).ok_or(Error::NoMessage)?;
        let old = message.source.as_ref().map_or(0, |source| source.len());
        let bytes = self.admit(source.as_ref().map_or(0, |source| source.len()), old)?;
        if let Some(message) = self.messages.get_mut(index) {
            message.source = source;
        }
        self.bytes = bytes;
        Ok(())
    }

    /// Collapses a message to its header, or expands it.
    pub fn set_collapsed(&mut self, index: usize, collapsed: bool) -> Result<bool, Error> {
        let message = self.messages.get_mut(index).ok_or(Error::NoMessage)?;
        if message.collapsed == collapsed {
            return Ok(false);
        }
        message.collapsed = collapsed;
        self.relayout_or_restore(index, |message| message.collapsed = !collapsed)?;
        Ok(true)
    }

    /// Collapses a titled section to its title row, or expands it; an
    /// untitled section is `NoSection`.
    pub fn set_section_collapsed(
        &mut self,
        index: usize,
        section: usize,
        collapsed: bool,
    ) -> Result<bool, Error> {
        let target = self
            .messages
            .get_mut(index)
            .ok_or(Error::NoMessage)?
            .sections
            .get_mut(section)
            .filter(|section| section.title.is_some())
            .ok_or(Error::NoSection)?;
        if target.collapsed == collapsed {
            return Ok(false);
        }
        target.collapsed = collapsed;
        self.relayout_or_restore(index, |message| {
            if let Some(target) = message.sections.get_mut(section) {
                target.collapsed = !collapsed;
            }
        })?;
        Ok(true)
    }

    fn relayout_or_restore(
        &mut self,
        index: usize,
        restore: impl FnOnce(&mut Message),
    ) -> Result<(), Error> {
        let result = self.relayout(index);
        if result.is_err() {
            if let Some(message) = self.messages.get_mut(index) {
                restore(message);
            }
        }
        result
    }

    /// Drops the oldest `count` messages, as a long transcript trims; the
    /// selection and focus follow the messages that stay, or go with them.
    /// A trim is never refused. It cannot grow a layout, so one that held
    /// still holds; a list with none (too many rows for `MAX_LINES` at this
    /// width) is laid out again, and `has_layout` says whether it now fits.
    pub fn remove_first(&mut self, count: usize) {
        let count = count.min(self.messages.len());
        if count == 0 {
            return;
        }
        let freed: usize = self.messages.iter().take(count).map(Message::bytes).sum();
        self.messages.drain(..count);
        self.bytes = self.bytes.saturating_sub(freed);
        self.selection = self
            .selection
            .and_then(|(anchor, head)| Some((shifted(anchor, count)?, shifted(head, count)?)));
        self.focus = self.focus.and_then(|focus| focus.checked_sub(count));
        // The first shown row keeps its place when its message stays, and
        // the list's start is shown when it went.
        let anchor = self.top_key().map(|(message, slot, start)| {
            message
                .checked_sub(u32::try_from(count).unwrap_or(u32::MAX))
                .map_or((0, 0, 0), |message| (message, slot, start))
        });
        // The surface and rectangle were admitted before, so only the row
        // budget can refuse, which leaves no layout until there is room.
        let _ = self.relayout_all(self.surface, self.rect, anchor);
    }

    fn admit(&self, add: usize, remove: usize) -> Result<usize, Error> {
        let bytes = self
            .bytes
            .saturating_sub(remove)
            .checked_add(add)
            .ok_or(Error::Limit)?;
        if bytes > MAX_TOTAL_BYTES {
            return Err(Error::Limit);
        }
        Ok(bytes)
    }

    fn scale(&self) -> u64 {
        self.surface.scale.value() as u64
    }

    fn cell(&self) -> i64 {
        (CELL_WIDTH * self.surface.scale.value()) as i64
    }

    /// The rows' area: the rectangle less the scrollbar gutter.
    fn view(&self) -> Rect {
        Rect {
            width: self
                .rect
                .width
                .saturating_sub((GUTTER * self.surface.scale.value()) as u32),
            ..self.rect
        }
    }

    fn view_height(&self) -> u64 {
        u64::from(self.rect.height) / self.scale()
    }

    fn columns(&self) -> usize {
        (i64::from(self.view().width) / self.cell()).saturating_sub(2) as usize
    }

    fn roomy(&self) -> bool {
        let s = self.surface.scale.value();
        self.rect.width as usize >= (GUTTER + (MIN_COLUMNS + 2) * CELL_WIDTH) * s
            && self.rect.height as usize >= ROW * s
    }

    fn anchor(&self) -> Option<(u32, u32, u32)> {
        if self.follow {
            return None;
        }
        self.lines.get(self.top).map(|line| line.key())
    }

    /// Message `index`'s rows.
    fn span(&self, index: usize) -> Result<(usize, usize), Error> {
        let start = *self.first_line.get(index).ok_or(Error::NoMessage)?;
        let end = self
            .first_line
            .get(index + 1)
            .copied()
            .unwrap_or(self.lines.len());
        if start > end || end > self.lines.len() {
            return Err(Error::Limit);
        }
        Ok((start, end))
    }

    /// Re-lays one message's rows, keeping the first shown row's place.
    fn relayout(&mut self, index: usize) -> Result<(), Error> {
        if !self.laid {
            return Ok(());
        }
        let message = self.messages.get(index).ok_or(Error::NoMessage)?;
        let (start, end) = self.span(index)?;
        let mut scratch = std::mem::take(&mut self.scratch);
        scratch.clear();
        let budget = MAX_LINES.saturating_sub(self.lines.len().saturating_sub(end - start));
        if let Err(error) = layout(index, message, self.columns(), budget, &mut scratch) {
            self.scratch = scratch;
            return Err(error);
        }
        self.splice_rows(index, start, end, scratch)
    }

    /// Re-wraps one section from its last shown text row on, as text
    /// appended to it streams in: rows before that one cannot change,
    /// since a row starts with nothing carried from the one before. A
    /// folded section, or an excerpt already cut short, shows no more.
    fn relayout_tail(&mut self, index: usize, section: usize) -> Result<(), Error> {
        if !self.laid {
            return Ok(());
        }
        let (start, end) = self.span(index)?;
        let wanted = u16::try_from(section).map_err(|_| Error::NoSection)?;
        let rows = self.lines.get(start..end).unwrap_or(&[]);
        // Backward from the message's end, past later sections only, so
        // streaming into the last section finds its last row at once.
        let mut last = None;
        for (at, line) in rows.iter().enumerate().rev() {
            match line.kind {
                Kind::Header => return Ok(()),
                _ if line.section != wanted => {}
                Kind::Text => {
                    last = Some(at);
                    break;
                }
                Kind::More | Kind::Title => return Ok(()),
                Kind::Gap => {}
            }
        }
        let Some(last) = last else {
            return Ok(());
        };
        let target = self
            .messages
            .get(index)
            .and_then(|message| message.sections.get(section))
            .ok_or(Error::NoSection)?;
        // Only an excerpt counts the rows it already shows, at most its
        // bound of them.
        let first = if target.excerpt {
            let before = rows.get(..last).unwrap_or(&[]);
            last - before
                .iter()
                .rev()
                .take(EXCERPT_ROWS)
                .take_while(|line| line.kind == Kind::Text && line.section == wanted)
                .count()
        } else {
            last
        };
        let from = rows.get(last).map_or(0, |line| line.start as usize);
        let mut scratch = std::mem::take(&mut self.scratch);
        scratch.clear();
        let budget = MAX_LINES.saturating_sub(self.lines.len().saturating_sub(1));
        let wrap = Wrap {
            message: index,
            section,
            target,
            columns: self.columns(),
            budget,
        };
        let laid = wrap_section(wrap, from, last.saturating_sub(first), &mut scratch);
        if let Err(error) = laid {
            self.scratch = scratch;
            return Err(error);
        }
        self.splice_rows(index, start + last, start + last + 1, scratch)
    }

    /// Puts `scratch` in place of rows `start..end`, which lie in message
    /// `index`'s, and keeps the first shown row's place: a row before them
    /// keeps its offset too, so streaming below the view costs no walk.
    fn splice_rows(
        &mut self,
        index: usize,
        start: usize,
        end: usize,
        mut scratch: Vec<Line>,
    ) -> Result<(), Error> {
        let removed = end.saturating_sub(start);
        let fits = end <= self.lines.len()
            && self
                .lines
                .try_reserve(scratch.len().saturating_sub(removed))
                .is_ok();
        if !fits {
            self.scratch = scratch;
            return Err(Error::Limit);
        }
        let anchor = self.anchor();
        let (top, top_y) = (self.top, self.top_y);
        let old: u64 = self
            .lines
            .get(start..end)
            .map_or(0, |lines| lines.iter().map(|line| line.height()).sum());
        let new: u64 = scratch.iter().map(|line| line.height()).sum();
        self.total_y = self.total_y.saturating_sub(old) + new;
        let added = scratch.len();
        self.lines.splice(start..end, scratch.drain(..));
        self.scratch = scratch;
        for first in self.first_line.iter_mut().skip(index + 1) {
            *first = first.saturating_sub(removed) + added;
        }
        let last = self.max_top();
        if anchor.is_some() && top < start && top <= last {
            self.top = top;
            self.top_y = top_y;
            self.follow = top == last;
        } else {
            self.settle(anchor);
        }
        Ok(())
    }

    /// Puts the first shown row at `anchor`'s place (the row holding it:
    /// the last that reads at or before it, so a rewrapped row's start
    /// stays shown and a collapsed section's text gives its title), or at
    /// the end when there is none, clamped to the last page.
    fn settle(&mut self, anchor: Option<(u32, u32, u32)>) {
        let last = self.max_top();
        self.top = match anchor {
            Some(key) if !self.follow => self
                .lines
                .partition_point(|line| line.key() <= key)
                .saturating_sub(1)
                .min(last),
            _ => last,
        };
        self.follow = self.top == last;
        self.top_y = self.y_of(self.top);
    }

    /// The font-pixel offset of row `index`, summed from the nearer end.
    fn y_of(&self, index: usize) -> u64 {
        let index = index.min(self.lines.len());
        if index <= self.lines.len() / 2 {
            self.lines
                .iter()
                .take(index)
                .map(|line| line.height())
                .sum()
        } else {
            let after: u64 = self
                .lines
                .iter()
                .skip(index)
                .map(|line| line.height())
                .sum();
            self.total_y.saturating_sub(after)
        }
    }

    /// The first shown row of the last page: the earliest from which the
    /// rest fit whole, but never past the last message's header, title,
    /// text or more row, so a view too short for it and its gap shows the
    /// gap cut and not alone.
    fn max_top(&self) -> usize {
        let room = self.view_height();
        let mut used = 0u64;
        let mut top = self.lines.len();
        for line in self.lines.iter().rev() {
            used += line.height();
            if used > room {
                break;
            }
            top = top.saturating_sub(1);
        }
        let content = self
            .lines
            .iter()
            .rposition(|line| line.kind != Kind::Gap)
            .unwrap_or(0);
        top.min(content)
    }

    fn set_top(&mut self, top: usize) -> bool {
        let top = top.min(self.max_top());
        let changed = top != self.top;
        self.top = top;
        self.top_y = self.y_of(top);
        self.follow = top == self.max_top();
        changed
    }

    fn page_down(&self) -> usize {
        let room = self.view_height();
        let mut used = 0u64;
        let mut index = self.top;
        while let Some(line) = self.lines.get(index) {
            if used + line.height() > room {
                break;
            }
            used += line.height();
            index += 1;
        }
        index.max(self.top + 1)
    }

    fn page_up(&self) -> usize {
        let room = self.view_height();
        let mut used = 0u64;
        let mut index = self.top;
        while let Some(line) = index.checked_sub(1).and_then(|i| self.lines.get(i)) {
            if used + line.height() > room {
                break;
            }
            used += line.height();
            index -= 1;
        }
        index.min(self.top.saturating_sub(1))
    }

    /// The shown rows from the first, each with its rectangle (its height
    /// whole; the last may run past the view, which clips it).
    fn rows(&self) -> impl Iterator<Item = (usize, Line, Rect)> + '_ {
        let view = self.view();
        let bottom = view.y.saturating_add(i64::from(view.height));
        let scale = self.scale();
        let mut y = view.y;
        let mut index = self.top;
        std::iter::from_fn(move || {
            if !self.laid || y >= bottom {
                return None;
            }
            let line = *self.lines.get(index)?;
            let height = (line.height() * scale) as u32;
            let rect = Rect { y, height, ..view };
            y = y.saturating_add(i64::from(height));
            index += 1;
            Some((index - 1, line, rect))
        })
    }

    /// The shown rows, for a consumer reading the list back.
    pub fn shown(&self) -> impl Iterator<Item = (Rect, Shown<'_>)> + '_ {
        self.rows().map(|(_, line, rect)| {
            let message = line.message as usize;
            let section = usize::from(line.section);
            let shown = match line.kind {
                Kind::Header => Shown::Header { message },
                Kind::Title => Shown::Title {
                    message,
                    section,
                    collapsed: self.section(line).is_some_and(|section| section.collapsed),
                },
                Kind::Text => Shown::Text {
                    message,
                    section,
                    text: self.text(line),
                },
                Kind::More => Shown::More { message, section },
                Kind::Gap => Shown::Gap { message },
            };
            (rect, shown)
        })
    }

    fn section(&self, line: Line) -> Option<&Section> {
        self.messages
            .get(line.message as usize)?
            .sections
            .get(usize::from(line.section))
    }

    fn text(&self, line: Line) -> &str {
        self.section(line)
            .and_then(|section| section.text.get(line.start as usize..line.end as usize))
            .unwrap_or("")
    }

    /// The copy button on a shown header's right, inset like a strip's.
    fn button(&self, header: Rect) -> Rect {
        let s = self.surface.scale.value();
        let width = ((COPY_LABEL.chars().count() + 2) * CELL_WIDTH * s) as u32;
        Rect {
            x: header.x + i64::from(header.width) - self.cell() - i64::from(width),
            y: header.y + (BUTTON_MARGIN * s) as i64,
            width,
            height: ((ROW - 2 * BUTTON_MARGIN) * s) as u32,
        }
    }

    /// The copy button of a message whose header is shown.
    pub fn copy_button(&self, index: usize) -> Option<Rect> {
        self.rows()
            .find(|(_, line, _)| line.kind == Kind::Header && line.message as usize == index)
            .map(|(_, _, rect)| self.button(rect))
    }

    /// The column a pointer at `x` falls before, rounded to the nearer
    /// cell edge, as a byte of the row.
    fn byte_at(&self, line: Line, x: i64, round: bool) -> usize {
        let cell = self.cell();
        let left = self.view().x + cell;
        let offset = x.saturating_sub(left);
        let offset = if round {
            offset.saturating_add(cell / 2)
        } else {
            offset
        };
        let column = offset.div_euclid(cell).max(0) as usize;
        let text = self.text(line);
        line.start as usize
            + text
                .char_indices()
                .nth(column)
                .map_or(text.len(), |(at, _)| at)
    }

    /// The point under (x, y), above the view the first shown row's start
    /// and below the shown rows the last one's end.
    fn point_at(&self, x: i64, y: i64) -> Option<Point> {
        let view = self.view();
        let mut last = None;
        for (_, line, rect) in self.rows() {
            if y < rect.y || (y >= rect.y && y < rect.y + i64::from(rect.height)) {
                let x = if y < view.y { i64::MIN } else { x };
                return Some(line.point(self.byte_at(line, x, true)));
            }
            last = Some(line);
        }
        last.map(|line| line.point(line.end as usize))
    }

    /// The selection's ends in reading order.
    fn ordered(&self) -> Option<(Point, Point)> {
        let (anchor, head) = self.selection?;
        Some((anchor.min(head), anchor.max(head)))
    }

    /// The selected bytes of a text row, if any.
    fn selected(&self, line: Line, lo: Point, hi: Point) -> Option<(usize, usize)> {
        if line.kind != Kind::Text {
            return None;
        }
        let from = line.point(line.start as usize).max(lo);
        let to = line.point(line.end as usize).min(hi);
        (from < to).then_some((from.byte, to.byte))
    }

    /// Visits the selected text in order. Each section's shown text is
    /// the source from its first shown row's start to its last's end, its
    /// newlines and empty rows included; the selection's part of that is
    /// one piece, and a blank line goes between two sections' pieces.
    /// Hidden text (a collapsed section, an excerpt's rest) and chrome
    /// rows are not visited.
    fn selected_pieces(&self, f: &mut dyn FnMut(&str)) {
        let Some((lo, hi)) = self.ordered().filter(|(lo, hi)| lo < hi) else {
            return;
        };
        let from = self.first_line.get(lo.message).copied().unwrap_or(0);
        let to = self
            .first_line
            .get(hi.message.saturating_add(1))
            .copied()
            .unwrap_or(self.lines.len());
        let mut any = false;
        let mut group: Option<Line> = None;
        for &line in self.lines.get(from..to).unwrap_or(&[]) {
            if line.kind != Kind::Text {
                continue;
            }
            match &mut group {
                Some(shown) if (shown.message, shown.section) == (line.message, line.section) => {
                    shown.end = line.end;
                }
                _ => {
                    if let Some(shown) = group.replace(line) {
                        self.selected_piece(shown, lo, hi, &mut any, f);
                    }
                }
            }
        }
        if let Some(shown) = group {
            self.selected_piece(shown, lo, hi, &mut any, f);
        }
    }

    /// The selection's part of a section's shown text `shown` (its first
    /// row's start to its last's end). A section the selection only
    /// touches at an end gives nothing; one it holds whole is a piece
    /// even when empty, as a whole-message copy keeps it.
    fn selected_piece(
        &self,
        shown: Line,
        lo: Point,
        hi: Point,
        any: &mut bool,
        f: &mut dyn FnMut(&str),
    ) {
        let start = shown.point(shown.start as usize);
        let end = shown.point(shown.end as usize);
        let (from, to) = (start.max(lo), end.min(hi));
        if from > to || (from == to && !(lo < start && end < hi)) {
            return;
        }
        let Some(section) = self.section(shown) else {
            return;
        };
        if std::mem::replace(any, true) {
            f("\n\n");
        }
        f(section.text.get(from.byte..to.byte).unwrap_or(""));
    }

    /// The selected text, without chrome; `None` when nothing is.
    pub fn selected_text(&self) -> Result<Option<Arc<str>>, Error> {
        gather(&mut |f| self.selected_pieces(f))
    }

    /// A message's whole source text, as its copy action copies it.
    pub fn message_text(&self, index: usize) -> Result<Arc<str>, Error> {
        let message = self.messages.get(index).ok_or(Error::NoMessage)?;
        if let Some(source) = &message.source {
            return Ok(source.clone());
        }
        Ok(gather(&mut |f| message.copy_pieces(f))?.unwrap_or_else(|| Arc::from("")))
    }

    /// Selects every shown text row.
    pub fn select_all(&mut self) -> bool {
        let first = self.lines.iter().find(|line| line.kind == Kind::Text);
        let last = self.lines.iter().rev().find(|line| line.kind == Kind::Text);
        let (Some(first), Some(last)) = (first, last) else {
            return false;
        };
        let selection = Some((
            first.point(first.start as usize),
            last.point(last.end as usize),
        ));
        let changed = selection != self.selection;
        self.selection = selection;
        changed
    }

    fn offer(text: Result<Option<Arc<str>>, Error>, clipboard: &mut dyn Clipboard) -> Outcome {
        match text {
            Ok(Some(text)) if !text.is_empty() => match clipboard.copy(text) {
                Ok(()) => Outcome::Copied,
                Err(refusal) => Outcome::Refused(refusal),
            },
            Ok(_) => Outcome::NothingToCopy,
            Err(_) => Outcome::Refused(Refusal::TooLong),
        }
    }

    fn copy_message(&self, index: usize, clipboard: &mut dyn Clipboard) -> Outcome {
        Self::offer(self.message_text(index).map(Some), clipboard)
    }

    /// Scrolls so a message's header is shown.
    fn reveal(&mut self, index: usize) {
        let Some(&header) = self.first_line.get(index) else {
            return;
        };
        let shown = self.rows().any(|(row, _, rect)| {
            row == header && rect.y + i64::from(rect.height) <= self.bottom()
        });
        if !shown {
            self.set_top(header);
        }
    }

    fn bottom(&self) -> i64 {
        self.rect.y.saturating_add(i64::from(self.rect.height))
    }

    fn changed(changed: bool) -> Outcome {
        if changed {
            Outcome::Changed
        } else {
            Outcome::Consumed
        }
    }

    /// Reads one event; a copy is offered through `clipboard` while the
    /// event is delivered, so it is made at its key's or press's serial.
    pub fn event(&mut self, event: Event, clipboard: &mut dyn Clipboard) -> Outcome {
        match event {
            Event::Focus(focused) => {
                self.dragging &= focused;
                let changed = self.focused != focused;
                self.focused = focused;
                Self::changed(changed)
            }
            Event::Cancel => {
                self.click = None;
                if std::mem::take(&mut self.dragging) {
                    Outcome::Consumed
                } else {
                    Outcome::Ignored
                }
            }
            _ if !self.laid => {
                self.dragging = false;
                Outcome::Ignored
            }
            Event::Key { key, repeat } => self.key(key, repeat, clipboard),
            Event::Wheel { rows } => {
                let top = if rows < 0 {
                    self.top.saturating_sub(rows.unsigned_abs())
                } else {
                    self.top.saturating_add(rows.unsigned_abs())
                };
                Self::changed(self.set_top(top))
            }
            Event::Press {
                x,
                y,
                extend,
                at_ms,
            } => self.press(x, y, extend, at_ms, clipboard),
            Event::Move { x, y } => {
                if !self.dragging {
                    return Outcome::Ignored;
                }
                // Dragging past the view scrolls a row a motion.
                let view = self.view();
                let scrolled = if y < view.y {
                    self.set_top(self.top.saturating_sub(1))
                } else if y >= self.bottom() {
                    self.set_top(self.top + 1)
                } else {
                    false
                };
                match self.extend_to(x, y) {
                    Outcome::Consumed if scrolled => Outcome::Changed,
                    outcome => outcome,
                }
            }
            Event::Release { x, y, at_ms } => {
                if !std::mem::take(&mut self.dragging) {
                    return Outcome::Ignored;
                }
                let outcome = self.extend_to(x, y);
                self.click = (!std::mem::take(&mut self.word)).then_some((at_ms, x, y));
                outcome
            }
        }
    }

    fn extend_to(&mut self, x: i64, y: i64) -> Outcome {
        let (Some(head), Some((anchor, old))) = (self.point_at(x, y), self.selection) else {
            return Outcome::Consumed;
        };
        if self.word {
            return Outcome::Consumed;
        }
        self.selection = Some((anchor, head));
        Self::changed(head != old)
    }

    fn key(&mut self, key: Key, repeat: bool, clipboard: &mut dyn Clipboard) -> Outcome {
        self.dragging = false;
        self.click = None;
        match key {
            Key::Up => Self::changed(self.set_top(self.top.saturating_sub(1))),
            Key::Down => Self::changed(self.set_top(self.top + 1)),
            Key::PageUp => Self::changed(self.set_top(self.page_up())),
            Key::PageDown => Self::changed(self.set_top(self.page_down())),
            Key::Home => Self::changed(self.set_top(0)),
            Key::End => Self::changed(self.set_top(usize::MAX)),
            Key::PreviousMessage | Key::NextMessage => {
                if self.messages.is_empty() {
                    return Outcome::Consumed;
                }
                let shown = self
                    .lines
                    .get(self.top)
                    .map_or(0, |line| line.message as usize);
                let last = self.messages.len() - 1;
                let focus = match (key, self.focus) {
                    (Key::NextMessage, Some(focus)) => (focus + 1).min(last),
                    (_, Some(focus)) => focus.saturating_sub(1),
                    (_, None) => shown.min(last),
                };
                let changed = self.focus != Some(focus);
                self.focus = Some(focus);
                let top = self.top;
                self.reveal(focus);
                Self::changed(changed || top != self.top)
            }
            _ if repeat => Outcome::Consumed,
            Key::Toggle => match self.focus {
                Some(focus) => {
                    let collapsed = self.messages.get(focus).is_some_and(|m| m.collapsed);
                    match self.fold(focus, None, !collapsed) {
                        Ok(changed) => {
                            self.reveal(focus);
                            Self::changed(changed)
                        }
                        Err(_) => Outcome::Consumed,
                    }
                }
                None => Outcome::Consumed,
            },
            Key::Copy => Self::offer(self.selected_text(), clipboard),
            Key::CopyMessage => match self.focus {
                Some(focus) => self.copy_message(focus, clipboard),
                None => Outcome::NothingToCopy,
            },
            Key::SelectAll => Self::changed(self.select_all()),
        }
    }

    /// A fold the user asked for: the view keeps its place instead of
    /// following the end, so the row they pressed stays where it was.
    fn fold(
        &mut self,
        message: usize,
        section: Option<usize>,
        collapsed: bool,
    ) -> Result<bool, Error> {
        let follow = std::mem::replace(&mut self.follow, false);
        let result = match section {
            Some(section) => self.set_section_collapsed(message, section, collapsed),
            None => self.set_collapsed(message, collapsed),
        };
        if !matches!(result, Ok(true)) {
            self.follow = follow;
        }
        result
    }

    fn press(
        &mut self,
        x: i64,
        y: i64,
        extend: bool,
        at_ms: u64,
        clipboard: &mut dyn Clipboard,
    ) -> Outcome {
        self.dragging = false;
        self.word = false;
        let click = self.click.take();
        if !self.rect.contains(x, y) {
            return Outcome::Ignored;
        }
        if !self.view().contains(x, y) {
            return Outcome::Consumed;
        }
        let row = self.rows().find(|(_, _, rect)| rect.contains(x, y));
        let Some((_, line, rect)) = row else {
            // Below the last row: a selection ends at the text's end.
            let end = self.lines.last().map(|line| line.point(line.end as usize));
            return Self::changed(self.begin(end, extend, None));
        };
        let message = line.message as usize;
        let focus_changed = self.focus != Some(message);
        self.focus = Some(message);
        match line.kind {
            Kind::Header => {
                if self.button(rect).contains(x, y) {
                    return self.copy_message(message, clipboard);
                }
                if x < rect.x + 3 * self.cell() {
                    let collapsed = self.messages.get(message).is_some_and(|m| m.collapsed);
                    return match self.fold(message, None, !collapsed) {
                        Ok(_) => Outcome::Changed,
                        Err(_) => Self::changed(focus_changed),
                    };
                }
                Self::changed(focus_changed)
            }
            Kind::Title => {
                let section = usize::from(line.section);
                let collapsed = self.section(line).is_some_and(|section| section.collapsed);
                match self.fold(message, Some(section), !collapsed) {
                    Ok(_) => Outcome::Changed,
                    Err(_) => Self::changed(focus_changed),
                }
            }
            _ => {
                let s = self.surface.scale.value() as i64;
                let paired = click.filter(|&(at, cx, cy)| {
                    at_ms
                        .checked_sub(at)
                        .is_some_and(|gap| gap <= MULTI_CLICK_MS)
                        && cx.abs_diff(x) <= (NEAR * s) as u64
                        && cy.abs_diff(y) <= (NEAR * s) as u64
                });
                let word = (paired.is_some() && !extend && line.kind == Kind::Text)
                    .then(|| self.word_at(line, x))
                    .flatten();
                let point = line.point(self.byte_at(line, x, true));
                let changed = self.begin(Some(point), extend, word);
                Self::changed(changed || focus_changed)
            }
        }
    }

    /// Starts a drag at `point`: a new empty selection there, the old one
    /// extended to it, or `word`'s, which a drag does not extend.
    fn begin(&mut self, point: Option<Point>, extend: bool, word: Option<(Point, Point)>) -> bool {
        let Some(point) = point else {
            return false;
        };
        let old = self.selection;
        self.selection = if let Some(word) = word {
            self.word = true;
            Some(word)
        } else {
            match self.selection {
                Some((anchor, _)) if extend => Some((anchor, point)),
                _ => Some((point, point)),
            }
        };
        self.dragging = true;
        old != self.selection
    }

    /// The word of letters and digits under `x`, across the section's
    /// wrapped rows but not past its shown text; `None` off a word, past
    /// the row's text among it.
    fn word_at(&self, line: Line, x: i64) -> Option<(Point, Point)> {
        let section = self.section(line)?;
        let at = self.byte_at(line, x, false);
        if at >= line.end as usize {
            return None;
        }
        let text = section.text.get(..self.shown_end(line)?)?;
        let after = text.get(at..)?;
        if !after.chars().next().is_some_and(char::is_alphanumeric) {
            return None;
        }
        let before = text.get(..at)?;
        let start = before
            .char_indices()
            .rev()
            .take_while(|(_, c)| c.is_alphanumeric())
            .last()
            .map_or(at, |(i, _)| i);
        let end = after
            .char_indices()
            .find(|(_, c)| !c.is_alphanumeric())
            .map_or(text.len(), |(i, _)| at + i);
        Some((line.point(start), line.point(end)))
    }

    /// The end of the last shown text row of `line`'s section.
    fn shown_end(&self, line: Line) -> Option<usize> {
        let (start, end) = self.span(line.message as usize).ok()?;
        self.lines
            .get(start..end)?
            .iter()
            .rev()
            .find(|row| row.kind == Kind::Text && row.section == line.section)
            .map(|row| row.end as usize)
    }

    /// Paints the list: the rectangle chrome, the rows' paper, each shown
    /// row and the scrollbar's thumb; nothing without a layout.
    pub fn emit(&self, damage: Rect, sink: &mut dyn FnMut(Draw)) {
        if !self.laid {
            return;
        }
        let view = self.view();
        fill(self.rect, CHROME, damage, sink);
        fill(view, PAPER, damage, sink);
        let selection = self.ordered().filter(|(lo, hi)| lo != hi);
        for (_, line, rect) in self.rows() {
            let Some(clip) = rect.intersection(view).and_then(|r| r.intersection(damage)) else {
                continue;
            };
            match line.kind {
                Kind::Header => self.paint_header(line, rect, clip, sink),
                Kind::Title => {
                    let collapsed = self.section(line).is_some_and(|section| section.collapsed);
                    let title = self
                        .section(line)
                        .and_then(|section| section.title.as_deref())
                        .unwrap_or("");
                    let mark = if collapsed { SHUT } else { OPEN };
                    self.run(
                        [mark, ' '].into_iter().chain(title.chars()),
                        rect,
                        0,
                        GlyphStyle::medium(LINE_NUMBER, PAPER),
                        clip,
                        sink,
                    );
                }
                Kind::More => self.run(
                    MORE.chars(),
                    rect,
                    0,
                    GlyphStyle::medium(LINE_NUMBER, PAPER),
                    clip,
                    sink,
                ),
                Kind::Text => self.paint_text(line, rect, selection, clip, sink),
                Kind::Gap => {}
            }
        }
        let s = self.surface.scale.value();
        let track = Rect {
            x: self.rect.x + i64::from(self.rect.width) - (GUTTER * s) as i64,
            y: self.rect.y,
            width: (TRACK * s) as u32,
            height: self.rect.height,
        };
        let bar = Scrollbar::new(
            track,
            self.view_height() as usize,
            self.total_y as usize,
            self.top_y as usize,
            self.surface.scale,
            false,
        );
        fill(
            bar.thumb,
            if bar.enabled() { LINE_NUMBER } else { BORDER },
            damage,
            sink,
        );
    }

    /// A run of glyphs on a row from `column` cells into its text area.
    fn run(
        &self,
        chars: impl Iterator<Item = char>,
        rect: Rect,
        column: usize,
        style: GlyphStyle,
        clip: Rect,
        sink: &mut dyn FnMut(Draw),
    ) {
        let cell = self.cell();
        let left = rect.x + cell;
        let bounds = Rect {
            x: left,
            width: (i64::from(rect.width) - 2 * cell).max(0) as u32,
            ..rect
        };
        text_run(
            self.surface.scale,
            chars.map(|c| if c == '\t' { ' ' } else { c }),
            (left + column as i64 * cell, rect.y),
            bounds,
            style,
            clip,
            sink,
        );
    }

    fn paint_text(
        &self,
        line: Line,
        rect: Rect,
        selection: Option<(Point, Point)>,
        clip: Rect,
        sink: &mut dyn FnMut(Draw),
    ) {
        let text = self.text(line);
        let start = line.start as usize;
        let normal = GlyphStyle::medium(INK, PAPER);
        let Some((a, b)) = selection.and_then(|(lo, hi)| self.selected(line, lo, hi)) else {
            return self.run(text.chars(), rect, 0, normal, clip, sink);
        };
        let columns = |byte: usize| {
            text.get(..byte.saturating_sub(start))
                .map_or(0, |head| head.chars().count())
        };
        let (from, to) = (columns(a), columns(b));
        let (ground, ink) = if self.focused {
            (SELECTED, PAPER)
        } else {
            (INACTIVE_SELECTION, INK)
        };
        let cell = self.cell();
        let shown = Rect {
            x: rect.x + cell,
            width: (i64::from(rect.width) - 2 * cell).max(0) as u32,
            ..rect
        };
        if let Some(area) = (Rect {
            x: rect.x + cell + from as i64 * cell,
            width: (to.saturating_sub(from) as i64 * cell) as u32,
            ..rect
        })
        .intersection(shown)
        {
            fill(area, ground, clip, sink);
        }
        self.run(text.chars().take(from), rect, 0, normal, clip, sink);
        self.run(
            text.chars().skip(from).take(to.saturating_sub(from)),
            rect,
            from,
            GlyphStyle::medium(ink, ground),
            clip,
            sink,
        );
        self.run(text.chars().skip(to), rect, to, normal, clip, sink);
    }

    fn paint_header(&self, line: Line, rect: Rect, clip: Rect, sink: &mut dyn FnMut(Draw)) {
        let Some(message) = self.messages.get(line.message as usize) else {
            return;
        };
        let s = self.surface.scale.value() as i64;
        let cell = self.cell();
        let focused = self.focus == Some(line.message as usize);
        let ground = if focused { SELECTED_ROW } else { CHROME };
        fill(rect, ground, clip, sink);
        fill(
            Rect {
                height: s as u32,
                ..rect
            },
            BORDER,
            clip,
            sink,
        );
        let button = self.button(rect);
        let y = rect.y + 4 * s;
        let glyphs = |chars: &mut dyn Iterator<Item = char>,
                      x: i64,
                      right: i64,
                      ink: u32,
                      sink: &mut dyn FnMut(Draw)| {
            let bounds = Rect {
                x,
                y: rect.y,
                width: (right - x).max(0) as u32,
                height: rect.height,
            };
            text_run(
                self.surface.scale,
                chars,
                (x, y),
                bounds,
                GlyphStyle::medium(ink, ground),
                clip,
                sink,
            );
        };
        let mark = if message.collapsed { SHUT } else { OPEN };
        glyphs(
            &mut std::iter::once(mark),
            rect.x + cell,
            button.x,
            INK,
            sink,
        );
        // The verdict ends a cell before the button; the label and status
        // stop a cell before the verdict.
        let right = button.x - cell;
        let verdict_x = message.verdict.as_ref().map_or(right, |(text, _)| {
            (right - (text.chars().count() as i64 + 2) * cell).max(rect.x + 3 * cell)
        });
        let label_x = rect.x + 3 * cell;
        let limit = verdict_x - cell;
        glyphs(&mut message.label.chars(), label_x, limit, INK, sink);
        if let Some((status, tone)) = &message.status {
            let x = label_x + (message.label.chars().count() as i64 + 2) * cell;
            glyphs(&mut status.chars(), x, limit, tone.ink(), sink);
        }
        if let Some((verdict, tone)) = &message.verdict {
            glyphs(
                &mut [tone.mark(), ' '].into_iter().chain(verdict.chars()),
                verdict_x,
                right,
                tone.ink(),
                sink,
            );
        }
        if let Some(button) = Button::new(self.surface, button) {
            button.emit(COPY_LABEL, false, true, clip, sink);
        }
    }
}

fn fill(rect: Rect, color: u32, damage: Rect, sink: &mut dyn FnMut(Draw)) {
    if let Some(area) = rect.intersection(damage) {
        sink(Draw {
            clip: area,
            primitive: Primitive::Fill { rect: area, color },
        });
    }
}

fn shifted(point: Point, count: usize) -> Option<Point> {
    Some(Point {
        message: point.message.checked_sub(count)?,
        ..point
    })
}

/// A walk over text in pieces, handing each to the visitor it is given.
type Pieces<'a> = dyn FnMut(&mut dyn FnMut(&str)) + 'a;

/// The pieces `visit` gives, gathered once their length is known: `None`
/// when they are empty, `Limit` past the clipboard's ceiling.
fn gather(visit: &mut Pieces<'_>) -> Result<Option<Arc<str>>, Error> {
    let mut length = 0usize;
    visit(&mut |piece| length = length.saturating_add(piece.len()));
    if length == 0 {
        return Ok(None);
    }
    if length > crate::clipboard::MAX_BYTES {
        return Err(Error::Limit);
    }
    let mut text = String::new();
    text.try_reserve_exact(length).map_err(|_| Error::Limit)?;
    visit(&mut |piece| text.push_str(piece));
    Ok(Some(Arc::from(text)))
}

fn push(out: &mut Vec<Line>, budget: usize, line: Line) -> Result<(), Error> {
    if out.len() >= budget {
        return Err(Error::Limit);
    }
    out.try_reserve(1).map_err(|_| Error::Limit)?;
    out.push(line);
    Ok(())
}

/// One laid-out row of message `message`.
fn row(
    message: usize,
    section: usize,
    kind: Kind,
    start: usize,
    end: usize,
) -> Result<Line, Error> {
    Ok(Line {
        message: u32::try_from(message).map_err(|_| Error::Limit)?,
        section: u16::try_from(section).map_err(|_| Error::Limit)?,
        kind,
        start: u32::try_from(start).map_err(|_| Error::Limit)?,
        end: u32::try_from(end).map_err(|_| Error::Limit)?,
    })
}

/// A section's text rows to lay out: whose they are, the columns they wrap
/// to and the end of the row budget in `out`.
struct Wrap<'a> {
    message: usize,
    section: usize,
    target: &'a Section,
    columns: usize,
    budget: usize,
}

/// Appends a section's text rows from byte `from` (a row's start) to
/// `out`, `shown` rows of it already laid before that one, an excerpt cut
/// at `EXCERPT_ROWS` with a `More` row.
fn wrap_section(
    wrap: Wrap<'_>,
    from: usize,
    shown: usize,
    out: &mut Vec<Line>,
) -> Result<(), Error> {
    let text = wrap.target.text.get(from..).ok_or(Error::Limit)?;
    let mut rows = shown;
    let mut more = false;
    self::wrap(text, wrap.columns, &mut |start, end| {
        if wrap.target.excerpt && rows == EXCERPT_ROWS {
            more = true;
            return Ok(false);
        }
        rows += 1;
        let line = row(
            wrap.message,
            wrap.section,
            Kind::Text,
            from + start,
            from + end,
        )?;
        push(out, wrap.budget, line)?;
        Ok(true)
    })?;
    if more {
        push(
            out,
            wrap.budget,
            row(wrap.message, wrap.section, Kind::More, 0, 0)?,
        )?;
    }
    Ok(())
}

/// Appends message `index`'s rows to `out`, at most `budget` rows in all:
/// its header; unless collapsed, each section's title row and, unless the
/// section is collapsed, its text wrapped to `columns`; and the gap below.
fn layout(
    index: usize,
    message: &Message,
    columns: usize,
    budget: usize,
    out: &mut Vec<Line>,
) -> Result<(), Error> {
    let budget = out.len().saturating_add(budget);
    push(out, budget, row(index, 0, Kind::Header, 0, 0)?)?;
    if !message.collapsed {
        for (number, section) in message.sections.iter().enumerate() {
            if section.title.is_some() {
                push(out, budget, row(index, number, Kind::Title, 0, 0)?)?;
                if section.collapsed {
                    continue;
                }
            }
            let wrap = Wrap {
                message: index,
                section: number,
                target: section,
                columns,
                budget,
            };
            wrap_section(wrap, 0, 0, out)?;
        }
    }
    push(out, budget, row(index, 0, Kind::Gap, 0, 0)?)
}

/// Wraps `text` into rows of at most `columns` scalars, handing each row's
/// byte range to `row` until it answers false. A newline ends a row and
/// belongs to none; a row breaks after the last space or tab that leaves
/// its word whole, else within the word; a space or tab reaching past the
/// last column stays on its row, unshown, so rows tile the text between
/// newlines. Empty text, or text ending in a newline, ends in an empty row;
/// text ending in such an unshown space does not.
fn wrap(
    text: &str,
    columns: usize,
    row: &mut dyn FnMut(usize, usize) -> Result<bool, Error>,
) -> Result<(), Error> {
    let columns = columns.max(1);
    let mut start = 0;
    let mut column = 0;
    let mut space: Option<(usize, usize)> = None;
    let mut overflowed = false;
    for (at, scalar) in text.char_indices() {
        // A row ended by an unshown space is ended; a newline right after
        // it starts the next and adds no empty row.
        let ended = std::mem::take(&mut overflowed);
        let blank = scalar == ' ' || scalar == '\t';
        if scalar == '\n' {
            if !ended && !row(start, at)? {
                return Ok(());
            }
            start = at + 1;
            column = 0;
            space = None;
            continue;
        }
        if column >= columns {
            if blank {
                let next = at + scalar.len_utf8();
                if !row(start, next)? {
                    return Ok(());
                }
                start = next;
                column = 0;
                space = None;
                overflowed = true;
                continue;
            }
            match space {
                Some((after, at_column)) if after > start => {
                    if !row(start, after)? {
                        return Ok(());
                    }
                    start = after;
                    column = column.saturating_sub(at_column);
                }
                _ => {
                    if !row(start, at)? {
                        return Ok(());
                    }
                    start = at;
                    column = 0;
                }
            }
            space = None;
        }
        column += 1;
        if blank {
            space = Some((at + scalar.len_utf8(), column));
        }
    }
    if !overflowed {
        row(start, text.len())?;
    }
    Ok(())
}

//! Bounded Markdown structure: CommonMark's blocks and inlines, as a chat
//! model writes them, reported as byte ranges of the source so a renderer
//! draws the source's own text and a copy takes the source.
#![forbid(unsafe_code)]

use std::ops::Range;

mod block;
mod inline;
#[cfg(test)]
mod tests;

/// The longest source parsed, in bytes: a chat message's, as long as
/// td-ui's message list holds in one section.
pub const MAX_BYTES: usize = 1024 * 1024;
/// The deepest nesting of block quotes and list items; a marker past it is
/// text.
pub const MAX_DEPTH: usize = 32;

/// Why a source was refused.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    /// Longer than `MAX_BYTES`.
    TooLong,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooLong => write!(f, "longer than {MAX_BYTES} bytes"),
        }
    }
}

impl std::error::Error for Error {}

/// A parsed source: its leaf blocks in order, each with the containers it
/// sits in (`levels`) and its inline spans, and the links they name.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Document {
    pub blocks: Vec<Block>,
    pub levels: Vec<Level>,
    pub spans: Vec<Span>,
    pub links: Vec<Link>,
}

/// One leaf block.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Block {
    pub kind: BlockKind,
    /// Its containers, outermost first, in `Document::levels`.
    pub levels: Range<usize>,
    /// Its inline spans, in `Document::spans`.
    pub spans: Range<usize>,
    /// The source it was parsed from: from its first line's first byte
    /// past its containers' markers (and an indented code block's
    /// indentation) to its last line's end, a fenced block's closing fence
    /// included; its containers' markers on its later lines lie inside it.
    /// An item with no block of its own has its marker's.
    pub source: Range<usize>,
    /// Whether a blank line came between it and the block before.
    pub blank_before: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BlockKind {
    Paragraph,
    /// ATX (`#` to `######`) or setext, its level 1 to 6.
    Heading(u8),
    /// Fenced or indented; `info` is a fence's info string, empty for none.
    /// Its spans are its lines as written, `HardBreak` between.
    Code {
        info: Range<usize>,
    },
    /// A thematic break; no spans.
    Rule,
    /// A pipe table, its lines as written, `HardBreak` between.
    Table,
}

/// A container a block sits in.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Level {
    Quote,
    /// A list item; `first` for the item's first block, where its marker
    /// shows.
    Item {
        marker: Marker,
        first: bool,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Marker {
    /// `-`, `+` or `*`.
    Bullet(char),
    /// A number and its delimiter, `.` or `)`.
    Ordered { number: u64, delimiter: char },
}

/// A run of a block's inline content.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Span {
    pub kind: SpanKind,
    /// The source bytes it shows: its text with markup and escapes'
    /// backslashes left out; an image's alt text as written; empty for a
    /// break, at its line's end.
    pub range: Range<usize>,
    pub style: Style,
    /// The link it is in, in `Document::links`.
    pub link: Option<usize>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SpanKind {
    Text,
    /// A code span's content.
    Code,
    /// A line ending inside a paragraph, shown as a space.
    SoftBreak,
    /// A hard line break, or a line ending in a code block or table.
    HardBreak,
    /// An image: `range` its alt text up to its first line's end, `source`
    /// its destination in `Document::links`.
    Image {
        source: usize,
    },
}

/// Emphasis a span is in, as bits.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Hash)]
pub struct Style(u8);

impl Style {
    pub const PLAIN: Self = Self(0);
    pub const EMPHASIS: Self = Self(1);
    pub const STRONG: Self = Self(2);
    pub const STRIKE: Self = Self(4);

    pub fn has(self, other: Self) -> bool {
        self.0 & other.0 == other.0 && other.0 != 0
    }

    pub fn with(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
}

/// A link's or image's destination and title, as written (an escape's
/// backslash kept).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Link {
    pub destination: Range<usize>,
    pub title: Option<Range<usize>>,
}

/// Parses `source`.
pub fn parse(source: &str) -> Result<Document, Error> {
    if source.len() > MAX_BYTES {
        return Err(Error::TooLong);
    }
    Ok(block::Parser::new(source).run())
}

impl Document {
    /// A block's containers.
    pub fn levels(&self, block: &Block) -> &[Level] {
        self.levels.get(block.levels.clone()).unwrap_or(&[])
    }

    /// A block's spans.
    pub fn spans(&self, block: &Block) -> &[Span] {
        self.spans.get(block.spans.clone()).unwrap_or(&[])
    }
}

/// The source text `range` names, or nothing for a range outside it.
pub fn text<'a>(source: &'a str, range: &Range<usize>) -> &'a str {
    source.get(range.clone()).unwrap_or("")
}

//! Styled text a message list section shows (`messages`): the text it
//! shows, built piece by piece by its owner from a source text (its
//! Markdown, say), with each shown byte's place in the source, the looks
//! of its runs, its blocks' prefixes and grounds, and the links it holds.
//! A copy of shown text takes the source it was built from, so what is
//! copied is what was written. Nothing here parses: the owner says what
//! each piece is. Each addition is checked and its room reserved before
//! anything changes, so a refused one leaves the text as it was. It holds
//! at most `MAX_BYTES` of shown text, of source and of prefixes,
//! `MAX_LINK_BYTES` of link targets and `MAX_RECORDS` records.

use std::ops::Range;

/// The shown text, the source text and the prefixes each, in bytes.
pub const MAX_BYTES: usize = crate::clipboard::MAX_BYTES;
/// The characters a block's prefix has.
pub const MAX_PREFIX_CHARS: usize = 64;
/// The bytes every link target holds together.
pub const MAX_LINK_BYTES: usize = 256 * 1024;
/// The records it keeps together: shown stretches, looks, blocks and
/// links.
pub const MAX_RECORDS: usize = 1 << 17;

/// Why a piece was refused.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    /// Past a bound above, or an allocation failed.
    Limit,
    /// A piece of text holds a line ending, a prefix a control character,
    /// a source range is reversed, outside the source or off a character
    /// boundary, or a link is unknown.
    Invalid,
    /// A block's later rows' prefix is not as wide as its first row's.
    Prefix,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Limit => "styled text limit",
            Self::Invalid => "invalid styled text piece",
            Self::Prefix => "styled block prefixes of unequal width",
        })
    }
}

impl std::error::Error for Error {}

/// How a run of shown text looks, as bits.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Hash)]
pub struct Look(u8);

impl Look {
    pub const PLAIN: Self = Self(0);
    pub const BOLD: Self = Self(1);
    pub const ITALIC: Self = Self(2);
    pub const STRIKE: Self = Self(4);
    /// Code, on a ground of its own.
    pub const CODE: Self = Self(8);
    /// Quieter ink: a quotation, an image's stand-in.
    pub const QUIET: Self = Self(16);

    pub fn has(self, other: Self) -> bool {
        other.0 != 0 && self.0 & other.0 == other.0
    }

    pub fn with(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
}

/// A stretch of shown text from `shown` on, standing for source bytes
/// `source..until`: byte for byte when `verbatim`, else as a whole.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Segment {
    shown: u32,
    source: u32,
    until: u32,
    verbatim: bool,
}

/// Shown text from `shown` on looks `look`, in link `link`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Run {
    shown: u32,
    look: Look,
    link: Option<u32>,
}

/// A block from shown byte `start` on: its first row's prefix and its
/// later rows', ranges of `Rich::prefixes`, as many characters each, and
/// whether its rows stand on the code ground.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Block {
    pub(crate) start: u32,
    first: Range<u32>,
    rest: Range<u32>,
    pub(crate) width: u16,
    pub(crate) code: bool,
}

/// A section's styled text: build it with `block`, `gap`, `text`, `put`,
/// `line` and `link`, then hand it to the list (`Message::rich`,
/// `Controller::set_rich`).
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Rich {
    source: String,
    shown: String,
    segments: Vec<Segment>,
    runs: Vec<Run>,
    blocks: Vec<Block>,
    prefixes: String,
    links: Vec<Range<u32>>,
    targets: String,
}

/// A piece checked and its room reserved (`Rich::prepare`): where it goes
/// and what it stands for, so that adding it cannot fail.
#[derive(Clone, Copy)]
struct Place {
    shown: u32,
    prefixes: u32,
    source: u32,
    until: u32,
    link: Option<u32>,
}

fn reserve<T>(vec: &mut Vec<T>, more: usize) -> Result<(), Error> {
    vec.try_reserve(more).map_err(|_| Error::Limit)
}

/// Room for `more` bytes in `to`, at most `bound` in all.
fn room(to: &mut String, more: usize, bound: usize) -> Result<(), Error> {
    if to.len().saturating_add(more) > bound {
        return Err(Error::Limit);
    }
    to.try_reserve(more).map_err(|_| Error::Limit)
}

fn offset(at: usize) -> Result<u32, Error> {
    u32::try_from(at).map_err(|_| Error::Limit)
}

/// Shown text holds no line ending: a row ends at a block or a `line`.
/// A tab shows as a space and another control character as the
/// replacement character, as in a plain section.
fn one_line(text: &str) -> Result<(), Error> {
    if text.contains('\n') {
        return Err(Error::Invalid);
    }
    Ok(())
}

/// A prefix holds no control character.
fn plain(text: &str) -> Result<(), Error> {
    if text.chars().any(char::is_control) {
        return Err(Error::Invalid);
    }
    Ok(())
}

impl Rich {
    /// Styled text over `source`, showing nothing yet.
    pub fn new(source: &str) -> Result<Self, Error> {
        let mut rich = Self::default();
        room(&mut rich.source, source.len(), MAX_BYTES)?;
        rich.source.push_str(source);
        Ok(rich)
    }

    pub fn source(&self) -> &str {
        &self.source
    }

    pub fn shown(&self) -> &str {
        &self.shown
    }

    /// Starts a block on a row of its own, its first row behind `first`
    /// and its later ones behind `rest`, which must be as many characters,
    /// on the code ground when `code`. The line ending before it stands
    /// for nothing, where the source shown so far ends, so a copy to the
    /// end of a row takes none of the next block's markup.
    pub fn block(&mut self, first: &str, rest: &str, code: bool) -> Result<(), Error> {
        plain(first)?;
        plain(rest)?;
        let width = first.chars().count();
        if rest.chars().count() != width {
            return Err(Error::Prefix);
        }
        if width > MAX_PREFIX_CHARS {
            return Err(Error::Limit);
        }
        let width = u16::try_from(width).map_err(|_| Error::Limit)?;
        let end = self.end();
        let mut place = self.prepare(1, end..end, None)?;
        let middle = offset(self.prefixes.len().saturating_add(first.len()))?;
        let last = offset(self.prefixes.len().saturating_add(first.len() + rest.len()))?;
        room(&mut self.prefixes, first.len() + rest.len(), MAX_BYTES)?;
        // Checked and reserved: nothing below fails.
        if !self.blocks.is_empty() {
            self.add("\n", place, Look::PLAIN, false);
            place.shown += 1;
        }
        self.prefixes.push_str(first);
        self.prefixes.push_str(rest);
        self.blocks.push(Block {
            start: place.shown,
            first: place.prefixes..middle,
            rest: middle..last,
            width,
            code,
        });
        Ok(())
    }

    /// A block with no prefix: a row of its own, empty unless text
    /// follows it before the next block.
    pub fn gap(&mut self) -> Result<(), Error> {
        self.block("", "", false)
    }

    /// Shows source bytes `range` as they are, looking `look`, in link
    /// `link` when given.
    pub fn text(
        &mut self,
        range: Range<usize>,
        look: Look,
        link: Option<usize>,
    ) -> Result<(), Error> {
        let length = {
            let piece = self.source.get(range.clone()).ok_or(Error::Invalid)?;
            one_line(piece)?;
            piece.len()
        };
        let place = self.prepare(length, range.clone(), link)?;
        if length > 0 {
            self.open(place);
            self.add_source(range, place, look);
        }
        Ok(())
    }

    /// Shows `text`, which the source does not hold as such (a break's
    /// space, an image's stand-in), standing for source bytes `range`
    /// whole: a copy reaching into it takes all of them.
    pub fn put(
        &mut self,
        text: &str,
        range: Range<usize>,
        look: Look,
        link: Option<usize>,
    ) -> Result<(), Error> {
        one_line(text)?;
        let place = self.prepare(text.len(), range, link)?;
        if !text.is_empty() {
            self.open(place);
            self.add(text, place, look, false);
        }
        Ok(())
    }

    /// Ends the row inside a block, standing for source bytes `range` (a
    /// hard break's): its next row is one of the block's later rows.
    pub fn line(&mut self, range: Range<usize>) -> Result<(), Error> {
        let place = self.prepare(1, range, None)?;
        self.open(place);
        self.add("\n", place, Look::PLAIN, false);
        Ok(())
    }

    /// A link to `target`, for `text` and `put` to name.
    pub fn link(&mut self, target: &str) -> Result<usize, Error> {
        if self.records() >= MAX_RECORDS {
            return Err(Error::Limit);
        }
        let start = offset(self.targets.len())?;
        let end = offset(self.targets.len().saturating_add(target.len()))?;
        room(&mut self.targets, target.len(), MAX_LINK_BYTES)?;
        reserve(&mut self.links, 1)?;
        self.targets.push_str(target);
        self.links.push(start..end);
        Ok(self.links.len() - 1)
    }

    /// Link `index`'s target.
    pub fn target(&self, index: usize) -> Option<&str> {
        let range = self.links.get(index)?;
        self.targets.get(range.start as usize..range.end as usize)
    }

    fn records(&self) -> usize {
        self.segments.len() + self.runs.len() + self.blocks.len() + self.links.len()
    }

    /// Checks a piece of `length` shown bytes standing for source bytes
    /// `range` in link `link`, and reserves its room, with a line ending
    /// and a block before it.
    fn prepare(
        &mut self,
        length: usize,
        range: Range<usize>,
        link: Option<usize>,
    ) -> Result<Place, Error> {
        if range.start > range.end
            || !self.source.is_char_boundary(range.start)
            || !self.source.is_char_boundary(range.end)
        {
            return Err(Error::Invalid);
        }
        let link = match link {
            Some(index) if index < self.links.len() => Some(offset(index)?),
            Some(_) => return Err(Error::Invalid),
            None => None,
        };
        // A segment and a run for a line ending and for the piece, and a
        // block.
        if self.records().saturating_add(5) > MAX_RECORDS {
            return Err(Error::Limit);
        }
        let more = length.saturating_add(1);
        let place = Place {
            shown: offset(self.shown.len())?,
            prefixes: offset(self.prefixes.len())?,
            source: offset(range.start)?,
            until: offset(range.end)?,
            link,
        };
        offset(self.shown.len().saturating_add(more))?;
        room(&mut self.shown, more, MAX_BYTES)?;
        reserve(&mut self.segments, 2)?;
        reserve(&mut self.runs, 2)?;
        reserve(&mut self.blocks, 1)?;
        Ok(place)
    }

    /// Text shown before any block is in one with no prefix.
    fn open(&mut self, place: Place) {
        if self.blocks.is_empty() {
            self.blocks.push(Block {
                start: place.shown,
                first: place.prefixes..place.prefixes,
                rest: place.prefixes..place.prefixes,
                width: 0,
                code: false,
            });
        }
    }

    /// Adds `text`, its room reserved, at the shown text's end.
    fn add(&mut self, text: &str, place: Place, look: Look, verbatim: bool) {
        self.mark(place, look, verbatim);
        self.shown.push_str(text);
    }

    /// Adds source bytes `range`, its room reserved, as they are.
    fn add_source(&mut self, range: Range<usize>, place: Place, look: Look) {
        self.mark(place, look, true);
        if let Some(piece) = self.source.get(range) {
            self.shown.push_str(piece);
        }
    }

    /// Records the segment and run of a piece starting at the shown
    /// text's end.
    fn mark(&mut self, place: Place, look: Look, verbatim: bool) {
        let shown = u32::try_from(self.shown.len()).unwrap_or(place.shown);
        // A verbatim piece right after another, contiguous in both,
        // extends it.
        match self.segments.last_mut() {
            Some(last) if verbatim && last.verbatim && last.until == place.source => {
                last.until = place.until;
            }
            _ => self.segments.push(Segment {
                shown,
                source: place.source,
                until: place.until,
                verbatim,
            }),
        }
        let link = place.link;
        if !self
            .runs
            .last()
            .is_some_and(|run| run.look == look && run.link == link)
        {
            self.runs.push(Run { shown, look, link });
        }
    }

    /// Where the source shown so far ends.
    fn end(&self) -> usize {
        self.segments
            .last()
            .map_or(0, |segment| segment.until as usize)
    }

    /// The segment holding shown byte `at`.
    fn segment(&self, at: usize) -> Option<&Segment> {
        let index = self
            .segments
            .partition_point(|segment| segment.shown as usize <= at)
            .checked_sub(1)?;
        self.segments.get(index)
    }

    /// Where a copy from shown byte `at` starts in the source: within a
    /// verbatim stretch the byte it shows, within another the start of
    /// what it stands for.
    fn start_of(&self, at: usize) -> usize {
        if at >= self.shown.len() {
            return self.end();
        }
        self.segment(at).map_or(0, |segment| {
            let source = segment.source as usize;
            if segment.verbatim {
                source + (at - segment.shown as usize)
            } else {
                source
            }
        })
    }

    /// Where a copy to shown byte `at` ends in the source, by the byte
    /// before it: within a verbatim stretch past the byte it shows,
    /// within another the end of what it stands for.
    fn end_of(&self, at: usize) -> usize {
        let Some(before) = at.checked_sub(1) else {
            return self.start_of(0);
        };
        self.segment(before).map_or(0, |segment| {
            if segment.verbatim {
                segment.source as usize + (at - segment.shown as usize)
            } else {
                segment.until as usize
            }
        })
    }

    /// The source text shown bytes `from..to` stand for: all of it for
    /// the whole shown text, its leading and trailing markup included, so
    /// a selection of it all copies the source whole.
    pub(crate) fn source_slice(&self, from: usize, to: usize) -> &str {
        if from == 0 && to >= self.shown.len() {
            return &self.source;
        }
        let start = self.start_of(from);
        let end = self.end_of(to).max(start);
        self.source.get(start..end).unwrap_or("")
    }

    /// How shown byte `at` looks, and its link.
    pub(crate) fn look(&self, at: usize) -> (Look, Option<usize>) {
        let index = self
            .runs
            .partition_point(|run| run.shown as usize <= at)
            .saturating_sub(1);
        self.runs.get(index).map_or((Look::PLAIN, None), |run| {
            (run.look, run.link.map(|link| link as usize))
        })
    }

    /// The runs' starts at or after shown byte `from` and before `to`,
    /// each with its look and link.
    pub(crate) fn changes(
        &self,
        from: usize,
        to: usize,
    ) -> impl Iterator<Item = (usize, Look, Option<usize>)> + '_ {
        let first = self
            .runs
            .partition_point(|run| (run.shown as usize) <= from);
        self.runs
            .get(first..)
            .unwrap_or(&[])
            .iter()
            .take_while(move |run| (run.shown as usize) < to)
            .map(|run| (run.shown as usize, run.look, run.link.map(|l| l as usize)))
    }

    /// The block shown byte `at` lies in.
    pub(crate) fn block_at(&self, at: usize) -> Option<&Block> {
        let index = self
            .blocks
            .partition_point(|block| block.start as usize <= at)
            .checked_sub(1)?;
        self.blocks.get(index)
    }

    pub(crate) fn blocks(&self) -> &[Block] {
        &self.blocks
    }

    /// A block's prefix on its first row, or on a later one.
    pub(crate) fn prefix(&self, block: &Block, first: bool) -> &str {
        let range = if first { &block.first } else { &block.rest };
        self.prefixes
            .get(range.start as usize..range.end as usize)
            .unwrap_or("")
    }

    /// The bytes it holds besides its shown text: its source, prefixes
    /// and targets, and the room its records take.
    pub(crate) fn extra_bytes(&self) -> usize {
        use std::mem::size_of;
        self.source.len()
            + self.prefixes.len()
            + self.targets.len()
            + self.segments.capacity() * size_of::<Segment>()
            + self.runs.capacity() * size_of::<Run>()
            + self.blocks.capacity() * size_of::<Block>()
            + self.links.capacity() * size_of::<Range<u32>>()
    }
}

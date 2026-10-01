//! A single-line text entry's editing state for `chrome::TextEntry`: the
//! bounded text, the caret and an optional selection anchor in character
//! columns, and the first shown column. The consumer owns its key bindings
//! and hands `Action`s in; `Action::from_chord` is the default set.
//! Masking is the painter's display option and the refusal of copy and
//! cut, not a trust boundary: an authentication field stays on the path
//! its consumer's design names.

use crate::chrome::{Field, TextEntry};
use crate::editor_model::wipe;
use std::sync::Arc;

/// The largest byte limit an entry takes, the clipboard's ceiling.
pub const MAX_LIMIT: usize = crate::editor_clipboard::MAX_BYTES;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Refusal {
    /// The text would exceed the entry's byte limit, or a limit exceeds
    /// `MAX_LIMIT` or cannot be reserved.
    Limit,
    /// The text holds a control character (newline, CR and tab included)
    /// or a line or paragraph separator.
    Control,
    /// Copy and cut refuse on a masked entry.
    Masked,
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Limit => "the entry is full",
            Self::Control => "the entry takes one line of printable text",
            Self::Masked => "a masked entry does not copy",
        })
    }
}

impl std::error::Error for Refusal {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Outcome {
    /// The text changed.
    Changed,
    /// Only the caret or the selection moved.
    Moved,
    Ignored,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Motion {
    Left,
    Right,
    /// To the start of the alphanumeric run at or before the caret.
    WordLeft,
    /// To the end of the alphanumeric run at or after the caret.
    WordRight,
    Home,
    End,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Action {
    /// `extend` keeps or sets the anchor, so the selection grows.
    Move {
        motion: Motion,
        extend: bool,
    },
    SelectAll,
    Backspace,
    Delete,
    /// Deletes back to where `Motion::WordLeft` goes, or the selection.
    DeleteWordLeft,
    /// Deletes up to where `Motion::WordRight` goes, or the selection.
    DeleteWordRight,
    Insert(char),
}

impl Action {
    /// The default bindings: arrows, `C-` for words and `S-` to extend,
    /// Home and End, Backspace and Delete with `C-` for words, `C-a` to
    /// select all, and one printable character to insert. Clipboard
    /// chords and Return stay with the consumer.
    pub fn from_chord(chord: &str) -> Option<Self> {
        let (word, rest) = match chord.strip_prefix("C-") {
            Some(rest) => (true, rest),
            None => (false, chord),
        };
        let (extend, key) = match rest.strip_prefix("S-") {
            Some(key) => (true, key),
            None => (false, rest),
        };
        let motion = match (word, key) {
            (false, "Left") => Some(Motion::Left),
            (false, "Right") => Some(Motion::Right),
            (true, "Left") => Some(Motion::WordLeft),
            (true, "Right") => Some(Motion::WordRight),
            (false, "Home") => Some(Motion::Home),
            (false, "End") => Some(Motion::End),
            _ => None,
        };
        if let Some(motion) = motion {
            return Some(Self::Move { motion, extend });
        }
        match chord {
            "C-a" => Some(Self::SelectAll),
            "Backspace" => Some(Self::Backspace),
            "Delete" => Some(Self::Delete),
            "C-Backspace" => Some(Self::DeleteWordLeft),
            "C-Delete" => Some(Self::DeleteWordRight),
            _ => {
                let mut chars = chord.chars();
                match (chars.next(), chars.next()) {
                    (Some(c), None) if !refused(c) => Some(Self::Insert(c)),
                    _ => None,
                }
            }
        }
    }
}

/// What a single-line entry never holds.
fn refused(c: char) -> bool {
    c.is_control() || matches!(c, '\u{2028}' | '\u{2029}')
}

/// The entry's state. Its buffer is reserved at the limit so an edit never
/// reallocates and abandons a copy. The bytes an edit leaves past the new
/// end are zeroed in place, and the whole buffer on drop: best effort and
/// not erasure. Copies outside the model are out of its reach: a key's
/// chord, the source of a paste, and the `Arc<str>` copy and cut return.
/// `Debug` shows the length, never the text.
pub struct EntryModel {
    text: String,
    limit: usize,
    caret: usize,
    anchor: Option<usize>,
    first: usize,
    masked: bool,
}

impl std::fmt::Debug for EntryModel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EntryModel")
            .field("bytes", &self.text.len())
            .field("limit", &self.limit)
            .field("caret", &self.caret)
            .field("anchor", &self.anchor)
            .field("first", &self.first)
            .field("masked", &self.masked)
            .finish()
    }
}

impl EntryModel {
    /// An empty entry holding at most `limit` bytes, reserved now; a limit
    /// past `MAX_LIMIT` or one the allocator refuses is `Limit`.
    pub fn new(limit: usize) -> Result<Self, Refusal> {
        if limit > MAX_LIMIT {
            return Err(Refusal::Limit);
        }
        let mut text = String::new();
        text.try_reserve_exact(limit).map_err(|_| Refusal::Limit)?;
        Ok(Self {
            text,
            limit,
            caret: 0,
            anchor: None,
            first: 0,
            masked: false,
        })
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn limit(&self) -> usize {
        self.limit
    }

    /// The caret's character column.
    pub fn caret(&self) -> usize {
        self.caret
    }

    pub fn anchor(&self) -> Option<usize> {
        self.anchor
    }

    /// The first shown character column, as `reveal` last left it.
    pub fn first(&self) -> usize {
        self.first
    }

    pub fn masked(&self) -> bool {
        self.masked
    }

    /// Masking paints a mask glyph per character, refuses copy and cut and
    /// makes word motions go to the ends, so neither the text nor where
    /// its words break is shown.
    pub fn set_masked(&mut self, masked: bool) {
        self.masked = masked;
    }

    /// The selected character columns, start before end; `None` when
    /// nothing is selected.
    pub fn selection(&self) -> Option<(usize, usize)> {
        let anchor = self.anchor?;
        (anchor != self.caret).then(|| (anchor.min(self.caret), anchor.max(self.caret)))
    }

    fn len(&self) -> usize {
        self.text.chars().count()
    }

    /// What a repaint shows of the caret and selection.
    fn shown(&self) -> (usize, Option<(usize, usize)>) {
        (self.caret, self.selection())
    }

    fn moved(&self, before: (usize, Option<(usize, usize)>)) -> Outcome {
        if self.shown() == before {
            Outcome::Ignored
        } else {
            Outcome::Moved
        }
    }

    fn byte(&self, column: usize) -> usize {
        self.text
            .char_indices()
            .nth(column)
            .map_or(self.text.len(), |(index, _)| index)
    }

    fn destination(&self, motion: Motion) -> usize {
        let len = self.len();
        let words = |c: char| c.is_alphanumeric();
        match motion {
            Motion::Left => self.caret.saturating_sub(1),
            Motion::Right => (self.caret + 1).min(len),
            Motion::Home => 0,
            Motion::End => len,
            Motion::WordLeft if self.masked => 0,
            Motion::WordRight if self.masked => len,
            Motion::WordLeft => {
                let mut column = self.caret;
                let mut word = false;
                let before = self.text.get(..self.byte(self.caret)).unwrap_or("");
                for c in before.chars().rev() {
                    if word && !words(c) {
                        break;
                    }
                    word |= words(c);
                    column -= 1;
                }
                column
            }
            Motion::WordRight => {
                let mut column = self.caret;
                let mut word = false;
                for c in self.text.chars().skip(self.caret) {
                    if word && !words(c) {
                        break;
                    }
                    word |= words(c);
                    column += 1;
                }
                column
            }
        }
    }

    /// Replaces columns `start..end` with `insert`, whole or refused, and
    /// leaves the caret after it with no selection and the first shown
    /// column inside the text.
    fn replace(&mut self, start: usize, end: usize, insert: &str) -> Result<Outcome, Refusal> {
        if insert.chars().any(refused) {
            return Err(Refusal::Control);
        }
        let (from, to) = (self.byte(start), self.byte(end));
        let removed = to.saturating_sub(from);
        if self.text.len() - removed + insert.len() > self.limit {
            return Err(Refusal::Limit);
        }
        if removed == 0 && insert.is_empty() {
            return Ok(Outcome::Ignored);
        }
        let before = self.text.len();
        self.text.replace_range(from..to, insert);
        scrub_tail(&mut self.text, before);
        self.caret = start + insert.chars().count();
        self.anchor = None;
        self.first = self.first.min(self.len());
        Ok(Outcome::Changed)
    }

    fn remove_selection(&mut self) -> Option<Result<Outcome, Refusal>> {
        let (start, end) = self.selection()?;
        Some(self.replace(start, end, ""))
    }

    pub fn act(&mut self, action: Action) -> Result<Outcome, Refusal> {
        match action {
            Action::Move { motion, extend } => {
                let before = self.shown();
                let caret = match (self.selection(), extend, motion) {
                    (Some((start, _)), false, Motion::Left) => start,
                    (Some((_, end)), false, Motion::Right) => end,
                    _ => self.destination(motion),
                };
                self.anchor = extend.then(|| self.anchor.unwrap_or(self.caret));
                self.caret = caret;
                Ok(self.moved(before))
            }
            Action::SelectAll => {
                let len = self.len();
                if len == 0 || self.selection() == Some((0, len)) {
                    return Ok(Outcome::Ignored);
                }
                self.anchor = Some(0);
                self.caret = len;
                Ok(Outcome::Moved)
            }
            Action::Backspace => self.remove_selection().unwrap_or_else(|| {
                let start = self.caret.saturating_sub(1);
                self.replace(start, self.caret, "")
            }),
            Action::Delete => self.remove_selection().unwrap_or_else(|| {
                let end = (self.caret + 1).min(self.len());
                self.replace(self.caret, end, "")
            }),
            Action::DeleteWordLeft => self.remove_selection().unwrap_or_else(|| {
                let start = self.destination(Motion::WordLeft);
                self.replace(start, self.caret, "")
            }),
            Action::DeleteWordRight => self.remove_selection().unwrap_or_else(|| {
                let end = self.destination(Motion::WordRight);
                self.replace(self.caret, end, "")
            }),
            Action::Insert(c) => self.paste(c.encode_utf8(&mut [0; 4])),
        }
    }

    /// Inserts `text` over the selection or at the caret, whole or refused:
    /// a control character or a result past the limit changes nothing.
    pub fn paste(&mut self, text: &str) -> Result<Outcome, Refusal> {
        let (start, end) = self.selection().unwrap_or((self.caret, self.caret));
        self.replace(start, end, text)
    }

    /// The selected text for the clipboard; `None` when nothing is
    /// selected.
    pub fn copy(&self) -> Result<Option<Arc<str>>, Refusal> {
        if self.masked {
            return Err(Refusal::Masked);
        }
        let Some((start, end)) = self.selection() else {
            return Ok(None);
        };
        Ok(self
            .text
            .get(self.byte(start)..self.byte(end))
            .map(Arc::from))
    }

    /// `copy`, then the selection removed.
    pub fn cut(&mut self) -> Result<Option<Arc<str>>, Refusal> {
        let copied = self.copy()?;
        if copied.is_some() {
            self.remove_selection();
        }
        Ok(copied)
    }

    /// Replaces the whole text, whole or refused, with the caret at its end
    /// and no selection; the same text only moves the caret and selection
    /// there.
    pub fn set_text(&mut self, text: &str) -> Result<Outcome, Refusal> {
        let len = self.len();
        if text == self.text {
            let before = self.shown();
            self.caret = len;
            self.anchor = None;
            return Ok(self.moved(before));
        }
        self.replace(0, len, text)
    }

    /// Forgets the text, zeroing it, with the caret, selection and scroll.
    pub fn clear(&mut self) -> Outcome {
        let changed = !self.text.is_empty();
        let before = self.text.len();
        self.text.clear();
        scrub_tail(&mut self.text, before);
        // With no text the caret and first column are already 0; only an
        // anchor that shows nothing can be left to forget.
        self.caret = 0;
        self.anchor = None;
        self.first = 0;
        if changed {
            Outcome::Changed
        } else {
            Outcome::Ignored
        }
    }

    /// Places the caret at a pointer point in `entry`; `extend` keeps or
    /// sets the anchor, as a shifted press does. A point outside the field
    /// is ignored.
    pub fn place(&mut self, entry: TextEntry, x: i64, y: i64, extend: bool) -> Outcome {
        let Some(column) = entry.hit(x, y, self.first, self.len()) else {
            return Outcome::Ignored;
        };
        self.extend_to(column, extend)
    }

    /// Extends the selection to a dragged point anywhere: inside the field
    /// as `place`, and past its left or right edge one column beyond the
    /// shown ones, so each drag event `reveal` follows scrolls one column.
    pub fn drag(&mut self, entry: TextEntry, x: i64, y: i64) -> Outcome {
        let rect = entry.rect();
        let len = self.len();
        let column = if x < rect.x {
            self.first.saturating_sub(1)
        } else if x >= rect.x.saturating_add(i64::from(rect.width)) {
            self.first
                .saturating_add(entry.columns())
                .saturating_add(1)
                .min(len)
        } else {
            let top = rect.y;
            let bottom = rect
                .y
                .saturating_add(i64::from(rect.height))
                .saturating_sub(1);
            let Some(column) = entry.hit(x, y.clamp(top, bottom), self.first, len) else {
                return Outcome::Ignored;
            };
            column
        };
        self.extend_to(column, true)
    }

    fn extend_to(&mut self, column: usize, extend: bool) -> Outcome {
        let before = self.shown();
        self.anchor = extend.then(|| self.anchor.unwrap_or(self.caret));
        self.caret = column;
        self.moved(before)
    }

    /// Scrolls the first shown column as little as keeps the caret shown in
    /// `entry`.
    pub fn reveal(&mut self, entry: TextEntry) {
        self.first = entry.reveal(self.len(), self.caret, self.first);
    }

    /// What `TextEntry::emit` paints for this state.
    pub fn field<'a>(
        &'a self,
        placeholder: &'a str,
        focused: bool,
        caret_visible: bool,
    ) -> Field<'a> {
        Field {
            text: &self.text,
            placeholder,
            caret: self.caret,
            anchor: self.anchor,
            first: self.first,
            masked: self.masked,
            focused,
            caret_visible,
        }
    }
}

impl Drop for EntryModel {
    fn drop(&mut self) {
        wipe(&mut self.text);
    }
}

/// Zeroes the bytes past `text`'s length up to `before`, in place: the
/// capacity is the limit, so neither write reallocates.
fn scrub_tail(text: &mut String, before: usize) {
    let keep = text.len();
    if before > keep {
        text.extend(std::iter::repeat_n('\0', before - keep));
        std::hint::black_box(text.as_str());
        text.truncate(keep);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::raster::{Rect, Scale, Surface};

    fn typed(limit: usize, text: &str) -> EntryModel {
        let mut entry = EntryModel::new(limit).unwrap();
        for c in text.chars() {
            assert_eq!(entry.act(Action::Insert(c)), Ok(Outcome::Changed));
        }
        entry
    }

    fn chord(entry: &mut EntryModel, chord: &str) -> Result<Outcome, Refusal> {
        entry.act(Action::from_chord(chord).unwrap())
    }

    #[test]
    fn the_default_chords_name_motions_edits_and_one_character() {
        let moving = |motion, extend| Some(Action::Move { motion, extend });
        assert_eq!(Action::from_chord("Left"), moving(Motion::Left, false));
        assert_eq!(Action::from_chord("S-Right"), moving(Motion::Right, true));
        assert_eq!(
            Action::from_chord("C-Left"),
            moving(Motion::WordLeft, false)
        );
        assert_eq!(
            Action::from_chord("C-S-Right"),
            moving(Motion::WordRight, true)
        );
        assert_eq!(Action::from_chord("S-Home"), moving(Motion::Home, true));
        assert_eq!(Action::from_chord("End"), moving(Motion::End, false));
        assert_eq!(Action::from_chord("C-a"), Some(Action::SelectAll));
        assert_eq!(Action::from_chord("Backspace"), Some(Action::Backspace));
        assert_eq!(Action::from_chord("Delete"), Some(Action::Delete));
        assert_eq!(
            Action::from_chord("C-Backspace"),
            Some(Action::DeleteWordLeft)
        );
        assert_eq!(
            Action::from_chord("C-Delete"),
            Some(Action::DeleteWordRight)
        );
        assert_eq!(Action::from_chord("\u{2028}"), None);
        assert_eq!(Action::from_chord("é"), Some(Action::Insert('é')));
        assert_eq!(Action::from_chord(" "), Some(Action::Insert(' ')));
        assert_eq!(Action::from_chord("-"), Some(Action::Insert('-')));
        for other in ["C-Home", "M-Left", "C-c", "Return", "Tab", "\t", "", "ab"] {
            assert_eq!(Action::from_chord(other), None, "{other:?}");
        }
    }

    #[test]
    fn editing_moves_by_characters_and_replaces_a_selection() {
        let mut entry = typed(64, "aé中z");
        assert_eq!((entry.text(), entry.caret()), ("aé中z", 4));
        chord(&mut entry, "Left").unwrap();
        assert_eq!(chord(&mut entry, "Backspace"), Ok(Outcome::Changed));
        assert_eq!((entry.text(), entry.caret()), ("aéz", 2));
        chord(&mut entry, "Home").unwrap();
        assert_eq!(chord(&mut entry, "Backspace"), Ok(Outcome::Ignored));
        assert_eq!(chord(&mut entry, "Delete"), Ok(Outcome::Changed));
        assert_eq!(entry.text(), "éz");
        chord(&mut entry, "End").unwrap();
        assert_eq!(chord(&mut entry, "Delete"), Ok(Outcome::Ignored));
        assert_eq!(chord(&mut entry, "Right"), Ok(Outcome::Ignored));

        assert_eq!(chord(&mut entry, "S-Left"), Ok(Outcome::Moved));
        assert_eq!((entry.anchor(), entry.selection()), (Some(2), Some((1, 2))));
        chord(&mut entry, "x").unwrap();
        assert_eq!(
            (entry.text(), entry.caret(), entry.anchor()),
            ("éx", 2, None)
        );

        assert_eq!(chord(&mut entry, "C-a"), Ok(Outcome::Moved));
        assert_eq!(chord(&mut entry, "C-a"), Ok(Outcome::Ignored));
        assert_eq!(chord(&mut entry, "Delete"), Ok(Outcome::Changed));
        assert_eq!((entry.text(), entry.caret()), ("", 0));
        assert_eq!(chord(&mut entry, "C-a"), Ok(Outcome::Ignored));
    }

    #[test]
    fn word_deletes_take_the_run_or_the_selection() {
        let mut entry = typed(64, "user@example.com");
        assert_eq!(chord(&mut entry, "C-Backspace"), Ok(Outcome::Changed));
        assert_eq!((entry.text(), entry.caret()), ("user@example.", 13));
        chord(&mut entry, "Home").unwrap();
        assert_eq!(chord(&mut entry, "C-Delete"), Ok(Outcome::Changed));
        assert_eq!((entry.text(), entry.caret()), ("@example.", 0));
        assert_eq!(chord(&mut entry, "C-Backspace"), Ok(Outcome::Ignored));
        chord(&mut entry, "S-Right").unwrap();
        assert_eq!(chord(&mut entry, "C-Delete"), Ok(Outcome::Changed));
        assert_eq!(entry.text(), "example.");
        entry.set_masked(true);
        chord(&mut entry, "End").unwrap();
        chord(&mut entry, "Left").unwrap();
        assert_eq!(chord(&mut entry, "C-Backspace"), Ok(Outcome::Changed));
        assert_eq!((entry.text(), entry.caret()), (".", 0));
    }

    #[test]
    fn moved_means_the_caret_or_the_shown_selection_moved() {
        let mut entry = typed(64, "ab");
        chord(&mut entry, "Home").unwrap();
        assert_eq!(chord(&mut entry, "S-Left"), Ok(Outcome::Ignored));
        assert_eq!(entry.selection(), None);
        assert_eq!(chord(&mut entry, "S-Right"), Ok(Outcome::Moved));
        chord(&mut entry, "End").unwrap();
        assert_eq!(chord(&mut entry, "S-End"), Ok(Outcome::Ignored));
    }

    #[test]
    fn set_text_puts_the_caret_at_the_end_even_for_the_same_text() {
        let mut entry = typed(64, "abc");
        chord(&mut entry, "C-a").unwrap();
        assert_eq!(entry.set_text("abc"), Ok(Outcome::Moved));
        assert_eq!((entry.caret(), entry.selection()), (3, None));
        assert_eq!(entry.set_text("abc"), Ok(Outcome::Ignored));
        chord(&mut entry, "x").unwrap();
        assert_eq!(entry.text(), "abcx");
    }

    #[test]
    fn a_shorter_text_pulls_the_first_shown_column_in() {
        let mut entry = typed(64, "0123456789abcdef");
        entry.first = 6;
        entry.set_text("abc").unwrap();
        assert_eq!(entry.first(), 3);
        entry.set_text("").unwrap();
        assert_eq!(entry.first(), 0);
    }

    #[test]
    fn a_limit_past_the_ceiling_is_refused_and_debug_hides_the_text() {
        assert_eq!(EntryModel::new(MAX_LIMIT + 1).err(), Some(Refusal::Limit));
        assert_eq!(EntryModel::new(usize::MAX).err(), Some(Refusal::Limit));
        assert!(EntryModel::new(MAX_LIMIT).is_ok());
        let entry = typed(64, "hunter2");
        let shown = format!("{entry:?}");
        assert!(!shown.contains("hunter2"), "{shown}");
        assert!(shown.contains("bytes: 7"), "{shown}");
    }

    #[test]
    fn an_unextended_arrow_collapses_the_selection_to_its_side() {
        let mut entry = typed(64, "abcdef");
        chord(&mut entry, "Left").unwrap();
        chord(&mut entry, "S-Left").unwrap();
        chord(&mut entry, "S-Left").unwrap();
        assert_eq!(entry.selection(), Some((3, 5)));
        assert_eq!(chord(&mut entry, "Right"), Ok(Outcome::Moved));
        assert_eq!((entry.caret(), entry.selection()), (5, None));
        chord(&mut entry, "S-Left").unwrap();
        chord(&mut entry, "S-Left").unwrap();
        chord(&mut entry, "Left").unwrap();
        assert_eq!((entry.caret(), entry.selection()), (3, None));
    }

    #[test]
    fn word_motions_cross_alphanumeric_runs_unless_masked() {
        let mut entry = typed(64, "user@example.com  x");
        chord(&mut entry, "C-Left").unwrap();
        assert_eq!(entry.caret(), 18);
        chord(&mut entry, "C-Left").unwrap();
        assert_eq!(entry.caret(), 13);
        chord(&mut entry, "C-S-Left").unwrap();
        assert_eq!((entry.caret(), entry.selection()), (5, Some((5, 13))));
        chord(&mut entry, "Home").unwrap();
        chord(&mut entry, "C-Right").unwrap();
        assert_eq!(entry.caret(), 4);
        chord(&mut entry, "C-Right").unwrap();
        assert_eq!(entry.caret(), 12);

        entry.set_masked(true);
        chord(&mut entry, "C-Left").unwrap();
        assert_eq!(entry.caret(), 0);
        chord(&mut entry, "C-Right").unwrap();
        assert_eq!(entry.caret(), 19);
    }

    #[test]
    fn a_paste_is_whole_or_refused_with_nothing_changed() {
        let mut entry = typed(8, "abc");
        chord(&mut entry, "S-Left").unwrap();
        for (text, refusal) in [
            ("x\ny", Refusal::Control),
            ("x\ty", Refusal::Control),
            ("x\ry", Refusal::Control),
            ("\u{7f}", Refusal::Control),
            ("x\u{2028}y", Refusal::Control),
            ("x\u{2029}y", Refusal::Control),
            ("\u{85}", Refusal::Control),
            ("123456à", Refusal::Limit),
        ] {
            assert_eq!(entry.paste(text), Err(refusal), "{text:?}");
            assert_eq!((entry.text(), entry.selection()), ("abc", Some((2, 3))));
        }
        assert_eq!(entry.act(Action::Insert('\u{1b}')), Err(Refusal::Control));
        assert_eq!(entry.paste("1234é"), Ok(Outcome::Changed));
        assert_eq!((entry.text(), entry.caret()), ("ab1234é", 7));
        assert_eq!(entry.act(Action::Insert('!')), Err(Refusal::Limit));
        assert_eq!(entry.paste(""), Ok(Outcome::Ignored));
        assert_eq!(entry.set_text("abcdefghi"), Err(Refusal::Limit));
        assert_eq!(entry.set_text("new"), Ok(Outcome::Changed));
        assert_eq!(
            (entry.text(), entry.caret(), entry.anchor()),
            ("new", 3, None)
        );
        assert_eq!(entry.set_text("new"), Ok(Outcome::Ignored));
    }

    #[test]
    fn an_edit_never_moves_the_reserved_buffer() {
        let mut entry = EntryModel::new(16).unwrap();
        let buffer = entry.text().as_ptr();
        entry.paste("0123456789abcdef").unwrap();
        chord(&mut entry, "Home").unwrap();
        chord(&mut entry, "Delete").unwrap();
        entry.paste("Z").unwrap();
        entry.set_text("short").unwrap();
        entry.clear();
        entry.paste("0123456789abcdef").unwrap();
        assert_eq!(entry.text().as_ptr(), buffer);
    }

    #[test]
    fn copy_and_cut_take_the_selection_and_a_masked_entry_refuses_both() {
        let mut entry = typed(64, "secret pass");
        assert_eq!(entry.copy(), Ok(None));
        assert_eq!(entry.cut(), Ok(None));
        for _ in 0..4 {
            chord(&mut entry, "S-Left").unwrap();
        }
        assert_eq!(entry.copy().unwrap().as_deref(), Some("pass"));
        assert_eq!(entry.text(), "secret pass");
        assert_eq!(entry.cut().unwrap().as_deref(), Some("pass"));
        assert_eq!((entry.text(), entry.caret()), ("secret ", 7));

        entry.set_masked(true);
        chord(&mut entry, "C-a").unwrap();
        assert_eq!(entry.copy(), Err(Refusal::Masked));
        assert_eq!(entry.cut(), Err(Refusal::Masked));
        assert_eq!(entry.text(), "secret ");
        assert!(entry.field("", true, true).masked);
    }

    #[test]
    fn clear_forgets_the_text_caret_selection_and_scroll() {
        let mut entry = typed(64, "remember me");
        chord(&mut entry, "S-Home").unwrap();
        entry.first = 3;
        assert_eq!(entry.clear(), Outcome::Changed);
        assert_eq!(
            (entry.text(), entry.caret(), entry.anchor(), entry.first()),
            ("", 0, None, 0)
        );
        assert_eq!(entry.clear(), Outcome::Ignored);
    }

    #[test]
    fn the_pointer_places_and_extends_through_the_painted_columns() {
        let surface = Surface::new(200, 50, Scale::new(1).unwrap()).unwrap();
        let rect = Rect {
            x: 0,
            y: 0,
            width: 16 + 8 * 10,
            height: 24,
        };
        let painter = TextEntry::new(surface, rect).unwrap();
        assert_eq!(painter.columns(), 10);
        let mut entry = typed(64, "0123456789abcdef");
        entry.reveal(painter);
        assert_eq!(entry.first(), 6);
        // Column 2 of the field, past the 8-pixel inset, is text column 8.
        assert_eq!(
            entry.place(painter, 8 + 2 * 8 + 1, 5, false),
            Outcome::Moved
        );
        assert_eq!((entry.caret(), entry.anchor()), (8, None));
        assert_eq!(entry.place(painter, 8 + 5 * 8, 5, true), Outcome::Moved);
        assert_eq!(entry.selection(), Some((8, 11)));
        assert_eq!(entry.place(painter, 8 + 5 * 8, 5, true), Outcome::Ignored);
        assert_eq!(entry.place(painter, 500, 5, false), Outcome::Ignored);
        assert_eq!(entry.selection(), Some((8, 11)));

        // A drag past an edge reaches one column beyond the shown ones, so
        // each event `reveal` follows scrolls one column.
        assert_eq!(entry.drag(painter, 500, 500), Outcome::Moved);
        assert_eq!((entry.caret(), entry.anchor()), (16, Some(8)));
        entry.reveal(painter);
        assert_eq!(entry.first(), 6);
        assert_eq!(entry.drag(painter, -5, 5), Outcome::Moved);
        assert_eq!(entry.caret(), 5);
        entry.reveal(painter);
        assert_eq!(entry.first(), 5);
        entry.drag(painter, -5, 5);
        entry.reveal(painter);
        assert_eq!((entry.caret(), entry.first()), (4, 4));
        // Below the field, a drag still maps the column.
        assert_eq!(entry.drag(painter, 8 + 3 * 8, 100), Outcome::Moved);
        assert_eq!((entry.caret(), entry.selection()), (7, Some((7, 8))));

        chord(&mut entry, "Home").unwrap();
        entry.reveal(painter);
        assert_eq!(entry.first(), 0);
        let field = entry.field("Search", true, false);
        assert_eq!(
            (field.text, field.placeholder, field.caret, field.first),
            ("0123456789abcdef", "Search", 0, 0)
        );
        assert!(field.focused && !field.caret_visible && !field.masked);
    }
}

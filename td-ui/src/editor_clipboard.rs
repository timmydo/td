//! Bounded, selection-bound clipboard transactions. No transport or clock.

use crate::editor_error::{Error, Result};
use crate::editor_model::{Command, Document, Editor, RevisionPoint, Selection, TabId};
use crate::editor_text as text;
use std::ops::Range;
use std::sync::Arc;

pub const MAX_BYTES: usize = 1024 * 1024;

struct Anchor {
    point: RevisionPoint,
    selection: Selection,
}

impl Anchor {
    fn capture(editor: &Editor, tab: TabId, revision: u64) -> Result<Self> {
        let point = editor.revision_point(tab, revision)?;
        if editor.active() != Some(tab) {
            return Err(Error::InvalidArgument);
        }
        Ok(Self {
            point,
            selection: editor.document(tab)?.selection(),
        })
    }

    fn check(&self, editor: &Editor) -> Result<()> {
        // Reuse the private editor-instance/revision guard, not a discard permit.
        editor.check_revision(&self.point)?;
        if editor.active() != Some(self.point.tab)
            || editor.document(self.point.tab)?.selection() != self.selection
        {
            return Err(Error::InvalidArgument);
        }
        Ok(())
    }
}

/// Immutable selected text, or the caret's logical line when unselected.
/// Capture does not edit or claim system ownership. An empty final line
/// returns None and leaves existing clipboard ownership untouched.
/// A snapshot cannot authorize discarding a dirty tab:
/// ```compile_fail,E0308
/// fn cannot_discard(snapshot: td_ui::editor_clipboard::Snapshot) {
///     let _ = td_ui::editor::Event::Discard(snapshot);
/// }
/// ```
pub struct Snapshot {
    anchor: Anchor,
    range: Range<usize>,
    line: bool,
    text: Arc<str>,
}

/// The range a copy or cut takes: the selection, or with none the caret's
/// logical line and its newline.
pub fn capture_range(doc: &Document) -> Result<Range<usize>> {
    let selection = doc.selection();
    let range = selection.range();
    if !range.is_empty() {
        return Ok(range);
    }
    let mut line = text::line(doc.text(), selection.caret)?;
    if doc.text().as_bytes().get(line.end) == Some(&b'\n') {
        line.end = line.end.checked_add(1).ok_or(Error::Exhausted)?;
    }
    Ok(line)
}

impl Snapshot {
    pub fn capture(editor: &Editor, tab: TabId, revision: u64) -> Result<Option<Self>> {
        Self::capture_from(editor, tab, revision, true)
    }

    /// The selection alone: none when it is empty, never the caret's line,
    /// for a host whose copy and cut take exactly what is selected, in the
    /// document's own line ending.
    pub fn capture_selection(editor: &Editor, tab: TabId, revision: u64) -> Result<Option<Self>> {
        Self::capture_from(editor, tab, revision, false)
    }

    fn capture_from(
        editor: &Editor,
        tab: TabId,
        revision: u64,
        line: bool,
    ) -> Result<Option<Self>> {
        let anchor = Anchor::capture(editor, tab, revision)?;
        let doc = editor.document(tab)?;
        let range = if line {
            capture_range(doc)?
        } else {
            doc.selection().range()
        };
        if range.is_empty() {
            return Ok(None);
        }
        if range.len() > MAX_BYTES {
            return Err(Error::Limit);
        }
        let text = doc
            .text()
            .get(range.clone())
            .ok_or(Error::InvalidPosition)?;
        // A selection copies the bytes the document stores: its CRLF, not
        // the LF the model holds. Pasting it back folds them again.
        let text: Arc<str> = if !line && doc.format().ending == crate::editor_text::LineEnding::CrLf
        {
            let stored = text.replace('\n', "\r\n");
            if stored.len() > MAX_BYTES {
                return Err(Error::Limit);
            }
            Arc::from(stored)
        } else {
            Arc::from(text)
        };
        Ok(Some(Self {
            line: anchor.selection.range().is_empty(),
            anchor,
            range,
            text,
        }))
    }

    /// Share the bounded snapshot with the source adapter without copying it.
    pub fn text(&self) -> Arc<str> {
        Arc::clone(&self.text)
    }

    /// Whether this snapshot came from a collapsed selection at the caret.
    pub fn whole_line(&self) -> bool {
        self.line
    }

    /// The deletion that cuts this snapshot's range, refused once the tab
    /// has moved past the captured revision or selection.
    pub fn cut(self, editor: &Editor) -> Result<(TabId, u64, Command)> {
        self.anchor.check(editor)?;
        Ok((
            self.anchor.point.tab,
            self.anchor.point.revision,
            Command::CutRange(self.range),
        ))
    }
}

impl std::fmt::Debug for Snapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Snapshot")
            .field("tab", &self.anchor.point.tab)
            .field("revision", &self.anchor.point.revision)
            .field("byte_count", &self.text.len())
            .finish_non_exhaustive()
    }
}

/// One incoming transfer. Drop cancels it without changing editor state.
/// The adapter dispatches Event::Paste only after successful EOF, never after
/// timeout, cancellation or I/O failure. Exceeding the limit poisons the whole
/// transfer, so accidentally dispatching it cannot insert an accepted prefix.
pub struct Paste {
    anchor: Anchor,
    bytes: Vec<u8>,
    oversized: bool,
}

impl Paste {
    pub fn begin(editor: &Editor, tab: TabId, revision: u64) -> Result<Self> {
        Ok(Self {
            anchor: Anchor::capture(editor, tab, revision)?,
            // Reserve once outside per-chunk reads; the window owns one transfer.
            bytes: Vec::with_capacity(MAX_BYTES),
            oversized: false,
        })
    }

    pub fn push(&mut self, bytes: &[u8]) -> Result<()> {
        if self.oversized || bytes.len() > MAX_BYTES - self.bytes.len() {
            self.oversized = true;
            self.bytes = Vec::new();
            return Err(Error::Limit);
        }
        self.bytes.extend_from_slice(bytes);
        Ok(())
    }

    /// The insertion the transfer's text makes, refused when oversized,
    /// stale or not UTF-8; nothing for an empty transfer.
    pub fn finish(self, editor: &Editor) -> Result<Option<(TabId, u64, Command)>> {
        if self.oversized {
            return Err(Error::Limit);
        }
        self.anchor.check(editor)?;
        if self.bytes.is_empty() {
            return Ok(None);
        }
        let text = String::from_utf8(self.bytes).map_err(|_| Error::InvalidText)?;
        Ok(Some((
            self.anchor.point.tab,
            self.anchor.point.revision,
            Command::Insert(text),
        )))
    }
}

impl std::fmt::Debug for Paste {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Paste")
            .field("tab", &self.anchor.point.tab)
            .field("revision", &self.anchor.point.revision)
            .field("buffered_byte_count", &self.bytes.len())
            .field("oversized", &self.oversized)
            .finish_non_exhaustive()
    }
}

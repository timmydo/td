//! Bounded, selection-bound clipboard transactions. No transport or clock.

use crate::model::{Command, Editor, RevisionPoint, Selection, TabId};
use crate::{Error, Result};
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

/// Immutable selected text. Capture does not edit or claim system ownership.
/// Empty selection returns None: it must not replace the existing clipboard.
/// A snapshot cannot authorize discarding a dirty tab:
/// ```compile_fail
/// fn cannot_discard(snapshot: td_editor::clipboard::Snapshot) {
///     let _ = td_editor::ui::Event::Discard(snapshot);
/// }
/// ```
pub struct Snapshot {
    anchor: Anchor,
    text: Arc<str>,
}

impl Snapshot {
    pub fn capture(editor: &Editor, tab: TabId, revision: u64) -> Result<Option<Self>> {
        let anchor = Anchor::capture(editor, tab, revision)?;
        let range = anchor.selection.range();
        if range.is_empty() {
            return Ok(None);
        }
        if range.len() > MAX_BYTES {
            return Err(Error::Limit);
        }
        let text = editor
            .document(tab)?
            .text()
            .get(range)
            .ok_or(Error::InvalidPosition)?;
        Ok(Some(Self {
            anchor,
            text: Arc::from(text),
        }))
    }

    /// Share the bounded snapshot with the source adapter without copying it.
    pub fn text(&self) -> Arc<str> {
        Arc::clone(&self.text)
    }

    pub(crate) fn cut(self, editor: &Editor) -> Result<(TabId, u64, Command)> {
        self.anchor.check(editor)?;
        Ok((
            self.anchor.point.tab,
            self.anchor.point.revision,
            Command::Insert(String::new()),
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

    pub(crate) fn finish(self, editor: &Editor) -> Result<Option<(TabId, u64, Command)>> {
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

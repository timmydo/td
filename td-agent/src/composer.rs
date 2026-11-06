//! The composer: td-ui's editor pane holding one document, as the window's
//! composer and the prompt dialog edit it.

use td_ui::editor::{self, Controller as Pane, Event as PaneEvent, Outcome as PaneOutcome};
use td_ui::editor_clipboard::{Paste, Snapshot};
use td_ui::editor_model::TabId;
use td_ui::raster::{Draw, Rect, Surface};
use td_ui::window::PointerPhase;

/// The composer: one editable document in td-ui's editor pane. `Return`
/// sends it, as `C-Return` does from outside a dialog; `S-Return` is the
/// pane's own newline.
pub struct Composer {
    pane: Pane,
    tab: Option<TabId>,
}

impl Composer {
    pub fn new() -> Result<Self, String> {
        let mut composer = Self {
            pane: Pane::pane().map_err(|e| format!("composer: {e}"))?,
            tab: None,
        };
        composer.fresh();
        Ok(composer)
    }

    pub fn event(&mut self, event: PaneEvent<'_>) -> PaneOutcome {
        self.pane.dispatch(event).unwrap_or(PaneOutcome::Ignored)
    }

    pub fn target(&self) -> Option<(TabId, u64)> {
        let tab = self.tab?;
        Some((tab, self.pane.editor().document(tab).ok()?.revision()))
    }

    /// An empty document in place of the one there was.
    pub fn fresh(&mut self) {
        if let Some(tab) = self.tab.take() {
            if let Ok((point, _)) = self.pane.editor().save_snapshot(tab) {
                self.event(PaneEvent::Saved(point));
            }
            if let Ok(document) = self.pane.editor().document(tab) {
                let revision = document.revision();
                self.event(PaneEvent::Close { tab, revision });
            }
        }
        if let PaneOutcome::Created(tab) = self.event(PaneEvent::New) {
            self.tab = Some(tab);
        }
    }

    pub fn text(&self) -> &str {
        self.tab
            .and_then(|tab| self.pane.editor().document(tab).ok())
            .map_or("", |document| document.text())
    }

    pub fn place(&mut self, rect: Rect, surface: Surface) {
        if rect.width > 0 && rect.height > 0 {
            self.event(PaneEvent::Frame { rect, surface });
        }
    }

    pub fn chord(&mut self, chord: &str) -> bool {
        let Some((tab, revision)) = self.target() else {
            return false;
        };
        matches!(
            self.event(PaneEvent::Key {
                tab,
                revision,
                chord,
            }),
            PaneOutcome::Changed | PaneOutcome::Prefix
        )
    }

    pub fn pointer(&mut self, phase: PointerPhase, x: i64, y: i64, extend: bool) -> bool {
        let Some((tab, revision)) = self.target() else {
            return false;
        };
        let phase = match phase {
            PointerPhase::Press => editor::PointerPhase::Press,
            PointerPhase::Move => editor::PointerPhase::Move,
            PointerPhase::Release => editor::PointerPhase::Release,
        };
        self.event(PaneEvent::Pointer {
            tab,
            revision,
            phase,
            x,
            cell_x: x,
            y,
            extend,
        }) == PaneOutcome::Changed
    }

    /// The selection, or the caret's line, as a copy takes it.
    pub fn selection(&self) -> Option<Snapshot> {
        let (tab, revision) = self.target()?;
        Snapshot::capture(self.pane.editor(), tab, revision)
            .ok()
            .flatten()
    }

    pub fn scroll(&mut self, rows: isize, columns: isize) -> bool {
        let Some((tab, revision)) = self.target() else {
            return false;
        };
        self.event(PaneEvent::Scroll {
            tab,
            revision,
            rows,
            columns,
        }) == PaneOutcome::Changed
    }

    pub fn insert(&mut self, text: &str) -> Result<bool, String> {
        let Some((tab, revision)) = self.target() else {
            return Ok(false);
        };
        let paste = Paste::begin(self.pane.editor(), tab, revision)
            .and_then(|mut paste| {
                paste.push(text.as_bytes())?;
                Ok(paste)
            })
            .map_err(|e| format!("the paste: {e}"))?;
        self.pane
            .dispatch(PaneEvent::Paste(paste))
            .map(|outcome| outcome == PaneOutcome::Changed)
            .map_err(|e| format!("the paste: {e}"))
    }

    /// `text` after everything there is, on lines of its own, leaving
    /// the draft and its selection as they were; whether it went in.
    pub fn append(&mut self, text: &str) -> Result<bool, String> {
        if self.text().is_empty() {
            return self.insert(text);
        }
        // To the end, which drops the selection rather than replacing it.
        self.chord("C-End");
        let tail = if self.text().ends_with('\n') {
            ""
        } else {
            "\n"
        };
        self.insert(&format!("{tail}{text}\n"))
    }

    pub fn emit(&self, damage: Rect, sink: &mut dyn FnMut(Draw)) {
        if let Ok(scene) = self.pane.scene(&[]) {
            scene.emit(damage, sink);
        }
    }
}

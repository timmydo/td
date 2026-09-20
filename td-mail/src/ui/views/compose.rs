//! Composing: the retained draft in an editable document pane.
//!
//! The draft is the file `compose::write_compose_draft` retained, read
//! back into the pane; Save writes the pane's text over it, whole or
//! not at all, and Close pops the view when nothing is unsaved, else
//! asks. Discarding keeps the file as it was last saved: a draft is
//! never deleted here. While the view holds a draft the file is its
//! own; an edit made to it elsewhere is overwritten by the next save.
//! Every chord is the pane's while the draft is being edited; the
//! view's own keys are the bar's labels and, while it asks, the answer.

use crate::backend::BackendResponse;
use crate::ui::frame::Draft;
use crate::ui::input::Key;
use crate::ui::views::{Body, Scene, View, ViewAction};
use std::io;
use std::path::PathBuf;

const EDIT_LABELS: &[&str] = &["Save", "Close"];
const EDIT_KEYS: &[Key] = &[Key::Request("save"), Key::Request("close-tab")];
const ASK_LABELS: &[&str] = &["Save", "Discard", "Cancel"];
const ASK_KEYS: &[Key] = &[Key::Char('y'), Key::Char('n'), Key::Escape];

pub struct ComposeView {
    path: PathBuf,
    /// The text as read from the file, which the pane loads once.
    text: String,
    key: String,
    /// Close was asked with the draft unsaved: the answer is the next key.
    asking: bool,
    /// Save was answered to a close: the view pops once the file is written.
    closing: bool,
    status: String,
}

impl ComposeView {
    /// The retained draft at `path`, read back; one past the pane's
    /// ceiling is refused here, retained as it is, rather than shown
    /// short.
    pub fn open(path: PathBuf, attachment_dir: Option<PathBuf>) -> io::Result<Self> {
        let text = std::fs::read_to_string(&path)?;
        if text.len() > td_editor::text::MAX_FILE_BYTES {
            return Err(io::Error::other(format!(
                "the draft is larger than the pane's ceiling of {} bytes",
                td_editor::text::MAX_FILE_BYTES
            )));
        }
        let mut status = format!("Draft retained at {}", path.display());
        if let Some(dir) = attachment_dir {
            status.push_str(&format!("; attachments at {}", dir.display()));
        }
        Ok(ComposeView {
            key: format!("draft:{}", path.display()),
            path,
            text,
            asking: false,
            closing: false,
            status,
        })
    }

    fn file_name(&self) -> String {
        self.path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default()
    }

    /// Writes the pane's text over the draft, whole or not at all, and
    /// marks the document saved at that state.
    fn save(&mut self, draft: &mut Draft<'_>) -> bool {
        let Some((point, bytes)) = draft.snapshot() else {
            self.status = "Nothing to save: the draft could not be shown".to_string();
            return false;
        };
        match crate::compose::replace_draft(&self.path, &bytes) {
            Ok(()) => {
                draft.saved(point);
                self.status = format!("Saved {}", self.path.display());
                true
            }
            Err(e) => {
                crate::log_error!("Failed to save the draft {}: {}", self.path.display(), e);
                self.status = format!("Could not save {}: {}", self.path.display(), e);
                false
            }
        }
    }
}

impl View for ComposeView {
    fn scene(&self) -> Scene<'_> {
        let (title, labels, keys) = if self.asking {
            (format!("Save {}?", self.file_name()), ASK_LABELS, ASK_KEYS)
        } else {
            (
                format!("Draft {}", self.file_name()),
                EDIT_LABELS,
                EDIT_KEYS,
            )
        };
        Scene {
            title,
            labels,
            keys,
            entry: None,
            body: Body::Edit {
                key: self.key.clone(),
                text: Box::new(|| self.text.clone()),
                focused: !self.asking,
            },
            status: self.status.clone(),
        }
    }

    /// While the draft is edited every chord is the pane's, so a key
    /// here is the answer to the question, or nothing.
    fn handle_key(&mut self, key: Key, _page: usize) -> ViewAction {
        if !self.asking {
            return ViewAction::Continue;
        }
        match key {
            Key::Char('y') | Key::Char('Y') => {
                self.asking = false;
                self.closing = true;
                ViewAction::Request("save")
            }
            Key::Char('n') | Key::Char('N') => {
                self.asking = false;
                ViewAction::Request("discard")
            }
            Key::Escape => {
                self.asking = false;
                ViewAction::Continue
            }
            _ => ViewAction::Continue,
        }
    }

    fn request(&mut self, name: &str, draft: &mut Draft<'_>) -> ViewAction {
        match name {
            "save" => {
                let saved = self.save(draft);
                if saved && self.closing {
                    self.closing = false;
                    return ViewAction::Pop;
                }
                self.closing = false;
                ViewAction::Continue
            }
            "close-tab" | "quit" => {
                if draft.dirty() {
                    self.asking = true;
                    ViewAction::Continue
                } else {
                    ViewAction::Pop
                }
            }
            "discard" => {
                draft.discard();
                ViewAction::Pop
            }
            _ => ViewAction::Continue,
        }
    }

    fn on_response(&mut self, _response: &BackendResponse) -> bool {
        false
    }
}

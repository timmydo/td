//! Composing: the retained draft in an editable document pane.
//!
//! The draft is the file `compose::write_compose_draft` retained, read
//! back into the pane; Save writes the pane's text over it, whole or
//! not at all, and Close pops the view when nothing is unsaved, else
//! asks. Discarding keeps the file as it was last saved: a draft is
//! never deleted here. While the view holds a draft the file is its
//! own; an edit made to it elsewhere is overwritten by the next save.
//! Send saves the draft and hands its path to the backend, which
//! submits it through the account's server; while the answer is
//! awaited the draft is the backend's, shown but not edited, saved or
//! closed, so what was sent is what the file holds, and on the answer
//! the draft is retired to the sent directory and the view pops, or
//! the refusal is the status row's and the draft is the pane's again,
//! to be mended. Every chord is the pane's while the draft is being
//! edited; the view's own keys are the bar's labels and, while it
//! asks, the answer.

use crate::backend::{BackendCommand, BackendResponse};
use crate::ui::frame::Draft;
use crate::ui::input::Key;
use crate::ui::views::{Body, Scene, View, ViewAction};
use std::io;
use std::path::PathBuf;
use std::sync::mpsc::Sender;

const EDIT_LABELS: &[&str] = &["Send", "Save", "Close"];
const EDIT_KEYS: &[Key] = &[
    Key::Request("send"),
    Key::Request("save"),
    Key::Request("close-tab"),
];
const ASK_LABELS: &[&str] = &["Save", "Discard", "Cancel"];
const ASK_KEYS: &[Key] = &[Key::Char('y'), Key::Char('n'), Key::Escape];

pub struct ComposeView {
    path: PathBuf,
    attachment_dir: Option<PathBuf>,
    /// The text as read from the file, which the pane loads once.
    text: String,
    key: String,
    /// Close was asked with the draft unsaved: the answer is the next key.
    asking: bool,
    /// Save was answered to a close: the view pops once the file is written.
    closing: bool,
    /// Send was asked: the backend has the draft until it answers, and
    /// the draft is held read-only meanwhile, a save or close refused.
    sending: bool,
    /// The server took the draft but it could not be retired: it is not
    /// sent again nor changed, and Send retries the move alone.
    sent: Option<crate::backend::SentDraft>,
    status: String,
    cmd_tx: Sender<BackendCommand>,
    pending: Option<ViewAction>,
}

impl ComposeView {
    /// The retained draft at `path`, read back; one past the pane's
    /// ceiling is refused here, retained as it is, rather than shown
    /// short.
    pub fn open(
        path: PathBuf,
        attachment_dir: Option<PathBuf>,
        cmd_tx: Sender<BackendCommand>,
    ) -> io::Result<Self> {
        let text = std::fs::read_to_string(&path)?;
        if text.len() > td_editor::text::MAX_FILE_BYTES {
            return Err(io::Error::other(format!(
                "the draft is larger than the pane's ceiling of {} bytes",
                td_editor::text::MAX_FILE_BYTES
            )));
        }
        let mut status = format!("Draft retained at {}", path.display());
        if let Some(dir) = &attachment_dir {
            status.push_str(&format!("; attachments at {}", dir.display()));
        }
        Ok(ComposeView {
            key: format!("draft:{}", path.display()),
            path,
            attachment_dir,
            text,
            asking: false,
            closing: false,
            sending: false,
            sent: None,
            status,
            cmd_tx,
            pending: None,
        })
    }

    fn file_name(&self) -> String {
        self.path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default()
    }

    /// Writes the pane's text over the draft, whole or not at all, and
    /// marks the document saved at that state; not while the backend
    /// has the draft, so the file sent is the file retired.
    fn save(&mut self, draft: &mut Draft<'_>) -> bool {
        if self.sending {
            self.status = format!("Sending {}; wait for the server", self.file_name());
            return false;
        }
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

    /// Saves the draft when it has unsaved changes, then hands its path
    /// to the backend; the answer comes as `DraftSent`. A draft the
    /// server already took is not sent twice, nor changed (it is held
    /// read-only): only its move is retried, the view popping when it
    /// goes.
    fn send(&mut self, draft: &mut Draft<'_>) -> ViewAction {
        if self.sending {
            self.status = format!(
                "Sending {} already; waiting for the server",
                self.file_name()
            );
            return ViewAction::Continue;
        }
        if let Some(sent) = self.sent.clone() {
            return self.sent(&sent);
        }
        if draft.dirty() && !self.save(draft) {
            return ViewAction::Continue;
        }
        match self.cmd_tx.send(BackendCommand::SendDraft {
            path: self.path.clone(),
        }) {
            Ok(()) => {
                self.sending = true;
                self.status = format!("Sending {}...", self.file_name());
            }
            Err(e) => {
                self.status = format!("Send failed to reach the backend: {e}");
            }
        }
        ViewAction::Continue
    }

    /// The server took the draft: it is retired to the sent directory,
    /// with its attachments, and the view pops; a draft that cannot be
    /// moved stays open, the status saying it was sent and where it is.
    fn sent(&mut self, sent: &crate::backend::SentDraft) -> ViewAction {
        match crate::compose::retire_draft(&self.path, self.attachment_dir.as_deref()) {
            Ok(retired) => {
                crate::log_info!(
                    "Sent {} (email {}, submission {}, kept in {}); the draft is now {}",
                    self.path.display(),
                    sent.email_id,
                    sent.submission_id,
                    sent.kept_in,
                    retired.display()
                );
                self.status = format!("Sent {}; kept in {}", self.file_name(), sent.kept_in);
                ViewAction::Pop
            }
            Err(e) => {
                crate::log_error!(
                    "Sent {} (email {}, submission {}) but could not retire the draft: {}",
                    self.path.display(),
                    sent.email_id,
                    sent.submission_id,
                    e
                );
                self.sent = Some(sent.clone());
                self.status = format!("Sent, kept in {}; not retired: {}", sent.kept_in, e);
                ViewAction::Continue
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
    /// here is the answer to the question, or nothing; while the
    /// backend has the draft, nothing.
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
            "send" => self.send(draft),
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
                if self.sending {
                    self.status = format!(
                        "Sending {}; it closes when the server answers",
                        self.file_name()
                    );
                    ViewAction::Continue
                } else if draft.dirty() {
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

    fn on_response(&mut self, response: &BackendResponse) -> bool {
        let BackendResponse::DraftSent { path, result } = response else {
            return false;
        };
        if *path != self.path {
            return false;
        }
        self.sending = false;
        match result {
            Ok(sent) => {
                if let action @ ViewAction::Pop = self.sent(sent) {
                    self.pending = Some(action);
                }
            }
            Err(e) => {
                crate::log_error!("Not sent {}: {}", self.path.display(), e);
                self.status = format!("Not sent: {e}");
            }
        }
        true
    }

    fn take_pending_action(&mut self) -> Option<ViewAction> {
        self.pending.take()
    }

    fn waiting(&self) -> bool {
        self.sending
    }

    fn held(&self) -> bool {
        self.sending || self.sent.is_some()
    }
}

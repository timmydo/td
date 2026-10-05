//! The window's questions, td-ui's confirmation dialog (td-ui/DESIGN.md)
//! centred over its body, `Cancel` focused first, so `Return` alone does
//! nothing: before a conversation is deleted (DESIGN.md §4), the card
//! that asks whether a workspace tool may act (§11), where `Cancel`
//! refuses it and `Allow` lets it run, and the card that asks whether a
//! template's remotes are admitted (§7).

use td_ui::chrome::ROW;
use td_ui::confirmations::{self, Choice, Controller, Event, Key, Model, Outcome};
use td_ui::raster::{Draw, Rect, Surface};
use td_ui::window::{Input, PointerPhase};
use td_ui::CELL_WIDTH;

use crate::store::Id;
use crate::workspace::Workspace;

/// The dialog's one action.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Act;

/// What a question is for.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Purpose {
    /// Deleting this conversation.
    Delete(Id),
    /// Letting call `call` (its `ToolCall`'s sequence number) of this
    /// conversation run.
    Approve { conversation: Id, call: u64 },
    /// Admitting `remotes`, the ones template `template` names that no
    /// admission covers, then making its workspace.
    Admit {
        template: String,
        remotes: Vec<String>,
    },
    /// Deleting this conversation though its repository workspace
    /// reports work that would be lost (DESIGN.md §7).
    Remove(Id),
}

/// What an input came to.
#[derive(Debug, Eq, PartialEq)]
pub enum Reply {
    /// Still open; whether it changed.
    Stay(bool),
    /// The human chose `Cancel`, or `Escape`: nothing deleted, the call
    /// refused.
    Closed,
    /// Closed without the human's choice, the window having lost the
    /// keyboard: a card is asked again when it comes back.
    SetAside,
    /// The human chose the action.
    Confirmed(Purpose),
}

/// The deletion question's title and its action's label.
pub const TITLE: &str = "Delete conversation";
pub const DELETE: &str = "Delete";
/// A card's action's label.
pub const ALLOW: &str = "Allow";
/// The admission card's title and its action's label.
pub const ADMIT_TITLE: &str = "Admit remotes";
pub const ADMIT: &str = "Admit";
/// The loss card's title and its action's label.
pub const REMOVE_TITLE: &str = "Delete and lose work";
pub const REMOVE: &str = "Delete anyway";

/// The open question: what it is for and the dialog.
pub struct Confirm {
    purpose: Purpose,
    dialog: Controller<Act, u64, ()>,
    revision: u64,
}

/// What becomes of a deleted conversation's workspace: td-agent's
/// directory goes with it, the human's stays.
fn worked(workspace: Option<&Workspace>) -> String {
    match workspace {
        Some(Workspace::Scratch) => {
            "Its scratch workspace and every file in it are removed with it.".to_string()
        }
        Some(Workspace::Template(name)) => format!(
            "Its workspace, a scratch directory made from template {name}, and every file in it are removed with it."
        ),
        Some(Workspace::Directory(directory)) => format!(
            "The directory it works in, {}, is yours and stays.",
            directory.display()
        ),
        Some(Workspace::Repositories(repositories)) => format!(
            "Its repository workspace, made from template {}, is removed with it: its worktrees in {} and its repositories. td-agent first asks each worktree what removing it would lose; a changed or untracked file, or a commit on a branch, tag or stash that is not in a commit its worktrees started at, is listed and asked about again. Files git ignores, such as build output, are not asked about.",
            repositories.template,
            repositories
                .tree()
                .map_or_else(|| "the workspace root".to_string(), |tree| tree.display().to_string())
        ),
        None => String::new(),
    }
}

/// The dialog's place: at most 64 cells by 10 rows, centred in `body`.
/// A card's is larger, at most 100 cells by 24 rows, so more of the
/// action shows at once.
fn place(surface: Surface, body: Rect, purpose: &Purpose) -> Rect {
    let (columns, rows) = match purpose {
        Purpose::Delete(_) => (64, 10),
        Purpose::Approve { .. } | Purpose::Admit { .. } | Purpose::Remove(_) => (100, 24),
    };
    let scale = surface.scale.value();
    let pixels = |logical: usize| u32::try_from(logical.saturating_mul(scale)).unwrap_or(u32::MAX);
    let width = pixels(CELL_WIDTH * columns).min(body.width);
    let height = pixels(ROW * rows).min(body.height);
    Rect {
        x: body.x + i64::from((body.width - width) / 2),
        y: body.y + i64::from((body.height - height) / 2),
        width,
        height,
    }
}

impl Confirm {
    /// Asks whether conversation `id`, titled `title`, is to be deleted.
    pub fn open(
        surface: Surface,
        body: Rect,
        id: Id,
        title: &str,
        workspace: Option<&Workspace>,
        revision: u64,
    ) -> Result<Self, String> {
        let named = format!("Delete \u{201c}{title}\u{201d} ({})?", id.as_str());
        let worked = worked(workspace);
        let mut details: Vec<&str> = vec![
            &named,
            "Its log, its todo list, its cost record and the messages waiting for it are removed from this machine for good. Messages it sent to other conversations stay in theirs. This cannot be undone.",
        ];
        if !worked.is_empty() {
            details.push(&worked);
        }
        let model = Model::new(TITLE, DELETE, &details, Act, revision)
            .map_err(|e| format!("the deletion question: {e}"))?;
        let purpose = Purpose::Delete(id);
        let dialog = Controller::new(model, surface, place(surface, body, &purpose), None)
            .map_err(|e| format!("the deletion question: {e}"))?;
        Ok(Self {
            purpose,
            dialog,
            revision,
        })
    }

    /// The card for call `call` of `conversation`: may it run, as `title`
    /// and `details` say?
    pub fn approve(
        surface: Surface,
        body: Rect,
        conversation: Id,
        call: u64,
        title: &str,
        details: &[String],
        revision: u64,
    ) -> Result<Self, String> {
        let details: Vec<&str> = details.iter().map(String::as_str).collect();
        let model = Model::new(title, ALLOW, &details, Act, revision)
            .map_err(|e| format!("the card: {e}"))?;
        let purpose = Purpose::Approve { conversation, call };
        let dialog = Controller::new(model, surface, place(surface, body, &purpose), None)
            .map_err(|e| format!("the card: {e}"))?;
        Ok(Self {
            purpose,
            dialog,
            revision,
        })
    }

    /// The card asking whether conversation `id`, titled `title`, is
    /// deleted though its workspace reports `lost`, a line a worktree.
    pub fn remove(
        surface: Surface,
        body: Rect,
        id: Id,
        title: &str,
        lost: &[String],
        revision: u64,
    ) -> Result<Self, String> {
        let asked = format!(
            "Deleting \u{201c}{}\u{201d} ({}) removes its repository workspace, whose worktrees report work that would be lost with it:",
            crate::tools::visible(title),
            id.as_str()
        );
        let mut details: Vec<&str> = vec![&asked];
        details.extend(lost.iter().map(String::as_str));
        details.push(
            "Each worktree's answer comes from git inside the workspace, which the model could have changed: td-agent can know no more than it says.",
        );
        details.push("Cancel keeps the conversation and its workspace as they are.");
        let model = Model::new(REMOVE_TITLE, REMOVE, &details, Act, revision)
            .map_err(|e| format!("the loss card: {e}"))?;
        let purpose = Purpose::Remove(id);
        let dialog = Controller::new(model, surface, place(surface, body, &purpose), None)
            .map_err(|e| format!("the loss card: {e}"))?;
        Ok(Self {
            purpose,
            dialog,
            revision,
        })
    }

    /// The card asking whether `remotes`, which template `template` names
    /// and nothing admits, are admitted (DESIGN.md §7): what that lets
    /// td-agent do, and that it lasts.
    pub fn admit(
        surface: Surface,
        body: Rect,
        template: String,
        remotes: Vec<String>,
        revision: u64,
    ) -> Result<Self, String> {
        let asked = match remotes.len() {
            1 => format!("Template \u{201c}{template}\u{201d} works on a remote no admission covers:"),
            n => format!(
                "Template \u{201c}{template}\u{201d} works on {n} remotes no admission covers, all admitted together:"
            ),
        };
        let mut details: Vec<&str> = vec![&asked];
        details.extend(remotes.iter().map(String::as_str));
        details.push(
            "Admitted, td-agent's git clones and fetches each for you, outside any jail and with your git credentials, bypassing the egress relay that holds the workspace's own network. Each stays admitted, with or without a final .git, for every later workspace: td-agent keeps them in the `remotes` file of its state directory, beside `remotes` in the configuration.",
        );
        details.push("Cancel makes nothing.");
        let model = Model::new(ADMIT_TITLE, ADMIT, &details, Act, revision)
            .map_err(|e| format!("the admission card: {e}"))?;
        let purpose = Purpose::Admit { template, remotes };
        let dialog = Controller::new(model, surface, place(surface, body, &purpose), None)
            .map_err(|e| format!("the admission card: {e}"))?;
        Ok(Self {
            purpose,
            dialog,
            revision,
        })
    }

    /// What it asks about.
    pub fn purpose(&self) -> &Purpose {
        &self.purpose
    }

    /// Where its keyboard is: `details`, `cancel`, or its action, `delete`,
    /// `allow`, `admit` or `remove`.
    pub fn focus(&self) -> &'static str {
        match self.dialog.focus() {
            confirmations::Focus::Details => "details",
            confirmations::Focus::Cancel => "cancel",
            confirmations::Focus::Alternate
            | confirmations::Focus::Further
            | confirmations::Focus::Confirm => match self.purpose {
                Purpose::Delete(_) => "delete",
                Purpose::Approve { .. } => "allow",
                Purpose::Admit { .. } => "admit",
                Purpose::Remove(_) => "remove",
            },
        }
    }

    /// Lays it out again for a new surface; false when it no longer fits,
    /// which closes it.
    pub fn resize(&mut self, surface: Surface, body: Rect) -> bool {
        let event = Event::Resize {
            surface,
            rect: place(surface, body, &self.purpose),
        };
        !matches!(
            self.dialog.event(Some(self.revision), false, event),
            Outcome::Closed { .. }
        )
    }

    /// An input, which is the dialog's while it is open: its keys, every
    /// other chord consumed, and the pointer.
    pub fn input(&mut self, input: &Input<'_>) -> Reply {
        let event = match *input {
            Input::Key { chord, repeat } => match Key::from_chord(chord) {
                Some(key) => Event::Key {
                    key,
                    repeated: repeat,
                },
                None => Event::Other,
            },
            Input::Pointer { phase, x, y, .. } => match phase {
                PointerPhase::Press => Event::Press { x, y },
                PointerPhase::Move => Event::Move { x, y },
                PointerPhase::Release => Event::Release { x, y },
            },
            Input::Wheel { rows, .. } => {
                let details = self.dialog.details_rect();
                Event::Wheel {
                    x: details.x,
                    y: details.y,
                    rows,
                }
            }
            Input::Focus(false) => Event::FocusLost,
            // The pointer left mid-gesture: nothing it pressed acts.
            Input::CancelPointer => Event::Other,
            Input::Resize(_)
            | Input::Focus(true)
            | Input::Paste(_)
            | Input::Hover(_)
            | Input::Context { .. }
            | Input::Close => return Reply::Stay(false),
        };
        let lost = matches!(event, Event::FocusLost);
        match self.dialog.event(Some(self.revision), false, event) {
            Outcome::Ignored | Outcome::Consumed => Reply::Stay(false),
            Outcome::Changed => Reply::Stay(true),
            Outcome::Closed {
                choice: Choice::Confirmed(Act),
                ..
            } => Reply::Confirmed(self.purpose.clone()),
            Outcome::Closed { .. } if lost => Reply::SetAside,
            Outcome::Closed { .. } => Reply::Closed,
        }
    }

    pub fn emit(&self, damage: Rect, sink: &mut dyn FnMut(Draw)) {
        self.dialog.emit(damage, sink);
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use td_ui::raster::Scale;

    fn open() -> Confirm {
        let surface = Surface::new(1024, 640, Scale::default()).unwrap();
        let id = Id::parse(&"c".repeat(32)).unwrap();
        let workspace = Workspace::Scratch;
        Confirm::open(surface, surface.bounds(), id, "a plan", Some(&workspace), 7).unwrap()
    }

    #[test]
    fn the_question_says_whether_the_workspace_goes_by_its_kind() {
        assert!(worked(Some(&Workspace::Scratch)).contains("removed with it"));
        // A template's is td-agent's scratch directory, whatever its name.
        let template = worked(Some(&Workspace::Template("notes".into())));
        assert!(template.contains("template notes") && template.contains("removed with it"));
        let directory = worked(Some(&Workspace::Directory("/home/u/notes".into())));
        assert_eq!(
            directory,
            "The directory it works in, /home/u/notes, is yours and stays."
        );
        assert_eq!(worked(None), "");
    }

    fn key(confirm: &mut Confirm, chord: &str) -> Reply {
        confirm.input(&Input::Key {
            chord,
            repeat: false,
        })
    }

    #[test]
    fn the_admission_card_admits_only_on_its_action() {
        let surface = Surface::new(1024, 640, Scale::default()).unwrap();
        let admit = || {
            Confirm::admit(
                surface,
                surface.bounds(),
                "td".into(),
                vec!["https://example.org/a/td".into()],
                3,
            )
            .unwrap()
        };
        let mut confirm = admit();
        assert_eq!(confirm.focus(), "cancel");
        assert_eq!(key(&mut confirm, "Return"), Reply::Closed);
        let mut confirm = admit();
        assert_eq!(key(&mut confirm, "Tab"), Reply::Stay(true));
        assert_eq!(confirm.focus(), "admit");
        assert_eq!(
            key(&mut confirm, "Return"),
            Reply::Confirmed(Purpose::Admit {
                template: "td".into(),
                remotes: vec!["https://example.org/a/td".into()],
            })
        );
    }

    #[test]
    fn cancel_is_focused_first_so_return_deletes_nothing() {
        let mut confirm = open();
        assert_eq!(confirm.focus(), "cancel");
        assert_eq!(key(&mut confirm, "Return"), Reply::Closed);
        let mut confirm = open();
        assert_eq!(key(&mut confirm, "Escape"), Reply::Closed);
        // Other chords are the dialog's, consumed.
        let mut confirm = open();
        assert_eq!(key(&mut confirm, "C-n"), Reply::Stay(false));
    }

    /// A press on Delete acts only on its release: a focus loss between
    /// closes the question, and a broken grab disarms it.
    #[test]
    fn focus_loss_cancels_and_a_broken_grab_disarms() {
        let mut confirm = open();
        let delete = confirm
            .dialog
            .action_rect(confirmations::Focus::Confirm)
            .unwrap();
        let (x, y) = (delete.x + 2, delete.y + 2);
        let pointer = |phase| Input::Pointer {
            phase,
            x,
            y,
            extend: false,
            follow: false,
        };
        confirm.input(&pointer(PointerPhase::Press));
        assert_eq!(confirm.input(&Input::Focus(false)), Reply::SetAside);
        let mut confirm = open();
        confirm.input(&pointer(PointerPhase::Press));
        confirm.input(&Input::CancelPointer);
        assert_ne!(
            confirm.input(&pointer(PointerPhase::Release)),
            Reply::Confirmed(Purpose::Delete(Id::parse(&"c".repeat(32)).unwrap()))
        );
        // Pressed and released over it, it confirms.
        let mut confirm = open();
        confirm.input(&pointer(PointerPhase::Press));
        assert_eq!(
            confirm.input(&pointer(PointerPhase::Release)),
            Reply::Confirmed(Purpose::Delete(Id::parse(&"c".repeat(32)).unwrap()))
        );
    }

    #[test]
    fn tab_to_delete_and_return_confirms_for_its_conversation() {
        let mut confirm = open();
        assert_eq!(key(&mut confirm, "Tab"), Reply::Stay(true));
        assert_eq!(confirm.focus(), "delete");
        assert_eq!(
            key(&mut confirm, "Return"),
            Reply::Confirmed(Purpose::Delete(Id::parse(&"c".repeat(32)).unwrap()))
        );
    }
}

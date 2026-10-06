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

/// The dialog's actions: its one confirmation, and a card's "always"
/// answers (DESIGN.md §11).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Act {
    Confirm,
    AlwaysDenyHere,
    AlwaysDenyEverywhere,
    AlwaysAllowHere,
}

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
    /// Deleting, or archiving, this conversation though its repository
    /// workspace reports work that would be lost (DESIGN.md §7).
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
    /// The human chose an action.
    Confirmed(Purpose, Act),
}

/// The deletion question's title and its action's label.
pub const TITLE: &str = "Delete conversation";
pub const DELETE: &str = "Delete";
/// A card's action's label, and its "always" answers'.
pub const ALLOW: &str = "Allow";
pub const ALWAYS_DENY_HERE: &str = "Always deny here";
pub const ALWAYS_DENY_EVERYWHERE: &str = "Always deny everywhere";
pub const ALWAYS_ALLOW_HERE: &str = "Always allow here";
/// The admission card's title and its action's label.
pub const ADMIT_TITLE: &str = "Admit remotes";
pub const ADMIT: &str = "Admit";
/// The loss card's titles and its action's labels, deleting and
/// archiving.
pub const REMOVE_TITLE: &str = "Delete and lose work";
pub const REMOVE: &str = "Delete anyway";
pub const ARCHIVE_TITLE: &str = "Archive and lose work";
pub const ARCHIVE: &str = "Archive anyway";

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

/// What a card's "always" answers would add to the human's rules, as its
/// last lines say, and why an allow for an interpreter or build tool is
/// broad (DESIGN.md §11).
fn remembered(always: &crate::rules::Always) -> Vec<String> {
    // A line a rule, so none passes the dialog's bound on a line.
    let rules = |lines: &mut Vec<String>, effect: &str| {
        lines.extend(
            always
                .bodies
                .iter()
                .map(|body| format!("    {effect} {body}")),
        );
    };
    let mut lines = Vec::new();
    if always.allow {
        lines.push(format!(
            "{ALWAYS_ALLOW_HERE} adds to your rules for this workspace, so a call they cover runs with no card:"
        ));
        rules(&mut lines, "allow");
        let broad: Vec<&str> = always
            .bodies
            .iter()
            .filter_map(|body| crate::rules::broad(body))
            .collect();
        if !broad.is_empty() {
            lines.push(format!(
                "An allow for {} is broad: what it runs is this workspace's own code, which the model can change.",
                broad.join(", ")
            ));
        }
    }
    lines.push(format!(
        "{ALWAYS_DENY_HERE} adds to your rules for this workspace, and {ALWAYS_DENY_EVERYWHERE} to your rules for every workspace:"
    ));
    rules(&mut lines, "deny");
    lines
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
        let model = Model::new(TITLE, DELETE, &details, Act::Confirm, revision)
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
    /// and `details` say? With `always`, its "always" answers too, and
    /// what each would add to the human's rules.
    #[allow(clippy::too_many_arguments)]
    pub fn approve(
        surface: Surface,
        body: Rect,
        conversation: Id,
        call: u64,
        title: &str,
        details: &[String],
        always: Option<&crate::rules::Always>,
        revision: u64,
    ) -> Result<Self, String> {
        let remembered = always.map(remembered).unwrap_or_default();
        let details: Vec<&str> = details
            .iter()
            .chain(&remembered)
            .map(String::as_str)
            .collect();
        let mut model = Model::new(title, ALLOW, &details, Act::Confirm, revision)
            .map_err(|e| format!("the card: {e}"))?;
        if let Some(always) = always {
            model = model
                .with_alternate(ALWAYS_DENY_HERE, Act::AlwaysDenyHere)
                .and_then(|m| m.with_further(ALWAYS_DENY_EVERYWHERE, Act::AlwaysDenyEverywhere))
                .map_err(|e| format!("the card: {e}"))?;
            if always.allow {
                model = model
                    .with_extra(ALWAYS_ALLOW_HERE, Act::AlwaysAllowHere)
                    .map_err(|e| format!("the card: {e}"))?;
            }
        }
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
    /// deleted, or archived when `archive`, though its workspace reports
    /// `lost`, a line a worktree.
    pub fn remove(
        surface: Surface,
        body: Rect,
        id: Id,
        title: &str,
        lost: &[String],
        archive: bool,
        revision: u64,
    ) -> Result<Self, String> {
        let (doing, card, action) = if archive {
            ("Archiving", ARCHIVE_TITLE, ARCHIVE)
        } else {
            ("Deleting", REMOVE_TITLE, REMOVE)
        };
        let asked = format!(
            "{doing} \u{201c}{}\u{201d} ({}) removes its repository workspace, whose worktrees report work that would be lost with it:",
            crate::tools::visible(title),
            id.as_str()
        );
        let mut details: Vec<&str> = vec![&asked];
        details.extend(lost.iter().map(String::as_str));
        details.push(
            "Each worktree's answer comes from git inside the workspace, which the model could have changed: td-agent can know no more than it says.",
        );
        details.push("Cancel keeps the conversation and its workspace as they are.");
        let model = Model::new(card, action, &details, Act::Confirm, revision)
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
        let model = Model::new(ADMIT_TITLE, ADMIT, &details, Act::Confirm, revision)
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

    /// Where its keyboard is: `details`, `cancel`, a card's "always"
    /// answer, or its action, `delete`, `allow`, `admit` or `remove`.
    pub fn focus(&self) -> &'static str {
        match self.dialog.focus() {
            confirmations::Focus::Details => "details",
            confirmations::Focus::Cancel => "cancel",
            confirmations::Focus::Alternate => "always deny here",
            confirmations::Focus::Further => "always deny everywhere",
            confirmations::Focus::Extra => "always allow here",
            confirmations::Focus::Confirm => match self.purpose {
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
                choice: Choice::Confirmed(act),
                ..
            } => Reply::Confirmed(self.purpose.clone(), act),
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
            Reply::Confirmed(
                Purpose::Admit {
                    template: "td".into(),
                    remotes: vec!["https://example.org/a/td".into()],
                },
                Act::Confirm
            )
        );
    }

    /// A card offering "always" answers: Cancel first, the denies next,
    /// the allows last and farthest from it, each confirming its own act;
    /// the lines below say what each adds, and that an allow for a build
    /// tool is broad. With no allow offered, only the denies.
    #[test]
    fn a_card_offers_its_always_answers_denies_first() {
        let surface = Surface::new(1024, 640, Scale::default()).unwrap();
        let id = Id::parse(&"c".repeat(32)).unwrap();
        let card = |allow: bool| {
            let always = crate::rules::Always {
                allow,
                bodies: vec!["shell cargo test".into(), "shell rm".into()],
            };
            Confirm::approve(
                surface,
                surface.bounds(),
                id.clone(),
                4,
                "Run a command",
                &["cargo test && rm x".into()],
                Some(&always),
                3,
            )
            .unwrap()
        };
        let approve = Purpose::Approve {
            conversation: id.clone(),
            call: 4,
        };
        for (tabs, focus, act) in [
            (1, "always deny here", Act::AlwaysDenyHere),
            (2, "always deny everywhere", Act::AlwaysDenyEverywhere),
            (3, "always allow here", Act::AlwaysAllowHere),
            (4, "allow", Act::Confirm),
        ] {
            let mut confirm = card(true);
            assert_eq!(confirm.focus(), "cancel");
            for _ in 0..tabs {
                key(&mut confirm, "Tab");
            }
            assert_eq!(confirm.focus(), focus);
            assert_eq!(
                key(&mut confirm, "Return"),
                Reply::Confirmed(approve.clone(), act)
            );
        }
        let mut confirm = card(false);
        for _ in 0..3 {
            key(&mut confirm, "Tab");
        }
        assert_eq!(confirm.focus(), "allow");
        let always = crate::rules::Always {
            allow: true,
            bodies: vec!["shell cargo test".into(), "shell rm".into()],
        };
        assert_eq!(
            remembered(&always),
            [
                "Always allow here adds to your rules for this workspace, so a call they cover runs with no card:",
                "    allow shell cargo test",
                "    allow shell rm",
                "An allow for cargo is broad: what it runs is this workspace's own code, which the model can change.",
                "Always deny here adds to your rules for this workspace, and Always deny everywhere to your rules for every workspace:",
                "    deny shell cargo test",
                "    deny shell rm",
            ]
        );
        let denies = remembered(&crate::rules::Always {
            allow: false,
            ..always
        });
        assert_eq!(denies.len(), 3);
        // The most a card offers, each rule its longest, still shows.
        let longest = crate::rules::Always {
            allow: true,
            bodies: (0..crate::rules::MAX_BODIES)
                .map(|n| format!("shell {n}{}", "x".repeat(crate::rules::MAX_LINE - 12)))
                .collect(),
        };
        crate::rules::Always::checked(true, longest.bodies.clone()).unwrap();
        Confirm::approve(
            surface,
            surface.bounds(),
            id.clone(),
            4,
            "Run a command",
            &["x".into()],
            Some(&longest),
            3,
        )
        .unwrap();
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
            Reply::Confirmed(
                Purpose::Delete(Id::parse(&"c".repeat(32)).unwrap()),
                Act::Confirm
            )
        );
        // Pressed and released over it, it confirms.
        let mut confirm = open();
        confirm.input(&pointer(PointerPhase::Press));
        assert_eq!(
            confirm.input(&pointer(PointerPhase::Release)),
            Reply::Confirmed(
                Purpose::Delete(Id::parse(&"c".repeat(32)).unwrap()),
                Act::Confirm
            )
        );
    }

    #[test]
    fn tab_to_delete_and_return_confirms_for_its_conversation() {
        let mut confirm = open();
        assert_eq!(key(&mut confirm, "Tab"), Reply::Stay(true));
        assert_eq!(confirm.focus(), "delete");
        assert_eq!(
            key(&mut confirm, "Return"),
            Reply::Confirmed(
                Purpose::Delete(Id::parse(&"c".repeat(32)).unwrap()),
                Act::Confirm
            )
        );
    }
}

//! The question before a conversation is deleted (DESIGN.md §4): td-ui's
//! confirmation dialog (td-ui/DESIGN.md), centred over the window's body,
//! `Cancel` focused first, so `Return` alone deletes nothing.

use td_ui::chrome::ROW;
use td_ui::confirmations::{self, Choice, Controller, Event, Key, Model, Outcome};
use td_ui::raster::{Draw, Rect, Surface};
use td_ui::window::{Input, PointerPhase};
use td_ui::CELL_WIDTH;

use crate::store::Id;

/// The dialog's one action.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Delete;

/// What an input came to.
#[derive(Debug, Eq, PartialEq)]
pub enum Reply {
    /// Still open; whether it changed.
    Stay(bool),
    /// Closed with nothing deleted.
    Closed,
    /// The human chose `Delete` for this conversation.
    Confirmed(Id),
}

/// The dialog's title and its action's label.
pub const TITLE: &str = "Delete conversation";
pub const DELETE: &str = "Delete";

/// The open question: the conversation it is about and the dialog.
pub struct Confirm {
    id: Id,
    dialog: Controller<Delete, u64, ()>,
    revision: u64,
}

/// The dialog's place: at most 64 cells by 10 rows, centred in `body`.
fn place(surface: Surface, body: Rect) -> Rect {
    let scale = surface.scale.value();
    let pixels = |logical: usize| u32::try_from(logical.saturating_mul(scale)).unwrap_or(u32::MAX);
    let width = pixels(CELL_WIDTH * 64).min(body.width);
    let height = pixels(ROW * 10).min(body.height);
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
        revision: u64,
    ) -> Result<Self, String> {
        let named = format!("Delete \u{201c}{title}\u{201d} ({})?", id.as_str());
        let details: &[&str] = &[
            &named,
            "Its log, its todo list, its cost record and the messages waiting for it are removed from this machine for good. Messages it sent to other conversations stay in theirs. This cannot be undone.",
        ];
        let model = Model::new(TITLE, DELETE, details, Delete, revision)
            .map_err(|e| format!("the deletion question: {e}"))?;
        let dialog = Controller::new(model, surface, place(surface, body), None)
            .map_err(|e| format!("the deletion question: {e}"))?;
        Ok(Self {
            id,
            dialog,
            revision,
        })
    }

    /// The conversation it asks about.
    pub fn id(&self) -> &Id {
        &self.id
    }

    /// Where its keyboard is: `details`, `cancel` or `delete`.
    pub fn focus(&self) -> &'static str {
        match self.dialog.focus() {
            confirmations::Focus::Details => "details",
            confirmations::Focus::Cancel => "cancel",
            confirmations::Focus::Alternate | confirmations::Focus::Confirm => "delete",
        }
    }

    /// Lays it out again for a new surface; false when it no longer fits,
    /// which closes it.
    pub fn resize(&mut self, surface: Surface, body: Rect) -> bool {
        let event = Event::Resize {
            surface,
            rect: place(surface, body),
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
            | Input::Close => return Reply::Stay(false),
        };
        match self.dialog.event(Some(self.revision), false, event) {
            Outcome::Ignored | Outcome::Consumed => Reply::Stay(false),
            Outcome::Changed => Reply::Stay(true),
            Outcome::Closed {
                choice: Choice::Confirmed(Delete),
                ..
            } => Reply::Confirmed(self.id.clone()),
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
        Confirm::open(surface, surface.bounds(), id, "a plan", 7).unwrap()
    }

    fn key(confirm: &mut Confirm, chord: &str) -> Reply {
        confirm.input(&Input::Key {
            chord,
            repeat: false,
        })
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
        assert_eq!(confirm.input(&Input::Focus(false)), Reply::Closed);
        let mut confirm = open();
        confirm.input(&pointer(PointerPhase::Press));
        confirm.input(&Input::CancelPointer);
        assert_ne!(
            confirm.input(&pointer(PointerPhase::Release)),
            Reply::Confirmed(Id::parse(&"c".repeat(32)).unwrap())
        );
        // Pressed and released over it, it confirms.
        let mut confirm = open();
        confirm.input(&pointer(PointerPhase::Press));
        assert_eq!(
            confirm.input(&pointer(PointerPhase::Release)),
            Reply::Confirmed(Id::parse(&"c".repeat(32)).unwrap())
        );
    }

    #[test]
    fn tab_to_delete_and_return_confirms_for_its_conversation() {
        let mut confirm = open();
        assert_eq!(key(&mut confirm, "Tab"), Reply::Stay(true));
        assert_eq!(confirm.focus(), "delete");
        assert_eq!(
            key(&mut confirm, "Return"),
            Reply::Confirmed(Id::parse(&"c".repeat(32)).unwrap())
        );
    }
}

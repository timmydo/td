//! The dialog File → Prompts… opens on one prompt (DESIGN.md §13, "The
//! person's prompts"): a modal panel over the window holding its name, what
//! its form is, td-ui's editor pane, a line for why it is refused, and td-ui's
//! buttons. Shipped text loads what td-agent ships for the person to save or
//! not, so a replaced text is lost only by saving over it.

use td_ui::chrome::{Buttons, ROW};
use td_ui::editor::{Event as PaneEvent, Outcome as PaneOutcome};
use td_ui::raster::LINE_NUMBER;
use td_ui::raster::{self, Draw, GlyphStyle, Primitive, Rect, Surface, BORDER, CHROME, INK};
use td_ui::window::{Clipboard, PointerPhase};
use td_ui::CELL_WIDTH;

use crate::composer::Composer;
use crate::keydialog::wrap;
use crate::prompts::{check, Entry, Name};

const BUTTONS: &[&str] = &["Cancel", "Shipped text", "Save"];
const SHIPPED: usize = 1;
const SAVE: usize = 2;
const FORM_ROWS: usize = 3;
const MESSAGE_ROWS: i64 = 2;
const SAVING: &str = "Saving\u{2026}";
const MIN_EDITOR_ROWS: i64 = 4;
const KEYS: &str = "C-s saves; Escape cancels.";
const DISCARD: &str = "Your changes are not saved: Escape again discards them.";
const NO_TOOLS: &str = "It has no {tools}, so the model is not told of its workspace or tools: save again to keep it so.";
const REPLACE: &str = "Your changes are not saved: Shipped text again replaces them.";
const UNOPENABLE: &str = "Its file holds what this editor cannot show, so this is the shipped text; Save replaces the file.";
const LOADED: &str = "This is the text td-agent ships; Save puts it back in force.";

/// What an input came to.
#[derive(Debug, Eq, PartialEq)]
pub enum Reply {
    /// The dialog stays; whether it must be painted again.
    Stay(bool),
    /// The dialog closed with nothing saved.
    Closed,
    /// Put `text` in force as `name`, which held `from` when the dialog opened.
    Save {
        name: Name,
        from: String,
        text: String,
    },
}

struct Layout {
    rect: Rect,
    title: Rect,
    form: Rect,
    editor: Rect,
    message: Rect,
    buttons: Buttons<'static>,
}

pub struct PromptDialog {
    name: Name,
    /// What the entry said, to be found still there when saving.
    from: String,
    /// What it showed on opening, or loaded since, for what is unsaved.
    baseline: String,
    /// The save asked for and not answered: it takes nothing more.
    saving: Option<u64>,
    composer: Composer,
    surface: Surface,
    form: Vec<String>,
    message: Option<String>,
    warned: bool,
    warned_replace: bool,
    warned_tools: bool,
    driven: bool,
    pressed: Option<usize>,
    selecting: bool,
}

impl PromptDialog {
    /// Open on `entry`, refused when the window cannot hold it.
    pub fn open(surface: Surface, entry: &Entry) -> Result<Self, String> {
        let mut dialog = Self {
            name: entry.name,
            from: entry.text.clone(),
            baseline: String::new(),
            saving: None,
            composer: Composer::new()?,
            surface,
            form: Vec::new(),
            message: None,
            warned: false,
            warned_replace: false,
            warned_tools: false,
            driven: false,
            pressed: None,
            selecting: false,
        };
        if !dialog.fit() {
            return Err(format!(
                "the window is too small for the prompt dialog, so {} stays as it is",
                entry.name.title()
            ));
        }
        if dialog.load(&entry.text).is_err() {
            dialog.load(entry.name.shipped())?;
            dialog.message = Some(UNOPENABLE.to_string());
        } else {
            dialog.message = entry.trouble.as_ref().map(|why| format!("Refused: {why}"));
        }
        dialog.baseline = dialog.composer.text().to_string();
        Ok(dialog)
    }

    /// Lays the dialog out for its surface; whether it holds there.
    fn fit(&mut self) -> bool {
        let columns = self.columns();
        self.form = wrap(&format!("{} {KEYS}", self.name.form()), columns, FORM_ROWS);
        let Some(layout) = self.layout() else {
            return false;
        };
        self.composer.place(layout.editor, self.surface);
        true
    }

    fn load(&mut self, text: &str) -> Result<(), String> {
        self.composer.fresh();
        self.composer.insert(text)?;
        self.composer.chord("C-Home");
        Ok(())
    }

    fn columns(&self) -> usize {
        let s = self.surface.scale.value();
        (self.surface.width / (CELL_WIDTH * s)).saturating_sub(4)
    }

    fn layout(&self) -> Option<Layout> {
        let s = self.surface.scale.value() as i64;
        let cell = CELL_WIDTH as i64 * s;
        let row = ROW as i64 * s;
        let width = self.surface.width as i64 - 2 * cell;
        let height = self.surface.height as i64 - 2 * row;
        let (x, y) = (cell, row);
        let inside = width - 2 * s;
        let form = self.form.len() as i64;
        let editor_height = height - 2 * s - (1 + form + MESSAGE_ROWS + 1) * row;
        if inside < 20 * cell || editor_height < MIN_EDITOR_ROWS * row {
            return None;
        }
        let band = |at: i64, rows: i64| Rect {
            x: x + s,
            y: y + s + at * row,
            width: inside as u32,
            height: (rows * row) as u32,
        };
        let below = y + s + (1 + form) * row + editor_height;
        Some(Layout {
            rect: Rect {
                x,
                y,
                width: width as u32,
                height: height as u32,
            },
            title: band(0, 1),
            form: band(1, form),
            editor: Rect {
                x: x + s,
                y: y + s + (1 + form) * row,
                width: inside as u32,
                height: editor_height as u32,
            },
            message: Rect {
                x: x + s,
                y: below,
                width: inside as u32,
                height: (MESSAGE_ROWS * row) as u32,
            },
            buttons: Buttons::in_band(
                self.surface,
                x + s,
                below + MESSAGE_ROWS * row,
                inside as u32,
                BUTTONS,
            ),
        })
    }

    pub fn name(&self) -> Name {
        self.name
    }

    pub fn text(&self) -> &str {
        self.composer.text()
    }

    /// Input came from the control seam, so nothing saves it.
    pub fn drive(&mut self) {
        self.driven = true;
    }

    pub fn driven(&self) -> bool {
        self.driven
    }

    /// Asked to save as `ask`; it waits for that answer.
    pub fn wait(&mut self, ask: u64) {
        self.cancel_pointer();
        self.saving = Some(ask);
        self.message = Some(SAVING.to_string());
    }

    pub fn waiting(&self) -> bool {
        self.saving.is_some()
    }

    pub fn waiting_for(&self, ask: u64) -> bool {
        self.saving == Some(ask)
    }

    /// Saving is refused, the dialog saying why and taking input again.
    pub fn refused(&mut self, why: String) {
        self.saving = None;
        self.message = Some(why);
    }

    /// Whether the dialog still holds on `surface`.
    pub fn resize(&mut self, surface: Surface) -> bool {
        self.surface = surface;
        self.fit()
    }

    pub fn focus(&mut self, focused: bool) {
        self.composer.event(PaneEvent::Focus(focused));
    }

    pub fn tick(&mut self, now: u64) -> bool {
        self.composer.event(PaneEvent::Tick(now)) == PaneOutcome::Changed
    }

    pub fn scroll(&mut self, rows: isize, columns: isize) -> bool {
        self.composer.scroll(rows, columns)
    }

    pub fn cancel_pointer(&mut self) {
        self.composer.event(PaneEvent::CancelPointer);
        self.pressed = None;
        self.selecting = false;
    }

    pub fn paste(&mut self, text: &str) -> Reply {
        if self.saving.is_some() {
            return Reply::Stay(false);
        }
        match self.composer.insert(text) {
            Ok(changed) => {
                if changed {
                    self.changed();
                }
                Reply::Stay(changed)
            }
            Err(why) => {
                self.message = Some(why);
                Reply::Stay(true)
            }
        }
    }

    fn changed(&mut self) {
        self.warned = false;
        self.warned_replace = false;
        self.warned_tools = false;
        self.message = None;
    }

    /// What `act` does to the text, said to the warnings if it changes it.
    fn editing(&mut self, act: impl FnOnce(&mut Composer) -> bool) -> bool {
        let before = self.composer.target().map(|(_, revision)| revision);
        let moved = act(&mut self.composer);
        if self.composer.target().map(|(_, revision)| revision) != before {
            self.changed();
        }
        moved
    }

    fn unsaved(&self) -> bool {
        self.composer.text() != self.baseline
    }

    fn cancel(&mut self) -> Reply {
        if self.warned || !self.unsaved() {
            return Reply::Closed;
        }
        self.warned = true;
        self.message = Some(DISCARD.to_string());
        Reply::Stay(true)
    }

    /// Saves what it shows, unchanged too: a file gone bad under it has this
    /// put in its place.
    fn save(&mut self) -> Reply {
        let text = self.composer.text().to_string();
        if let Err(why) = check(self.name, &text) {
            self.message = Some(format!("It cannot be saved: {why}."));
            return Reply::Stay(true);
        }
        if self.name == Name::System && !text.contains("{tools}") && !self.warned_tools {
            self.warned_tools = true;
            self.message = Some(NO_TOOLS.to_string());
            return Reply::Stay(true);
        }
        Reply::Save {
            name: self.name,
            from: self.from.clone(),
            text,
        }
    }

    fn shipped(&mut self) -> Reply {
        let shipped = self.name.shipped();
        if self.unsaved() && self.composer.text() != shipped && !self.warned_replace {
            self.warned_replace = true;
            self.message = Some(REPLACE.to_string());
            return Reply::Stay(true);
        }
        match self.load(shipped) {
            Ok(()) => {
                self.changed();
                self.message = Some(LOADED.to_string());
            }
            Err(why) => self.message = Some(why),
        }
        Reply::Stay(true)
    }

    fn button(&mut self, at: usize) -> Reply {
        match at {
            SHIPPED => self.shipped(),
            SAVE => self.save(),
            _ => self.cancel(),
        }
    }

    pub fn key(&mut self, chord: &str, repeat: bool, clipboard: &mut dyn Clipboard) -> Reply {
        // The answer comes as a note when it is gone.
        if self.saving.is_some() {
            return if chord == "Escape" {
                Reply::Closed
            } else {
                Reply::Stay(false)
            };
        }
        match chord {
            "Escape" if !repeat => self.cancel(),
            "C-s" if !repeat => self.save(),
            "Return" | "S-Return" => Reply::Stay(self.editing(|composer| composer.chord("Return"))),
            "C-c" | "C-Insert" | "C-x" if !repeat => {
                let Some(snapshot) = self.composer.selection() else {
                    return Reply::Stay(false);
                };
                match clipboard.copy(snapshot.text()) {
                    Ok(()) if chord == "C-x" => {
                        self.editing(|composer| {
                            composer.event(PaneEvent::Cut(snapshot));
                            true
                        });
                        Reply::Stay(true)
                    }
                    Ok(()) => Reply::Stay(false),
                    Err(why) => {
                        self.message = Some(format!("copy: {why}"));
                        Reply::Stay(true)
                    }
                }
            }
            "C-v" | "S-Insert" if !repeat => match clipboard.paste() {
                Ok(()) => Reply::Stay(false),
                Err(why) => {
                    self.message = Some(format!("paste: {why}"));
                    Reply::Stay(true)
                }
            },
            _ => Reply::Stay(self.editing(|composer| composer.chord(chord))),
        }
    }

    pub fn pointer(&mut self, phase: PointerPhase, x: i64, y: i64, extend: bool) -> Reply {
        let Some(layout) = self.layout() else {
            return Reply::Stay(false);
        };
        if self.saving.is_some() {
            return Reply::Stay(false);
        }
        match phase {
            PointerPhase::Press => {
                if let Some(at) = layout.buttons.hit(x, y) {
                    self.pressed = Some(at);
                    return Reply::Stay(true);
                }
                if layout.editor.contains(x, y) {
                    self.selecting = true;
                    return Reply::Stay(self.composer.pointer(phase, x, y, extend));
                }
                Reply::Stay(false)
            }
            PointerPhase::Move => {
                Reply::Stay(self.selecting && self.composer.pointer(phase, x, y, extend))
            }
            PointerPhase::Release => {
                if std::mem::take(&mut self.selecting) {
                    return Reply::Stay(self.composer.pointer(phase, x, y, extend));
                }
                match self.pressed.take() {
                    Some(at) if layout.buttons.hit(x, y) == Some(at) => self.button(at),
                    Some(_) => Reply::Stay(true),
                    None => Reply::Stay(false),
                }
            }
        }
    }

    pub fn emit(&self, damage: Rect, sink: &mut dyn FnMut(Draw)) {
        let Some(layout) = self.layout() else {
            return;
        };
        let s = self.surface.scale.value();
        let fill = |rect: Rect, color: u32, sink: &mut dyn FnMut(Draw)| {
            if let Some(clip) = rect.intersection(damage) {
                sink(Draw {
                    clip,
                    primitive: Primitive::Fill { rect: clip, color },
                });
            }
        };
        fill(layout.rect, BORDER, sink);
        let inner = Rect {
            x: layout.rect.x + s as i64,
            y: layout.rect.y + s as i64,
            width: layout.rect.width.saturating_sub(2 * s as u32),
            height: layout.rect.height.saturating_sub(2 * s as u32),
        };
        fill(inner, CHROME, sink);
        let line = |rect: Rect, text: &str, style: GlyphStyle, sink: &mut dyn FnMut(Draw)| {
            raster::text_run(
                self.surface.scale,
                text.chars(),
                (
                    rect.x + (CELL_WIDTH * s) as i64 - s as i64,
                    rect.y + (4 * s) as i64,
                ),
                rect,
                style,
                damage,
                sink,
            );
        };
        let plain = GlyphStyle::regular(INK, CHROME);
        line(
            layout.title,
            self.name.title(),
            GlyphStyle::medium(INK, CHROME),
            sink,
        );
        let row = (ROW * s) as i64;
        for (at, text) in self.form.iter().enumerate() {
            let rect = Rect {
                y: layout.form.y + at as i64 * row,
                height: row as u32,
                ..layout.form
            };
            line(rect, text, plain, sink);
        }
        if let Some(clip) = layout.editor.intersection(damage) {
            self.composer.emit(clip, sink);
        }
        if let Some(message) = &self.message {
            for (at, text) in wrap(message, self.columns(), MESSAGE_ROWS as usize)
                .iter()
                .enumerate()
            {
                let rect = Rect {
                    y: layout.message.y + at as i64 * row,
                    height: row as u32,
                    ..layout.message
                };
                line(
                    rect,
                    text,
                    GlyphStyle {
                        ink: LINE_NUMBER,
                        ..plain
                    },
                    sink,
                );
            }
        }
        layout.buttons.emit(
            (0..BUTTONS.len()).map(|at| (self.pressed == Some(at), true)),
            damage,
            sink,
        );
    }

    #[cfg(test)]
    pub fn button_at(&self, at: usize) -> Option<(i64, i64)> {
        let rect = self.layout()?.buttons.button(at)?.rect();
        Some((rect.x + 2, rect.y + 2))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
    use super::*;
    use crate::prompts::Prompts;
    use td_ui::raster::Scale;
    use td_ui::window::NoClipboard;

    fn surface() -> Surface {
        Surface::new(1024, 640, Scale::default()).unwrap()
    }

    fn open(name: Name) -> PromptDialog {
        let entries = Prompts::default().entries();
        let entry = entries.iter().find(|e| e.name == name).unwrap();
        PromptDialog::open(surface(), entry).unwrap()
    }

    fn key(dialog: &mut PromptDialog, chord: &str) -> Reply {
        dialog.key(chord, false, &mut NoClipboard)
    }

    fn click(dialog: &mut PromptDialog, at: usize) -> Reply {
        let (x, y) = dialog.button_at(at).unwrap();
        dialog.pointer(PointerPhase::Press, x, y, false);
        dialog.pointer(PointerPhase::Release, x, y, false)
    }

    #[test]
    fn it_opens_on_the_text_and_saves_what_was_typed() {
        let mut dialog = open(Name::Classifier);
        assert_eq!(dialog.text(), Name::Classifier.shipped());
        // Saved as it stands, for a file gone bad under it.
        assert_eq!(
            key(&mut dialog, "C-s"),
            Reply::Save {
                name: Name::Classifier,
                from: Name::Classifier.shipped().to_string(),
                text: Name::Classifier.shipped().to_string(),
            }
        );
        dialog.paste("MINE ");
        let Reply::Save { name, text, .. } = key(&mut dialog, "C-s") else {
            panic!("not saved");
        };
        assert_eq!(name, Name::Classifier);
        assert!(text.starts_with("MINE You are"), "{text:.40}");
        // Return is a new line in the text, not an answer to the dialog.
        let mut dialog = open(Name::System);
        assert_eq!(key(&mut dialog, "Return"), Reply::Stay(true));
        assert!(dialog.text().starts_with('\n'));
    }

    #[test]
    fn escape_asks_again_before_changes_are_lost() {
        let mut dialog = open(Name::System);
        assert_eq!(key(&mut dialog, "Escape"), Reply::Closed);
        let mut dialog = open(Name::System);
        dialog.paste("x");
        assert_eq!(key(&mut dialog, "Escape"), Reply::Stay(true));
        assert_eq!(dialog.message.as_deref(), Some(DISCARD));
        // Typing takes the warning back, even one that keeps the length.
        dialog.paste("y");
        assert_eq!(dialog.message, None);
        assert_eq!(key(&mut dialog, "Escape"), Reply::Stay(true));
        key(&mut dialog, "BackSpace");
        key(&mut dialog, "z");
        assert_eq!(dialog.message, None);
        assert_eq!(key(&mut dialog, "Escape"), Reply::Stay(true));
        assert_eq!(key(&mut dialog, "Escape"), Reply::Closed);
        // Cancel asks as Escape does.
        let mut dialog = open(Name::System);
        dialog.paste("x");
        assert_eq!(click(&mut dialog, 0), Reply::Stay(true));
        assert_eq!(click(&mut dialog, 0), Reply::Closed);
    }

    #[test]
    fn a_text_past_its_form_stays_and_says_why() {
        let mut dialog = open(Name::JevDiscloses);
        dialog.paste("no form here\n");
        assert_eq!(key(&mut dialog, "C-s"), Reply::Stay(true));
        assert!(dialog.message.as_deref().unwrap().contains("instructions"));
        assert!(dialog.text().starts_with("no form here"));
    }

    #[test]
    fn shipped_text_is_loaded_and_saved_only_on_save() {
        let entry = Entry {
            name: Name::Classifier,
            text: "mine".into(),
            custom: true,
            trouble: None,
        };
        let mut dialog = PromptDialog::open(surface(), &entry).unwrap();
        assert_eq!(dialog.text(), "mine");
        assert_eq!(click(&mut dialog, SHIPPED), Reply::Stay(true));
        assert_eq!(dialog.text(), Name::Classifier.shipped());
        assert_eq!(
            click(&mut dialog, SAVE),
            Reply::Save {
                name: Name::Classifier,
                from: "mine".to_string(),
                text: Name::Classifier.shipped().to_string(),
            }
        );
        // Edits are not thrown away at the first press.
        let mut dialog = PromptDialog::open(surface(), &entry).unwrap();
        dialog.paste("more ");
        assert_eq!(click(&mut dialog, SHIPPED), Reply::Stay(true));
        assert_ne!(dialog.text(), Name::Classifier.shipped());
        assert_eq!(click(&mut dialog, SHIPPED), Reply::Stay(true));
        assert_eq!(dialog.text(), Name::Classifier.shipped());
        // A press carried off a button does nothing.
        let (x, y) = dialog.button_at(SAVE).unwrap();
        dialog.pointer(PointerPhase::Press, x, y, false);
        assert_eq!(
            dialog.pointer(PointerPhase::Release, 0, 0, false),
            Reply::Stay(true)
        );
    }

    #[test]
    fn a_system_prompt_without_its_placeholder_is_saved_only_when_asked_twice() {
        let entry = Entry {
            name: Name::System,
            text: "mine".into(),
            custom: true,
            trouble: None,
        };
        let mut dialog = PromptDialog::open(surface(), &entry).unwrap();
        assert_eq!(key(&mut dialog, "C-s"), Reply::Stay(true));
        assert_eq!(dialog.message.as_deref(), Some(NO_TOOLS));
        assert!(matches!(key(&mut dialog, "C-s"), Reply::Save { .. }));
    }

    #[test]
    fn a_file_that_cannot_be_taken_opens_and_can_be_replaced() {
        let shipped = Name::JevRequest.shipped();
        let entry = |text: &str| Entry {
            name: Name::JevRequest,
            text: text.into(),
            custom: true,
            trouble: Some("why".into()),
        };
        // What the editor cannot hold shows the shipped text and says so; nothing
        // the person did is unsaved.
        let mut dialog = PromptDialog::open(surface(), &entry("a\u{1b}b")).unwrap();
        assert_eq!(dialog.text(), shipped);
        assert_eq!(dialog.message.as_deref(), Some(UNOPENABLE));
        assert_eq!(
            click(&mut dialog, SAVE),
            Reply::Save {
                name: Name::JevRequest,
                from: "a\u{1b}b".to_string(),
                text: shipped.to_string(),
            }
        );
        let mut dialog = PromptDialog::open(surface(), &entry("a\u{1b}b")).unwrap();
        assert_eq!(key(&mut dialog, "Escape"), Reply::Closed);
        // What it can is there to mend, with why it is refused.
        let mut dialog = PromptDialog::open(surface(), &entry("no form")).unwrap();
        assert_eq!(dialog.text(), "no form");
        assert_eq!(dialog.message.as_deref(), Some("Refused: why"));
        assert_eq!(click(&mut dialog, SHIPPED), Reply::Stay(true));
        assert_eq!(dialog.text(), shipped);
    }

    #[test]
    fn while_it_waits_for_its_answer_it_takes_nothing_but_escape() {
        let mut dialog = open(Name::Classifier);
        dialog.paste("MINE ");
        let text = dialog.text().to_string();
        dialog.wait(1);
        assert_eq!(key(&mut dialog, "x"), Reply::Stay(false));
        assert_eq!(key(&mut dialog, "C-s"), Reply::Stay(false));
        assert_eq!(dialog.paste("y"), Reply::Stay(false));
        assert_eq!(click(&mut dialog, SAVE), Reply::Stay(false));
        assert_eq!(dialog.text(), text);
        // Refused, it takes input again.
        dialog.refused("no".into());
        assert!(!dialog.waiting());
        assert!(matches!(key(&mut dialog, "C-s"), Reply::Save { .. }));
        // A drag it was in the middle of is over, so the next click is a click.
        let editor = dialog.layout().unwrap().editor;
        dialog.pointer(PointerPhase::Press, editor.x + 4, editor.y + 4, false);
        dialog.wait(2);
        assert!(dialog.waiting_for(2) && !dialog.waiting_for(1));
        dialog.pointer(PointerPhase::Release, editor.x + 4, editor.y + 4, false);
        dialog.refused("no".into());
        assert!(matches!(click(&mut dialog, SAVE), Reply::Save { .. }));
        dialog.wait(3);
        assert_eq!(key(&mut dialog, "Escape"), Reply::Closed);
    }

    #[test]
    fn a_window_too_small_refuses_it() {
        let small = Surface::new(320, 120, Scale::default()).unwrap();
        let entries = Prompts::default().entries();
        assert!(PromptDialog::open(small, &entries[0]).is_err());
        let mut dialog = open(Name::System);
        assert!(!dialog.resize(small));
        assert!(dialog.resize(surface()));
    }
}

//! The dialog File → Set OpenRouter key… opens (DESIGN.md §4, §6): a
//! modal panel over the window holding a title, what the dialog does and
//! where the key is stored, one masked td-ui entry (`entry_model` in its
//! masked mode, painted by `chrome::TextEntry`), a line for why a key is
//! refused or what is odd about it, and td-ui's Cancel and Save buttons.
//! Replacing a stored key is confirmed through td-ui's confirmation
//! dialog. The dialog is composed from those widgets as td-pass composes
//! its PIN prompt; nothing here edits text, draws a button or a field, or
//! navigates a confirmation itself.
//!
//! The key is never shown, copied or said: the entry is masked, so it
//! paints a mask glyph per character and refuses copy and cut; the
//! dialog's facts for the driven seam are its focus, its length and its
//! message, which names faults and never the text; and the entry is
//! cleared, its bytes zeroed, when the dialog closes, cancelled or saved.

use td_ui::chrome::{Buttons, TextEntry, ROW};
use td_ui::confirmations::{
    self, Choice, Controller as Confirm, Event as ConfirmEvent, Key as ConfirmKey,
    Model as ConfirmModel, Outcome as Confirmed,
};
use td_ui::entry_model::{Action, EntryModel, Outcome as Typed, Refusal};
use td_ui::raster::{
    self, Draw, GlyphStyle, Primitive, Rect, Surface, Weight, BORDER, CHROME, INK, LINE_NUMBER,
};
use td_ui::window::{Clipboard, PointerPhase};
use td_ui::CELL_WIDTH;

use crate::key::{self, Secret};

const TITLE: &str = "Set OpenRouter key";
const BUTTONS: [&str; 2] = ["Cancel", "Save"];
/// The most rows the explanation takes.
const MAX_LINES: usize = 6;
/// The rows a refusal or a warning takes, wrapped.
const MESSAGE_ROWS: usize = 3;
/// The widest the dialog grows, in cells.
const MAX_COLUMNS: usize = 72;
/// The fewest text columns it lays out in.
const MIN_COLUMNS: usize = 24;
/// The replace confirmation's rows and widest cells.
const CONFIRM_ROWS: i64 = 7;
const CONFIRM_COLUMNS: i64 = 56;
/// The confirmation's one revision: it is the dialog's alone.
const REVISION: u64 = 1;

/// The part of the dialog the keyboard is on.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Part {
    Entry,
    Cancel,
    Save,
}

impl Part {
    pub fn word(self) -> &'static str {
        match self {
            Self::Entry => "entry",
            Self::Cancel => "cancel",
            Self::Save => "save",
        }
    }
}

/// The confirmation's action: replace the stored key.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Replace;

/// What the window does after an input to the dialog.
#[derive(Debug, Eq, PartialEq)]
pub enum Reply {
    /// The dialog stays; whether it must be painted again.
    Stay(bool),
    /// The dialog closed, cancelled, its entry cleared.
    Closed,
    /// Store this key, replacing a stored one only when `replace` says.
    Save { secret: Secret, replace: bool },
}

/// Where each part is over the surface.
#[derive(Clone, Copy)]
struct Layout {
    rect: Rect,
    title: Rect,
    /// The explanation's first row; each line is a `ROW` under the last.
    lines: Rect,
    entry: TextEntry,
    message: Rect,
    buttons: Buttons<'static>,
}

pub struct KeyDialog {
    entry: EntryModel,
    focus: Part,
    /// A button pressed and not yet released.
    armed: Option<Part>,
    /// A press on the entry, extending its selection until released.
    dragging: bool,
    /// Why the text is refused, or what is odd about it; never the text.
    message: Option<String>,
    explanation: String,
    /// The explanation wrapped for the surface's width.
    lines: Vec<String>,
    surface: Surface,
    confirm: Option<Confirm<Replace, u64, ()>>,
    /// The dialog asked the clipboard for its text, not yet come.
    pasting: bool,
}

impl std::fmt::Debug for KeyDialog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KeyDialog")
            .field("entry", &self.entry)
            .field("focus", &self.focus)
            .field("confirming", &self.confirm.is_some())
            .finish_non_exhaustive()
    }
}

/// `text` wrapped at `columns` on spaces, a word longer than a row broken
/// where the row ends, at most `rows` rows, the last ending in an
/// ellipsis when there was more.
fn wrap(text: &str, columns: usize, rows: usize) -> Vec<String> {
    let columns = columns.max(1);
    let mut lines: Vec<String> = Vec::new();
    let mut line = String::new();
    let mut width = 0usize;
    for word in text.split(' ').filter(|w| !w.is_empty()) {
        let mut chars: Vec<char> = word.chars().collect();
        if width > 0 && width + 1 + chars.len() > columns {
            lines.push(std::mem::take(&mut line));
            width = 0;
        }
        if width > 0 {
            line.push(' ');
            width += 1;
        }
        while width + chars.len() > columns {
            let room = columns.saturating_sub(width).min(chars.len());
            line.extend(chars.drain(..room));
            lines.push(std::mem::take(&mut line));
            width = 0;
        }
        width += chars.len();
        line.extend(chars);
    }
    if !line.is_empty() {
        lines.push(line);
    }
    if lines.len() > rows {
        lines.truncate(rows);
        if let Some(last) = lines.last_mut() {
            if last.chars().count() >= columns {
                last.pop();
            }
            last.push('\u{2026}');
        }
    }
    lines
}

/// What the entry's refusal means here.
fn refusal(refusal: Refusal) -> String {
    match refusal {
        Refusal::Limit => format!("a key is at most {} bytes", key::MAX_KEY),
        Refusal::Control => "a key is one line of printable ASCII".into(),
        Refusal::Masked => refusal.to_string(),
    }
}

impl KeyDialog {
    /// The dialog over `surface`, saying the key goes to `path`; refused
    /// when the surface cannot hold it.
    pub fn open(surface: Surface, path: &str) -> Result<Self, String> {
        let mut entry =
            EntryModel::new(key::MAX_KEY).map_err(|e| format!("the key's entry: {e}"))?;
        entry.set_masked(true);
        let mut dialog = Self {
            entry,
            focus: Part::Entry,
            armed: None,
            dragging: false,
            message: None,
            explanation: format!(
                "Paste your OpenRouter API key with C-v, or type it; Return saves and Escape cancels. \
                 It is stored in {path}, mode 0600, and every conversation uses it from now on."
            ),
            lines: Vec::new(),
            surface,
            confirm: None,
            pasting: false,
        };
        dialog.relayout();
        if dialog.layout().is_none() {
            return Err("the window is too small for the key dialog".into());
        }
        Ok(dialog)
    }

    /// The part the keyboard is on, or `replace` while the confirmation
    /// asks.
    pub fn part(&self) -> &'static str {
        if self.confirm.is_some() {
            "replace"
        } else {
            self.focus.word()
        }
    }

    /// How many characters the entry holds; the text itself is never
    /// handed out.
    pub fn length(&self) -> usize {
        self.entry.text().chars().count()
    }

    pub fn message(&self) -> Option<&str> {
        self.message.as_deref()
    }

    pub fn pasting(&self) -> bool {
        self.pasting
    }

    /// Whether the entry is masked; it always is.
    pub fn masked(&self) -> bool {
        self.entry.masked()
    }

    /// Forgets the text, zeroing its bytes, before the dialog goes.
    pub fn close(&mut self) {
        self.entry.clear();
        self.confirm = None;
        self.message = None;
        self.pasting = false;
        self.armed = None;
        self.dragging = false;
    }

    fn columns(&self) -> usize {
        let s = self.surface.scale.value();
        let cells = (self.surface.width / (CELL_WIDTH * s))
            .saturating_sub(2)
            .min(MAX_COLUMNS);
        cells.saturating_sub(2)
    }

    fn relayout(&mut self) {
        self.lines = wrap(&self.explanation, self.columns(), MAX_LINES);
        if let Some(layout) = self.layout() {
            self.entry.reveal(layout.entry);
        }
    }

    fn layout(&self) -> Option<Layout> {
        let columns = self.columns();
        if columns < MIN_COLUMNS {
            return None;
        }
        let s = self.surface.scale.value() as i64;
        let cell = CELL_WIDTH as i64 * s;
        let row = ROW as i64 * s;
        let width = (columns as i64 + 2) * cell;
        let lines = self.lines.len() as i64;
        let message_rows = MESSAGE_ROWS as i64;
        let height = (lines + 3 + message_rows) * row + 2 * s;
        if height > self.surface.height as i64 {
            return None;
        }
        let x = (self.surface.width as i64 - width) / 2;
        let y = (self.surface.height as i64 - height) / 2;
        let inside = (width - 2 * s) as u32;
        let band = |at: i64| Rect {
            x: x + s,
            y: y + s + at * row,
            width: inside,
            height: row as u32,
        };
        let entry = TextEntry::new(
            self.surface,
            Rect {
                x: x + cell,
                y: y + s + (1 + lines) * row,
                width: (width - 2 * cell) as u32,
                height: row as u32,
            },
        )?;
        Some(Layout {
            rect: Rect {
                x,
                y,
                width: width as u32,
                height: height as u32,
            },
            title: band(0),
            lines: band(1),
            entry,
            message: Rect {
                height: (message_rows * row) as u32,
                ..band(2 + lines)
            },
            buttons: Buttons::in_band(
                self.surface,
                x + s,
                y + s + (2 + lines + message_rows) * row,
                inside,
                &BUTTONS,
            ),
        })
    }

    /// The surface changed: the dialog lays out again, and answers whether
    /// it still fits.
    pub fn resize(&mut self, surface: Surface) -> bool {
        self.surface = surface;
        self.armed = None;
        self.dragging = false;
        self.relayout();
        if let Some(confirm) = self.confirm.as_mut() {
            let rect = confirm_rects(surface)[0];
            if matches!(
                confirm.event(
                    Some(REVISION),
                    false,
                    ConfirmEvent::Resize { surface, rect }
                ),
                Confirmed::Closed { .. }
            ) {
                self.confirm = None;
            }
        }
        self.layout().is_some()
    }

    /// The window lost the keyboard: a gesture ends, and a confirmation
    /// asking is cancelled, as td-ui's confirmation does.
    pub fn focus_lost(&mut self) {
        self.armed = None;
        self.dragging = false;
        if let Some(confirm) = self.confirm.as_mut() {
            if matches!(
                confirm.event(Some(REVISION), false, ConfirmEvent::FocusLost),
                Confirmed::Closed { .. }
            ) {
                self.confirm = None;
            }
        }
    }

    /// The pointer left mid-gesture: nothing it pressed acts.
    pub fn cancel_pointer(&mut self) {
        self.armed = None;
        self.dragging = false;
        if let Some(confirm) = self.confirm.as_mut() {
            confirm.event(Some(REVISION), false, ConfirmEvent::Other);
        }
    }

    /// After an edit: the entry's caret shown, and the message the text
    /// earns now (none while it is empty).
    fn edited(&mut self) {
        let text = self.entry.text();
        self.message = if text.is_empty() {
            None
        } else {
            match key::check_key(text) {
                Err(why) | Ok(Some(why)) => Some(why.to_string()),
                Ok(None) => None,
            }
        };
        if let Some(layout) = self.layout() {
            self.entry.reveal(layout.entry);
        }
    }

    fn say(&mut self, message: impl Into<String>) -> Reply {
        self.message = Some(message.into());
        Reply::Stay(true)
    }

    /// Save: the text checked, and handed on when it is a key.
    fn save(&mut self, replace: bool) -> Reply {
        match key::check_key(self.entry.text()) {
            Err(why) => self.say(why),
            Ok(_) => Reply::Save {
                secret: Secret::new(self.entry.text().to_string()),
                replace,
            },
        }
    }

    fn activate(&mut self, part: Part) -> Reply {
        match part {
            Part::Cancel => {
                self.close();
                Reply::Closed
            }
            Part::Save | Part::Entry => self.save(false),
        }
    }

    /// A key, by its chord. Every chord is the dialog's while it is open:
    /// what it does not use is consumed.
    pub fn key(&mut self, chord: &str, repeat: bool, clipboard: &mut dyn Clipboard) -> Reply {
        if self.confirm.is_some() {
            let event = ConfirmKey::from_chord(chord).map_or(ConfirmEvent::Other, |key| {
                ConfirmEvent::Key {
                    key,
                    repeated: repeat,
                }
            });
            return self.confirming(event);
        }
        let order = [Part::Entry, Part::Cancel, Part::Save];
        let at = order.iter().position(|p| *p == self.focus).unwrap_or(0);
        match chord {
            "Escape" if !repeat => self.activate(Part::Cancel),
            "Return" if !repeat => self.activate(self.focus),
            "Space" | " " if !repeat && self.focus != Part::Entry => self.activate(self.focus),
            "Tab" | "S-Tab" => {
                let step = if chord == "Tab" { 1 } else { order.len() - 1 };
                self.focus = order
                    .get((at + step) % order.len())
                    .copied()
                    .unwrap_or(Part::Entry);
                Reply::Stay(true)
            }
            "C-v" | "S-Insert" if !repeat => {
                self.focus = Part::Entry;
                match clipboard.paste() {
                    Ok(()) => {
                        self.pasting = true;
                        Reply::Stay(true)
                    }
                    Err(why) => self.say(format!("paste: {why}")),
                }
            }
            // A masked entry refuses to copy; the clipboard is never asked.
            "C-c" | "C-Insert" if !repeat => match self.entry.copy() {
                Err(why) => self.say(refusal(why)),
                Ok(_) => Reply::Stay(false),
            },
            "C-x" | "S-Delete" if !repeat => match self.entry.cut() {
                Err(why) => self.say(refusal(why)),
                Ok(_) => Reply::Stay(false),
            },
            _ if self.focus == Part::Entry => match Action::from_chord(chord) {
                Some(action) => match self.entry.act(action) {
                    Ok(Typed::Changed | Typed::Moved) => {
                        self.edited();
                        Reply::Stay(true)
                    }
                    Ok(Typed::Ignored) => Reply::Stay(false),
                    Err(why) => self.say(refusal(why)),
                },
                None => Reply::Stay(false),
            },
            _ => Reply::Stay(false),
        }
    }

    /// The clipboard's text the dialog asked for, trimmed of the
    /// whitespace around it, over the selection or at the caret.
    pub fn paste(&mut self, text: &str) -> Reply {
        self.pasting = false;
        self.focus = Part::Entry;
        match self.entry.paste(key::trim_paste(text)) {
            Ok(_) => {
                self.edited();
                Reply::Stay(true)
            }
            Err(why) => self.say(refusal(why)),
        }
    }

    /// The pointer, at a surface pixel. A press outside the dialog is
    /// consumed: it is modal.
    pub fn pointer(&mut self, phase: PointerPhase, x: i64, y: i64, extend: bool) -> Reply {
        if self.confirm.is_some() {
            let event = match phase {
                PointerPhase::Press => ConfirmEvent::Press { x, y },
                PointerPhase::Move => ConfirmEvent::Move { x, y },
                PointerPhase::Release => ConfirmEvent::Release { x, y },
            };
            return self.confirming(event);
        }
        let Some(layout) = self.layout() else {
            return Reply::Stay(false);
        };
        let button = |x, y| match layout.buttons.hit(x, y) {
            Some(0) => Some(Part::Cancel),
            Some(1) => Some(Part::Save),
            _ => None,
        };
        match phase {
            PointerPhase::Press => {
                self.armed = None;
                if layout.entry.rect().contains(x, y) {
                    self.focus = Part::Entry;
                    self.dragging = true;
                    self.entry.place(layout.entry, x, y, extend);
                    self.entry.reveal(layout.entry);
                    Reply::Stay(true)
                } else if let Some(part) = button(x, y) {
                    self.focus = part;
                    self.armed = Some(part);
                    Reply::Stay(true)
                } else {
                    Reply::Stay(false)
                }
            }
            PointerPhase::Move => {
                if self.dragging && self.entry.drag(layout.entry, x, y) != Typed::Ignored {
                    self.entry.reveal(layout.entry);
                    return Reply::Stay(true);
                }
                Reply::Stay(false)
            }
            PointerPhase::Release => {
                self.dragging = false;
                match self.armed.take() {
                    Some(part) if button(x, y) == Some(part) => self.activate(part),
                    Some(_) => Reply::Stay(true),
                    None => Reply::Stay(false),
                }
            }
        }
    }

    fn confirming(&mut self, event: ConfirmEvent) -> Reply {
        let Some(confirm) = self.confirm.as_mut() else {
            return Reply::Stay(false);
        };
        match confirm.event(Some(REVISION), false, event) {
            Confirmed::Ignored | Confirmed::Consumed => Reply::Stay(false),
            Confirmed::Changed => Reply::Stay(true),
            Confirmed::Closed { choice, .. } => {
                self.confirm = None;
                match choice {
                    Choice::Confirmed(Replace) => self.save(true),
                    Choice::Cancelled | Choice::Stale => self.say("the stored key is kept"),
                    Choice::Unavailable(e) => self.say(format!("the stored key is kept: {e}")),
                }
            }
        }
    }

    /// The window found a key stored: ask whether to replace it, the
    /// confirmation's Replace kept from under `pointer`, the press that
    /// asked to save.
    pub fn ask_replace(&mut self, pointer: Option<(i64, i64)>) {
        let mut failed = None;
        for rect in confirm_rects(self.surface) {
            let made = ConfirmModel::new(
                "Replace the OpenRouter key",
                "Replace",
                &["A key is already stored; replace it?"],
                Replace,
                REVISION,
            )
            .and_then(|model| Confirm::new(model, self.surface, rect, None));
            let confirm = match made {
                Ok(confirm) => confirm,
                Err(e) => {
                    failed = Some(e);
                    continue;
                }
            };
            let under = pointer.is_some_and(|(x, y)| {
                confirm
                    .action_rect(confirmations::Focus::Confirm)
                    .is_some_and(|r| r.contains(x, y))
            });
            if !under {
                self.confirm = Some(confirm);
                return;
            }
        }
        self.message = Some(match failed {
            Some(e) => {
                format!("a key is already stored, and replacing it cannot be asked here: {e}")
            }
            None => "a key is already stored; move the pointer and save again to replace it".into(),
        });
    }

    /// The window refused to store the key, for the reason given.
    pub fn refused(&mut self, why: String) {
        self.message = Some(why);
    }

    /// Paints the dialog, and the confirmation over it while it asks.
    pub fn emit(&self, focused: bool, damage: Rect, sink: &mut dyn FnMut(Draw)) {
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
        let plain = GlyphStyle {
            ink: INK,
            background: CHROME,
            weight: Weight::Regular,
        };
        line(layout.title, TITLE, GlyphStyle::medium(INK, CHROME), sink);
        let row = (ROW * s) as i64;
        for (at, text) in self.lines.iter().enumerate() {
            let rect = Rect {
                y: layout.lines.y + at as i64 * row,
                ..layout.lines
            };
            line(rect, text, plain, sink);
        }
        layout.entry.emit(
            self.entry.field(
                "sk-or-v1-\u{2026}",
                focused && self.focus == Part::Entry,
                focused && self.focus == Part::Entry && self.confirm.is_none(),
            ),
            damage,
            sink,
        );
        if let Some(message) = &self.message {
            for (at, text) in wrap(message, self.columns(), MESSAGE_ROWS)
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
            [
                (self.focus == Part::Cancel, true),
                (self.focus == Part::Save, true),
            ],
            damage,
            sink,
        );
        if let Some(confirm) = &self.confirm {
            confirm.emit(damage, sink);
        }
    }
}

/// Where the replace confirmation may go, in the order tried: centred,
/// then at the top and the foot of the surface.
fn confirm_rects(surface: Surface) -> [Rect; 3] {
    let s = surface.scale.value() as i64;
    let cell = CELL_WIDTH as i64 * s;
    let row = ROW as i64 * s;
    let (sw, sh) = (surface.width as i64, surface.height as i64);
    let width = (sw - 2 * cell).clamp(0, CONFIRM_COLUMNS * cell);
    let height = (CONFIRM_ROWS * row).min(sh);
    let x = (sw - width) / 2;
    let rect = |y: i64| Rect {
        x,
        y,
        width: width as u32,
        height: height as u32,
    };
    [rect((sh - height) / 2), rect(0), rect(sh - height)]
}

#[cfg(test)]
pub mod tests {
    #![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
    use super::*;
    use std::sync::Arc;
    use td_ui::raster::{Composition, Scale};
    use td_ui::window::{NoClipboard, Refusal as ClipboardRefusal};

    pub const KEY: &str = "sk-or-v1-0123456789abcdef";

    /// A clipboard that records what it is asked: a paste asked for, and
    /// every text offered it, which must stay empty. `inflight` is a
    /// paste asked for whose reply has not come, as td-ui's clipboard
    /// says until it hands the text over or gives up.
    #[derive(Default)]
    pub struct Recorder {
        pub pastes: usize,
        pub copies: Vec<String>,
        pub inflight: bool,
    }

    impl Clipboard for Recorder {
        fn available(&self) -> bool {
            true
        }
        fn has_text(&self) -> bool {
            true
        }
        fn pasting(&self) -> bool {
            self.inflight
        }
        fn copy(&mut self, text: Arc<str>) -> Result<(), ClipboardRefusal> {
            self.copies.push(text.to_string());
            Ok(())
        }
        fn paste(&mut self) -> Result<(), ClipboardRefusal> {
            self.pastes += 1;
            self.inflight = true;
            Ok(())
        }
    }

    fn surface() -> Surface {
        Surface::new(1024, 640, Scale::default()).unwrap()
    }

    fn dialog() -> KeyDialog {
        KeyDialog::open(surface(), "/home/me/.config/td-agent/openrouter.key").unwrap()
    }

    fn type_text(dialog: &mut KeyDialog, text: &str) {
        for c in text.chars() {
            assert_eq!(
                dialog.key(&c.to_string(), false, &mut NoClipboard),
                Reply::Stay(true)
            );
        }
    }

    /// What the dialog shows, as the driven seam's `text` reads it.
    struct Shown<'a>(&'a KeyDialog);
    impl Composition for Shown<'_> {
        fn surface(&self) -> Surface {
            self.0.surface
        }
        fn emit(&self, damage: Rect, sink: &mut dyn FnMut(Draw)) {
            self.0.emit(true, damage, sink);
        }
    }

    pub fn shown(dialog: &KeyDialog) -> String {
        td_ui::driven::text(&Shown(dialog)).unwrap().2
    }

    #[test]
    fn the_entry_is_masked_and_what_is_typed_never_shows() {
        let mut dialog = dialog();
        assert!(dialog.masked());
        type_text(&mut dialog, KEY);
        assert_eq!(dialog.length(), KEY.len());
        let text = shown(&dialog);
        assert!(text.contains(TITLE), "{text}");
        assert!(text.contains("openrouter.key, mode 0600"), "{text}");
        assert!(text.contains(&"\u{2022}".repeat(KEY.len())), "{text}");
        assert!(!text.contains("0123456789abcdef"), "{text}");
        assert!(!format!("{dialog:?}").contains("0123456789"));
        // Copy and cut refuse, and never reach the clipboard.
        let mut clipboard = Recorder::default();
        dialog.key("C-a", false, &mut clipboard);
        for chord in ["C-c", "C-x", "C-Insert", "S-Delete"] {
            assert_eq!(dialog.key(chord, false, &mut clipboard), Reply::Stay(true));
            assert_eq!(dialog.message(), Some("a masked entry does not copy"));
        }
        assert!(clipboard.copies.is_empty());
        assert_eq!(dialog.length(), KEY.len(), "a refused cut removes nothing");
    }

    #[test]
    fn a_paste_is_trimmed_and_a_text_that_is_no_key_is_refused_with_why() {
        let mut dialog = dialog();
        let mut clipboard = Recorder::default();
        assert_eq!(dialog.key("C-v", false, &mut clipboard), Reply::Stay(true));
        assert_eq!(clipboard.pastes, 1);
        assert!(dialog.pasting());
        assert_eq!(dialog.paste(&format!("  {KEY}\n")), Reply::Stay(true));
        assert!(!dialog.pasting());
        assert_eq!(dialog.length(), KEY.len());
        assert_eq!(dialog.message(), None);
        // Saving hands the key on.
        assert_eq!(
            dialog.key("Return", false, &mut clipboard),
            Reply::Save {
                secret: Secret::new(KEY.into()),
                replace: false
            }
        );
        // Two lines: the entry refuses the paste whole.
        let mut dialog = super::tests::dialog();
        dialog.paste("sk-or-1\nsk-or-2");
        assert_eq!(
            dialog.message(),
            Some("a key is one line of printable ASCII")
        );
        assert_eq!(dialog.length(), 0);
        // Past the bound.
        dialog.paste(&format!("sk-or-{}", "k".repeat(key::MAX_KEY)));
        assert_eq!(dialog.message(), Some("a key is at most 256 bytes"));
        // Not ASCII: said as it is typed, and refused when saved.
        type_text(&mut dialog, "sk-or-\u{e9}");
        assert!(dialog.message().unwrap().contains("printable ASCII"));
        assert_eq!(
            dialog.key("Return", false, &mut NoClipboard),
            Reply::Stay(true)
        );
        // Empty: refused when saved.
        let mut dialog = super::tests::dialog();
        assert_eq!(
            dialog.key("Return", false, &mut NoClipboard),
            Reply::Stay(true)
        );
        assert_eq!(dialog.message(), Some("the key is empty"));
        // Another provider's key: warned, and saved.
        type_text(&mut dialog, "sk-other-1");
        assert!(dialog.message().unwrap().contains("starts with sk-or-"));
        assert!(matches!(
            dialog.key("Return", false, &mut NoClipboard),
            Reply::Save { .. }
        ));
    }

    #[test]
    fn tab_moves_between_the_entry_and_the_buttons_and_escape_cancels_clearing() {
        let mut dialog = dialog();
        type_text(&mut dialog, KEY);
        assert_eq!(dialog.part(), "entry");
        dialog.key("Tab", false, &mut NoClipboard);
        assert_eq!(dialog.part(), "cancel");
        dialog.key("Tab", false, &mut NoClipboard);
        assert_eq!(dialog.part(), "save");
        // Typing is the entry's alone.
        assert_eq!(dialog.key("x", false, &mut NoClipboard), Reply::Stay(false));
        assert!(matches!(
            dialog.key("Space", false, &mut NoClipboard),
            Reply::Save { .. }
        ));
        dialog.key("S-Tab", false, &mut NoClipboard);
        assert_eq!(dialog.part(), "cancel");
        assert_eq!(dialog.key("Return", false, &mut NoClipboard), Reply::Closed);
        assert_eq!(dialog.length(), 0, "cancelling clears the entry");
        let mut dialog = super::tests::dialog();
        type_text(&mut dialog, KEY);
        // A repeated Escape does nothing; a press does.
        assert_eq!(
            dialog.key("Escape", true, &mut NoClipboard),
            Reply::Stay(false)
        );
        assert_eq!(dialog.key("Escape", false, &mut NoClipboard), Reply::Closed);
        assert_eq!(dialog.length(), 0);
        // Saving and then closing clears it too.
        let mut dialog = super::tests::dialog();
        type_text(&mut dialog, KEY);
        dialog.close();
        assert_eq!(dialog.length(), 0);
        assert_eq!(dialog.entry.text(), "");
    }

    #[test]
    fn the_buttons_act_on_press_and_release_and_the_entry_takes_the_caret() {
        let mut dialog = dialog();
        type_text(&mut dialog, KEY);
        let layout = dialog.layout().unwrap();
        let save = layout.buttons.button(1).unwrap().rect();
        let cancel = layout.buttons.button(0).unwrap().rect();
        // Pressed on Save, released on Cancel: nothing.
        dialog.pointer(PointerPhase::Press, save.x + 2, save.y + 2, false);
        assert_eq!(
            dialog.pointer(PointerPhase::Release, cancel.x + 2, cancel.y + 2, false),
            Reply::Stay(true)
        );
        dialog.pointer(PointerPhase::Press, save.x + 2, save.y + 2, false);
        assert!(matches!(
            dialog.pointer(PointerPhase::Release, save.x + 2, save.y + 2, false),
            Reply::Save { replace: false, .. }
        ));
        // A press on the entry puts the caret there and focuses it.
        let entry = layout.entry.rect();
        dialog.key("Tab", false, &mut NoClipboard);
        dialog.pointer(PointerPhase::Press, entry.x + 1, entry.y + 2, false);
        assert_eq!(dialog.part(), "entry");
        assert_eq!(dialog.entry.caret(), 0);
        // A press outside is the dialog's, consumed.
        assert_eq!(
            dialog.pointer(PointerPhase::Press, 1, 1, false),
            Reply::Stay(false)
        );
        dialog.pointer(PointerPhase::Press, cancel.x + 2, cancel.y + 2, false);
        assert_eq!(
            dialog.pointer(PointerPhase::Release, cancel.x + 2, cancel.y + 2, false),
            Reply::Closed
        );
        assert_eq!(dialog.length(), 0);
    }

    #[test]
    fn replacing_a_stored_key_is_confirmed_away_from_the_pointer() {
        let mut dialog = dialog();
        type_text(&mut dialog, KEY);
        dialog.ask_replace(None);
        assert_eq!(dialog.part(), "replace");
        let text = shown(&dialog);
        assert!(
            text.contains("A key is already stored; replace it?"),
            "{text}"
        );
        // Cancel is the default: Return keeps the stored key.
        assert_eq!(
            dialog.key("Return", false, &mut NoClipboard),
            Reply::Stay(true)
        );
        assert_eq!(dialog.message(), Some("the stored key is kept"));
        assert_eq!(dialog.length(), KEY.len(), "the dialog keeps its text");
        // Tab to Replace and choose it.
        dialog.ask_replace(None);
        dialog.key("Tab", false, &mut NoClipboard);
        assert_eq!(
            dialog.key("Return", false, &mut NoClipboard),
            Reply::Save {
                secret: Secret::new(KEY.into()),
                replace: true
            }
        );
        // With the pointer where a centred Replace would be, it is put
        // elsewhere.
        dialog.ask_replace(None);
        let centred = dialog
            .confirm
            .as_ref()
            .unwrap()
            .action_rect(confirmations::Focus::Confirm)
            .unwrap();
        let pointer = (centred.x + 2, centred.y + 2);
        dialog.ask_replace(Some(pointer));
        let moved = dialog
            .confirm
            .as_ref()
            .unwrap()
            .action_rect(confirmations::Focus::Confirm)
            .unwrap();
        assert!(!moved.contains(pointer.0, pointer.1));
    }

    /// A refusal as long as a save's longest is shown whole, wrapped.
    #[test]
    fn a_long_refusal_is_shown_whole() {
        let mut dialog = dialog();
        let why = format!(
            "{}/openrouter.key.tmp already exists: a save that did not finish left it; remove it and save again",
            "/home/someone/with/a/long/configuration/path/td-agent"
        );
        dialog.refused(why);
        let text = shown(&dialog);
        assert!(text.contains("/home/someone/with"), "{text}");
        assert!(text.contains("save again"), "{text}");
    }

    #[test]
    fn the_explanation_wraps_and_a_window_too_small_refuses_the_dialog() {
        assert_eq!(wrap("aa bb cc", 5, MAX_LINES), ["aa bb", "cc"]);
        assert_eq!(wrap("abcdefgh", 3, MAX_LINES), ["abc", "def", "gh"]);
        assert_eq!(wrap("aa bb cc", 2, 2), ["aa", "b\u{2026}"]);
        let many = wrap(&"word ".repeat(100), 10, MAX_LINES);
        assert_eq!(many.len(), MAX_LINES);
        assert!(many.last().unwrap().ends_with('\u{2026}'));
        let small = Surface::new(120, 400, Scale::default()).unwrap();
        assert!(KeyDialog::open(small, "/k").is_err());
        let mut dialog = dialog();
        assert!(!dialog.resize(small));
        assert!(dialog.resize(surface()));
    }
}

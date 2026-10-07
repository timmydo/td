//! The dialog File → New template… and Edit template… open (DESIGN.md §7,
//! Templates made in the window): a modal panel over the window holding a
//! title, what the dialog does, five td-ui entries (`entry_model`, painted
//! by `chrome::TextEntry`), each under its label, a line for why the
//! template is refused, and td-ui's buttons: Cancel and Save, and Remove
//! between them for a template being edited, which td-ui's confirmation
//! asks about first. The dialog is composed from those widgets as the key
//! dialog is; nothing here edits text, draws a button or a field, or
//! navigates a confirmation itself. Save checks the fields as preparing
//! the template would; the window checks the name against the other
//! templates and keeps it.

use std::sync::Arc;

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

use crate::config::Template;
use crate::keydialog::{confirm_rects, wrap};

const NEW_TITLE: &str = "New template";
const EDIT_TITLE: &str = "Edit template";
const NEW_BUTTONS: &[&str] = &["Cancel", "Save"];
const EDIT_BUTTONS: &[&str] = &["Cancel", "Remove", "Save"];
/// Each field: its label, its placeholder, and the most bytes it takes.
const FIELDS: &[(&str, &str, usize)] = &[
    ("Name", "td", crate::config::MAX_TEMPLATE_NAME),
    (
        "Remote: a URL, or a local repository's absolute path",
        "/srv/git/td",
        crate::git::MAX_TEXT,
    ),
    (
        "Base: the branch each worktree starts from",
        "main",
        crate::config::MAX_NAME,
    ),
    (
        "Branch: the branch the conversation works and pushes on",
        "agent",
        crate::git::PUSH_BRANCH,
    ),
    (
        "Sparse paths, parted by spaces; none checks out the whole tree",
        "td-agent td-ui",
        SPARSE_BYTES,
    ),
];
/// The name, remote, base, branch and sparse paths' fields, by place.
const NAME: usize = 0;
const REMOTE: usize = 1;
const BASE: usize = 2;
const BRANCH: usize = 3;
const SPARSE: usize = 4;
/// The most bytes the sparse paths' field takes: past a workspace
/// record's bound, so every template the file holds opens here.
const SPARSE_BYTES: usize = 20 * 1024;
/// The most rows the explanation takes.
const MAX_LINES: usize = 3;
/// The rows a refusal takes, wrapped.
const MESSAGE_ROWS: usize = 2;
/// The widest the dialog grows, in cells.
const MAX_COLUMNS: usize = 72;
/// The fewest text columns it lays out in.
const MIN_COLUMNS: usize = 24;
/// The confirmation's one revision: it is the dialog's alone.
const REVISION: u64 = 1;

/// The part of the dialog the keyboard is on.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Part {
    Field(usize),
    Cancel,
    Remove,
    Save,
}

impl Part {
    pub fn word(self) -> &'static str {
        match self {
            Self::Field(NAME) => "name",
            Self::Field(REMOTE) => "remote",
            Self::Field(BASE) => "base",
            Self::Field(BRANCH) => "branch",
            Self::Field(_) => "sparse",
            Self::Cancel => "cancel",
            Self::Remove => "remove",
            Self::Save => "save",
        }
    }
}

/// The confirmation's action: remove the template.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RemoveIt;

/// What the window does after an input to the dialog.
#[derive(Debug, Eq, PartialEq)]
pub enum Reply {
    /// The dialog stays; whether it must be painted again.
    Stay(bool),
    /// The dialog closed, cancelled.
    Closed,
    /// Keep `template`, in place of the one named `replacing` when it
    /// was being edited.
    Save {
        template: Template,
        replacing: Option<String>,
    },
    /// Remove the template being edited, so named.
    Remove(String),
}

/// Where each part is over the surface.
#[derive(Clone, Copy)]
struct Layout {
    rect: Rect,
    title: Rect,
    /// The explanation's first row; each line is a `ROW` under the last.
    lines: Rect,
    /// Each field's label row, then its entry.
    labels: [Rect; 5],
    entries: [TextEntry; 5],
    message: Rect,
    buttons: Buttons<'static>,
}

pub struct TemplateDialog {
    entries: Vec<EntryModel>,
    focus: Part,
    /// The field last focused, which a paste goes to.
    field: usize,
    /// A button pressed and not yet released.
    armed: Option<Part>,
    /// A press on an entry, extending its selection until released.
    dragging: Option<usize>,
    /// Why the template is refused.
    message: Option<String>,
    explanation: String,
    /// The explanation wrapped for the surface's width.
    lines: Vec<String>,
    surface: Surface,
    /// The template being edited, whose repositories past the first,
    /// which the dialog does not show, are kept as they are, as are its
    /// first's sparse paths while their field is as it opened.
    editing: Option<Template>,
    /// The sparse paths' field as it opened.
    sparse_opened: String,
    confirm: Option<Confirm<RemoveIt, u64, ()>>,
    /// The dialog asked the clipboard for its text, not yet come.
    pasting: bool,
}

impl std::fmt::Debug for TemplateDialog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TemplateDialog")
            .field("focus", &self.focus)
            .field("editing", &self.editing.as_ref().map(|t| &t.name))
            .field("confirming", &self.confirm.is_some())
            .finish_non_exhaustive()
    }
}

/// What the entry's refusal means here.
fn refusal(refusal: Refusal) -> String {
    match refusal {
        Refusal::Limit => "that is more than the field takes".into(),
        Refusal::Control => "a field is one line of text".into(),
        Refusal::Masked => refusal.to_string(),
    }
}

impl TemplateDialog {
    /// A new template's dialog over `surface`, its remote `remote` when
    /// one is given, its base `main` and its branch `agent`; refused when
    /// the surface cannot hold it.
    pub fn new_template(surface: Surface, remote: Option<&str>) -> Result<Self, String> {
        Self::open(
            surface,
            ["", remote.unwrap_or(""), "main", "agent", ""],
            None,
        )
    }

    /// `template`'s dialog, its fields its first repository's, to save in
    /// its place or remove.
    pub fn edit(surface: Surface, template: Template) -> Result<Self, String> {
        let first = template
            .repos
            .first()
            .cloned()
            .ok_or_else(|| format!("the template {:?} names no repository", template.name))?;
        let sparse = first
            .sparse
            .as_deref()
            .map(|paths| paths.join(" "))
            .unwrap_or_default();
        Self::open(
            surface,
            [
                &template.name,
                &first.remote,
                &first.base,
                &first.branch,
                &sparse,
            ],
            Some(template.clone()),
        )
    }

    fn open(surface: Surface, texts: [&str; 5], editing: Option<Template>) -> Result<Self, String> {
        let mut entries = Vec::with_capacity(FIELDS.len());
        for ((label, _, limit), text) in FIELDS.iter().zip(texts) {
            let mut entry = EntryModel::new(*limit).map_err(|e| format!("{label}: {e}"))?;
            entry
                .set_text(text)
                .map_err(|e| format!("{label}: {}", refusal(e)))?;
            entries.push(entry);
        }
        let more = editing
            .as_ref()
            .map_or(0, |template| template.repos.len().saturating_sub(1));
        let explanation = match &editing {
            None => "A repository template: each conversation made from it works on a worktree of its own of the remote, on the branch, from the base. Return saves, Tab moves, Escape cancels.".to_string(),
            Some(_) if more > 0 => format!("Save keeps the template in place of what it was; Remove removes it. Its other {more} repositories are kept as they are. Workspaces made from it are not touched."),
            Some(_) => "Save keeps the template in place of what it was; Remove removes it. Workspaces made from it are not touched.".to_string(),
        };
        let mut dialog = Self {
            sparse_opened: texts[SPARSE].to_string(),
            entries,
            focus: Part::Field(NAME),
            field: NAME,
            armed: None,
            dragging: None,
            message: None,
            explanation,
            lines: Vec::new(),
            surface,
            editing,
            confirm: None,
            pasting: false,
        };
        dialog.relayout();
        if dialog.layout().is_none() {
            return Err("the window is too small for the template dialog".into());
        }
        Ok(dialog)
    }

    /// The part the keyboard is on, or `confirm` while the confirmation
    /// asks.
    pub fn part(&self) -> &'static str {
        if self.confirm.is_some() {
            "confirm"
        } else {
            self.focus.word()
        }
    }

    /// The text of field `at`: name, remote, base, branch, sparse paths.
    pub fn text(&self, at: usize) -> &str {
        self.entries.get(at).map_or("", EntryModel::text)
    }

    pub fn message(&self) -> Option<&str> {
        self.message.as_deref()
    }

    pub fn pasting(&self) -> bool {
        self.pasting
    }

    /// The name of the template being edited.
    pub fn editing(&self) -> Option<&str> {
        self.editing.as_ref().map(|template| template.name.as_str())
    }

    fn buttons(&self) -> &'static [&'static str] {
        if self.editing.is_some() {
            EDIT_BUTTONS
        } else {
            NEW_BUTTONS
        }
    }

    /// Every part the keyboard visits, in order.
    fn order(&self) -> Vec<Part> {
        let mut order: Vec<Part> = (0..FIELDS.len()).map(Part::Field).collect();
        order.push(Part::Cancel);
        if self.editing.is_some() {
            order.push(Part::Remove);
        }
        order.push(Part::Save);
        order
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
        self.reveal();
    }

    /// Each entry's caret shown.
    fn reveal(&mut self) {
        if let Some(layout) = self.layout() {
            for (entry, field) in self.entries.iter_mut().zip(layout.entries) {
                entry.reveal(field);
            }
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
        let fields = FIELDS.len() as i64;
        let message_rows = MESSAGE_ROWS as i64;
        let height = (1 + lines + 2 * fields + message_rows + 1) * row + 2 * s;
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
        let label = |field: i64| band(1 + lines + 2 * field);
        let entry = |field: i64| {
            TextEntry::new(
                self.surface,
                Rect {
                    x: x + cell,
                    y: y + s + (2 + lines + 2 * field) * row,
                    width: (width - 2 * cell) as u32,
                    height: row as u32,
                },
            )
        };
        Some(Layout {
            rect: Rect {
                x,
                y,
                width: width as u32,
                height: height as u32,
            },
            title: band(0),
            lines: band(1),
            labels: [label(0), label(1), label(2), label(3), label(4)],
            entries: [entry(0)?, entry(1)?, entry(2)?, entry(3)?, entry(4)?],
            message: Rect {
                height: (message_rows * row) as u32,
                ..band(1 + lines + 2 * fields)
            },
            buttons: Buttons::in_band(
                self.surface,
                x + s,
                y + s + (1 + lines + 2 * fields + message_rows) * row,
                inside,
                self.buttons(),
            ),
        })
    }

    /// The surface changed: the dialog lays out again, and answers whether
    /// it still fits.
    pub fn resize(&mut self, surface: Surface) -> bool {
        self.surface = surface;
        self.armed = None;
        self.dragging = None;
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
        self.dragging = None;
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
        self.dragging = None;
        if let Some(confirm) = self.confirm.as_mut() {
            confirm.event(Some(REVISION), false, ConfirmEvent::Other);
        }
    }

    fn say(&mut self, message: impl Into<String>) -> Reply {
        self.message = Some(message.into());
        Reply::Stay(true)
    }

    /// Refused for field `at`: said, and the keyboard put on it.
    fn refuse(&mut self, at: usize, why: impl Into<String>) -> Reply {
        self.focus = Part::Field(at);
        self.field = at;
        self.say(why)
    }

    /// Save: every field checked as preparing the template would check
    /// it, the first at fault said and focused; else the template.
    fn save(&mut self) -> Reply {
        let text = |at: usize| self.text(at).trim().to_string();
        let name = match crate::config::template_name(&text(NAME)) {
            Ok(name) => name,
            Err(why) => return self.refuse(NAME, why),
        };
        let remote = text(REMOTE);
        if let Err(why) = crate::git::Remote::parse(&remote) {
            return self.refuse(REMOTE, why);
        }
        let base = text(BASE);
        if let Err(why) = crate::git::branch_name(&base) {
            return self.refuse(BASE, format!("the base: {why}"));
        }
        let branch = text(BRANCH);
        if let Err(why) = crate::git::push_branch(&branch) {
            return self.refuse(BRANCH, format!("the branch: {why}"));
        }
        // Unchanged, the paths stay as they were: one holding a space, or
        // none at all rather than the whole tree, reads back the same.
        let kept = self
            .editing
            .as_ref()
            .filter(|_| self.text(SPARSE) == self.sparse_opened)
            .and_then(|editing| editing.repos.first())
            .map(|first| first.sparse.clone());
        let sparse = kept.unwrap_or_else(|| {
            let paths: Vec<String> = text(SPARSE)
                .split_whitespace()
                .map(str::to_string)
                .collect();
            (!paths.is_empty()).then_some(paths)
        });
        // What is left to refuse is the sparse paths', but for a remote
        // too long once recorded.
        let repo = match crate::config::checked_repo(&remote, &base, &branch, sparse) {
            Ok(repo) => repo,
            Err(why) if why.contains("sparse") => return self.refuse(SPARSE, why),
            Err(why) => return self.refuse(REMOTE, why),
        };
        let mut repos = vec![repo];
        if let Some(editing) = &self.editing {
            repos.extend(editing.repos.iter().skip(1).cloned());
        }
        Reply::Save {
            template: Template {
                name,
                repos,
                shared: None,
            },
            replacing: self.editing().map(str::to_string),
        }
    }

    /// Acts on `part`, chosen by a key, or by the pointer released at
    /// `pointer`, from under which a confirmation is kept.
    fn activate(&mut self, part: Part, pointer: Option<(i64, i64)>) -> Reply {
        match part {
            Part::Cancel => Reply::Closed,
            Part::Remove => {
                self.ask_remove(pointer);
                Reply::Stay(true)
            }
            Part::Save | Part::Field(_) => self.save(),
        }
    }

    /// Moves the keyboard to `part`.
    fn focus_on(&mut self, part: Part) {
        self.focus = part;
        if let Part::Field(at) = part {
            self.field = at;
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
        let order = self.order();
        let at = order.iter().position(|p| *p == self.focus).unwrap_or(0);
        let step = |by: usize| order.get((at + by) % order.len()).copied();
        let field = match self.focus {
            Part::Field(at) => Some(at),
            _ => None,
        };
        match chord {
            "Escape" if !repeat => self.activate(Part::Cancel, None),
            "Return" if !repeat => self.activate(self.focus, None),
            "Space" | " " if !repeat && field.is_none() => self.activate(self.focus, None),
            "Tab" | "Down" => {
                if let Some(part) = step(1) {
                    self.focus_on(part);
                }
                Reply::Stay(true)
            }
            "S-Tab" | "Up" => {
                if let Some(part) = step(order.len() - 1) {
                    self.focus_on(part);
                }
                Reply::Stay(true)
            }
            "C-v" | "S-Insert" if !repeat => {
                self.focus_on(Part::Field(self.field));
                match clipboard.paste() {
                    Ok(()) => {
                        self.pasting = true;
                        Reply::Stay(true)
                    }
                    Err(why) => self.say(format!("paste: {why}")),
                }
            }
            "C-c" | "C-Insert" if !repeat => {
                let copied = field
                    .and_then(|at| self.entries.get(at))
                    .map(EntryModel::copy);
                self.copied(copied, clipboard)
            }
            "C-x" | "S-Delete" if !repeat => {
                let cut = field
                    .and_then(|at| self.entries.get_mut(at))
                    .map(EntryModel::cut);
                let reply = self.copied(cut, clipboard);
                self.reveal();
                reply
            }
            _ => match (field, Action::from_chord(chord)) {
                (Some(at), Some(action)) => {
                    let acted = self.entries.get_mut(at).map(|entry| entry.act(action));
                    match acted {
                        Some(Ok(Typed::Changed | Typed::Moved)) => {
                            self.reveal();
                            Reply::Stay(true)
                        }
                        Some(Err(why)) => self.say(refusal(why)),
                        Some(Ok(Typed::Ignored)) | None => Reply::Stay(false),
                    }
                }
                _ => Reply::Stay(false),
            },
        }
    }

    /// What a copy or cut gave, handed to the clipboard.
    fn copied(
        &mut self,
        taken: Option<Result<Option<Arc<str>>, Refusal>>,
        clipboard: &mut dyn Clipboard,
    ) -> Reply {
        match taken {
            Some(Ok(Some(text))) => match clipboard.copy(text) {
                Ok(()) => Reply::Stay(true),
                Err(why) => self.say(format!("copy: {why}")),
            },
            Some(Err(why)) => self.say(refusal(why)),
            Some(Ok(None)) | None => Reply::Stay(false),
        }
    }

    /// The clipboard's text the dialog asked for: its first line, trimmed,
    /// over the selection or at the caret of the field last focused.
    pub fn paste(&mut self, text: &str) -> Reply {
        self.pasting = false;
        self.focus_on(Part::Field(self.field));
        let line = text
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .unwrap_or("");
        let pasted = self
            .entries
            .get_mut(self.field)
            .map(|entry| entry.paste(line));
        match pasted {
            Some(Ok(_)) => {
                self.reveal();
                Reply::Stay(true)
            }
            Some(Err(why)) => self.say(refusal(why)),
            None => Reply::Stay(false),
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
        let editing = self.editing.is_some();
        let button = |x, y| match (layout.buttons.hit(x, y), editing) {
            (Some(0), _) => Some(Part::Cancel),
            (Some(1), true) => Some(Part::Remove),
            (Some(1), false) | (Some(2), true) => Some(Part::Save),
            _ => None,
        };
        match phase {
            PointerPhase::Press => {
                self.armed = None;
                let under = layout
                    .entries
                    .iter()
                    .position(|entry| entry.rect().contains(x, y));
                if let Some(at) = under {
                    self.focus_on(Part::Field(at));
                    self.dragging = Some(at);
                    if let (Some(entry), Some(field)) =
                        (self.entries.get_mut(at), layout.entries.get(at))
                    {
                        entry.place(*field, x, y, extend);
                        entry.reveal(*field);
                    }
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
                let dragged = self.dragging.and_then(|at| {
                    let field = *layout.entries.get(at)?;
                    let entry = self.entries.get_mut(at)?;
                    (entry.drag(field, x, y) != Typed::Ignored).then(|| entry.reveal(field))
                });
                Reply::Stay(dragged.is_some())
            }
            PointerPhase::Release => {
                self.dragging = None;
                match self.armed.take() {
                    Some(part) if button(x, y) == Some(part) => self.activate(part, Some((x, y))),
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
                    Choice::Confirmed(RemoveIt) => match self.editing() {
                        Some(name) => Reply::Remove(name.to_string()),
                        None => Reply::Stay(true),
                    },
                    Choice::Cancelled | Choice::Stale => self.say("the template is kept"),
                    Choice::Unavailable(e) => self.say(format!("the template is kept: {e}")),
                }
            }
        }
    }

    /// Asks whether to remove the template being edited, the
    /// confirmation's Remove kept from under `pointer`.
    fn ask_remove(&mut self, pointer: Option<(i64, i64)>) {
        let Some(name) = self.editing().map(str::to_string) else {
            return;
        };
        let asked = format!(
            "Remove the template \u{201c}{name}\u{201d}? Workspaces made from it are kept."
        );
        let mut failed = None;
        for rect in confirm_rects(self.surface) {
            let made = ConfirmModel::new(
                "Remove the template",
                "Remove",
                &[&asked],
                RemoveIt,
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
            Some(e) => format!("removing it cannot be asked here: {e}"),
            None => "move the pointer and choose Remove again".into(),
        });
    }

    /// The window refused the template, for the reason given.
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
        let title = if self.editing.is_some() {
            EDIT_TITLE
        } else {
            NEW_TITLE
        };
        line(layout.title, title, GlyphStyle::medium(INK, CHROME), sink);
        let row = (ROW * s) as i64;
        for (at, text) in self.lines.iter().enumerate() {
            let rect = Rect {
                y: layout.lines.y + at as i64 * row,
                ..layout.lines
            };
            line(rect, text, plain, sink);
        }
        let live = focused && self.confirm.is_none();
        for (at, ((label, placeholder, _), entry)) in FIELDS.iter().zip(&self.entries).enumerate() {
            if let (Some(rect), Some(field)) = (layout.labels.get(at), layout.entries.get(at)) {
                line(*rect, label, plain, sink);
                let here = self.focus == Part::Field(at);
                field.emit(
                    entry.field(placeholder, focused && here, live && here),
                    damage,
                    sink,
                );
            }
        }
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
        let parts: &[Part] = if self.editing.is_some() {
            &[Part::Cancel, Part::Remove, Part::Save]
        } else {
            &[Part::Cancel, Part::Save]
        };
        layout.buttons.emit(
            parts.iter().map(|part| (self.focus == *part, true)),
            damage,
            sink,
        );
        if let Some(confirm) = &self.confirm {
            confirm.emit(damage, sink);
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
    use super::*;
    use crate::keydialog::tests::Recorder;
    use td_ui::raster::Scale;
    use td_ui::window::NoClipboard;

    fn surface() -> Surface {
        Surface::new(1024, 768, Scale::default()).unwrap()
    }

    fn typed(dialog: &mut TemplateDialog, text: &str) {
        for c in text.chars() {
            let chord = c.to_string();
            dialog.key(&chord, false, &mut NoClipboard);
        }
    }

    fn clear(dialog: &mut TemplateDialog) {
        dialog.key("C-a", false, &mut NoClipboard);
        dialog.key("Backspace", false, &mut NoClipboard);
    }

    /// A new template's dialog starts on its name, the base `main` and
    /// the branch `agent` filled in and the remote when given; Save
    /// checks each field, says the first at fault and goes to it, and
    /// hands on the template, its remote as td-agent records it.
    #[test]
    fn a_new_template_is_checked_field_by_field() {
        let mut dialog = TemplateDialog::new_template(surface(), Some("/srv/git/td")).unwrap();
        assert_eq!(dialog.part(), "name");
        assert_eq!(
            [
                dialog.text(0),
                dialog.text(1),
                dialog.text(2),
                dialog.text(3),
                dialog.text(4)
            ],
            ["", "/srv/git/td", "main", "agent", ""]
        );
        assert_eq!(
            dialog.key("Return", false, &mut NoClipboard),
            Reply::Stay(true)
        );
        assert_eq!(dialog.part(), "name");
        assert!(
            dialog.message().unwrap().contains("name"),
            "{:?}",
            dialog.message()
        );
        typed(&mut dialog, "td");
        dialog.key("Tab", false, &mut NoClipboard);
        assert_eq!(dialog.part(), "remote");
        clear(&mut dialog);
        typed(&mut dialog, "../td");
        dialog.key("Return", false, &mut NoClipboard);
        assert_eq!(dialog.part(), "remote");
        assert!(
            dialog.message().unwrap().contains("relative"),
            "{:?}",
            dialog.message()
        );
        clear(&mut dialog);
        typed(&mut dialog, "/srv/git/td");
        dialog.key("Down", false, &mut NoClipboard);
        dialog.key("Down", false, &mut NoClipboard);
        assert_eq!(dialog.part(), "branch");
        clear(&mut dialog);
        typed(&mut dialog, "refs/heads/x");
        dialog.key("Return", false, &mut NoClipboard);
        assert_eq!(dialog.part(), "branch");
        assert!(
            dialog.message().unwrap().contains("the branch"),
            "{:?}",
            dialog.message()
        );
        clear(&mut dialog);
        typed(&mut dialog, "agent");
        dialog.key("Tab", false, &mut NoClipboard);
        typed(&mut dialog, "td-agent ../x");
        dialog.key("Return", false, &mut NoClipboard);
        assert_eq!(dialog.part(), "sparse");
        clear(&mut dialog);
        typed(&mut dialog, "td-agent  td-ui");
        assert_eq!(
            dialog.key("Return", false, &mut NoClipboard),
            Reply::Save {
                template: Template {
                    name: "td".into(),
                    repos: vec![crate::config::checked_repo(
                        "/srv/git/td",
                        "main",
                        "agent",
                        Some(vec!["td-agent".into(), "td-ui".into()])
                    )
                    .unwrap()],
                    shared: None,
                },
                replacing: None,
            }
        );
        // Escape cancels, from anywhere; Tab goes round the buttons.
        for _ in 0..2 {
            dialog.key("Tab", false, &mut NoClipboard);
        }
        assert_eq!(dialog.part(), "save");
        dialog.key("Tab", false, &mut NoClipboard);
        assert_eq!(dialog.part(), "name");
        assert_eq!(dialog.key("Escape", false, &mut NoClipboard), Reply::Closed);
    }

    /// An edited template's dialog holds its first repository; Save puts
    /// the new one in its place, its other repositories kept, and Remove
    /// asks first, Cancel keeping it.
    #[test]
    fn an_edited_template_is_saved_in_place_or_removed_once_confirmed() {
        let first = crate::config::checked_repo("/srv/git/td", "main", "agent", None).unwrap();
        let second =
            crate::config::checked_repo("https://example.org/a/b", "main", "agent", None).unwrap();
        let template = Template {
            name: "td".into(),
            repos: vec![first, second.clone()],
            shared: None,
        };
        let mut dialog = TemplateDialog::edit(surface(), template.clone()).unwrap();
        assert_eq!(dialog.editing(), Some("td"));
        assert_eq!(dialog.text(1), "file:///srv/git/td");
        dialog.key("Tab", false, &mut NoClipboard);
        dialog.key("Tab", false, &mut NoClipboard);
        clear(&mut dialog);
        typed(&mut dialog, "next");
        let Reply::Save {
            template: saved,
            replacing,
        } = dialog.key("Return", false, &mut NoClipboard)
        else {
            panic!("not saved");
        };
        assert_eq!(replacing.as_deref(), Some("td"));
        assert_eq!(saved.repos[0].base, "next");
        assert_eq!(saved.repos[1], second);
        // Remove: asked, cancelled, kept; asked, confirmed, removed.
        for _ in 0..4 {
            dialog.key("Tab", false, &mut NoClipboard);
        }
        assert_eq!(dialog.part(), "remove");
        assert_eq!(
            dialog.key("Return", false, &mut NoClipboard),
            Reply::Stay(true)
        );
        assert_eq!(dialog.part(), "confirm");
        dialog.key("Escape", false, &mut NoClipboard);
        assert_eq!(dialog.message(), Some("the template is kept"));
        dialog.key("Return", false, &mut NoClipboard);
        dialog.key("Tab", false, &mut NoClipboard);
        assert_eq!(
            dialog.key("Return", false, &mut NoClipboard),
            Reply::Remove("td".into())
        );
        // A new template's dialog has no Remove.
        let mut new = TemplateDialog::new_template(surface(), None).unwrap();
        for _ in 0..6 {
            new.key("Tab", false, &mut NoClipboard);
        }
        assert_eq!(new.part(), "save");
    }

    /// An edited template's sparse paths, unchanged in their field, are
    /// kept as they were: one holding a space, and none at all; changed,
    /// they are the field's, parted by spaces.
    #[test]
    fn unchanged_sparse_paths_are_kept_as_they_were() {
        for sparse in [Some(vec!["docs/API guide".to_string()]), Some(Vec::new())] {
            let repo = crate::config::checked_repo("/srv/td", "main", "a", sparse.clone()).unwrap();
            let template = Template {
                name: "td".into(),
                repos: vec![repo],
                shared: None,
            };
            let mut dialog = TemplateDialog::edit(surface(), template).unwrap();
            let Reply::Save { template, .. } = dialog.key("Return", false, &mut NoClipboard) else {
                panic!("not saved");
            };
            assert_eq!(template.repos[0].sparse, sparse);
            for _ in 0..4 {
                dialog.key("Tab", false, &mut NoClipboard);
            }
            typed(&mut dialog, " x");
            let Reply::Save { template, .. } = dialog.key("Return", false, &mut NoClipboard) else {
                panic!("not saved");
            };
            let changed = template.repos[0].sparse.clone().unwrap();
            assert_eq!(changed.last().map(String::as_str), Some("x"), "{changed:?}");
        }
    }

    /// The pointer presses a button where it is released on the one it
    /// pressed, and nothing where it is released elsewhere; a press on a
    /// field puts the keyboard there; Remove's confirmation opens away
    /// from the pointer.
    #[test]
    fn the_pointer_presses_buttons_and_focuses_fields() {
        let template = Template {
            name: "td".into(),
            repos: vec![crate::config::checked_repo("/srv/td", "main", "a", None).unwrap()],
            shared: None,
        };
        let mut dialog = TemplateDialog::edit(surface(), template).unwrap();
        let layout = dialog.layout().unwrap();
        let at = |rect: Rect| (rect.x + 2, rect.y + 2);
        let (cancel, remove, save) = (
            at(layout.buttons.button(0).unwrap().rect()),
            at(layout.buttons.button(1).unwrap().rect()),
            at(layout.buttons.button(2).unwrap().rect()),
        );
        let remote = at(layout.entries[REMOTE].rect());
        dialog.pointer(PointerPhase::Press, remote.0, remote.1, false);
        dialog.pointer(PointerPhase::Release, remote.0, remote.1, false);
        assert_eq!(dialog.part(), "remote");
        dialog.pointer(PointerPhase::Press, save.0, save.1, false);
        assert_eq!(
            dialog.pointer(PointerPhase::Release, cancel.0, cancel.1, false),
            Reply::Stay(true)
        );
        dialog.pointer(PointerPhase::Press, remove.0, remove.1, false);
        dialog.pointer(PointerPhase::Release, remove.0, remove.1, false);
        assert_eq!(dialog.part(), "confirm");
        let confirm = dialog.confirm.as_ref().unwrap();
        assert!(!confirm
            .action_rect(confirmations::Focus::Confirm)
            .is_some_and(|r| r.contains(remove.0, remove.1)));
        dialog.key("Escape", false, &mut NoClipboard);
        dialog.pointer(PointerPhase::Press, save.0, save.1, false);
        assert!(matches!(
            dialog.pointer(PointerPhase::Release, save.0, save.1, false),
            Reply::Save { .. }
        ));
        dialog.pointer(PointerPhase::Press, cancel.0, cancel.1, false);
        assert_eq!(
            dialog.pointer(PointerPhase::Release, cancel.0, cancel.1, false),
            Reply::Closed
        );
    }

    /// A paste goes to the field last focused, its first line trimmed; a
    /// copy hands the selection to the clipboard.
    #[test]
    fn a_paste_goes_to_the_field_last_focused() {
        let mut dialog = TemplateDialog::new_template(surface(), None).unwrap();
        dialog.key("Tab", false, &mut NoClipboard);
        let mut clipboard = Recorder::default();
        dialog.key("C-v", false, &mut clipboard);
        assert_eq!(clipboard.pastes, 1);
        assert!(dialog.pasting());
        dialog.paste("\n  /srv/git/td \nmore\n");
        assert!(!dialog.pasting());
        assert_eq!(dialog.text(1), "/srv/git/td");
        dialog.key("C-a", false, &mut NoClipboard);
        dialog.key("C-c", false, &mut clipboard);
        assert_eq!(clipboard.copies, ["/srv/git/td"]);
        // Too small a window refuses the dialog.
        let small = Surface::new(200, 100, Scale::default()).unwrap();
        assert!(TemplateDialog::new_template(small, None).is_err());
    }
}

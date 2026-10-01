//! The notebook window's state: locked or unlocked, the open entry in
//! td-ui's editor pane under the vault-document policy, the search field
//! and title list, the key prompt and the confirmation dialog. It reaches
//! the vault only through `Out` commands and `Reply` answers, so it holds
//! no key and runs in tests without a token.

mod input;
mod layout;
mod paint;

use std::sync::Arc;

use td_ui::confirmations::{self, Choice, Model};
use td_ui::editor::{Controller, Event, Outcome, PointerPhase as PanePhase};
use td_ui::editor_clipboard::{Paste, Snapshot};
use td_ui::editor_model::{SavePoint, TabId};
use td_ui::editor_search::{Found, History};
use td_ui::entry_model::{Action, EntryModel, Outcome as Typed};
use td_ui::list_model::{ListModel, Step};
use td_ui::raster::{Raster, Surface};
use td_ui::window::{Clipboard, Input, PointerPhase};

use crate::plain::{self, Bytes, Text};
use crate::protocol::{
    Answer, Ask, Change, Command, EntryId, Failure, Item, KeyLabel, Op, PinUse, Reply,
};

/// The largest title, search or find text a field holds; td-secret
/// refuses a longer title on save.
const FIELD_BYTES: usize = 512;
/// A PIN is at most 63 bytes.
const PIN_BYTES: usize = 63;
/// The caret's blink, as the editor pane's.
const BLINK_MS: u64 = 500;

/// What the window asks of the vault's thread.
#[derive(Debug)]
pub enum Out {
    Send(Command),
    Answer(Op, Answer),
    /// Abandon the operation in flight.
    Cancel,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Focus {
    Keys,
    Search,
    List,
    Title,
    Editor,
    Find,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Act {
    Save,
    Discard,
    Delete,
}

/// What a decision about unsaved changes was asked for.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Then {
    Select(usize),
    New,
    Lock,
    Quit,
}

type Dialog = confirmations::Controller<Act, u64, Focus>;

enum Phase {
    Opening,
    Refused(String),
    /// `keys` is `None` when no vault exists yet.
    Locked {
        keys: Option<Vec<KeyLabel>>,
        list: ListModel,
    },
    /// The vault is being dropped; its keys come back with the answer.
    Locking,
    Unlocked(Box<Notebook>),
}

/// The entry in the editor pane: `id` is `None` until a new entry's first
/// save, and `revision` is the entry revision a change is made against.
struct Open {
    id: Option<EntryId>,
    revision: u64,
    saved_title: Text,
    tab: Option<TabId>,
}

struct Notebook {
    entries: Vec<Item>,
    /// The entries the search shows, as indices into `entries`.
    shown: Vec<usize>,
    search: EntryModel,
    list: ListModel,
    title: EntryModel,
    find: EntryModel,
    finding: bool,
    open: Option<Open>,
    reading: Option<EntryId>,
}

enum Busy {
    Unlock(Op),
    Create(Op),
    /// `tab` is the document saved: the commit is the open entry's only
    /// while that document is still the one open.
    Save {
        op: Op,
        tab: Option<TabId>,
        point: Option<SavePoint>,
        title: Text,
        then: Option<Then>,
    },
    Delete {
        op: Op,
        id: EntryId,
        tab: Option<TabId>,
    },
}

impl Busy {
    fn op(&self) -> Op {
        match self {
            Self::Unlock(op) | Self::Create(op) => *op,
            Self::Save { op, .. } | Self::Delete { op, .. } => *op,
        }
    }
}

struct Prompting {
    op: Op,
    ask: Ask,
    pin: EntryModel,
}

/// A drag that began on a field or the pane goes on there until release.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Drag {
    Pane,
    Field(Focus),
    Pin,
}

/// Where a pasted text goes: the place that asked for it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Target {
    Pane,
    Field(Focus),
    Pin,
}

pub struct App {
    surface: Surface,
    phase: Phase,
    pane: Controller,
    history: History,
    focus: Focus,
    window_focused: bool,
    busy: Option<Busy>,
    prompt: Option<Prompting>,
    dialog: Option<(Dialog, Option<Then>)>,
    dialog_revision: u64,
    next_op: Op,
    status: String,
    drag: Option<Drag>,
    pointer: Option<(i64, i64)>,
    paste: Option<Target>,
    caret_visible: bool,
    redraw: bool,
    withdraw: bool,
    /// What a question asked during an operation was for, kept until the
    /// operation ends and the question can be asked again.
    deferred: Option<Then>,
    quit: bool,
    out: Vec<Out>,
}

impl App {
    pub fn new() -> Result<Self, String> {
        let surface = Surface::new(800, 600, td_ui::raster::Scale::default())
            .map_err(|error| error.to_string())?;
        Ok(Self {
            surface,
            phase: Phase::Opening,
            pane: Controller::pane().map_err(|error| error.to_string())?,
            history: History::default(),
            focus: Focus::Keys,
            window_focused: true,
            busy: None,
            prompt: None,
            dialog: None,
            dialog_revision: 0,
            next_op: 0,
            status: "Opening the notebook".to_owned(),
            drag: None,
            pointer: None,
            paste: None,
            caret_visible: true,
            redraw: true,
            withdraw: false,
            deferred: None,
            quit: false,
            out: vec![Out::Send(Command::Open)],
        })
    }

    pub fn take_out(&mut self) -> Vec<Out> {
        std::mem::take(&mut self.out)
    }

    pub fn needs_redraw(&self) -> bool {
        self.redraw
    }

    pub fn quitting(&self) -> bool {
        self.quit
    }

    /// Whether the clipboard text this window offered must be withdrawn;
    /// answered once per lock.
    pub fn take_withdrawal(&mut self) -> bool {
        std::mem::take(&mut self.withdraw)
    }

    pub fn paint(&mut self, raster: &mut Raster<'_, '_>, surface: Surface) -> Result<(), String> {
        if surface != self.surface {
            self.resize(surface);
        }
        self.redraw = false;
        raster
            .paint(&paint::Frame { app: self }, surface.bounds())
            .map_err(|error| error.to_string())
    }

    fn say(&mut self, status: impl Into<String>) {
        self.status = status.into();
        self.redraw = true;
    }

    fn op(&mut self) -> Op {
        self.next_op += 1;
        self.next_op
    }

    fn notebook(&mut self) -> Option<&mut Notebook> {
        match &mut self.phase {
            Phase::Unlocked(notebook) => Some(notebook),
            _ => None,
        }
    }

    // The clock.

    pub fn tick(&mut self, now: u64) {
        if let Ok(Outcome::Changed) = self.pane.dispatch(Event::Tick(now)) {
            self.redraw = true;
        }
        let visible = (now / BLINK_MS).is_multiple_of(2);
        if visible != self.caret_visible {
            self.caret_visible = visible;
            if self.focus != Focus::Editor {
                self.redraw = true;
            }
        }
    }

    // Answers from the vault's thread.

    pub fn reply(&mut self, reply: Reply) {
        self.redraw = true;
        match reply {
            Reply::Opened { keys } | Reply::Locked { keys } => self.locked(keys),
            Reply::Refused { text } => {
                self.clear();
                self.phase = Phase::Refused(text.clone());
                self.say(text);
            }
            Reply::Ask { op, ask } => {
                if self.busy.as_ref().map(Busy::op) != Some(op) {
                    self.out.push(Out::Answer(op, Answer::Decline));
                    return;
                }
                let mut pin = match EntryModel::new(PIN_BYTES) {
                    Ok(pin) => pin,
                    Err(_) => {
                        self.out.push(Out::Answer(op, Answer::Decline));
                        return;
                    }
                };
                pin.set_masked(true);
                self.say(asking(&ask));
                self.end_prompt();
                self.prompt = Some(Prompting { op, ask, pin });
                // The prompt takes every input, so no question stays over
                // it; what it was for is asked again when the operation
                // ends.
                if let Some((_, then)) = self.dialog.take() {
                    self.deferred = stronger(self.deferred, then);
                    self.sync_focus();
                }
            }
            Reply::Unlocked { op, entries } => {
                if !matches!(self.busy, Some(Busy::Unlock(o) | Busy::Create(o)) if o == op) {
                    return;
                }
                self.busy = None;
                self.end_prompt();
                let count = entries.len();
                match notebook(entries) {
                    Ok(notebook) => {
                        self.phase = Phase::Unlocked(Box::new(notebook));
                        self.focus = Focus::Search;
                        self.refilter(None);
                        self.relayout();
                        self.say(match count {
                            1 => "Unlocked: 1 entry".to_owned(),
                            count => format!("Unlocked: {count} entries"),
                        });
                    }
                    Err(text) => {
                        // The thread holds the vault it just opened.
                        self.out.push(Out::Send(Command::Lock));
                        self.clear();
                        self.say(text);
                    }
                }
            }
            Reply::Entry {
                id,
                revision,
                title,
                body,
            } => self.show(id, revision, title, body),
            Reply::Missing { id } => {
                if let Some(notebook) = self.notebook() {
                    if notebook.reading == Some(id) {
                        notebook.reading = None;
                        self.say("That entry is no longer in the notebook");
                    }
                }
            }
            Reply::Committed { op, id, revision } => {
                self.committed(op, id, revision);
                self.reask();
            }
            Reply::Failed { op, failure } => {
                self.failed(op, &failure);
                self.reask();
            }
        }
    }

    fn locked(&mut self, keys: Option<Vec<KeyLabel>>) {
        let mut list = ListModel::default();
        if let (Some(keys), Some(view)) = (&keys, layout::keys(self.surface)) {
            list.set_items(keys.len(), (!keys.is_empty()).then_some(0), view);
        }
        self.say(match &keys {
            Some(_) => "Locked: choose a key and press Unlock",
            None => "No notebook yet: Create enrolls a primary and then a backup key",
        });
        self.phase = Phase::Locked { keys, list };
        self.focus = Focus::Keys;
    }

    /// An unsaved-changes question asked while an operation was in flight
    /// is asked again once it ends, against what is now unsaved: a save
    /// that committed leaves nothing to discard.
    fn reask(&mut self) {
        if self.busy.is_some() {
            return;
        }
        let open = match &self.dialog {
            Some((_, Some(then))) => Some(*then),
            _ => None,
        };
        let Some(then) = stronger(open, self.deferred.take()) else {
            return;
        };
        if open.is_some() {
            self.dialog = None;
            self.sync_focus();
        }
        self.request(then, self.pointer);
    }

    fn failed(&mut self, op: Op, failure: &Failure) {
        if self.busy.as_ref().map(Busy::op) != Some(op) {
            return;
        }
        self.busy = None;
        self.end_prompt();
        self.say(if failure.cancelled {
            "Cancelled".to_owned()
        } else if failure.uncertain {
            format!(
                "{}: lock and unlock again to see what was saved",
                failure.text
            )
        } else {
            failure.text.clone()
        });
    }

    fn committed(&mut self, op: Op, id: EntryId, revision: Option<u64>) {
        if self.busy.as_ref().map(Busy::op) != Some(op) {
            return;
        }
        self.end_prompt();
        match self.busy.take() {
            Some(Busy::Save {
                tab,
                point,
                title,
                then,
                ..
            }) => {
                if let Some(point) = point {
                    let _ = self.pane.dispatch(Event::Saved(point));
                }
                let Some(notebook) = self.notebook() else {
                    return;
                };
                let revision = revision.unwrap_or_default();
                match notebook.entries.iter_mut().find(|item| item.id == id) {
                    Some(item) => {
                        item.revision = revision;
                        item.title = title.clone();
                    }
                    None => notebook.entries.push(Item {
                        id,
                        revision,
                        title: title.clone(),
                    }),
                }
                if let Some(open) = notebook.open.as_mut().filter(|open| open.tab == tab) {
                    open.id = Some(id);
                    open.revision = revision;
                    open.saved_title = title;
                }
                self.refilter(None);
                self.say("Saved");
                // What the save was for goes through the question again, so
                // edits made while saving are asked about; `reask` asks it,
                // keeping a Lock or Quit asked meanwhile over it.
                self.deferred = stronger(self.deferred, then);
            }
            Some(Busy::Delete { id, tab, .. }) => {
                // The deleted entry's document closes only if it is still
                // the one open.
                let open = self
                    .notebook()
                    .and_then(|notebook| notebook.open.as_ref())
                    .is_some_and(|open| open.id == Some(id) && open.tab == tab);
                if open {
                    self.close_document();
                }
                if let Some(notebook) = self.notebook() {
                    notebook.entries.retain(|item| item.id != id);
                    if open {
                        notebook.open = None;
                        notebook.title.clear();
                    }
                }
                self.refilter(None);
                self.focus = Focus::List;
                self.say("Deleted");
            }
            other => self.busy = other,
        }
    }

    /// An entry read for the pane: shown when it is still the one asked.
    fn show(&mut self, id: EntryId, revision: u64, title: Text, body: Bytes) {
        let Some(notebook) = self.notebook() else {
            return;
        };
        if notebook.reading != Some(id) {
            return;
        }
        notebook.reading = None;
        if self.dirty() {
            // Edits made while the entry was read are not given up for it.
            self.refilter(None);
            return self.say("The open entry changed meanwhile; it stays open");
        }
        self.close_document();
        let tab = self.load(body.as_slice());
        drop(body);
        let Some(notebook) = self.notebook() else {
            return;
        };
        let _ = notebook.title.set_text(title.as_str());
        notebook.open = Some(Open {
            id: Some(id),
            revision,
            saved_title: title,
            tab,
        });
        self.refilter(Some(id));
        self.relayout();
        if tab.is_none() {
            self.say("This entry's text cannot be shown here");
        } else {
            self.say("");
        }
    }

    /// A document for an entry's text, under the vault-document policy.
    fn load(&mut self, body: &[u8]) -> Option<TabId> {
        let Ok(Outcome::Created(tab)) = self.pane.dispatch(Event::Load(body)) else {
            return None;
        };
        if self
            .pane
            .dispatch(Event::Fillable {
                tab,
                enabled: false,
            })
            .is_err()
        {
            self.discard_tab(tab);
            return None;
        }
        Some(tab)
    }

    fn discard_tab(&mut self, tab: TabId) {
        if let Ok((point, bytes)) = self.pane.editor().save_snapshot(tab) {
            plain::wipe(bytes);
            let _ = self.pane.dispatch(Event::Saved(point));
        }
        if let Ok(revision) = self.pane.editor().document(tab).map(|d| d.revision()) {
            let _ = self.pane.dispatch(Event::Close { tab, revision });
        }
    }

    /// Ends the prompt, and a paste it asked for with it.
    fn end_prompt(&mut self) {
        self.prompt = None;
        if self.paste == Some(Target::Pin) {
            self.paste = None;
        }
    }

    /// Closes the open entry's document, giving up its changes; a paste
    /// asked for into it or its title no longer has a place.
    fn close_document(&mut self) {
        if matches!(self.paste, Some(Target::Pane | Target::Field(Focus::Title))) {
            self.paste = None;
        }
        let tab = self
            .notebook()
            .and_then(|notebook| notebook.open.as_mut())
            .and_then(|open| open.tab.take());
        if let Some(tab) = tab {
            self.discard_tab(tab);
        }
    }

    fn open_tab(&self) -> Option<(TabId, u64)> {
        let Phase::Unlocked(notebook) = &self.phase else {
            return None;
        };
        let tab = notebook.open.as_ref()?.tab?;
        let revision = self.pane.editor().document(tab).ok()?.revision();
        Some((tab, revision))
    }

    fn body_dirty(&self) -> bool {
        self.open_tab().is_some_and(|(tab, _)| {
            self.pane
                .editor()
                .document(tab)
                .is_ok_and(|document| document.dirty())
        })
    }

    fn title_dirty(&self) -> bool {
        let Phase::Unlocked(notebook) = &self.phase else {
            return false;
        };
        notebook
            .open
            .as_ref()
            .is_some_and(|open| open.saved_title.as_str() != notebook.title.text())
    }

    fn dirty(&self) -> bool {
        self.body_dirty() || self.title_dirty()
    }

    /// Shows the entries the search matches, keeping `keep` selected when
    /// it is among them, else the open entry.
    fn refilter(&mut self, keep: Option<EntryId>) {
        let surface = self.surface;
        let Phase::Unlocked(notebook) = &mut self.phase else {
            return;
        };
        let query = notebook.search.text();
        notebook.shown = notebook
            .entries
            .iter()
            .enumerate()
            .filter(|(_, item)| matches_folded(item.title.as_str(), query))
            .map(|(index, _)| index)
            .collect();
        let keep = keep.or_else(|| notebook.open.as_ref().and_then(|open| open.id));
        let selected = keep.and_then(|id| {
            notebook.shown.iter().position(|&index| {
                notebook
                    .entries
                    .get(index)
                    .is_some_and(|item| item.id == id)
            })
        });
        if let Some(list) = layout::panes(surface, notebook.finding).list {
            notebook
                .list
                .set_items(notebook.shown.len(), selected, list);
        }
        self.redraw = true;
    }

    /// Lays the pane and the lists out again for the surface.
    fn relayout(&mut self) {
        let surface = self.surface;
        match &mut self.phase {
            Phase::Unlocked(notebook) => {
                let panes = layout::panes(surface, notebook.finding);
                if let Some(list) = panes.list {
                    notebook.list.relayout(list);
                }
                for (model, field) in [
                    (&mut notebook.search, panes.search),
                    (&mut notebook.title, panes.title),
                    (&mut notebook.find, panes.find),
                ] {
                    if let Some(field) = field {
                        model.reveal(field);
                    }
                }
                let _ = self.pane.dispatch(Event::Frame {
                    rect: panes.pane,
                    surface,
                });
            }
            Phase::Locked { list, .. } => {
                if let Some(view) = layout::keys(surface) {
                    list.relayout(view);
                }
            }
            _ => {}
        }
        self.sync_focus();
        self.redraw = true;
    }

    fn resize(&mut self, surface: Surface) {
        self.surface = surface;
        self.relayout();
        let [rect, ..] = layout::dialog(surface);
        if let Some((dialog, _)) = &mut self.dialog {
            let outcome = dialog.event(
                Some(self.dialog_revision),
                true,
                confirmations::Event::Resize { surface, rect },
            );
            self.dialog_outcome(outcome);
        }
    }

    fn sync_focus(&mut self) {
        let focused = self.window_focused
            && self.focus == Focus::Editor
            && self.prompt.is_none()
            && self.dialog.is_none();
        let _ = self.pane.dispatch(Event::Focus(focused));
    }

    fn set_focus(&mut self, focus: Focus) {
        if self.focus != focus {
            self.focus = focus;
            self.sync_focus();
            self.redraw = true;
        }
    }

    // Operations.

    fn unlock(&mut self) {
        if self.busy.is_some() {
            return;
        }
        let Phase::Locked { keys, list } = &self.phase else {
            return;
        };
        match (keys, list.selected()) {
            (Some(_), Some(key)) => {
                let op = self.op();
                self.out.push(Out::Send(Command::Unlock { op, key }));
                self.busy = Some(Busy::Unlock(op));
                self.say("Unlocking");
            }
            (Some(_), None) => self.say("Choose a key to unlock with"),
            (None, _) => self.create(),
        }
    }

    fn create(&mut self) {
        if self.busy.is_some() || !matches!(self.phase, Phase::Locked { keys: None, .. }) {
            return;
        }
        let op = self.op();
        self.out.push(Out::Send(Command::Create { op }));
        self.busy = Some(Busy::Create(op));
        self.say("Creating the notebook");
    }

    /// Asks about unsaved changes before `then`, or does it now.
    fn request(&mut self, then: Then, opener: Option<(i64, i64)>) {
        // Only Lock and Quit may give up an entry while its save is in
        // flight; another entry waits for the save's answer.
        if self.busy.is_some() && matches!(then, Then::Select(_) | Then::New) {
            return self.say("Wait for the current operation to finish");
        }
        if !self.dirty() {
            self.run(then);
            return;
        }
        let details = ["This entry has changes that are not saved."];
        self.dialog_revision += 1;
        let revision = self.dialog_revision;
        let busy = self.busy.is_some();
        let model = move || {
            if busy {
                // A save in flight cannot be joined; only giving up remains.
                Model::new(
                    "Unsaved changes",
                    "Discard",
                    &details,
                    Act::Discard,
                    revision,
                )
            } else {
                Model::new("Unsaved changes", "Save", &details, Act::Save, revision)
                    .and_then(|model| model.with_alternate("Discard", Act::Discard))
            }
        };
        self.open_dialog(&model, Some(then), opener);
    }

    /// Opens the dialog where neither of its actions lies under the
    /// pointer that opened it.
    fn open_dialog(
        &mut self,
        model: &dyn Fn() -> Result<Model<Act, u64>, confirmations::Error>,
        then: Option<Then>,
        opener: Option<(i64, i64)>,
    ) {
        for rect in layout::dialog(self.surface) {
            let dialog =
                model().and_then(|model| Dialog::new(model, self.surface, rect, Some(self.focus)));
            let Ok(dialog) = dialog else {
                continue;
            };
            let under = opener.is_some_and(|(x, y)| {
                [
                    confirmations::Focus::Confirm,
                    confirmations::Focus::Alternate,
                ]
                .into_iter()
                .filter_map(|focus| dialog.action_rect(focus))
                .any(|action| action.contains(x, y))
            });
            if !under {
                self.dialog = Some((dialog, then));
                self.sync_focus();
                self.redraw = true;
                return;
            }
        }
        // No place fits, or every place puts an action under the pointer.
        self.say(if opener.is_some() {
            "No place for the question away from the pointer; use the keys"
        } else {
            "The window is too small for the question"
        });
    }

    fn run(&mut self, then: Then) {
        match then {
            Then::Select(index) => self.read(index),
            Then::New => self.new_entry(),
            Then::Lock => self.lock_now(),
            Then::Quit => {
                self.lock_now();
                self.quit = true;
            }
        }
    }

    /// Reads the entry at `index` in `entries` into the pane.
    fn read(&mut self, index: usize) {
        let Some(notebook) = self.notebook() else {
            return;
        };
        let Some(item) = notebook.entries.get(index) else {
            return;
        };
        let id = item.id;
        notebook.reading = Some(id);
        self.out.push(Out::Send(Command::Read { id }));
    }

    fn new_entry(&mut self) {
        if self.busy.is_some() {
            self.say("Wait for the current operation to finish");
            return;
        }
        self.close_document();
        let tab = self.load(b"");
        let surface = self.surface;
        let Some(notebook) = self.notebook() else {
            return;
        };
        notebook.title.clear();
        notebook.reading = None;
        notebook.open = Some(Open {
            id: None,
            revision: 0,
            saved_title: Text::default(),
            tab,
        });
        if let Some(list) = layout::panes(surface, notebook.finding).list {
            notebook.list.select(None, list);
        }
        self.set_focus(Focus::Title);
        self.relayout();
        self.say("New entry: give it a title, then Save");
    }

    fn save(&mut self, then: Option<Then>) {
        if self.busy.is_some() {
            self.say("Wait for the current operation to finish");
            return;
        }
        if self
            .notebook()
            .is_some_and(|notebook| notebook.reading.is_some())
        {
            return self.say("Wait for the entry to open");
        }
        let body_dirty = self.body_dirty();
        let title_dirty = self.title_dirty();
        let tab = self.open_tab();
        let Phase::Unlocked(notebook) = &self.phase else {
            return;
        };
        let Some(open) = &notebook.open else {
            self.say("No entry is open");
            return;
        };
        let title = notebook.title.text();
        if title.is_empty() {
            self.say("An entry needs a title");
            return;
        }
        let title = Text::new(title.to_owned());
        let (id, base) = (open.id, open.revision);
        let snapshot = match tab {
            Some((tab, _)) if body_dirty || id.is_none() => {
                match self.pane.editor().save_snapshot(tab) {
                    Ok((point, bytes)) => match String::from_utf8(bytes) {
                        Ok(body) => Some((point, Text::new(body))),
                        Err(error) => {
                            plain::wipe(error.into_bytes());
                            self.say("This entry's text cannot be saved");
                            return;
                        }
                    },
                    Err(error) => {
                        self.say(error.to_string());
                        return;
                    }
                }
            }
            _ => None,
        };
        let (point, change) = match (id, snapshot) {
            (None, Some((point, body))) => (
                Some(point),
                Change::Create {
                    title: title.clone(),
                    body,
                },
            ),
            (None, None) => (
                None,
                Change::Create {
                    title: title.clone(),
                    body: Text::default(),
                },
            ),
            (Some(id), Some((point, body))) => (
                Some(point),
                Change::Edit {
                    id,
                    base,
                    title: title.clone(),
                    body,
                },
            ),
            (Some(id), None) if title_dirty => (
                None,
                Change::Rename {
                    id,
                    base,
                    title: title.clone(),
                },
            ),
            (Some(_), None) => {
                self.say("Nothing to save");
                if let Some(then) = then {
                    self.run(then);
                }
                return;
            }
        };
        let op = self.op();
        self.out.push(Out::Send(Command::Apply { op, change }));
        self.busy = Some(Busy::Save {
            op,
            tab: tab.map(|(tab, _)| tab),
            point,
            title,
            then,
        });
        self.say("Saving");
    }

    fn delete(&mut self, opener: Option<(i64, i64)>) {
        if self.busy.is_some() {
            self.say("Wait for the current operation to finish");
            return;
        }
        if self
            .notebook()
            .is_some_and(|notebook| notebook.reading.is_some())
        {
            return self.say("Wait for the entry to open");
        }
        let Phase::Unlocked(notebook) = &self.phase else {
            return;
        };
        if notebook.open.as_ref().and_then(|open| open.id).is_none() {
            self.say("Open the entry to delete first");
            return;
        }
        self.dialog_revision += 1;
        let revision = self.dialog_revision;
        let model = move || {
            Model::new(
                "Delete this entry?",
                "Delete",
                &["The entry and its text are removed from the notebook."],
                Act::Delete,
                revision,
            )
        };
        self.open_dialog(&model, None, opener);
    }

    fn delete_now(&mut self) {
        let Phase::Unlocked(notebook) = &self.phase else {
            return;
        };
        let Some((id, base)) = notebook
            .open
            .as_ref()
            .and_then(|open| Some((open.id?, open.revision)))
        else {
            return;
        };
        let op = self.op();
        self.out.push(Out::Send(Command::Apply {
            op,
            change: Change::Delete { id, base },
        }));
        let tab = self.open_tab().map(|(tab, _)| tab);
        self.busy = Some(Busy::Delete { op, id, tab });
        self.say("Deleting");
    }

    /// Gives up the open entry's unsaved changes: its document closes and
    /// the entry is no longer open, so no edit stays on screen as if it
    /// were saved; what follows opens whatever comes next.
    fn discard(&mut self) {
        self.close_document();
        if let Some(notebook) = self.notebook() {
            notebook.open = None;
            notebook.title.clear();
        }
        self.refilter(None);
    }

    /// Locks now: the operation in flight is abandoned, the vault dropped,
    /// and every title, body, query and clipboard offer this window holds
    /// is forgotten.
    fn lock_now(&mut self) {
        if self.busy.is_some() {
            self.out.push(Out::Cancel);
        }
        if matches!(self.phase, Phase::Unlocked(_)) {
            self.out.push(Out::Send(Command::Lock));
            self.clear();
            self.phase = Phase::Locking;
            self.say("Locking");
        } else {
            self.busy = None;
            self.end_prompt();
        }
    }

    fn clear(&mut self) {
        let _ = self.pane.dispatch(Event::Clear);
        self.history = History::default();
        self.busy = None;
        self.prompt = None;
        self.dialog = None;
        self.paste = None;
        self.drag = None;
        self.deferred = None;
        self.withdraw = true;
        self.phase = Phase::Locking;
        self.focus = Focus::Keys;
        self.redraw = true;
    }
}

fn notebook(entries: Vec<Item>) -> Result<Notebook, String> {
    let field =
        || EntryModel::new(FIELD_BYTES).map_err(|_| "td-pass cannot hold a text field".to_owned());
    Ok(Notebook {
        shown: (0..entries.len()).collect(),
        entries,
        search: field()?,
        list: ListModel::with_margin(1),
        title: field()?,
        find: field()?,
        finding: false,
        open: None,
        reading: None,
    })
}

/// Of two pending follow-ons, the one that must not be lost: Quit over
/// Lock over the rest, and the earlier of two alike.
fn stronger(first: Option<Then>, second: Option<Then>) -> Option<Then> {
    let rank = |then: &Option<Then>| match then {
        Some(Then::Quit) => 3,
        Some(Then::Lock) => 2,
        Some(_) => 1,
        None => 0,
    };
    if rank(&second) > rank(&first) {
        second
    } else {
        first
    }
}

/// Whether `needle` occurs in `title`, comparing lowercase forms, without
/// making a copy of either.
fn matches_folded(title: &str, needle: &str) -> bool {
    fn fold(text: &str) -> impl Iterator<Item = char> + '_ {
        text.chars().flat_map(char::to_lowercase)
    }
    let mut starts = title.char_indices().map(|(at, _)| at);
    std::iter::from_fn(|| starts.next())
        .chain(std::iter::once(title.len()))
        .any(|at| {
            let mut hay = fold(title.get(at..).unwrap_or_default());
            fold(needle).all(|c| hay.next() == Some(c))
        })
}

/// The status line while a prompt waits.
fn asking(ask: &Ask) -> String {
    let key = match &ask.key {
        Some(fingerprint) => format!("the {} key {fingerprint}", ask.role.name()),
        None => format!("the key to enroll as {}", ask.role.name()),
    };
    match ask.pin {
        None => format!("{}: connect {key}", ask.operation),
        Some(PinUse::Authorize) => format!("{}: the PIN of {key}", ask.operation),
        Some(PinUse::Enroll) => format!("{}: the PIN of {key}", ask.operation),
        Some(PinUse::Proof) => format!("{}: the PIN of {key} again, to prove it", ask.operation),
    }
}

#[cfg(test)]
mod tests;

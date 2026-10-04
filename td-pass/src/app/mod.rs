//! The notebook window's state: locked or unlocked, the open entry in
//! td-ui's editor pane under the vault-document policy, the search field
//! and title list, the keys view, the finder for an encrypted copy, the
//! key prompt and the confirmation dialog. It reaches the vault only
//! through `Out` commands and `Reply` answers, and folders only through
//! `Out::List`, so it holds no key and runs in tests without a token.

mod input;
mod layout;
mod paint;

use std::path::PathBuf;
use std::sync::Arc;

use td_ui::confirmations::{self, Choice, Model};
use td_ui::editor::{Controller, Event, Outcome, PointerPhase as PanePhase};
use td_ui::editor_clipboard::{Paste, Snapshot};
use td_ui::editor_model::{SavePoint, TabId};
use td_ui::editor_search::{Found, History};
use td_ui::entry_model::{Action, EntryModel, Outcome as Typed};
use td_ui::finder;
use td_ui::list_model::{ListModel, Step};
use td_ui::raster::{Raster, Surface};
use td_ui::window::{Clipboard, Input, PointerPhase};

use crate::plain::{self, Bytes, Text};
use crate::protocol::{
    Answer, Ask, Change, Command, EntryId, Failure, HostEvent, Item, KeyLabel, Keys, Op, PinUse,
    Reply, Role,
};

/// The largest title, search or find text a field holds; td-secret
/// refuses a longer title on save.
const FIELD_BYTES: usize = 512;
/// The status row while the keys view is up and idle.
const KEYS_HINT: &str =
    "Keys: Space or Shift+click marks keys; Insert adds a backup; Delete replaces";
/// What the create question says, the risk of one key before the remedy
/// of two.
const CREATE_DETAILS: &[&str] = &[
    "Only an enrolled security key and its PIN open the notebook. \
     There is no password and no reset.",
    "With one key, losing that key or blocking its PIN loses the \
     notebook and everything in it for good.",
    "With a primary and a backup, either key opens it alone, so \
     losing one key, or blocking its PIN, loses nothing.",
    "Keys can be added later from Keys (Ctrl+K).",
];
/// The status row while the keys view of a notebook with one key is up.
const ONE_KEY_HINT: &str =
    "Keys: this key alone opens the notebook; Insert adds a backup so losing it loses nothing";
/// Appended when a notebook with one key opens.
const ONE_KEY_NOTE: &str = ". One key opens it: Ctrl+K, then Insert, adds a backup";
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
    /// List `folder`, or the starting folder, for the finder `chooser`,
    /// with files that may be chosen when `files`; answered with
    /// `App::listed`.
    List {
        chooser: u64,
        folder: Option<PathBuf>,
        select: Option<String>,
        files: bool,
    },
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
    Replace,
    AcceptSwap,
    CreateWithBackup,
    CreateOneKey,
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
    /// Swap on these devices can put memory on storage, and the person
    /// has not accepted it: nothing is open.
    Swap(Vec<String>),
    Refused(String),
    /// `keys` is `None` when no vault exists yet.
    Locked {
        keys: Option<Vec<KeyLabel>>,
        list: ListModel,
    },
    /// The vault is being dropped; its keys come back with the answer.
    Locking,
    /// An encrypted copy is read: the keys it opens with, one to import
    /// it with.
    Importing {
        keys: Vec<KeyLabel>,
        list: ListModel,
    },
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

/// The unlocked notebook's keys, shown in place of the panes: `marked`
/// are the keys a replacement revokes.
struct KeyView {
    keys: Keys,
    list: ListModel,
    marked: Vec<bool>,
    showing: bool,
    /// The focus to return to when the view is put away.
    before: Focus,
}

struct Notebook {
    keys: KeyView,
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

/// Why the host locked the notebook.
#[derive(Clone, Copy, Debug)]
enum HostCause {
    Screen,
    Sleep,
    Lost,
}

impl HostCause {
    fn locking(self) -> &'static str {
        match self {
            Self::Screen => "Locking with the screen",
            Self::Sleep => "Locking for sleep",
            Self::Lost => "Locking: the host's events stopped",
        }
    }

    fn locked(self) -> &'static str {
        match self {
            Self::Screen => "Locked with the screen",
            Self::Sleep => "Locked for sleep",
            Self::Lost => "Locked as the host's events stopped",
        }
    }
}

/// What a host lock interrupted.
#[derive(Clone, Copy, Debug, Default)]
struct Interrupted {
    /// The open entry had unsaved edits.
    edits: bool,
    /// A change was being published.
    write: bool,
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
        /// The body's revision the save sent, so an edit made since can
        /// be told from the edits it carries.
        revision: Option<u64>,
        title: Text,
        then: Option<Then>,
    },
    Delete {
        op: Op,
        id: EntryId,
        tab: Option<TabId>,
    },
    Keys {
        op: Op,
        what: KeyOp,
    },
    Export(Op),
    ReadCopy(Op),
    Import(Op),
}

/// What the finder chooses for.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Purpose {
    /// A folder to write the encrypted copy into.
    Export,
    /// A copy to import.
    Import,
}

/// The finder over the folder `folder`; `finder` is `None` until its
/// first listing comes. `id` tells its listings from an earlier finder's.
struct Chooser {
    id: u64,
    finder: Option<finder::Controller>,
    folder: PathBuf,
    purpose: Purpose,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum KeyOp {
    Add,
    Replace,
}

impl Busy {
    fn op(&self) -> Op {
        match self {
            Self::Unlock(op)
            | Self::Create(op)
            | Self::Export(op)
            | Self::ReadCopy(op)
            | Self::Import(op) => *op,
            Self::Save { op, .. } | Self::Delete { op, .. } | Self::Keys { op, .. } => *op,
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
    chooser: Option<Chooser>,
    next_chooser: u64,
    /// What an export that finished during a lock wrote, shown with the
    /// locked view that follows.
    exported: Option<String>,
    /// Why the host locked the notebook, shown with the locked view
    /// until the next unlock.
    host_locked: Option<HostCause>,
    /// The host's lock is watched, or its failure has been told.
    host_ready: bool,
    /// What a host lock interrupted, reported at the next unlock.
    interrupted: Interrupted,
    /// Why the host's lock and sleep are not, or not fully, watched.
    host_warning: Option<String>,
    /// The host's lock is known not to be watched.
    unwatched: bool,
    next_op: Op,
    status: String,
    drag: Option<Drag>,
    pointer: Option<(i64, i64)>,
    paste: Option<Target>,
    caret_visible: bool,
    redraw: bool,
    withdraw: bool,
    /// The window's kept frames are to be cleared, asked once per lock.
    scrub: bool,
    /// What a question asked during an operation was for, kept until the
    /// operation ends and the question can be asked again.
    deferred: Option<Then>,
    quit: bool,
    /// The strip's Help was pressed: the window shows td-ui's key list,
    /// asked once. Not the notebook's encryption keys view.
    key_list_asked: bool,
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
            chooser: None,
            next_chooser: 0,
            exported: None,
            host_locked: None,
            host_ready: false,
            interrupted: Interrupted::default(),
            host_warning: None,
            unwatched: false,
            next_op: 0,
            status: "Opening the notebook".to_owned(),
            drag: None,
            pointer: None,
            paste: None,
            caret_visible: true,
            redraw: true,
            withdraw: false,
            scrub: false,
            deferred: None,
            quit: false,
            key_list_asked: false,
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

    /// The labels of the strip this phase shows.
    fn strip(&self) -> &'static [&'static str] {
        match &self.phase {
            Phase::Unlocked(notebook) if notebook.keys.showing => layout::KEYS,
            Phase::Unlocked(_) => layout::NOTEBOOK,
            Phase::Importing { .. } => layout::IMPORT,
            Phase::Locked { .. }
            | Phase::Opening
            | Phase::Swap(_)
            | Phase::Locking
            | Phase::Refused(_) => layout::LOCKED,
        }
    }

    /// Whether the strip's Help asked for td-ui's key list since this was
    /// last asked; answered once per press.
    pub fn take_key_list_asked(&mut self) -> bool {
        std::mem::take(&mut self.key_list_asked)
    }

    /// Whether the clipboard text this window offered must be withdrawn;
    /// answered once per lock.
    pub fn take_withdrawal(&mut self) -> bool {
        std::mem::take(&mut self.withdraw)
    }

    /// Whether the frames the window keeps must be cleared; answered once
    /// per lock.
    pub fn take_scrub(&mut self) -> bool {
        std::mem::take(&mut self.scrub)
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
            Reply::Swap { devices } => {
                self.phase = Phase::Swap(devices);
                // Unasked for, so not where the pointer might click.
                self.ask_swap(self.pointer);
            }
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
            Reply::Unlocked { op, entries, keys } => {
                if !matches!(
                    self.busy,
                    Some(Busy::Unlock(o) | Busy::Create(o) | Busy::Import(o)) if o == op
                ) {
                    return;
                }
                self.busy = None;
                self.end_prompt();
                self.host_locked = None;
                let count = entries.len();
                match notebook(entries) {
                    Ok(notebook) => {
                        self.phase = Phase::Unlocked(Box::new(notebook));
                        self.focus = Focus::Search;
                        self.set_keys(keys);
                        self.refilter(None);
                        self.relayout();
                        let mut status = match count {
                            1 => "Unlocked: 1 entry".to_owned(),
                            count => format!("Unlocked: {count} entries"),
                        };
                        let interrupted = std::mem::take(&mut self.interrupted);
                        status.push_str(match (interrupted.edits, interrupted.write) {
                            (true, true) => {
                                ". The host's lock gave up edits and may have stopped a change"
                            }
                            (true, false) => ". The host's lock gave up unsaved edits",
                            (false, true) => ". The host's lock may have stopped a change",
                            (false, false) => "",
                        });
                        // A host lock's note takes the row before this one.
                        if self.one_key() && !interrupted.edits && !interrupted.write {
                            status.push_str(ONE_KEY_NOTE);
                        }
                        self.say(status);
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
            Reply::Keys { op, keys } => {
                self.keys_changed(op, keys);
                self.reask();
            }
            Reply::Exported { op, path } => {
                // An export cannot be called back: one that finished after
                // a lock is still reported, since its file is there.
                if matches!(self.busy, Some(Busy::Export(o)) if o == op) {
                    self.busy = None;
                }
                let note = format!("Exported an encrypted copy to {path}");
                if matches!(self.phase, Phase::Locking) {
                    self.exported = Some(note);
                } else {
                    self.say(note);
                }
                self.reask();
            }
            Reply::Copy { op, keys } => {
                if !matches!(self.busy, Some(Busy::ReadCopy(o)) if o == op) {
                    return;
                }
                self.busy = None;
                let mut list = ListModel::default();
                if let Some(view) = layout::copy_keys(self.surface) {
                    list.set_items(keys.len(), (!keys.is_empty()).then_some(0), view);
                }
                self.phase = Phase::Importing { keys, list };
                self.focus = Focus::Keys;
                self.say("Choose the key to import the copy with, then Import");
            }
        }
    }

    /// A key operation committed: the list shows the keys now.
    fn keys_changed(&mut self, op: Op, keys: Keys) {
        let what = match &self.busy {
            Some(Busy::Keys { op: o, what }) if *o == op => *what,
            _ => return,
        };
        self.busy = None;
        self.end_prompt();
        let added = self.notebook().and_then(|notebook| {
            keys.labels
                .iter()
                .find(|label| !notebook.keys.keys.labels.contains(label))
                .cloned()
        });
        self.set_keys(keys);
        self.say(match (what, added) {
            (KeyOp::Add, Some(key)) => {
                format!("Added the {} key {}", key.role.name(), key.fingerprint)
            }
            (KeyOp::Replace, Some(key)) => format!(
                "Replaced: the new {} key is {}; revoked keys no longer open the notebook",
                key.role.name(),
                key.fingerprint
            ),
            _ => "The keys changed".to_owned(),
        });
    }

    /// Shows `keys` in the keys view, selecting the one that authorizes
    /// adding a key.
    fn set_keys(&mut self, keys: Keys) {
        let surface = self.surface;
        let Some(notebook) = self.notebook() else {
            return;
        };
        let view = &mut notebook.keys;
        view.marked = vec![false; keys.labels.len()];
        if let Some(list) = layout::enrolled(surface) {
            view.list.set_items(keys.labels.len(), keys.using, list);
        }
        view.keys = keys;
        self.redraw = true;
    }

    fn locked(&mut self, keys: Option<Vec<KeyLabel>>) {
        let mut list = ListModel::default();
        if let (Some(keys), Some(view)) = (&keys, layout::keys(self.surface)) {
            list.set_items(keys.len(), (!keys.is_empty()).then_some(0), view);
        }
        self.phase = Phase::Locked { keys, list };
        self.focus = Focus::Keys;
        let note = self.exported.take();
        self.say_locked(note);
    }

    /// The locked view's status: what it follows, why it locked, what to
    /// do, and any warning about the host, short enough for the row.
    fn say_locked(&mut self, note: Option<String>) {
        let Phase::Locked { keys, .. } = &self.phase else {
            return;
        };
        let status = match keys {
            Some(_) => format!(
                "{}: choose a key and press Unlock",
                self.host_locked.map_or("Locked", HostCause::locked)
            ),
            None => "No notebook yet: Create enrolls a key, and a backup if you choose".to_owned(),
        };
        let parts: Vec<String> = note
            .into_iter()
            .chain([status])
            .chain(self.host_warning.clone())
            .collect();
        self.say(parts.join(". "));
    }

    /// What the host did. A lock or sleep locks at once, asking nothing
    /// and giving up unsaved edits, which the next unlock reports; a host
    /// whose lock cannot be watched is warned of. Unlocking waits until
    /// the watch has started or its failure has been told.
    pub fn host(&mut self, event: HostEvent) {
        self.redraw = true;
        let cause = match event {
            HostEvent::Watched => {
                self.host_ready = true;
                return;
            }
            HostEvent::Lock => HostCause::Screen,
            HostEvent::Suspend => HostCause::Sleep,
            HostEvent::Lost(reason) => {
                self.host_ready = true;
                self.unwatched = true;
                self.warn(
                    format!("Screen lock no longer watched: {reason}"),
                    format!(
                        "The screen lock is no longer watched ({reason}): lock the notebook \
                         before leaving it"
                    ),
                );
                HostCause::Lost
            }
            HostEvent::Unwatched(reason) => {
                self.host_ready = true;
                self.unwatched = true;
                return self.warn(
                    format!("Screen lock not watched: {reason}"),
                    format!(
                        "The screen lock is not watched ({reason}): lock the notebook before \
                         leaving it"
                    ),
                );
            }
            HostEvent::Undelayed => {
                self.host_ready = true;
                if !self.unwatched {
                    self.warn(
                        "Sleep may come before the lock".to_owned(),
                        "Sleep does not wait for the notebook to lock, so it may lock only on \
                         waking"
                            .to_owned(),
                    );
                }
                return;
            }
        };
        self.host_lock(cause);
    }

    /// Keeps `short` for the locked status and says `long` where the
    /// notebook is open.
    fn warn(&mut self, short: String, long: String) {
        self.host_warning = Some(short);
        match self.phase {
            // A prompt's instruction stays; the warning shows next time.
            Phase::Locked { .. } if self.busy.is_none() => self.say_locked(None),
            Phase::Unlocked(_) => self.say(long),
            _ => {}
        }
    }

    /// Whether unlocking must wait for the host's lock to be watched.
    fn awaiting_host(&mut self) -> bool {
        if !self.host_ready {
            self.say("Starting to watch the host's screen lock: try again in a moment");
        }
        !self.host_ready
    }

    /// Locks for the host: the operation in flight is abandoned, and the
    /// vault's thread is told to lock even when nothing shows unlocked,
    /// so an unlock that finished as it was cancelled is dropped too.
    fn host_lock(&mut self, cause: HostCause) {
        match self.phase {
            // Nothing is held, or nothing is under way.
            Phase::Opening | Phase::Swap(_) | Phase::Refused(_) => return,
            Phase::Locked { .. } if self.busy.is_none() => return,
            // The thread already has its lock; another would answer late,
            // over whatever the locked view is then doing.
            Phase::Locking => {
                self.host_locked = Some(cause);
                return;
            }
            _ => {}
        }
        self.interrupted.edits |= self.edited_unsaved();
        self.interrupted.write |= matches!(
            self.busy,
            Some(
                Busy::Save { .. }
                    | Busy::Delete { .. }
                    | Busy::Keys { .. }
                    | Busy::Create(_)
                    | Busy::Import(_)
            )
        );
        if self.busy.is_some() {
            self.out.push(Out::Cancel);
        }
        self.out.push(Out::Send(Command::Lock));
        self.clear();
        self.host_locked = Some(cause);
        self.say(cause.locking());
    }

    /// Whether a lock now gives up edits: during a save, only those made
    /// since it was sent, since the save itself may yet commit.
    fn edited_unsaved(&self) -> bool {
        let Some(Busy::Save {
            tab,
            revision,
            title,
            ..
        }) = &self.busy
        else {
            return self.dirty();
        };
        let body = tab.is_some_and(|tab| {
            self.pane
                .editor()
                .document(tab)
                .is_ok_and(|document| Some(document.revision()) != *revision)
        });
        let renamed = match &self.phase {
            Phase::Unlocked(notebook) => notebook.title.text() != title.as_str(),
            _ => false,
        };
        body || renamed
    }

    /// Nothing is held or under way: the vault's thread has answered the
    /// last lock, or none was asked for.
    pub fn settled(&self) -> bool {
        self.busy.is_none()
            && matches!(
                self.phase,
                Phase::Opening | Phase::Swap(_) | Phase::Refused(_) | Phase::Locked { .. }
            )
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
        let keys = matches!(self.busy, Some(Busy::Keys { .. }));
        self.busy = None;
        self.end_prompt();
        self.say(if failure.cancelled {
            "Cancelled".to_owned()
        } else if failure.uncertain {
            format!(
                "{}: lock and unlock again to see what {}",
                failure.text,
                if keys {
                    "keys the vault holds"
                } else {
                    "was saved"
                }
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
                self.set_focus(Focus::List);
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
                if let Some(list) = layout::enrolled(surface) {
                    // A list laid out while the window had no room for it
                    // takes its count now.
                    let count = notebook.keys.keys.labels.len();
                    if notebook.keys.list.count() == count {
                        notebook.keys.list.relayout(list);
                    } else {
                        notebook
                            .keys
                            .list
                            .set_items(count, notebook.keys.keys.using, list);
                    }
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
            Phase::Importing { keys, list } => {
                if let Some(view) = layout::copy_keys(surface) {
                    if list.count() == keys.len() {
                        list.relayout(view);
                    } else {
                        list.set_items(keys.len(), (!keys.is_empty()).then_some(0), view);
                    }
                }
            }
            Phase::Locked { keys, list } => {
                if let Some(view) = layout::keys(surface) {
                    let count = keys.as_ref().map_or(0, Vec::len);
                    if list.count() == count {
                        list.relayout(view);
                    } else {
                        list.set_items(count, (count > 0).then_some(0), view);
                    }
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
        // A finder the window can no longer hold closes.
        if let Some(chooser) = self.chooser.as_mut() {
            let rect = layout::finder(surface, chooser.purpose == Purpose::Import);
            if let Some(finder) = chooser.finder.as_mut() {
                if let finder::Outcome::Closed(_) =
                    finder.event(finder::Event::Resize { surface, rect })
                {
                    self.close_chooser("The window is too small for the finder");
                }
            }
        }
        let [rect, ..] = layout::dialog(surface, self.dialog_rows());
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
            && self.dialog.is_none()
            && self.chooser.is_none();
        let _ = self.pane.dispatch(Event::Focus(focused));
    }

    fn set_focus(&mut self, focus: Focus) {
        // What finishes under the keys view moves the focus it returns to.
        if focus != Focus::Keys {
            if let Some(notebook) = self.notebook().filter(|notebook| notebook.keys.showing) {
                notebook.keys.before = focus;
                return;
            }
        }
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
        match (keys.is_some(), list.selected()) {
            (true, Some(_)) if !self.host_ready => {
                self.awaiting_host();
            }
            (true, Some(key)) => {
                let op = self.op();
                self.out.push(Out::Send(Command::Unlock { op, key }));
                self.busy = Some(Busy::Unlock(op));
                self.say("Unlocking");
            }
            (true, None) => self.say("Choose a key to unlock with"),
            (false, _) => self.create(None),
        }
    }

    /// Asks which keys the notebook is created with: a primary and a
    /// backup, or, as the person's explicit decision, the primary alone.
    fn create(&mut self, opener: Option<(i64, i64)>) {
        if self.busy.is_some()
            || !matches!(self.phase, Phase::Locked { keys: None, .. })
            || self.awaiting_host()
        {
            return;
        }
        self.dialog_revision += 1;
        let revision = self.dialog_revision;
        let model = move || {
            Model::new(
                "Create a notebook with which keys?",
                "Primary and backup",
                CREATE_DETAILS,
                Act::CreateWithBackup,
                revision,
            )
            .and_then(|model| model.with_alternate("One key only", Act::CreateOneKey))
        };
        if self.open_dialog(&model, None, opener) {
            self.say("Create with a primary and a backup key, or with one key only");
        }
    }

    fn create_now(&mut self, backup: bool) {
        if self.busy.is_some() || !matches!(self.phase, Phase::Locked { keys: None, .. }) {
            return;
        }
        let op = self.op();
        self.out.push(Out::Send(Command::Create { op, backup }));
        self.busy = Some(Busy::Create(op));
        self.say(if backup {
            "Creating the notebook with a primary and a backup key"
        } else {
            "Creating the notebook with one key"
        });
    }

    /// Whether the open notebook has a single key.
    fn one_key(&self) -> bool {
        matches!(&self.phase, Phase::Unlocked(notebook) if notebook.keys.keys.labels.len() == 1)
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

    /// The rows of the question that is or would be open: in the swap
    /// phase only the swap question can be, and with no notebook only the
    /// create question.
    fn dialog_rows(&self) -> i64 {
        match self.phase {
            Phase::Swap(_) => layout::SWAP_ROWS,
            Phase::Locked { keys: None, .. } => layout::CREATE_ROWS,
            _ => layout::DIALOG_ROWS,
        }
    }

    /// Opens the dialog where neither of its actions lies under the
    /// pointer that opened it.
    fn open_dialog(
        &mut self,
        model: &dyn Fn() -> Result<Model<Act, u64>, confirmations::Error>,
        then: Option<Then>,
        opener: Option<(i64, i64)>,
    ) -> bool {
        for rect in layout::dialog(self.surface, self.dialog_rows()) {
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
                return true;
            }
        }
        // No place fits, or every place puts an action under the pointer.
        self.say(if opener.is_some() {
            "No place for the question away from the pointer; use the keys"
        } else {
            "The window is too small for the question"
        });
        false
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
        let tab = tab.map(|(tab, _)| tab);
        let revision = tab.and_then(|tab| {
            self.pane
                .editor()
                .document(tab)
                .ok()
                .map(|document| document.revision())
        });
        self.busy = Some(Busy::Save {
            op,
            tab,
            point,
            revision,
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

    /// Asks whether to open over swap on storage: what it risks first,
    /// then each device as its own detail, escaped and bounded, so any
    /// table the kernel lists can be asked about. `opener`, the pointer
    /// when the question comes unasked, keeps Open anyway from under it.
    fn ask_swap(&mut self, opener: Option<(i64, i64)>) {
        let Phase::Swap(devices) = &self.phase else {
            return;
        };
        let mut details = Vec::with_capacity(devices.len().min(SWAP_DEVICES) + 5);
        details.push(SWAP_SUMMARY.to_owned());
        details.extend(
            devices
                .iter()
                .take(SWAP_DEVICES)
                .map(|device| shown(device)),
        );
        if let Some(more) = devices.len().checked_sub(SWAP_DEVICES).filter(|n| *n > 0) {
            details.push(format!("and {more} more"));
        }
        details.extend([SWAP_SCOPE, SWAP_RISK, SWAP_REMEDY].map(str::to_owned));
        self.dialog_revision += 1;
        let revision = self.dialog_revision;
        let model = move || {
            let details: Vec<&str> = details.iter().map(String::as_str).collect();
            Model::new(
                "Swap on storage",
                "Open anyway",
                &details,
                Act::AcceptSwap,
                revision,
            )
        };
        if self.open_dialog(&model, None, opener) {
            self.say("Swap on storage is active: open anyway, or cancel");
        }
    }

    /// Declining keeps nothing open; Return asks again.
    fn swap_declined(&mut self) {
        self.say("Swap on storage is active, so the vault stays closed: Return asks again");
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

    // The keys view.

    /// Puts the keys view up or away. While it is up the panes take no
    /// input: a paste asked for them is dropped and a drag ends.
    fn show_keys(&mut self, showing: bool) {
        let focus = self.focus;
        let Some(notebook) = self.notebook() else {
            return;
        };
        if notebook.keys.showing == showing {
            return;
        }
        notebook.keys.showing = showing;
        let next = if showing {
            notebook.keys.before = focus;
            Focus::Keys
        } else {
            notebook.keys.marked.fill(false);
            notebook.keys.before
        };
        if showing {
            if matches!(self.paste, Some(Target::Pane | Target::Field(_))) {
                self.paste = None;
            }
            if self.drag.take().is_some() {
                let _ = self.pane.dispatch(Event::CancelPointer);
            }
        }
        self.set_focus(next);
        // The status row keeps what an operation reported.
        if showing && self.busy.is_none() {
            self.say(if self.one_key() {
                ONE_KEY_HINT
            } else {
                KEYS_HINT
            });
        } else if !showing && (self.status == KEYS_HINT || self.status == ONE_KEY_HINT) {
            self.say("");
        }
        self.redraw = true;
    }

    /// The selected key, when the keys view is up.
    fn selected_key(&self) -> Option<usize> {
        match &self.phase {
            Phase::Unlocked(notebook) if notebook.keys.showing => notebook
                .keys
                .list
                .selected()
                .filter(|&index| index < notebook.keys.keys.labels.len()),
            _ => None,
        }
    }

    fn toggle_mark(&mut self) {
        let Some(index) = self.selected_key() else {
            return;
        };
        if let Some(mark) = self
            .notebook()
            .and_then(|notebook| notebook.keys.marked.get_mut(index))
        {
            *mark = !*mark;
            self.redraw = true;
        }
    }

    fn add_key(&mut self) {
        if self.busy.is_some() {
            return self.say("Wait for the current operation to finish");
        }
        if !self
            .notebook()
            .is_some_and(|notebook| notebook.keys.showing)
        {
            return;
        }
        let op = self.op();
        self.out.push(Out::Send(Command::AddKey { op }));
        self.busy = Some(Busy::Keys {
            op,
            what: KeyOp::Add,
        });
        self.say("Adding a backup key");
    }

    /// The keys a replacement revokes: the marked ones, else the selected.
    fn revoking(&self) -> Vec<usize> {
        let Phase::Unlocked(notebook) = &self.phase else {
            return Vec::new();
        };
        let marked: Vec<usize> = notebook
            .keys
            .marked
            .iter()
            .enumerate()
            .filter(|(_, marked)| **marked)
            .map(|(index, _)| index)
            .filter(|&index| index < notebook.keys.keys.labels.len())
            .collect();
        if marked.is_empty() {
            self.selected_key().into_iter().collect()
        } else {
            marked
        }
    }

    /// Asks before revoking: a revoked key no longer opens the notebook.
    fn replace_keys(&mut self, opener: Option<(i64, i64)>) {
        if self.busy.is_some() {
            return self.say("Wait for the current operation to finish");
        }
        let revoked = self.revoking();
        let Phase::Unlocked(notebook) = &self.phase else {
            return;
        };
        let labels = &notebook.keys.keys.labels;
        if revoked.is_empty() {
            return self.say("Choose or mark the keys to replace");
        }
        if labels.len() == 1 {
            return self.say("Add a backup first (Insert): a kept key authorizes the replacement");
        }
        if revoked.len() >= labels.len() {
            return self.say("Keep at least one key: the kept keys authorize the replacement");
        }
        let mut details: Vec<String> = revoked
            .iter()
            .filter_map(|&index| labels.get(index))
            .map(|key| format!("Revoked: the {} key {}", key.role.name(), key.fingerprint))
            .collect();
        let primary = revoked.iter().any(|&index| {
            labels
                .get(index)
                .is_some_and(|key| key.role == crate::protocol::Role::Primary)
        });
        details.push(format!(
            "Each kept key is asked for, then one new key is enrolled as the {}.",
            if primary { "primary" } else { "backup" }
        ));
        details.push(
            "A revoked key no longer opens this notebook. Copies exported earlier still open with it."
                .to_owned(),
        );
        let title = match revoked.len() {
            1 => "Replace this key?".to_owned(),
            count => format!("Replace these {count} keys?"),
        };
        self.dialog_revision += 1;
        let revision = self.dialog_revision;
        let model = move || {
            let details: Vec<&str> = details.iter().map(String::as_str).collect();
            Model::new(&title, "Replace", &details, Act::Replace, revision)
        };
        self.open_dialog(&model, None, opener);
    }

    fn replace_now(&mut self) {
        if self.busy.is_some() {
            return;
        }
        let revoked = self.revoking();
        if revoked.is_empty() {
            return;
        }
        let op = self.op();
        self.out
            .push(Out::Send(Command::ReplaceKeys { op, revoked }));
        self.busy = Some(Busy::Keys {
            op,
            what: KeyOp::Replace,
        });
        self.say("Replacing keys");
    }

    // Encrypted copies.

    /// Opens the finder on a folder to write the encrypted copy into.
    fn start_export(&mut self) {
        if self.busy.is_some() {
            return self.say("Wait for the current operation to finish");
        }
        if !matches!(self.phase, Phase::Unlocked(_)) {
            return;
        }
        self.open_chooser(Purpose::Export);
        self.say("Choose a folder: Return opens one, Ctrl+Return exports into it, Escape cancels");
    }

    /// Opens the finder on a copy to import, when the account holds no
    /// notebook.
    fn start_import(&mut self) {
        if self.busy.is_some() || !matches!(self.phase, Phase::Locked { keys: None, .. }) {
            return;
        }
        self.open_chooser(Purpose::Import);
        self.say("Choose an encrypted copy: Return opens a folder or reads the copy");
    }

    fn open_chooser(&mut self, purpose: Purpose) {
        self.next_chooser += 1;
        let id = self.next_chooser;
        self.chooser = Some(Chooser {
            id,
            finder: None,
            folder: PathBuf::new(),
            purpose,
        });
        self.out.push(Out::List {
            chooser: id,
            folder: None,
            select: None,
            files: purpose == Purpose::Import,
        });
        self.sync_focus();
        self.redraw = true;
    }

    /// Closes the finder, the focus back where the finder took it from.
    fn close_chooser(&mut self, status: &str) {
        self.chooser = None;
        self.sync_focus();
        self.say(status);
    }

    /// The listing `Out::List` asked for: installed in the finder, or,
    /// when it could not be read, noted there; a first listing that
    /// cannot be shown closes the finder. One for a finder since closed
    /// is dropped.
    pub fn listed(
        &mut self,
        chooser: u64,
        folder: PathBuf,
        listing: Result<finder::Listing, String>,
        select: Option<&str>,
    ) {
        let surface = self.surface;
        let Some(chooser) = self.chooser.as_mut().filter(|open| open.id == chooser) else {
            return;
        };
        let choose = match chooser.purpose {
            Purpose::Export => finder::Choose::Folder,
            Purpose::Import => finder::Choose::File,
        };
        self.redraw = true;
        let refused = match (listing, &mut chooser.finder) {
            (Ok(listing), Some(finder)) => match finder.set_listing(listing, select) {
                Ok(()) => {
                    chooser.folder = folder;
                    return;
                }
                Err(error) => error.to_string(),
            },
            (Ok(listing), None) => {
                let rect = layout::finder(surface, chooser.purpose == Purpose::Import);
                match finder::Controller::new(listing, choose, surface, rect, select) {
                    Ok(finder) => {
                        chooser.finder = Some(finder);
                        chooser.folder = folder;
                        return;
                    }
                    Err(finder::Error::NoRoom | finder::Error::InvalidSurface) => {
                        return self.close_chooser("The window is too small for the finder");
                    }
                    Err(error) => {
                        return self.close_chooser(&format!("The finder cannot open: {error}"));
                    }
                }
            }
            (Err(text), _) => text,
        };
        match chooser.finder.as_mut() {
            Some(finder) => {
                let _ = finder.set_note(&fitted(&refused));
            }
            None => self.close_chooser(&refused),
        }
    }

    /// One finder event and what it asks: a folder listed, the export
    /// written, or a copy read.
    fn chooser_event(&mut self, event: finder::Event) {
        let Some(chooser) = &mut self.chooser else {
            return;
        };
        let Some(finder) = chooser.finder.as_mut() else {
            if let finder::Event::Key {
                key: finder::Key::Escape,
                ..
            } = event
            {
                self.close_chooser("");
            }
            return;
        };
        let files = chooser.purpose == Purpose::Import;
        let id = chooser.id;
        match finder.event(event) {
            finder::Outcome::Ignored | finder::Outcome::Consumed => {}
            finder::Outcome::Changed => self.redraw = true,
            finder::Outcome::Descend(index) => {
                if let Some(entry) = finder.listing().entries().get(index) {
                    let folder = chooser.folder.join(entry.name());
                    self.out.push(Out::List {
                        chooser: id,
                        folder: Some(folder),
                        select: None,
                        files,
                    });
                }
            }
            finder::Outcome::Ascend => {
                if let Some(parent) = chooser.folder.parent() {
                    let select = chooser
                        .folder
                        .file_name()
                        .and_then(|name| name.to_str())
                        .map(str::to_owned);
                    self.out.push(Out::List {
                        chooser: id,
                        folder: Some(parent.to_path_buf()),
                        select,
                        files,
                    });
                }
            }
            finder::Outcome::Closed(choice) => {
                let name = match choice {
                    finder::Choice::Entry(index) => finder
                        .listing()
                        .entries()
                        .get(index)
                        .map(|entry| entry.name().to_owned()),
                    _ => None,
                };
                let Some(chooser) = self.chooser.take() else {
                    return;
                };
                self.sync_focus();
                self.redraw = true;
                match (choice, chooser.purpose, name) {
                    (finder::Choice::Here, Purpose::Export, _) => self.export(chooser.folder),
                    (finder::Choice::Entry(_), Purpose::Import, Some(name)) => {
                        self.read_copy(chooser.folder.join(name));
                    }
                    (finder::Choice::Unavailable(error), _, _) => self.say(error.to_string()),
                    _ => self.say(""),
                }
            }
        }
    }

    fn export(&mut self, folder: PathBuf) {
        let op = self.op();
        self.out.push(Out::Send(Command::Export { op, folder }));
        self.busy = Some(Busy::Export(op));
        self.say("Exporting");
    }

    fn read_copy(&mut self, path: PathBuf) {
        let op = self.op();
        self.out.push(Out::Send(Command::ReadCopy { op, path }));
        self.busy = Some(Busy::ReadCopy(op));
        self.say("Reading the copy");
    }

    /// Imports the copy read with the selected key.
    fn import(&mut self) {
        if self.busy.is_some() {
            return;
        }
        let Phase::Importing { keys, list } = &self.phase else {
            return;
        };
        let Some(key) = list.selected().filter(|&key| key < keys.len()) else {
            return self.say("Choose a key to import the copy with");
        };
        if self.awaiting_host() {
            return;
        }
        let op = self.op();
        self.out.push(Out::Send(Command::Import { op, key }));
        self.busy = Some(Busy::Import(op));
        self.say("Importing");
    }

    /// Gives the copy up: the thread drops it and lists the account's
    /// keys again.
    fn cancel_import(&mut self) {
        if self.busy.is_some() {
            self.out.push(Out::Cancel);
            self.busy = None;
            self.end_prompt();
        }
        self.out.push(Out::Send(Command::Lock));
        self.phase = Phase::Locking;
        self.say("");
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
        self.chooser = None;
        self.paste = None;
        self.drag = None;
        self.deferred = None;
        self.withdraw = true;
        self.scrub = true;
        self.phase = Phase::Locking;
        self.focus = Focus::Keys;
        self.redraw = true;
    }
}

fn notebook(entries: Vec<Item>) -> Result<Notebook, String> {
    let field =
        || EntryModel::new(FIELD_BYTES).map_err(|_| "td-pass cannot hold a text field".to_owned());
    Ok(Notebook {
        keys: KeyView {
            keys: Keys::default(),
            list: ListModel::default(),
            marked: Vec::new(),
            showing: false,
            before: Focus::Search,
        },
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

/// A note cut to the finder's bound at a character, its tail kept.
fn fitted(note: &str) -> String {
    let flat: String = note
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    if flat.len() <= finder::NOTE_BYTES {
        return flat;
    }
    let keep = finder::NOTE_BYTES - '\u{2026}'.len_utf8();
    let mut start = flat.len() - keep;
    while !flat.is_char_boundary(start) {
        start += 1;
    }
    format!("\u{2026}{}", flat.get(start..).unwrap_or_default())
}

/// What swap on storage risks, for the question that asks to accept it.
const SWAP_SUMMARY: &str = "While td-pass runs, the kernel may write the PIN you \
    type, the key that opens the vault and entry text to this swap, where they can \
    stay after td-pass exits:";

/// What accepting covers.
const SWAP_SCOPE: &str = "Open anyway accepts that for this run only; td-pass \
    asks again next time.";

/// What storage swap exposes, and to whom.
const SWAP_RISK: &str = "Turning swap off does not erase what was written. Anyone \
    who can read that storage can read it, or who can unlock it if it is \
    encrypted with a lasting key, as under full-disk encryption. Swap given a \
    fresh key at every boot loses it at the next restart, not before. \
    Hibernation writes all of memory to swap the same way.";

/// The most devices the swap question lists one by one; the kernel
/// allows fewer.
const SWAP_DEVICES: usize = 64;

/// The most characters of a device's name the question shows.
const SWAP_NAME: usize = 512;

/// `device` as the swap question shows it: control characters escaped,
/// as a dialog shows none, the kernel's own escapes as it wrote them, and
/// long names cut.
fn shown(device: &str) -> String {
    let mut escaped = device.chars().flat_map(|c| {
        let control = c.is_control();
        c.escape_default()
            .filter(move |_| control)
            .chain((!control).then_some(c))
    });
    let mut shown: String = escaped.by_ref().take(SWAP_NAME).collect();
    if escaped.next().is_some() {
        shown.push('…');
    }
    shown
}

/// How to open without the question.
const SWAP_REMEDY: &str = "To avoid this, turn swap off (swapoff -a) or swap only \
    to zram without a writeback device, then start td-pass again.";

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

/// What the prompt asks of the person: the key and what to do with it.
/// A backup is asked for as a key not already enrolled, since a token
/// holding one of the vault's credentials refuses to enroll again.
fn instruction(ask: &Ask) -> String {
    let key = match &ask.key {
        Some(fingerprint) => format!("the {} key {fingerprint}", ask.role.name()),
        None => format!("the key to enroll as {}", ask.role.name()),
    };
    match ask.pin {
        None if ask.key.is_none() && ask.role == Role::Backup => {
            format!("Connect {key}, not one already enrolled")
        }
        None => format!("Connect {key}"),
        Some(PinUse::Authorize | PinUse::Enroll) => format!("Type the PIN of {key}"),
        Some(PinUse::Proof) => format!("Once more, type the PIN of {key}"),
    }
}

/// The status line while a prompt waits: the instruction, then the
/// operation, which the prompt shows as its title.
fn asking(ask: &Ask) -> String {
    format!("{}, to {}", instruction(ask), ask.operation)
}

/// The prompt's geometry with its title and instruction wrapped to it, so
/// no word of either is cut. A window too short for every row keeps the
/// field and buttons: the instruction keeps its rows before the title,
/// each keeps one, and a text cut short ends in an ellipsis.
fn prompt_view(surface: Surface, ask: &Ask) -> (layout::Prompt, Vec<String>, Vec<String>) {
    let columns = layout::prompt_columns(surface);
    let pin = ask.pin.is_some();
    let rows = layout::prompt_text_rows(surface, pin);
    let mut line = td_ui::text::wrap(&instruction(ask), columns);
    let mut title = td_ui::text::wrap(ask.operation, columns);
    shorten(&mut line, rows.saturating_sub(1).max(1), columns);
    shorten(&mut title, rows.saturating_sub(line.len()).max(1), columns);
    let view = layout::prompt(surface, pin, title.len(), line.len());
    (view, title, line)
}

/// Keeps `rows`' first `keep`, the last of them ending in an ellipsis when
/// any were dropped.
fn shorten(rows: &mut Vec<String>, keep: usize, columns: usize) {
    if rows.len() <= keep {
        return;
    }
    rows.truncate(keep);
    if let Some(last) = rows.last_mut() {
        if last.chars().count() >= columns {
            last.pop();
        }
        last.push('…');
    }
}

#[cfg(test)]
mod tests;

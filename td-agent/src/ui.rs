//! The window's state and drawing (DESIGN.md §4), apart from the
//! compositor and the processes so it is tested on its own: a horizontal
//! split with the conversation list on the left, a td-ui tree table, and
//! on the right the active conversation's transcript, a td-ui message
//! list, over the composer, td-ui's editor pane, and a status row.
//!
//! The list holds every conversation, most recently active first. Each
//! conversation is a row of its own; one with a workspace (DESIGN.md §7)
//! names it in the status row.
//!
//! What the window must do outside itself (open a conversation, start a
//! new one, send a message, ask a failed turn again, save the split) it
//! asks through `Request`s, which the session takes after every input.
//!
//! The status row says the open conversation's model, the context its
//! last request used against the model's length, what the conversation
//! has cost, what today has, and the key's credit (DESIGN.md §4, §5).
//!
//! The open conversation's todo list (DESIGN.md §12), when it has one, is
//! drawn above the composer, collapsed to the item in progress; `C-t`
//! shows the whole list and `C-S-t` clears it. `C-S-p` pauses or resumes
//! the open conversation (§3). `C-z` undoes its latest step that changed
//! files, `C-S-z` redoes the latest undone (§12).
//!
//! A menu bar holds the File, Conversation and Help menus (`menu`), which
//! a press on a header opens, `F10` opening File. File → Set OpenRouter
//! key… opens the key dialog (`keydialog`) and Conversation → Model… the
//! model picker (`picker`), each modal over the window until it closes;
//! Help → Keys opens the window's key list.

use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use td_ui::chrome::ROW;
use td_ui::editor::{self, Controller as Pane, Event as PaneEvent, Outcome as PaneOutcome};
use td_ui::editor_clipboard::{Paste, Snapshot};
use td_ui::editor_model::TabId;
use td_ui::keys::{self, Section};
use td_ui::messages::{self, Error as MessagesError, Message, Tone};
use td_ui::raster::{
    self, Composition, Draw, GlyphStyle, Primitive, Rect, Surface, Weight, BORDER, CHROME, INK,
    PAPER,
};
use td_ui::split;
use td_ui::tree_table::{self, Cell, Column, Model, Row as TreeRow};
use td_ui::window::{Clipboard, Input, PointerPhase};
use td_ui::{CELL_HEIGHT, CELL_WIDTH};

use crate::chooser::Chooser;
use crate::config::Mode;
use crate::confirm::{self, Confirm};
use crate::cost;
use crate::key::Secret;
use crate::keydialog::{KeyDialog, Reply};
use crate::menu;
use crate::picker::{Offer, Picker};
use crate::post::Entry;
use crate::protocol::{Up, MAX_TEXT};
use crate::store::{Event, Held, Id, Kind, Purpose, Role, Status, TodoItem};
use crate::supervisor::Update;
use crate::tools;
use crate::workspace::Workspace;

/// Why a hello came without the prefix (DESIGN.md §4).
const LONG_PREFIX: &str = "the conversation's prefix file is longer than the window is sent";

/// The composer's text rows when the window has room for them.
const COMPOSER_ROWS: usize = 6;
/// The most rows the todo list takes when shown whole.
const TODO_ROWS: usize = 12;
/// The split's child minima, in logical pixels.
const LIST_MIN: u32 = 160;
const CONVERSATION_MIN: u32 = 320;
/// The list's columns, which fit the list at its default share of a
/// window 1024 pixels wide.
const COLUMNS: &[Column<'static>] = &[
    Column {
        title: "Conversation",
        minimum: 120,
        preferred: 152,
        numeric: false,
    },
    Column {
        title: "State",
        minimum: 72,
        preferred: 72,
        numeric: false,
    },
    Column {
        title: "Workspace",
        minimum: 96,
        preferred: 96,
        numeric: false,
    },
];

/// A conversation's process as the list shows it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RowState {
    /// Not open: it has no process.
    Closed,
    Starting,
    Idle,
    /// A turn is under way.
    Running,
    Restarting,
    Failed,
}

impl RowState {
    pub fn word(self) -> &'static str {
        match self {
            Self::Closed => "",
            Self::Starting => "starting",
            Self::Idle => "idle",
            Self::Running => "running",
            Self::Restarting => "restarting",
            Self::Failed => "failed",
        }
    }
}

/// A card a conversation asks the human, until decided or withdrawn: may
/// its call `call` run?
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Card {
    pub conversation: Id,
    pub call: u64,
    pub title: String,
    pub details: Vec<String>,
    /// What its "always" answers would remember, when it offers them.
    pub always: Option<crate::rules::Offer>,
}

/// The human's answer to a card (DESIGN.md §11): allow or deny this once,
/// or always, the rules for `bodies` added to their rules for this
/// workspace, or for every workspace.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Answer {
    Once(bool),
    Always {
        allow: bool,
        everywhere: bool,
        bodies: Vec<String>,
    },
    /// Always, for a crossing: whether the asking conversation may `op`
    /// conversation `to`, that way only.
    Crossing {
        allow: bool,
        op: crate::rules::Crossed,
        to: String,
    },
}

/// What the list says of a conversation that asks the human.
pub const ASKS: &str = "asks you";
/// How long a card that came unbidden ignores keys and presses, so that
/// what the human was typing or clicking does not decide it.
const CARD_SETTLE: Duration = Duration::from_millis(750);

/// What the status row says of the open conversation, from its log.
#[derive(Clone, Debug, Default)]
struct Meter {
    /// Each request's sequence number, purpose and reservation.
    requests: Vec<(u64, Purpose, u64)>,
    /// Each turn reply's request and its message's index.
    replies: Vec<(u64, usize)>,
    /// The requests a usage record charged.
    charged: Vec<u64>,
    spent: u64,
    /// The last turn request's prompt and completion tokens.
    context: Option<u64>,
    /// The last turn failed in a way the human may ask again.
    retry: bool,
}

impl Meter {
    fn request(&self, seq: u64) -> Option<(Purpose, u64)> {
        self.requests
            .iter()
            .rev()
            .find(|(s, _, _)| *s == seq)
            .map(|(_, purpose, reserved)| (*purpose, *reserved))
    }
}

/// A count of tokens, short.
fn tokens(count: u64) -> String {
    if count >= 10_000 {
        format!("{}k", count / 1000)
    } else {
        count.to_string()
    }
}

/// A reply the transcript draws as it streams in: its request, its
/// message's index, and whether that message has a reasoning section
/// (the first) before its text.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Streaming {
    request: u64,
    index: usize,
    reasoning: bool,
}

/// What the open picker chooses.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Picking {
    /// The open conversation's model.
    Model,
    /// The default model.
    Default,
    /// A new conversation's workspace template.
    Template,
}

/// How much of a background process's command its row shows.
const MAX_PROCESS_LABEL: usize = 120;
/// How many lines of a process's output make one entry of its view.
const OUTPUT_LINES: usize = 40;

/// One conversation in the list.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Row {
    pub id: Id,
    pub title: String,
    /// When it was last active, in milliseconds since the epoch: what the
    /// list orders the conversations by, then their ids.
    pub activity: u64,
    pub state: RowState,
    /// The human paused it: messages from other conversations wait.
    pub paused: bool,
    /// What it works in; the list's Workspace column names it.
    pub workspace: Option<Workspace>,
    /// The human archived it: the list hides it unless archived ones
    /// show, and it opens only once unarchived.
    pub archived: bool,
}

impl Row {
    /// Its state as the list names it.
    pub fn word(&self) -> &'static str {
        match self.state {
            _ if self.archived => "archived",
            RowState::Closed | RowState::Idle if self.paused => "paused",
            state => state.word(),
        }
    }
}

/// What the window asks of the session.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Request {
    /// Open this conversation in its own process; the one open before is
    /// closed.
    Open(Id),
    /// Start a new conversation with no workspace, when none can be made.
    New,
    /// Start a new conversation in a scratch workspace: the Empty
    /// template.
    NewScratch,
    /// Start a new conversation in this directory, once admitted.
    NewIn(PathBuf),
    /// Start a new conversation from the configured template named.
    NewFrom(String),
    /// Send the human's message to the open conversation.
    Send(String),
    /// Ask the open conversation's failed turn again.
    Retry,
    /// Interrupt the open conversation's turn.
    Interrupt,
    /// Pause, or resume, the open conversation.
    Pause(bool),
    /// Clear the open conversation's todo list.
    ClearTodo,
    /// Undo the open conversation's step snapshotted at this place.
    Undo(u64),
    /// Redo it.
    Redo(u64),
    /// Save the split's preferred share.
    SaveShare(u32, u32),
    /// Store the OpenRouter key from the key dialog, replacing a stored
    /// one only when `replace` says so; the session answers through
    /// `key_saved`, `key_exists` or `key_refused`.
    SaveKey { secret: Secret, replace: bool },
    /// Write the diagnostics archive, from File → Export diagnostics.
    Export,
    /// Delete this conversation for good, as the human confirmed.
    Delete(Id),
    /// Archive this conversation, or bring it back, from its row's menu.
    Archive { id: Id, archived: bool },
    /// Kill conversation `id`'s background process `number`, from its
    /// row's menu (DESIGN.md §12).
    Kill { id: Id, number: u64 },
    /// Read the output of conversation `id`'s background process
    /// `number`; the session answers through `show_output`.
    Output { id: Id, number: u64 },
    /// Make this the default model, from Conversation → Default model….
    SetDefault(String),
    /// Put conversation `conversation`'s workspace in this mode, from
    /// Conversation → Auto mode in this workspace (DESIGN.md §11).
    SetMode { conversation: Id, mode: Mode },
    /// Read what this conversation's workspace card shows; the session
    /// answers through `show_workspace` (DESIGN.md §7).
    Workspace(Id),
    /// The human answered the loss card of conversation `id`: delete it
    /// and its workspace (`remove`), or keep both (DESIGN.md §7).
    Remove { id: Id, remove: bool },
    /// The human admitted `remotes` on the admission card, so template
    /// `template`'s workspace can be made (DESIGN.md §7).
    Admit {
        template: String,
        remotes: Vec<String>,
    },
    /// The human decided a card: whether call `call` of `conversation`
    /// may run (DESIGN.md §11).
    Decide {
        conversation: Id,
        call: u64,
        answer: Answer,
    },
    /// Close the window, from File → Quit.
    Quit,
    /// The open conversation's model and effort as the human chose them,
    /// whole; none is the default.
    Choose {
        model: Option<String>,
        effort: Option<String>,
    },
}

/// What has the keyboard.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Focus {
    List,
    Transcript,
    Composer,
}

impl Focus {
    pub fn word(self) -> &'static str {
        match self {
            Self::List => "list",
            Self::Transcript => "transcript",
            Self::Composer => "composer",
        }
    }
}

/// Which widget a held button's press landed on.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Capture {
    /// The divider, with the share it had at the press.
    Split(split::Share),
    List,
    Transcript,
    Composer,
}

/// Where each part of the right side is.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Regions {
    list: Rect,
    transcript: Rect,
    /// The todo list, empty when there is none.
    todo: Rect,
    /// The rule above the composer.
    rule: Rect,
    composer: Rect,
    status: Rect,
}

/// The composer: one editable document in td-ui's editor pane. `Return`
/// sends it, as `C-Return` does from outside a dialog; `S-Return` is the
/// pane's own newline.
struct Composer {
    pane: Pane,
    tab: Option<TabId>,
}

impl Composer {
    fn new() -> Result<Self, String> {
        let mut composer = Self {
            pane: Pane::pane().map_err(|e| format!("composer: {e}"))?,
            tab: None,
        };
        composer.fresh();
        Ok(composer)
    }

    fn event(&mut self, event: PaneEvent<'_>) -> PaneOutcome {
        self.pane.dispatch(event).unwrap_or(PaneOutcome::Ignored)
    }

    fn target(&self) -> Option<(TabId, u64)> {
        let tab = self.tab?;
        Some((tab, self.pane.editor().document(tab).ok()?.revision()))
    }

    /// An empty document in place of the one there was.
    fn fresh(&mut self) {
        if let Some(tab) = self.tab.take() {
            if let Ok((point, _)) = self.pane.editor().save_snapshot(tab) {
                self.event(PaneEvent::Saved(point));
            }
            if let Ok(document) = self.pane.editor().document(tab) {
                let revision = document.revision();
                self.event(PaneEvent::Close { tab, revision });
            }
        }
        if let PaneOutcome::Created(tab) = self.event(PaneEvent::New) {
            self.tab = Some(tab);
        }
    }

    fn text(&self) -> &str {
        self.tab
            .and_then(|tab| self.pane.editor().document(tab).ok())
            .map_or("", |document| document.text())
    }

    fn place(&mut self, rect: Rect, surface: Surface) {
        if rect.width > 0 && rect.height > 0 {
            self.event(PaneEvent::Frame { rect, surface });
        }
    }

    fn chord(&mut self, chord: &str) -> bool {
        let Some((tab, revision)) = self.target() else {
            return false;
        };
        matches!(
            self.event(PaneEvent::Key {
                tab,
                revision,
                chord,
            }),
            PaneOutcome::Changed | PaneOutcome::Prefix
        )
    }

    fn pointer(&mut self, phase: PointerPhase, x: i64, y: i64, extend: bool) -> bool {
        let Some((tab, revision)) = self.target() else {
            return false;
        };
        let phase = match phase {
            PointerPhase::Press => editor::PointerPhase::Press,
            PointerPhase::Move => editor::PointerPhase::Move,
            PointerPhase::Release => editor::PointerPhase::Release,
        };
        self.event(PaneEvent::Pointer {
            tab,
            revision,
            phase,
            x,
            cell_x: x,
            y,
            extend,
        }) == PaneOutcome::Changed
    }

    /// The selection, or the caret's line, as a copy takes it.
    fn selection(&self) -> Option<Snapshot> {
        let (tab, revision) = self.target()?;
        Snapshot::capture(self.pane.editor(), tab, revision)
            .ok()
            .flatten()
    }

    fn scroll(&mut self, rows: isize, columns: isize) -> bool {
        let Some((tab, revision)) = self.target() else {
            return false;
        };
        self.event(PaneEvent::Scroll {
            tab,
            revision,
            rows,
            columns,
        }) == PaneOutcome::Changed
    }

    fn insert(&mut self, text: &str) -> Result<bool, String> {
        let Some((tab, revision)) = self.target() else {
            return Ok(false);
        };
        let paste = Paste::begin(self.pane.editor(), tab, revision)
            .and_then(|mut paste| {
                paste.push(text.as_bytes())?;
                Ok(paste)
            })
            .map_err(|e| format!("the paste: {e}"))?;
        self.pane
            .dispatch(PaneEvent::Paste(paste))
            .map(|outcome| outcome == PaneOutcome::Changed)
            .map_err(|e| format!("the paste: {e}"))
    }

    /// `text` after everything there is, on lines of its own, leaving
    /// the draft and its selection as they were; whether it went in.
    fn append(&mut self, text: &str) -> Result<bool, String> {
        if self.text().is_empty() {
            return self.insert(text);
        }
        // To the end, which drops the selection rather than replacing it.
        self.chord("C-End");
        let tail = if self.text().ends_with('\n') {
            ""
        } else {
            "\n"
        };
        self.insert(&format!("{tail}{text}\n"))
    }

    fn emit(&self, damage: Rect, sink: &mut dyn FnMut(Draw)) {
        if let Ok(scene) = self.pane.scene(&[]) {
            scene.emit(damage, sink);
        }
    }
}

/// The focused widgets' keys for the key list, as `key` binds them; the
/// window's own chords are `control::BINDINGS`.
const LIST_KEYS: &[(&str, &str)] = &[
    ("Up/Down", "Select a conversation."),
    ("PageUp/PageDown", "Select a page away."),
    ("Home/End", "Select the first or last conversation."),
    ("Return", "Open the selected conversation."),
];
const TRANSCRIPT_KEYS: &[(&str, &str)] = &[
    ("Up/Down", "Scroll a row."),
    ("PageUp/PageDown", "Scroll a page."),
    ("Home/End", "Scroll to the top or the end."),
    ("M-Up/M-Down", "Focus the previous or next message."),
    ("Return", "Collapse or expand the focused message."),
    ("C-a", "Select the whole transcript."),
    ("C-c", "Copy the selection."),
    ("C-S-c", "Copy the focused message whole."),
];
const COMPOSER_KEYS: &[(&str, &str)] = &[
    (
        "Return/C-Return",
        "Send the text to the open conversation and empty the composer; blank text is not sent.",
    ),
    ("S-Return", "Start a new line."),
    ("C-c/C-Insert", "Copy the selection."),
    ("C-x", "Cut the selection."),
    ("C-v/S-Insert", "Paste."),
];

/// The window's state.
pub struct App {
    surface: Surface,
    rows: Vec<Row>,
    active: Option<Id>,
    list: tree_table::Controller<usize>,
    split: split::Controller,
    transcript: messages::Controller,
    composer: Composer,
    regions: Option<Regions>,
    focus: Focus,
    focused: bool,
    capture: Option<Capture>,
    /// The configuration's mode, a workspace's own when the human's rules
    /// set none.
    mode: Mode,
    /// Each workspace's mode as the human's rules set it, by its key;
    /// none while the rules cannot be read, every workspace then `ask`.
    modes: Option<Vec<(String, Mode)>>,
    /// `jev_required = false`: the classifier may allow on its reasoning
    /// stage alone, which the status row says (DESIGN.md §11).
    without_jev: bool,
    /// td-agent's notes, which the Messages window shows.
    log: crate::notes::Log,
    /// Each row's workspace as the list's third column names it.
    labels: Vec<String>,
    /// Whether the list shows archived conversations.
    show_archived: bool,
    /// The open conversation's todo list, and whether it is shown whole.
    todo: Vec<TodoItem>,
    todo_open: bool,
    /// The open conversation's steps to undo and redo, from its log.
    steps: crate::store::Steps,
    /// Each user message's sequence number and its index in the
    /// transcript, and each started turn's and its user message's.
    messages: Vec<(u64, usize)>,
    turns: Vec<(u64, u64)>,
    /// The turn each conversation in the background is running.
    background_turns: Vec<(Id, u64)>,
    /// The last event shown, so one heard twice (read from the log when a
    /// running conversation is opened again, then from its process) is
    /// shown once.
    last_seq: u64,
    meter: Meter,
    /// The reply streaming in, until its request ends.
    streaming: Option<Streaming>,
    /// A request whose reply the transcript could not draw as it came:
    /// its deltas are dropped, and its reply shown once logged.
    undrawn: Option<u64>,
    /// The model a conversation with none of its own uses.
    default_model: String,
    /// Each model's context length, as the models list gives it.
    contexts: Vec<(String, u64)>,
    /// The configuration's reasoning effort.
    effort: String,
    /// The models list as the picker offers it.
    offers: Vec<Offer>,
    /// The open conversation's model and effort as the human chose them,
    /// from its log; none is the configuration's.
    choice: (Option<String>, Option<String>),
    /// The choices asked for and not yet heard logged, oldest first: the
    /// next one builds on the last, though the process has not taken it.
    asked: VecDeque<(Option<String>, Option<String>)>,
    /// A prefix is in force, shown or said not to be, since the
    /// transcript was last cleared, so a later one replaces it.
    system_shown: bool,
    /// The picker while it is open, modal over the window, and what it
    /// chooses.
    picker: Option<Picker>,
    /// The Messages window while it is open, modal over the window.
    notes: Option<crate::notes::Panel>,
    picking: Picking,
    /// The configured templates the template chooser lists after the
    /// built-ins, each a name and whether it names repositories.
    templates: Vec<(String, bool)>,
    /// The directory chooser while it is open, modal over the window, and
    /// the folder it opens on.
    chooser: Option<Chooser>,
    chooser_start: PathBuf,
    /// Why no workspace can be made, when none can: said at once, before
    /// a directory is chosen.
    no_workspaces: Option<String>,
    /// The question before a conversation is deleted, or a card, while
    /// it is open, modal over the window; its revision counts the
    /// questions asked.
    confirm: Option<Confirm>,
    confirm_revision: u64,
    /// The cards the conversations ask, in the order they came; the open
    /// conversation's first is shown whenever nothing else is modal.
    cards: Vec<Card>,
    /// When the card shown appeared, and how long it ignores keys and
    /// presses from then.
    card_shown: Option<Instant>,
    settle: Duration,
    /// The card that could not be drawn, said once until one is.
    card_trouble: Option<(Id, u64)>,
    /// An admission card set aside when the window lost the keyboard:
    /// its template and remotes, asked again when it comes back.
    admission: Option<(String, Vec<String>)>,
    /// Conversations being deleted or archived whose workspaces are being
    /// asked what removing them would lose, each with the list's word for
    /// it: none opens or takes a message meanwhile.
    closing: Vec<(Id, &'static str)>,
    /// Loss cards not yet answered, each conversation's with its title,
    /// what its workspace reported and whether it is being archived:
    /// asked until answered, since the conversation stays stopped until
    /// then.
    removals: Vec<(Id, String, Vec<String>, bool)>,
    /// The loss card that could not be shown, said once.
    removal_trouble: Option<Id>,
    /// The menu's revision: it is built again, from the state of the
    /// moment, each time it opens.
    menu_revision: u64,
    /// The conversation the menu is the row menu of, while it is one.
    menu_row: Option<Id>,
    /// The background process whose row menu is open.
    menu_process: Option<(Id, u64)>,
    /// Each conversation's background processes running, as its events
    /// say, by number with its command (DESIGN.md §12).
    processes: BTreeMap<Id, Vec<(u64, String)>>,
    /// The list's process rows, after its conversations: the row at
    /// `rows.len() + n` is the `n`th here.
    process_items: Vec<(Id, u64)>,
    /// Each process row's label: its number and command.
    process_labels: Vec<String>,
    /// The id of the list's first process row: `rows.len()` when the list
    /// was last built, which a change to `rows` before its rebuild keeps.
    process_base: usize,
    credit: Option<String>,
    today: Option<u64>,
    limits: cost::Limits,
    requests: Vec<Request>,
    /// The bar's menu, closed until opened.
    menu: menu::Menu,
    /// Help → Keys was chosen during the input `input_live` delivers;
    /// cleared before each, so a choice through another path (the
    /// control seam's) never reaches the window.
    keys_chosen: bool,
    /// An input of `input_live` is being handled: only the human's own
    /// pointer or keyboard sets a workspace's mode (DESIGN.md §11), never
    /// the control seam.
    live: bool,
    /// The key dialog while it is open, modal over the window.
    dialog: Option<KeyDialog>,
    /// Where the key file is, as the dialog says it; none when there is
    /// no configuration directory to put it in.
    key_path: Option<String>,
    /// Whether the session has a key to hand the conversations.
    keyed: bool,
    /// Where the press that is the dialog's latest input was, to keep a
    /// confirmation's action from under it; none after a key.
    press: Option<(i64, i64)>,
    /// The key dialog asked the clipboard for its text, and the paste has
    /// not come: it is the dialog's, and dropped if the dialog has gone,
    /// never the composer's.
    dialog_paste: bool,
    clock: u64,
    dirty: bool,
    /// Counts the changes that need a paint, so a driven input can say
    /// whether it changed anything whether or not a paint has happened.
    generation: u64,
}

impl App {
    /// The window over `surface`, its split at `share` (first and total
    /// pixels) when one was saved.
    pub fn new(surface: Surface, share: Option<(u32, u32)>, mode: Mode) -> Result<Self, String> {
        let share = share
            .and_then(|(first, total)| split::Share::new(first, total))
            .unwrap_or_else(|| split::Share::new(1, 3).unwrap_or_default());
        let split = split::Controller::new(
            split::Config {
                axis: split::Axis::Horizontal,
                first_min: LIST_MIN,
                second_min: CONVERSATION_MIN,
            },
            share,
            surface,
            body(surface),
        )
        .map_err(|e| e.to_string())?;
        let model = Model::new(&[], COLUMNS).map_err(|e| e.to_string())?;
        let mut app = Self {
            surface,
            rows: Vec::new(),
            active: None,
            list: tree_table::Controller::new(model, surface, surface.bounds())
                .map_err(|e| e.to_string())?,
            split,
            transcript: messages::Controller::new(surface, surface.bounds())
                .map_err(|e| e.to_string())?,
            composer: Composer::new()?,
            regions: None,
            focus: Focus::Composer,
            focused: true,
            capture: None,
            mode,
            modes: Some(Vec::new()),
            without_jev: false,
            log: crate::notes::Log::default(),
            labels: Vec::new(),
            show_archived: false,
            todo: Vec::new(),
            todo_open: false,
            steps: crate::store::Steps::default(),
            messages: Vec::new(),
            turns: Vec::new(),
            last_seq: 0,
            background_turns: Vec::new(),
            meter: Meter::default(),
            streaming: None,
            undrawn: None,
            default_model: String::new(),
            contexts: Vec::new(),
            effort: String::new(),
            offers: Vec::new(),
            choice: (None, None),
            asked: VecDeque::new(),
            system_shown: false,
            picker: None,
            notes: None,
            picking: Picking::Model,
            templates: Vec::new(),
            chooser: None,
            chooser_start: std::env::var_os("HOME")
                .map(PathBuf::from)
                .filter(|home| home.is_absolute())
                .unwrap_or_else(|| PathBuf::from("/")),
            no_workspaces: None,
            confirm: None,
            confirm_revision: 0,
            cards: Vec::new(),
            card_shown: None,
            settle: CARD_SETTLE,
            card_trouble: None,
            admission: None,
            closing: Vec::new(),
            removals: Vec::new(),
            removal_trouble: None,
            menu_revision: 1,
            menu_row: None,
            menu_process: None,
            processes: BTreeMap::new(),
            process_items: Vec::new(),
            process_labels: Vec::new(),
            process_base: 0,
            credit: None,
            today: None,
            limits: cost::Limits {
                turn: None,
                conversation: None,
                day: None,
            },
            requests: Vec::new(),
            menu: menu::menu(surface, menu::State::default(), 1)
                .map_err(|e| format!("the menu: {e}"))?,
            keys_chosen: false,
            live: false,
            dialog: None,
            key_path: None,
            keyed: true,
            press: None,
            dialog_paste: false,
            clock: 0,
            dirty: true,
            generation: 0,
        };
        app.layout();
        app.apply_focus();
        Ok(app)
    }

    // --- what the session reads -------------------------------------

    pub fn take_requests(&mut self) -> Vec<Request> {
        std::mem::take(&mut self.requests)
    }

    pub fn has_requests(&self) -> bool {
        !self.requests.is_empty()
    }

    pub fn dirty(&self) -> bool {
        self.dirty
    }

    pub fn painted(&mut self) {
        self.dirty = false;
    }

    /// Changes so far: one more each time something needs painting.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    fn touch(&mut self) {
        self.dirty = true;
        self.generation = self.generation.wrapping_add(1);
    }

    pub fn active(&self) -> Option<&Id> {
        self.active.as_ref()
    }

    pub fn rows(&self) -> &[Row] {
        &self.rows
    }

    /// The conversations as the post routes between them.
    pub fn directory(&self) -> Vec<Entry> {
        self.rows
            .iter()
            .map(|row| Entry {
                id: row.id.clone(),
                state: match row.word() {
                    "" => "idle",
                    word => word,
                }
                .to_string(),
                failed: row.state == RowState::Failed || self.closing(&row.id),
                archived: row.archived,
            })
            .collect()
    }

    /// The open conversation's todo list.
    pub fn todo(&self) -> &[TodoItem] {
        &self.todo
    }

    pub fn focus(&self) -> Focus {
        self.focus
    }

    pub fn composed(&self) -> &str {
        self.composer.text()
    }

    pub fn transcript(&self) -> &messages::Controller {
        &self.transcript
    }

    /// The newest note.
    pub fn notice(&self) -> Option<&str> {
        self.log.last()
    }

    /// The notes not yet shown in the Messages window.
    pub fn unread(&self) -> usize {
        self.log.unread()
    }

    /// The Messages window, while it is open.
    pub fn messages_window(&self) -> Option<&crate::notes::Panel> {
        self.notes
            .as_ref()
            .filter(|panel| panel.card_of().is_none())
    }

    /// The workspace card, while it is open.
    pub fn workspace_card(&self) -> Option<&crate::notes::Panel> {
        self.notes
            .as_ref()
            .filter(|panel| panel.name() == crate::notes::WORKSPACE_CARD)
    }

    /// The open conversation's repository workspace, if it has one.
    fn repositories(&self) -> Option<(&Id, &crate::workspace::Repositories)> {
        let id = self.active.as_ref()?;
        match &self.rows.iter().find(|r| &r.id == id)?.workspace {
            Some(Workspace::Repositories(repositories)) => Some((id, repositories)),
            _ => None,
        }
    }

    /// The status row's line.
    pub fn status_line(&self) -> String {
        let row = self
            .active
            .as_ref()
            .and_then(|id| self.rows.iter().find(|r| &r.id == id));
        let state = row.map_or("no conversation", Row::word);
        // Only items of a fixed width: a note, however long, is counted
        // here and read whole in the Messages window.
        let unread = match self.log.unread() {
            0 => String::new(),
            1 => " | 1 new message: C-S-m".to_string(),
            n => format!(" | {n} new messages: C-S-m"),
        };
        let retry = if self.meter.retry {
            " | C-r asks again"
        } else {
            ""
        };
        let keyless = if self.keyed { "" } else { " | no key: F10" };
        let card = if self.repositories().is_some() {
            " | workspace: C-S-w"
        } else {
            ""
        };
        let model = self.model();
        let effort = if self.reasoning(model) {
            self.effort()
        } else {
            "no reasoning"
        };
        let length = self
            .contexts
            .iter()
            .find(|(id, _)| id == model)
            .map(|(_, length)| *length);
        let context = match (self.meter.context, length) {
            (Some(used), Some(length)) => format!("ctx {}/{}", tokens(used), tokens(length)),
            (Some(used), None) => format!("ctx {}", tokens(used)),
            (None, Some(length)) => format!("ctx 0/{}", tokens(length)),
            (None, None) => "ctx -".to_string(),
        };
        // A spent amount, and the limit on it when one is set.
        let of = |spent: u64, limit: Option<u64>| match limit {
            Some(limit) => format!("{}/{}", cost::show(spent), cost::show(limit)),
            None => cost::show(spent),
        };
        let today = self
            .today
            .map(|t| format!(" | today {}", of(t, self.limits.day)))
            .unwrap_or_default();
        let credit = self
            .credit
            .as_deref()
            .map(|c| format!(" | {c}"))
            .unwrap_or_default();
        format!(
            "{state}{retry}{keyless}{unread}{card} | {model} {effort} | {context} | cost {}{today}{credit} | mode {}{} | no limits | {} background",
            of(self.meter.spent, self.limits.conversation),
            self.shown_mode().word(),
            if self.without_jev {
                ", classifier without Jev"
            } else {
                ""
            },
            self.processes.values().map(Vec::len).sum::<usize>()
        )
    }

    /// Whether the classifier may allow without Jev.
    pub fn set_without_jev(&mut self, without: bool) {
        self.without_jev = without;
        self.touch();
    }

    /// Each workspace's mode as the human's rules set it, or none while
    /// they cannot be read.
    pub fn set_modes(&mut self, modes: Option<Vec<(String, Mode)>>) {
        self.modes = modes;
        self.touch();
    }

    /// The mode the status row says: the open conversation's workspace's,
    /// `ask` for one with no workspace, the configuration's with none
    /// open.
    fn shown_mode(&self) -> Mode {
        let open = self
            .active
            .as_ref()
            .and_then(|id| self.rows.iter().find(|r| &r.id == id));
        match open {
            Some(row) if row.workspace.is_none() => Mode::Ask,
            _ => self.workspace_mode().unwrap_or(self.mode),
        }
    }

    /// The open conversation's workspace's mode: the human's rules', else
    /// the configuration's, `ask` while the rules cannot be read; none
    /// with no workspace open.
    fn workspace_mode(&self) -> Option<Mode> {
        let id = self.active.as_ref()?;
        let row = self.rows.iter().find(|r| &r.id == id)?;
        let key = row.workspace.as_ref()?.key(id);
        Some(match &self.modes {
            None => Mode::Ask,
            Some(modes) => modes
                .iter()
                .rev()
                .find(|(of, _)| of == &key)
                .map_or(self.mode, |(_, mode)| *mode),
        })
    }

    /// The open conversation's model: the human's choice, else the
    /// default.
    pub fn model(&self) -> &str {
        if let Some(model) = &self.choice.0 {
            return model;
        }
        &self.default_model
    }

    /// The open conversation's reasoning effort: the human's choice, else
    /// the configuration's.
    pub fn effort(&self) -> &str {
        self.choice.1.as_deref().unwrap_or(&self.effort)
    }

    /// Whether `model` takes a reasoning effort, as far as the models list
    /// says; one it does not name may.
    fn reasoning(&self, model: &str) -> bool {
        self.offers
            .iter()
            .find(|o| o.id == model)
            .is_none_or(|o| o.reasoning)
    }

    /// The models list as the picker offers it, and the configuration's
    /// reasoning effort.
    pub fn set_offers(&mut self, offers: Vec<Offer>, effort: &str) {
        self.offers = offers;
        self.effort = effort.to_string();
        self.touch();
    }

    /// The picker, the model's or the template's, while it is open.
    pub fn picker(&self) -> Option<&Picker> {
        self.picker.as_ref()
    }

    /// The directory chooser, while it is open.
    pub fn chooser(&self) -> Option<&Chooser> {
        self.chooser.as_ref()
    }

    /// The folder the directory chooser opens on.
    pub fn set_chooser_start(&mut self, folder: PathBuf) {
        self.chooser_start = folder;
    }

    /// Why no workspace can be made, or none when they can.
    pub fn set_no_workspaces(&mut self, why: Option<String>) {
        self.no_workspaces = why;
    }

    /// The configured templates, each a name and whether it names
    /// repositories.
    pub fn set_templates(&mut self, templates: Vec<(String, bool)>) {
        self.templates = templates;
    }

    /// The most recently active conversation's id, archived ones aside,
    /// when the list holds any.
    pub fn most_recent(&self) -> Option<Id> {
        self.rows
            .iter()
            .find(|r| !r.archived && !self.closing(&r.id))
            .map(|r| r.id.clone())
    }

    /// Conversation `id` is being deleted or archived, `doing` saying
    /// which in the list, its workspace asked what removing it would
    /// lose, or no longer: while it is, it does not open, takes no
    /// message, and is closed when it was open.
    pub fn set_closing(&mut self, id: &Id, doing: Option<&'static str>) {
        self.closing.retain(|(other, _)| other != id);
        if let Some(doing) = doing {
            self.closing.push((id.clone(), doing));
            self.background_turns.retain(|(of, _)| of != id);
            // Its process is ended with them, saying nothing more.
            self.processes.remove(id);
            if let Some(row) = self.rows.iter_mut().find(|r| &r.id == id) {
                row.state = RowState::Closed;
            }
            if self.active.as_ref() == Some(id) {
                self.active = None;
                self.clear_transcript();
            }
        } else {
            self.removals.retain(|(of, _, _, _)| of != id);
        }
        self.refresh_list();
        self.touch();
    }

    /// Whether conversation `id` is being deleted or archived, its
    /// workspace asked.
    pub fn closing(&self, id: &Id) -> bool {
        self.closing.iter().any(|(of, _)| of == id)
    }

    /// The list's word for a conversation being deleted or archived.
    fn closing_word(&self, id: &Id) -> Option<&'static str> {
        self.closing
            .iter()
            .find(|(of, _)| of == id)
            .map(|(_, doing)| *doing)
    }

    /// Asks whether conversation `id`, titled `title`, is deleted, or
    /// archived when `archive`, though its workspace reports `lost`: a
    /// card, asked when nothing else is modal and until answered
    /// (`Request::Remove`).
    pub fn ask_removal(&mut self, id: Id, title: String, lost: Vec<String>, archive: bool) {
        self.removals.retain(|(of, _, _, _)| of != &id);
        self.removals.push((id, title, lost, archive));
        self.offer();
    }

    /// The conversation the open menu is the row menu of.
    pub fn menu_row(&self) -> Option<&Id> {
        self.menu_row.as_ref().filter(|_| self.menu.is_open())
    }

    /// Whether the list shows archived conversations.
    pub fn shows_archived(&self) -> bool {
        self.show_archived
    }

    /// Shows archived conversations in the list, or hides them again, as
    /// Conversation → Show archived does.
    pub fn toggle_archived(&mut self) {
        self.show_archived = !self.show_archived;
        self.refresh_list();
    }

    /// Conversation `id` is archived, or back: the list hides an archived
    /// one unless archived ones show, and it is closed when it was open.
    pub fn set_archived(&mut self, id: &Id, archived: bool) {
        if let Some(row) = self.rows.iter_mut().find(|r| &r.id == id) {
            row.archived = archived;
            if archived {
                row.state = RowState::Closed;
            }
        }
        if archived {
            self.background_turns.retain(|(of, _)| of != id);
            // Its process is ended with them, saying nothing more.
            self.processes.remove(id);
            if self.active.as_ref() == Some(id) {
                self.active = None;
                self.clear_transcript();
            }
        }
        self.refresh_list();
    }

    /// The default model is now `model`.
    pub fn set_default_model(&mut self, model: &str) {
        self.default_model = model.to_string();
        self.touch();
    }

    /// The default model: the one a conversation with no model of its
    /// own uses.
    pub fn default_model(&self) -> &str {
        &self.default_model
    }

    /// What the open picker chooses: `default`, `conversation`,
    /// `template`, or `none` when it is closed.
    pub fn picking(&self) -> &'static str {
        match (&self.picker, self.picking) {
            (None, _) => "none",
            (Some(_), Picking::Default) => "default",
            (Some(_), Picking::Model) => "conversation",
            (Some(_), Picking::Template) => "template",
        }
    }

    /// The question before a deletion, while it is open.
    pub fn confirm(&self) -> Option<&Confirm> {
        self.confirm.as_ref()
    }

    /// Conversation `id` is deleted: its row goes, and the transcript with
    /// it when it was the one open.
    pub fn remove_row(&mut self, id: &Id) {
        self.rows.retain(|row| &row.id != id);
        self.background_turns.retain(|(of, _)| of != id);
        self.processes.remove(id);
        if self.active.as_ref() == Some(id) {
            self.active = None;
            self.clear_transcript();
        }
        self.refresh_list();
        self.touch();
    }

    /// The models the status row names, and their context lengths.
    pub fn set_models(&mut self, default: &str, contexts: Vec<(String, u64)>) {
        self.default_model = default.to_string();
        self.contexts = contexts;
        self.touch();
    }

    /// The key's credit as the status row says it.
    pub fn set_credit(&mut self, credit: Option<String>) {
        self.credit = credit;
        self.touch();
    }

    /// The spending limits the cost and today's total are shown against.
    pub fn set_limits(&mut self, limits: cost::Limits) {
        self.limits = limits;
        self.touch();
    }

    /// Where the key file is, which the key dialog names; `None` when
    /// there is nowhere to store one.
    pub fn set_key_path(&mut self, path: Option<String>) {
        self.key_path = path;
    }

    /// Whether there is a key; without one the status row says how to
    /// store one.
    pub fn set_keyed(&mut self, keyed: bool) {
        self.keyed = keyed;
        self.touch();
    }

    /// The key dialog, while it is open.
    pub fn dialog(&self) -> Option<&KeyDialog> {
        self.dialog.as_ref()
    }

    /// Whether the menu is open.
    pub fn menu_open(&self) -> bool {
        self.menu.is_open()
    }

    /// What today has spent across conversations.
    pub fn set_today(&mut self, today: u64) {
        if self.today != Some(today) {
            self.today = Some(today);
            self.touch();
        }
    }

    // --- what the session tells it ----------------------------------

    /// Keeps `message` for the Messages window, which shows it at once
    /// when open; the status row counts it until then.
    pub fn note(&mut self, message: impl Into<String>) {
        let at = crate::store::now();
        let text = self.log.push(at, message.into()).to_string();
        if let Some(panel) = self
            .notes
            .as_mut()
            .filter(|panel| panel.card_of().is_none())
        {
            self.log.read();
            panel.push(at, &text, &self.log);
        }
        self.touch();
    }

    /// The conversations the store holds, replacing the list's.
    pub fn set_rows(&mut self, rows: Vec<Row>) {
        self.rows = rows;
        self.refresh_list();
    }

    /// A conversation the session started, its process starting.
    pub fn add_row(&mut self, row: Row) {
        self.rows.retain(|r| r.id != row.id);
        self.rows.push(row);
        self.refresh_list();
    }

    /// Shows conversation `id`, its transcript empty until its process
    /// replays its log, and every other conversation closed.
    pub fn set_active(&mut self, id: Id) {
        // A modal opened over one conversation is not left over another.
        if self.picker.take().is_some() | self.confirm.take().is_some() {
            self.apply_focus();
        }
        // The conversation left goes on in the background while its turn
        // runs (its process is kept); the others keep what they showed.
        let running = self.turns.last().map(|(turn, _)| *turn).filter(|_| {
            self.active_row()
                .is_some_and(|r| r.state == RowState::Running)
        });
        if let (Some(left), Some(turn)) = (self.active.clone(), running) {
            if left != id {
                self.background_turns.retain(|(of, _)| *of != left);
                self.background_turns.push((left, turn));
            }
        }
        self.background_turns.retain(|(of, _)| *of != id);
        let background = &self.background_turns;
        for row in &mut self.rows {
            if row.id == id {
                row.state = RowState::Starting;
            } else if !background.iter().any(|(of, _)| *of == row.id)
                && row.state != RowState::Failed
            {
                row.state = RowState::Closed;
            }
        }
        self.active = Some(id);
        self.clear_transcript();
        self.refresh_list();
        // The selection follows the conversation opened.
        self.list.select(self.shown_active(), true);
        self.offer();
    }

    /// The cards still asked.
    pub fn cards(&self) -> &[Card] {
        &self.cards
    }

    /// Conversation `card.conversation` asks the human `card`: shown at
    /// once if it is the open conversation's and nothing else is modal,
    /// else when it is.
    pub fn ask(&mut self, card: Card) {
        // One shown for the same call is closed, so what the human
        // answers is the card they see.
        let shown = self.confirm.as_ref().is_some_and(|confirm| {
            confirm.purpose()
                == &confirm::Purpose::Approve {
                    conversation: card.conversation.clone(),
                    call: card.call,
                }
        });
        if shown {
            self.close_confirm();
        }
        self.cards
            .retain(|c| (&c.conversation, c.call) != (&card.conversation, card.call));
        self.cards.push(card);
        self.refresh_list();
        self.offer();
    }

    /// Conversation `conversation` withdrew its card for `call`, or every
    /// card it asked when `call` is none, as its process ended: its card
    /// is closed if shown.
    pub fn withdraw(&mut self, conversation: &Id, call: Option<u64>) {
        let gone = |c: &Card| &c.conversation == conversation && call.is_none_or(|n| n == c.call);
        if !self.cards.iter().any(gone) {
            return;
        }
        self.cards.retain(|c| !gone(c));
        let shown = self
            .confirm
            .as_ref()
            .is_some_and(|confirm| match confirm.purpose() {
                confirm::Purpose::Approve {
                    conversation: of,
                    call: n,
                } => of == conversation && call.is_none_or(|call| call == *n),
                confirm::Purpose::Delete(_)
                | confirm::Purpose::Admit { .. }
                | confirm::Purpose::Remove(_) => false,
            });
        if shown {
            self.close_confirm();
        }
        self.refresh_list();
        self.offer();
    }

    /// Shows the open conversation's first card when nothing else is
    /// modal and the window has the keyboard. A card that cannot be drawn,
    /// in a window too small, say, waits, and that is said once; the
    /// human can still interrupt the turn, which withdraws it.
    fn offer(&mut self) {
        if self.modal() || !self.focused || self.menu.is_open() {
            return;
        }
        if let Some((template, remotes)) = self.admission.take() {
            return self.ask_admission(template, remotes);
        }
        if let Some((id, title, lost, archive)) = self.removals.first().cloned() {
            self.cancel_pointer();
            self.press = None;
            self.confirm_revision = self.confirm_revision.wrapping_add(1);
            match Confirm::remove(
                self.surface,
                body(self.surface),
                id.clone(),
                &title,
                &lost,
                archive,
                self.confirm_revision,
            ) {
                Ok(confirm) => {
                    self.confirm = Some(confirm);
                    self.card_shown = Some(Instant::now());
                    self.removal_trouble = None;
                    self.apply_focus();
                    self.touch();
                }
                Err(e) => {
                    if self.removal_trouble.as_ref() != Some(&id) {
                        self.removal_trouble = Some(id);
                        self.note(format!(
                            "the loss card for {title:?} cannot be shown ({e}): make the window larger to answer it; until then it is neither archived, deleted nor opened"
                        ));
                    }
                }
            }
            return;
        }
        let Some(card) = self
            .active
            .as_ref()
            .and_then(|id| self.cards.iter().find(|c| &c.conversation == id))
            .cloned()
        else {
            return;
        };
        self.cancel_pointer();
        self.press = None;
        self.confirm_revision = self.confirm_revision.wrapping_add(1);
        match Confirm::approve(
            self.surface,
            body(self.surface),
            card.conversation.clone(),
            card.call,
            &card.title,
            &card.details,
            card.always.as_ref(),
            self.confirm_revision,
        ) {
            Ok(confirm) => {
                self.confirm = Some(confirm);
                self.card_shown = Some(Instant::now());
                self.card_trouble = None;
                self.apply_focus();
                self.touch();
            }
            Err(e) => {
                let trouble = Some((card.conversation, card.call));
                if self.card_trouble != trouble {
                    self.card_trouble = trouble;
                    self.note(format!(
                        "a card cannot be shown ({e}); make the window larger to decide it, or interrupt the turn"
                    ));
                }
            }
        }
    }

    /// The human's decision on a card, sent and the card forgotten: `act`
    /// its action, or none for Cancel.
    fn decide(&mut self, conversation: Id, call: u64, act: Option<confirm::Act>) {
        let always = self
            .cards
            .iter()
            .find(|c| (&c.conversation, c.call) == (&conversation, call))
            .and_then(|c| c.always.clone());
        let answer = match (act, always) {
            (None, _) => Answer::Once(false),
            (Some(confirm::Act::Confirm), _) => Answer::Once(true),
            (Some(act), Some(crate::rules::Offer::Rules(always))) => Answer::Always {
                allow: act == confirm::Act::AlwaysAllowHere && always.allow,
                everywhere: act == confirm::Act::AlwaysDenyEverywhere,
                bodies: always.bodies,
            },
            (Some(act), Some(crate::rules::Offer::Crossing { op, to })) => Answer::Crossing {
                allow: act == confirm::Act::AlwaysAllowHere,
                op,
                to,
            },
            // An "always" action on a card that offered none.
            (Some(act), None) => Answer::Once(act == confirm::Act::AlwaysAllowHere),
        };
        self.cards
            .retain(|c| (&c.conversation, c.call) != (&conversation, call));
        self.requests.push(Request::Decide {
            conversation,
            call,
            answer,
        });
        self.refresh_list();
    }

    fn clear_transcript(&mut self) {
        self.transcript.remove_first(self.transcript.len());
        self.messages.clear();
        self.turns.clear();
        self.last_seq = 0;
        self.meter = Meter::default();
        self.choice = (None, None);
        self.asked.clear();
        self.system_shown = false;
        self.streaming = None;
        self.undrawn = None;
        self.steps = crate::store::Steps::default();
        if !self.todo.is_empty() {
            self.todo.clear();
            self.place();
        }
        self.touch();
    }

    /// A running conversation opened again: its log as read from the
    /// store, its process's later events to follow.
    pub fn replay(&mut self, prefix: Result<&str, &str>, events: Vec<Event>) {
        self.clear_transcript();
        // Counted again from the log.
        if let Some(id) = self.active.clone() {
            self.processes.remove(&id);
        }
        self.system(prefix, false);
        for event in events {
            self.event(event);
        }
        // The replayed turns set the state: `running` while the last is
        // open, `idle` once it has ended (it may have ended between the
        // window's last poll and the opening).
        if let Some(row) = self.active_row() {
            if row.state == RowState::Starting {
                row.state = RowState::Idle;
            }
        }
        self.refresh_list();
    }

    /// What a conversation that is not open said: only its state and
    /// activity show, in its row; `now` is in milliseconds.
    pub fn background(&mut self, id: &Id, update: &Update, now: u64) {
        match update {
            Update::Up(Up::Event(event)) => {
                self.heard_process(id, &event.kind);
            }
            // Its log, replayed next, counts them again; a failed process
            // took them with it.
            Update::Up(Up::Hello { .. }) | Update::Failed { .. } => {
                self.processes.remove(id);
            }
            _ => {}
        }
        let Some(row) = self.rows.iter_mut().find(|r| &r.id == id) else {
            return;
        };
        let mut notice = None;
        let state = match update {
            Update::Up(Up::Event(Event {
                seq,
                kind:
                    Kind::Started {
                        effect: crate::store::Effect::Turn,
                        ..
                    },
                ..
            })) => {
                self.background_turns.retain(|(turn_of, _)| turn_of != id);
                self.background_turns.push((id.clone(), *seq));
                Some(RowState::Running)
            }
            // The turn's end, not one of its requests': its process is
            // let go, so the row is closed.
            Update::Up(Up::Event(Event {
                kind: Kind::Finished { started, .. } | Kind::Interrupted { started },
                ..
            })) => {
                row.activity = now;
                let at = self
                    .background_turns
                    .iter()
                    .position(|(turn_of, turn)| turn_of == id && turn == started);
                at.map(|at| {
                    self.background_turns.swap_remove(at);
                    RowState::Closed
                })
            }
            Update::Up(Up::Delivered { .. }) => {
                row.activity = now;
                None
            }
            Update::Up(Up::Event(Event {
                kind: Kind::Pause { paused },
                ..
            })) => {
                row.paused = *paused;
                None
            }
            // What td-agent tells the human there, such as a wake budget
            // spent, is said here too.
            Update::Up(Up::Event(Event {
                kind: Kind::Notice { text } | Kind::Notification { text },
                ..
            })) => {
                notice = Some(format!("{}: {text}", row.title));
                None
            }
            Update::Failed { .. } => Some(RowState::Failed),
            Update::Up(Up::Title { title }) => {
                row.title = crate::store::title(title);
                None
            }
            _ => None,
        };
        if let Some(state) = state {
            row.state = state;
        }
        self.refresh_list();
        if let Some(notice) = notice {
            self.note(notice);
        }
    }

    fn active_row(&mut self) -> Option<&mut Row> {
        let id = self.active.as_ref()?;
        self.rows.iter_mut().find(|r| &r.id == id)
    }

    /// What the open conversation's process said, or what became of it;
    /// `now` is in milliseconds.
    pub fn update(&mut self, update: Update, now: u64) {
        match update {
            Update::Up(Up::Hello {
                title,
                torn,
                interrupted,
                paused,
                prefix,
                ..
            }) => {
                self.clear_transcript();
                // Its log, replayed next, counts its processes again.
                if let Some(id) = self.active.clone() {
                    self.processes.remove(&id);
                }
                self.system(prefix.as_deref().ok_or(LONG_PREFIX), false);
                if let Some(row) = self.active_row() {
                    row.title = crate::store::title(&title);
                    row.state = RowState::Idle;
                    row.paused = paused;
                }
                if let Some(bytes) = torn {
                    self.note(format!("dropped a torn final log line of {bytes} bytes"));
                }
                if !interrupted.is_empty() {
                    self.note(format!(
                        "a restart interrupted {} turn(s), request(s) or tool call(s); none is repeated",
                        interrupted.len()
                    ));
                }
                self.refresh_list();
            }
            Update::Up(Up::Event(event)) => self.event(event),
            // The window hands cards to `ask` and `withdraw`.
            Update::Up(Up::Ask { .. } | Up::Withdraw { .. }) => {}
            Update::Up(Up::Delivered { .. }) => {
                if let Some(row) = self.active_row() {
                    row.activity = now;
                }
                self.refresh_list();
            }
            Update::Up(Up::Title { title }) => {
                if let Some(row) = self.active_row() {
                    row.title = crate::store::title(&title);
                }
                self.refresh_list();
            }
            Update::Refused {
                text: Some(text),
                reason,
            } => self.restore(&text, format!("refused: {reason}")),
            Update::Refused { text: None, reason } | Update::Up(Up::Refused { reason, .. }) => {
                self.note(format!("refused: {reason}"))
            }
            // The window's ledger and post answer these; nothing shows.
            Update::Up(
                Up::Reserve { .. }
                | Up::Spent { .. }
                | Up::Send { .. }
                | Up::Query { .. }
                | Up::Fetch { .. }
                | Up::Heads { .. }
                | Up::Prepared { .. }
                | Up::Restored
                | Up::Waking,
            )
            | Update::Undeliverable { .. } => {}
            Update::Up(Up::Delta {
                request,
                reasoning,
                content,
            }) => self.delta(request, &reasoning, &content),
            Update::Restarting { reason } => {
                self.stream_died();
                if let Some(row) = self.active_row() {
                    row.state = RowState::Restarting;
                }
                self.note(format!(
                    "the conversation's process failed ({reason}); restarting it from its log"
                ));
                self.refresh_list();
            }
            Update::Failed { reason } => {
                self.stream_died();
                // Its background processes went with its process.
                if let Some(id) = self.active.clone() {
                    self.processes.remove(&id);
                }
                if let Some(row) = self.active_row() {
                    row.state = RowState::Failed;
                }
                self.note(format!(
                    "the conversation's process failed ({reason}); open it again to retry"
                ));
                self.refresh_list();
            }
        }
    }

    /// One log event into the transcript.
    fn event(&mut self, event: Event) {
        if event.seq <= self.last_seq {
            return;
        }
        self.last_seq = event.seq;
        self.steps.apply(&event);
        if let Some(id) = self.active.clone() {
            if self.heard_process(&id, &event.kind) {
                self.refresh_list();
            }
        }
        // From the end: a turn's records follow its message closely, so a
        // replay finds each at once rather than scanning the whole log.
        let message_of = |messages: &[(u64, usize)], turns: &[(u64, u64)], started: u64| {
            let user = turns.iter().rev().find(|(s, _)| *s == started)?.1;
            messages
                .iter()
                .rev()
                .find(|(seq, _)| *seq == user)
                .map(|m| m.1)
        };
        match event.kind {
            Kind::User { text, .. } => {
                let pushed = Message::new("you")
                    .and_then(|m| m.text(&text))
                    .map_err(|e| e.to_string())
                    .and_then(|m| self.push_message(m));
                match pushed {
                    Ok(index) => self.messages.push((event.seq, index)),
                    Err(e) => self.note(format!("the transcript refused a message: {e}")),
                }
            }
            Kind::Started { of, .. } => {
                self.turns.push((event.seq, of));
                self.meter.retry = false;
                if let Some(row) = self.active_row() {
                    row.state = RowState::Running;
                }
                self.refresh_list();
                if let Some(index) = message_of(&self.messages, &self.turns, event.seq) {
                    let _ = self
                        .transcript
                        .set_status(index, Some(("running", Tone::Neutral)));
                }
            }
            Kind::Finished {
                started,
                outcome,
                retry,
            } => {
                if !self.turns.iter().any(|(turn, _)| *turn == started) {
                    // A request's finish: its turn's says how it went. A
                    // reply it was streaming that never reached the log
                    // stays as drawn, marked.
                    if let Some(streaming) = self.streaming.take_if(|s| s.request == started) {
                        let _ = self
                            .transcript
                            .set_status(streaming.index, Some(("not logged", Tone::Bad)));
                    }
                    return self.touch();
                }
                self.meter.retry = retry;
                if let Some(row) = self.active_row() {
                    if row.state == RowState::Running {
                        row.state = RowState::Idle;
                    }
                }
                self.refresh_list();
                let replied = outcome.starts_with("replied");
                // "no model" is how increment 4's turns ended.
                let status = if replied {
                    ("replied", Tone::Good)
                } else if outcome == "no model" {
                    ("no model", Tone::Neutral)
                } else {
                    ("not answered", Tone::Bad)
                };
                if let Some(index) = message_of(&self.messages, &self.turns, started) {
                    let _ = self.transcript.set_status(index, Some(status));
                }
                if outcome != "replied" && outcome != "no model" {
                    let text = if retry {
                        format!("{outcome}\nC-r asks again.")
                    } else {
                        outcome
                    };
                    let pushed = Message::new("td-agent")
                        .and_then(|m| m.status("turn", Tone::Neutral))
                        .and_then(|m| m.text(&text))
                        .map_err(|e| e.to_string())
                        .and_then(|m| self.push_message(m));
                    if let Err(e) = pushed {
                        self.note(format!("the transcript refused a notice: {e}"));
                    }
                }
            }
            Kind::Interrupted { started } => {
                let charged = self.meter.charged.contains(&started);
                if let Some((_, reserved)) = self.meter.request(started).filter(|_| !charged) {
                    self.meter.spent = self.meter.spent.saturating_add(reserved);
                }
                if self.turns.iter().any(|(turn, _)| *turn == started) {
                    if let Some(row) = self.active_row() {
                        if row.state == RowState::Running {
                            row.state = RowState::Idle;
                        }
                    }
                    self.refresh_list();
                }
                if let Some(index) = message_of(&self.messages, &self.turns, started) {
                    let _ = self
                        .transcript
                        .set_status(index, Some(("interrupted", Tone::Bad)));
                }
            }
            Kind::Request {
                purpose, reserved, ..
            } => self.meter.requests.push((event.seq, purpose, reserved)),
            Kind::Assistant {
                request,
                content,
                reasoning,
                finish,
                incomplete,
                calls,
                ..
            } => {
                let reasoning = reasoning.filter(|r| !r.trim().is_empty());
                let text = content.unwrap_or_default();
                let text = match (text.is_empty(), calls.is_empty()) {
                    (true, true) => "(no text)",
                    (true, false) => "(tool calls)",
                    _ => &text,
                };
                let called = calls
                    .iter()
                    .map(|c| format!("{}({})", c.name, c.arguments))
                    .collect::<Vec<String>>()
                    .join("\n");
                let verdict = if incomplete {
                    Some(("incomplete", Tone::Bad))
                } else if finish == "stop" {
                    None
                } else {
                    Some((finish.as_str(), Tone::Neutral))
                };
                let mut message = Message::new("assistant");
                if let Some(reasoning) = &reasoning {
                    message = message.and_then(|m| m.section("reasoning", reasoning, true));
                }
                let message = message
                    .and_then(|m| m.text(text))
                    .and_then(|m| match called.is_empty() {
                        true => Ok(m),
                        false => m.excerpt("tool calls", &called),
                    })
                    .and_then(|m| match verdict {
                        Some((verdict, tone)) => m.verdict(verdict, tone),
                        None => Ok(m),
                    });
                if self.undrawn == Some(request) {
                    self.undrawn = None;
                }
                let shown = match message.map_err(|e| e.to_string()) {
                    Err(e) => Err(e),
                    Ok(message) => match self.streaming.filter(|s| s.request == request) {
                        Some(_) => {
                            // The calls are a section of their own, which a
                            // stream never draws: a reply with calls is
                            // always drawn again whole.
                            let sections: Vec<&str> = reasoning
                                .as_deref()
                                .into_iter()
                                .chain([text])
                                .chain((!called.is_empty()).then_some(called.as_str()))
                                .collect();
                            self.settle_stream(&sections, verdict, message)
                        }
                        None => self.push_message(message),
                    },
                };
                match shown {
                    Ok(index) => self.meter.replies.push((request, index)),
                    Err(e) => self.note(format!("the transcript refused a reply: {e}")),
                }
            }
            Kind::Usage {
                request,
                tokens: counts,
                cost,
                ..
            } => {
                self.meter.charged.push(request);
                self.meter.spent = self.meter.spent.saturating_add(cost);
                if self.meter.request(request).map(|(p, _)| p) == Some(Purpose::Turn)
                    && counts.prompt > 0
                {
                    self.meter.context = Some(counts.prompt.saturating_add(counts.completion));
                }
                let index = self
                    .meter
                    .replies
                    .iter()
                    .rev()
                    .find(|(r, _)| *r == request)
                    .map(|(_, index)| *index);
                if let Some(index) = index {
                    let status = format!(
                        "in {} (cached {}, written {}) out {} {}",
                        tokens(counts.prompt),
                        tokens(counts.cached),
                        tokens(counts.cache_write),
                        tokens(counts.completion),
                        cost::show(cost)
                    );
                    let _ = self
                        .transcript
                        .set_status(index, Some((&status, Tone::Neutral)));
                }
            }
            Kind::Prefix { text } => self.system(Ok(&text), self.system_shown),
            Kind::Title { .. } | Kind::ToolCall { .. } => {}
            Kind::Notice { text } | Kind::Notification { text } => self.notice_message(&text),
            Kind::Message {
                from,
                role,
                text,
                status,
                held,
                ..
            } => {
                let header = match (role, &status) {
                    (Role::Orchestrator, _) => "the orchestrator".to_string(),
                    (Role::Conversation, None) => format!("conversation {}", self.named(&from)),
                    (Role::Conversation, Some(status)) => {
                        format!("report ({status}) from {}", self.named(&from))
                    }
                };
                let held = held.map(|held| match held {
                    Held::Paused => "held: paused",
                    Held::Budget => "held: wake budget",
                });
                let pushed = Message::new(&header)
                    .and_then(|m| m.text(&text))
                    .and_then(|m| match held {
                        Some(held) => m.verdict(held, Tone::Neutral),
                        None => Ok(m),
                    })
                    .map_err(|e| e.to_string())
                    .and_then(|m| self.push_message(m));
                match pushed {
                    Ok(index) => self.messages.push((event.seq, index)),
                    Err(e) => self.note(format!("the transcript refused a message: {e}")),
                }
            }
            Kind::ToolResult {
                name,
                content,
                error,
                ..
            } => {
                let source: std::sync::Arc<str> = std::sync::Arc::from(content.as_str());
                let mut message = Message::new(&format!("tool {name}"))
                    .and_then(|m| m.excerpt("result", &content))
                    .and_then(|m| m.source(source));
                if error {
                    message = message.and_then(|m| m.verdict("error", Tone::Bad));
                }
                let pushed = message
                    .map_err(|e| e.to_string())
                    .and_then(|m| self.push_message(m));
                if let Err(e) = pushed {
                    self.note(format!("the transcript refused a tool result: {e}"));
                }
            }
            Kind::Snapshot {
                worktrees,
                background,
                ..
            } => {
                let mut lines: Vec<String> = worktrees
                    .iter()
                    .map(crate::history::snapshot_line)
                    .collect();
                if !background.is_empty() {
                    lines.push(crate::history::running_line(&background));
                }
                self.notice_message(&format!(
                    "this step changed {}; C-z undoes it",
                    lines.join("; ")
                ));
            }
            Kind::Restore { step, undo } => self.notice_message(&format!(
                "you {} the step snapshotted at #{step}",
                if undo { "undid" } else { "redid" }
            )),
            // The `shell` call's result says it started.
            Kind::Process { .. } => {}
            Kind::Ended {
                number, how, held, ..
            } => self.notice_message(&format!(
                "background process p{number} ended: {how}{}",
                match held {
                    Some(Held::Paused) => "; held: paused",
                    Some(Held::Budget) => "; held: wake budget",
                    None => "",
                }
            )),
            Kind::Todo { items, cleared } => {
                if cleared {
                    self.notice_message("you cleared the todo list");
                }
                let rows = self.todo_rows();
                self.todo = items;
                if self.todo_rows() != rows {
                    self.place();
                }
            }
            Kind::Choice { model, effort } => {
                self.choice = (model, effort);
                // The process logs choices in the order they were asked.
                if self.asked.front() == Some(&self.choice) {
                    self.asked.pop_front();
                }
                let effort = if self.reasoning(self.model()) {
                    format!("reasoning effort {}", self.effort())
                } else {
                    "no reasoning".to_string()
                };
                let said = format!("model {}, {effort}, from the next request", self.model());
                self.notice_message(&said);
                self.touch();
            }
            Kind::Pause { paused } => {
                if let Some(row) = self.active_row() {
                    row.paused = paused;
                }
                self.refresh_list();
                self.notice_message(if paused {
                    "paused: messages from other conversations wait, held, until you resume it (C-S-p) or write here"
                } else {
                    "resumed"
                });
            }
            Kind::Approval { outcome, by, .. } => {
                self.notice_message(&format!("approval: {outcome}, decided by {by}"));
            }
        }
        self.touch();
    }

    /// The system context's message (DESIGN.md §4): the prefix in force
    /// from here, folded, or a notice of why the window has none.
    fn system(&mut self, prefix: Result<&str, &str>, replaced: bool) {
        let prefix = match prefix {
            Ok(prefix) => prefix,
            Err(why) => {
                self.system_shown = true;
                return self.notice_message(&format!("the system context is not shown: {why}"));
            }
        };
        let pushed = crate::system::message(prefix, replaced).and_then(|message| match message {
            Some(message) => self.push_message(message).map(|_| self.system_shown = true),
            None => Ok(()),
        });
        if let Err(e) = pushed {
            self.note(format!("the transcript refused the system context: {e}"));
        }
    }

    /// A notice of td-agent's in the transcript.
    fn notice_message(&mut self, text: &str) {
        let pushed = Message::new("td-agent")
            .and_then(|m| m.status("notice", Tone::Neutral))
            .and_then(|m| m.text(text))
            .map_err(|e| e.to_string())
            .and_then(|m| self.push_message(m));
        if let Err(e) = pushed {
            self.note(format!("the transcript refused a notice: {e}"));
        }
    }

    /// Conversation `id` as a message's header names it: its title, short,
    /// and the start of its id.
    fn named(&self, id: &Id) -> String {
        let short: String = id.as_str().chars().take(8).collect();
        match self.rows.iter().find(|r| &r.id == id) {
            Some(row) => {
                let title: String = row.title.chars().take(48).collect();
                format!("{title} ({short})")
            }
            None => short,
        }
    }

    /// The rows the todo list takes.
    fn todo_rows(&self) -> usize {
        match (self.todo.is_empty(), self.todo_open) {
            (true, _) => 0,
            (false, false) => 1,
            (false, true) => self.todo.len().min(TODO_ROWS),
        }
    }

    /// The todo list's lines as drawn: collapsed, the item in progress,
    /// or the first not done; open, every item that fits.
    pub fn todo_lines(&self) -> Vec<String> {
        let line = |item: &TodoItem| format!("{} {}", tools::mark(item.status), item.content);
        if self.todo.is_empty() {
            return Vec::new();
        }
        if !self.todo_open {
            let done = self
                .todo
                .iter()
                .filter(|i| i.status == Status::Done)
                .count();
            let current = self
                .todo
                .iter()
                .find(|i| i.status == Status::InProgress)
                .or_else(|| self.todo.iter().find(|i| i.status == Status::Pending))
                .map_or_else(|| "all done".to_string(), line);
            return vec![format!(
                "todo {done}/{}: {current} | C-t shows all, C-S-t clears",
                self.todo.len()
            )];
        }
        let shown = self.todo_rows();
        let mut lines: Vec<String> = self.todo.iter().take(shown).map(line).collect();
        if self.todo.len() > shown {
            if let Some(last) = lines.last_mut() {
                *last = format!("\u{2026} {} more", self.todo.len() - shown + 1);
            }
        }
        lines
    }

    /// Appends a message to the transcript. The list's bounds are smaller
    /// than a log's, so when one refuses it the oldest messages go, an
    /// eighth at a time, and the transcript keeps the most recent; the
    /// log still holds every one.
    fn push_message(&mut self, message: Message) -> Result<usize, String> {
        loop {
            match self.transcript.push(message.clone()) {
                Ok(index) => return Ok(index),
                Err(e) if self.transcript.is_empty() || e != MessagesError::Limit => {
                    return Err(e.to_string())
                }
                Err(_) => self.evict(),
            }
        }
    }

    /// Drops the transcript's oldest eighth, keeping the indices its
    /// turns, replies and a streaming reply find their messages by.
    fn evict(&mut self) {
        let count = (self.transcript.len() / 8).max(1);
        self.transcript.remove_first(count);
        for list in [&mut self.messages, &mut self.meter.replies] {
            list.retain(|(_, index)| *index >= count);
            for (_, index) in list.iter_mut() {
                *index = index.saturating_sub(count);
            }
        }
        self.streaming = self
            .streaming
            .filter(|s| s.index >= count)
            .map(|s| Streaming {
                index: s.index - count,
                ..s
            });
        self.note("the transcript shows the most recent messages; the log keeps every one");
    }

    /// What a streaming reply brought since the last, drawn into its
    /// message, which the request's first delta starts (DESIGN.md §4).
    /// Once the transcript cannot draw it, the rest waits for the log.
    fn delta(&mut self, request: u64, reasoning: &str, content: &str) {
        if (reasoning.is_empty() && content.is_empty()) || self.undrawn == Some(request) {
            return;
        }
        let drawn = match self.streaming.filter(|s| s.request == request) {
            None => self.start_stream(request, reasoning, content),
            Some(streaming) => self.extend_stream(streaming, reasoning, content),
        };
        if let Err(e) = drawn {
            self.undrawn = Some(request);
            if let Some(streaming) = self.streaming.filter(|s| s.request == request) {
                let _ = self
                    .transcript
                    .set_status(streaming.index, Some(("cut short", Tone::Bad)));
            }
            self.note(format!(
                "the transcript cannot draw the reply as it comes ({e}); it shows it once whole"
            ));
        }
        self.touch();
    }

    /// The process died mid-stream: what was drawn stays, marked. The log
    /// never holds it; a restart shows the request interrupted.
    fn stream_died(&mut self) {
        if let Some(streaming) = self.streaming.take() {
            let _ = self
                .transcript
                .set_status(streaming.index, Some(("interrupted", Tone::Bad)));
        }
        self.undrawn = None;
    }

    /// `edit` on the streaming message, room made as `push_message` makes
    /// it when the transcript is at its limit; the message's index after.
    fn edit_stream(
        &mut self,
        mut edit: impl FnMut(&mut messages::Controller, usize) -> Result<(), MessagesError>,
    ) -> Result<usize, String> {
        loop {
            let Some(index) = self.streaming.map(|s| s.index) else {
                return Err("its message was dropped from the transcript".into());
            };
            match edit(&mut self.transcript, index) {
                Ok(()) => return Ok(index),
                Err(e) if self.transcript.len() <= 1 || e != MessagesError::Limit => {
                    return Err(e.to_string())
                }
                Err(_) => self.evict(),
            }
        }
    }

    /// A streaming message: its reasoning section, when there is
    /// reasoning, then its text.
    fn streaming_message(reasoning: Option<&str>, text: &str) -> Result<Message, String> {
        let mut message =
            Message::new("assistant").and_then(|m| m.status("streaming", Tone::Neutral));
        if let Some(reasoning) = reasoning {
            message = message.and_then(|m| m.section("reasoning", reasoning, true));
        }
        message
            .and_then(|m| m.text(text))
            .map_err(|e| e.to_string())
    }

    fn start_stream(&mut self, request: u64, reasoning: &str, content: &str) -> Result<(), String> {
        let has = !reasoning.is_empty();
        let message = Self::streaming_message(has.then_some(reasoning), content)?;
        let index = self.push_message(message)?;
        self.streaming = Some(Streaming {
            request,
            index,
            reasoning: has,
        });
        Ok(())
    }

    fn extend_stream(
        &mut self,
        mut streaming: Streaming,
        reasoning: &str,
        content: &str,
    ) -> Result<(), String> {
        if !reasoning.is_empty() {
            if streaming.reasoning {
                self.edit_stream(|list, index| list.append(index, 0, reasoning))?;
            } else {
                // Reasoning after text: drawn again with its section first.
                let text = self
                    .transcript
                    .message(streaming.index)
                    .and_then(|m| m.section_text(0))
                    .unwrap_or_default()
                    .to_string();
                let message = Self::streaming_message(Some(reasoning), &text)?;
                self.edit_stream(|list, index| list.replace(index, message.clone()))?;
                if let Some(drawn) = self.streaming.as_mut() {
                    drawn.reasoning = true;
                }
                streaming.reasoning = true;
            }
        }
        if !content.is_empty() {
            let section = usize::from(streaming.reasoning);
            self.edit_stream(|list, index| list.append(index, section, content))?;
        }
        Ok(())
    }

    /// A streamed reply's message once the reply is logged: kept as drawn,
    /// its reasoning left open or closed as the human left it, when its
    /// sections are the logged reply's; else the logged reply replaces it,
    /// as when the window opened the conversation mid-stream or could not
    /// draw it all. A reply the transcript cannot hold stays as drawn,
    /// marked cut short.
    fn settle_stream(
        &mut self,
        sections: &[&str],
        verdict: Option<(&str, Tone)>,
        logged: Message,
    ) -> Result<usize, String> {
        let same = self.streaming.is_some_and(|streaming| {
            self.transcript
                .message(streaming.index)
                .is_some_and(|drawn| {
                    drawn.sections() == sections.len()
                        && sections
                            .iter()
                            .enumerate()
                            .all(|(at, text)| drawn.section_text(at) == Some(*text))
                })
        });
        let settled = if same {
            self.edit_stream(|list, index| {
                list.set_status(index, None)
                    .and_then(|()| list.set_verdict(index, verdict))
            })
        } else {
            self.edit_stream(|list, index| list.replace(index, logged.clone()))
        };
        let Some(streaming) = self.streaming.take() else {
            // Its message was evicted to make room: the reply is pushed.
            return self.push_message(logged);
        };
        if let Err(e) = settled {
            // The verdict, which the usage that follows leaves alone.
            let _ = self
                .transcript
                .set_status(streaming.index, None)
                .and_then(|()| {
                    self.transcript
                        .set_verdict(streaming.index, Some(("cut short", Tone::Bad)))
                });
            self.note(format!(
                "the transcript could not show the whole reply: {e}"
            ));
        }
        Ok(streaming.index)
    }

    /// Orders the rows, the most recently active first, and gives the
    /// list a model of them. The selected row stays selected while it is
    /// shown, so an update does not move the keyboard's place; else the
    /// open conversation is.
    fn refresh_list(&mut self) {
        let kept_process = self
            .list
            .selected()
            .and_then(|index| self.process_item(index))
            .cloned();
        let kept = self
            .list
            .selected()
            .filter(|index| *index < self.process_base)
            .and_then(|index| self.rows.get(index))
            .map(|row| row.id.clone());
        self.rows
            .sort_by(|a, b| b.activity.cmp(&a.activity).then(a.id.cmp(&b.id)));
        self.labels = self
            .rows
            .iter()
            .map(|row| match &row.workspace {
                None => "none".to_string(),
                // The folder's own name, which the column has room for;
                // the deletion question names it whole.
                Some(Workspace::Directory(path)) => path.file_name().map_or_else(
                    || path.display().to_string(),
                    |name| name.to_string_lossy().into_owned(),
                ),
                Some(workspace) => workspace.label(),
            })
            .collect();
        let show = self.show_archived;
        // Each conversation's background processes under it, always shown.
        let mut tree: Vec<TreeRow<usize>> = Vec::new();
        let mut items: Vec<(Id, u64)> = Vec::new();
        let mut labels: Vec<String> = Vec::new();
        for (index, row) in self.rows.iter().enumerate() {
            if !show && row.archived {
                continue;
            }
            let running = self.processes.get(&row.id).filter(|p| !p.is_empty());
            tree.push(TreeRow {
                id: index,
                parent: None,
                depth: 0,
                children: running.is_some(),
                expanded: running.is_some(),
            });
            for (number, command) in running.into_iter().flatten() {
                let cut: String = command.chars().take(MAX_PROCESS_LABEL).collect();
                labels.push(format!("p{number} {}", crate::tools::visible(&cut)));
                tree.push(TreeRow {
                    id: self.rows.len() + items.len(),
                    parent: Some(index),
                    depth: 1,
                    children: false,
                    expanded: false,
                });
                items.push((row.id.clone(), *number));
            }
        }
        self.process_items = items;
        self.process_labels = labels;
        self.process_base = self.rows.len();
        match Model::new(&tree, COLUMNS) {
            Ok(model) => {
                if let Err(e) = self.list.replace(model) {
                    self.note(format!("the list: {e}"));
                }
            }
            Err(e) => self.note(format!("the list: {e}")),
        }
        let selected = kept_process
            .as_ref()
            .and_then(|kept| {
                self.process_items
                    .iter()
                    .position(|item| item == kept)
                    .map(|n| self.process_base + n)
            })
            // A process that ended leaves its conversation selected.
            .or_else(|| {
                kept.or(kept_process.map(|(id, _)| id)).and_then(|id| {
                    self.rows
                        .iter()
                        .position(|r| r.id == id && (show || !r.archived))
                })
            })
            .or_else(|| self.shown_active());
        self.list.select(selected, true);
        self.touch();
    }

    /// The list's row `index` activated: its conversation opened, or its
    /// background process's output shown.
    fn activate(&mut self, index: usize) {
        if let Some((id, number)) = self.process_item(index).cloned() {
            self.cancel_pointer();
            return self.requests.push(Request::Output { id, number });
        }
        if let Some(row) = self.rows.get(index) {
            let id = row.id.clone();
            self.open(id);
        }
    }

    /// The open conversation's index in `rows`, when the list shows it.
    fn shown_active(&self) -> Option<usize> {
        self.active.as_ref().and_then(|id| {
            self.rows
                .iter()
                .position(|r| &r.id == id && (self.show_archived || !r.archived))
        })
    }

    // --- input ------------------------------------------------------

    /// A clock tick: the composer's caret blinks by it.
    pub fn tick(&mut self, now: u64) {
        self.clock = now;
        if self.composer.event(PaneEvent::Tick(now)) == PaneOutcome::Changed {
            self.touch();
        }
    }

    /// The surface changed size.
    pub fn resize(&mut self, surface: Surface) {
        self.surface = surface;
        self.capture = None;
        self.layout();
    }

    fn layout(&mut self) {
        let _ = self.split.event(split::Event::Resize {
            surface: self.surface,
            rect: body(self.surface),
        });
        let _ = self.menu.event(
            Some(self.menu_revision),
            td_ui::menus::Event::Resize(self.surface),
        );
        if let Some(picker) = self.picker.as_mut() {
            if !picker.resize(self.surface, body(self.surface)) {
                self.close_picker();
                self.note("the window is now too small for the picker, which is closed");
            }
        }
        // Too small for its list, it stays open and says so.
        if let Some(notes) = self.notes.as_mut() {
            notes.resize(self.surface, body(self.surface));
        }
        if let Some(chooser) = self.chooser.as_mut() {
            if !chooser.resize(self.surface, body(self.surface)) {
                self.close_chooser();
                self.note("the window is now too small for the directory chooser, which is closed");
            }
        }
        if let Some(confirm) = self.confirm.as_mut() {
            if !confirm.resize(self.surface, body(self.surface)) {
                let said = match confirm.purpose() {
                    confirm::Purpose::Admit { template, .. } => format!(
                        "the window is now too small for the admission card, which is closed: no conversation from template {template:?} was made"
                    ),
                    // A tool's card stays the conversation's, shown again
                    // when there is room (`offer`).
                    confirm::Purpose::Approve { .. } => "the window is now too small for the card, which waits until there is room for it: nothing was decided".to_string(),
                    confirm::Purpose::Delete(_) => "the window is now too small for the question, which is closed: nothing was deleted".to_string(),
                    // Asked again when there is room (`offer`), which says
                    // nothing more meanwhile.
                    confirm::Purpose::Remove(id) => {
                        self.removal_trouble = Some(id.clone());
                        "the window is now too small for the loss card, which waits until there is room for it: nothing was removed".to_string()
                    }
                };
                self.close_confirm();
                self.note(said);
            }
        }
        if let Some(dialog) = self.dialog.as_mut() {
            if !dialog.resize(self.surface) {
                self.close_dialog();
                self.note("the window is now too small for the key dialog, which is closed");
            }
        }
        self.place();
    }

    /// The panes where the split's layout puts them. A drag of the
    /// divider comes here alone: a resize of the split would end it.
    fn place(&mut self) {
        let todo_rows = self.todo_rows();
        self.regions = self.split.layout().map(|layout| {
            let s = self.surface.scale.value();
            let right = layout.second;
            let status_height = ((ROW * s) as u32).min(right.height);
            let rest = right.height - status_height;
            let wanted = (COMPOSER_ROWS * CELL_HEIGHT * s + 2 * s) as u32;
            let composer_height = wanted.min(rest / 2);
            let rule_height = (s as u32).min(rest - composer_height);
            let above = rest - composer_height - rule_height;
            let todo_height = ((todo_rows * ROW * s) as u32).min(above / 2);
            let transcript_height = above - todo_height;
            let at = |y: u32, height: u32| Rect {
                x: right.x,
                y: right.y + i64::from(y),
                width: right.width,
                height,
            };
            Regions {
                list: layout.first,
                transcript: at(0, transcript_height),
                todo: at(transcript_height, todo_height),
                rule: at(above, rule_height),
                composer: at(above + rule_height, composer_height),
                status: at(rest, status_height),
            }
        });
        if let Some(regions) = self.regions {
            if let Err(e) = self.list.resize(self.surface, regions.list) {
                self.note(format!("the list: {e}"));
            }
            // A narrower transcript wraps to more rows than its bound
            // may hold: the oldest messages go until it fits.
            loop {
                match self.transcript.resize(self.surface, regions.transcript) {
                    Err(MessagesError::Limit) if !self.transcript.is_empty() => self.evict(),
                    Err(e) => {
                        self.note(format!("the transcript: {e}"));
                        break;
                    }
                    Ok(()) => break,
                }
            }
            self.composer.place(regions.composer, self.surface);
        }
        self.touch();
    }

    fn set_focus(&mut self, focus: Focus) {
        self.focus = focus;
        self.apply_focus();
    }

    /// Hands the keyboard focus to the focused widget, and takes it from
    /// the others, so exactly one shows a focused caret or selection.
    fn apply_focus(&mut self) {
        // A modal dialog has the keyboard: no widget under it shows focus.
        let on = |f: Focus| {
            self.focused
                && self.dialog.is_none()
                && self.picker.is_none()
                && self.notes.is_none()
                && self.chooser.is_none()
                && self.confirm.is_none()
                && self.focus == f
        };
        let list_focus = if on(Focus::List) {
            tree_table::Focus::Rows
        } else {
            tree_table::Focus::None
        };
        let (list, transcript, composer) = (list_focus, on(Focus::Transcript), on(Focus::Composer));
        let _ = self.list.set_focus(list);
        let notes_focus = self.focused && self.dialog.is_none();
        if let Some(notes) = self.notes.as_mut() {
            notes.focus(notes_focus);
        }
        self.transcript.event(
            messages::Event::Focus(transcript),
            &mut td_ui::window::NoClipboard,
        );
        self.composer.event(PaneEvent::Focus(composer));
        self.touch();
    }

    /// The conversation `step` rows from the active one, opened.
    /// Archived conversations are passed over.
    fn switch(&mut self, step: isize) {
        let live: Vec<&Id> = self
            .rows
            .iter()
            .filter(|r| !r.archived && !self.closing(&r.id))
            .map(|r| &r.id)
            .collect();
        let at = self
            .active
            .as_ref()
            .and_then(|id| live.iter().position(|r| *r == id));
        let next = match at {
            Some(at) => at.checked_add_signed(step),
            None => Some(0),
        };
        if let Some(id) = next.and_then(|i| live.get(i)).map(|id| (*id).clone()) {
            self.open(id);
        }
    }

    /// Opens conversation `id`; opening the one already open again
    /// retries it when its process failed.
    fn open(&mut self, id: Id) {
        if let Some(row) = self.rows.iter().find(|r| r.id == id && r.archived) {
            let said = format!(
                "{:?} is archived: Unarchive in its row's menu opens it again",
                row.title
            );
            // The list's selection goes back to the open one.
            self.list.select(self.shown_active(), false);
            self.touch();
            return self.note(said);
        }
        if let Some((row, doing)) = self
            .rows
            .iter()
            .find(|r| r.id == id)
            .zip(self.closing_word(&id))
        {
            let said = format!(
                "{:?} does not open while td-agent is {doing} it: its workspace is being asked what removing it would lose",
                row.title
            );
            self.list.select(self.shown_active(), false);
            self.touch();
            return self.note(said);
        }
        if self.active.as_ref() != Some(&id) || self.active_failed() {
            self.set_active(id.clone());
            self.requests.push(Request::Open(id));
        }
    }

    /// Asks the open conversation to undo its latest step, or redo the
    /// latest undone, naming it, between turns (DESIGN.md §12).
    fn restore_step(&mut self, undo: bool) {
        if self.active.is_none() {
            return;
        }
        if self.running() {
            return self.note("a step is undone or redone between turns");
        }
        let step = if undo {
            self.steps.undo()
        } else {
            self.steps.redo()
        };
        match step {
            Some(step) if undo => self.requests.push(Request::Undo(step)),
            Some(step) => self.requests.push(Request::Redo(step)),
            None if undo => self.note("no step to undo"),
            None => self.note("no step to redo"),
        }
    }

    /// Whether the open conversation is running a turn.
    fn running(&self) -> bool {
        self.active.as_ref().is_some_and(|active| {
            self.rows
                .iter()
                .any(|row| &row.id == active && row.state == RowState::Running)
        })
    }

    fn active_failed(&self) -> bool {
        self.active.as_ref().is_some_and(|active| {
            self.rows
                .iter()
                .any(|row| &row.id == active && row.state == RowState::Failed)
        })
    }

    /// A message the window could not hand on, back in the composer,
    /// empty or not, so nothing typed is lost.
    pub fn restore(&mut self, text: &str, why: String) {
        match self.composer.append(text) {
            Ok(true) => self.note(format!("{why}; the message is back in the composer")),
            refused => {
                // Not lost without a word: the whole message goes to
                // standard error, which the window always has.
                let e = refused
                    .err()
                    .unwrap_or_else(|| "the composer took nothing".into());
                eprintln!("td-agent: {why}; a message could not be put back ({e}):\n{text}");
                self.note(format!(
                    "{why}; the message could not be put back ({e}) and is on standard error"
                ));
            }
        }
    }

    /// Sends the composer's text, when there is any, and empties it.
    fn send(&mut self) {
        let text = self.composer.text().to_string();
        if text.trim().is_empty() {
            return;
        }
        if text.len() > MAX_TEXT {
            self.note(format!(
                "a message is at most {} KiB; this one is {} bytes",
                MAX_TEXT / 1024,
                text.len()
            ));
            return;
        }
        if self.active_failed() {
            self.note(
                "the conversation's process failed; open it again (Return on its row) to retry",
            );
            return;
        }
        self.composer.fresh();
        self.apply_focus();
        self.requests.push(Request::Send(text));
        self.touch();
    }

    /// One input from td-ui's window, the live pointer's or the physical
    /// keyboard's, as `input`; true when it chose Help → Keys, which the
    /// window answers by showing its key list. The control seam delivers
    /// through `input` alone, so its choice of the item shows nothing:
    /// only a choice made inside this call is reported.
    pub fn input_live(&mut self, input: Input<'_>, clipboard: &mut dyn Clipboard) -> bool {
        self.keys_chosen = false;
        self.live = true;
        self.input(input, clipboard);
        self.live = false;
        std::mem::take(&mut self.keys_chosen)
    }

    /// One input, with its clipboard: td-ui's window's through
    /// `input_live`, or the control seam's. A card waiting is shown once
    /// it has made way for it.
    pub fn input(&mut self, input: Input<'_>, clipboard: &mut dyn Clipboard) {
        self.handle(input, clipboard);
        self.offer();
    }

    fn handle(&mut self, input: Input<'_>, clipboard: &mut dyn Clipboard) {
        if let Input::Paste(text) = input {
            return self.pasted(text);
        }
        // The dialog's paste that the clipboard gave up on (a focus loss,
        // a failed transfer, the device going away) never comes; td-ui hands a paste over only
        // after it stops saying one is in flight. Only the window's own
        // clipboard can say so: the control seam's has none, and its
        // inputs never release the dialog's paste.
        if self.dialog_paste && clipboard.available() && !clipboard.pasting() {
            self.dialog_paste = false;
        }
        if self.dialog.is_some() {
            return self.dialog_input(input, clipboard);
        }
        if self.picker.is_some() {
            return self.picker_input(input);
        }
        if self.notes.is_some() {
            return self.notes_input(input, clipboard);
        }
        if self.chooser.is_some() {
            return self.chooser_input(input);
        }
        if self.confirm.is_some() {
            return self.confirm_input(input);
        }
        if self.menu.is_open() && self.menu_input(&input) {
            return;
        }
        match input {
            Input::Key { chord, repeat } => self.key(chord, repeat, clipboard),
            Input::Pointer {
                phase,
                x,
                y,
                extend,
                ..
            } => self.pointer(phase, x, y, extend, clipboard),
            Input::CancelPointer => self.cancel_pointer(),
            Input::Hover(_) => {}
            Input::Context { x, y } => self.context(x, y),
            Input::Wheel { rows, columns } => {
                let changed = match self.focus {
                    Focus::List => {
                        self.list.event(tree_table::Event::Scroll {
                            rows: rows as i64,
                            columns: columns as i64,
                        }) != tree_table::Outcome::Ignored
                    }
                    Focus::Composer => self.composer.scroll(rows, columns),
                    Focus::Transcript => {
                        self.transcript
                            .event(messages::Event::Wheel { rows }, clipboard)
                            != messages::Outcome::Ignored
                    }
                };
                if changed {
                    self.touch();
                }
            }
            Input::Resize(surface) => self.resize(surface),
            Input::Focus(focused) => {
                self.focused = focused;
                if !focused {
                    self.cancel_pointer();
                }
                self.apply_focus();
            }
            Input::Close => {}
            // Taken above, by `pasted`.
            Input::Paste(_) => {}
        }
    }

    /// The clipboard's text, for whoever asked: the key dialog's paste is
    /// its own, and dropped once it has closed; while the dialog is open
    /// no other paste goes anywhere; otherwise the focused composer
    /// takes it.
    fn pasted(&mut self, text: &str) {
        if std::mem::take(&mut self.dialog_paste) {
            match self.dialog.as_mut() {
                Some(dialog) => {
                    let reply = dialog.paste(text);
                    self.reply(reply);
                }
                None => self.note("the key dialog closed before its paste came; it is dropped"),
            }
            return;
        }
        if self.dialog.is_some() {
            return self.note("a paste that came while the key dialog was open is dropped");
        }
        if self.picker.is_some() {
            return self.note("a paste that came while the picker was open is dropped");
        }
        if let Some(panel) = &self.notes {
            let name = panel.name();
            return self.note(format!(
                "a paste that came while the {name} was open is dropped"
            ));
        }
        if self.chooser.is_some() {
            return self.note("a paste that came while the directory chooser was open is dropped");
        }
        if self.confirm.is_some() {
            return self.note("a paste that came while the deletion question was open is dropped");
        }
        if self.focus == Focus::Composer {
            match self.composer.insert(text) {
                Ok(true) => self.touch(),
                Ok(false) => {}
                Err(e) => self.note(e),
            }
        }
    }

    fn cancel_pointer(&mut self) {
        match self.capture.take() {
            Some(Capture::Split(_)) => {
                let _ = self.split.event(split::Event::FocusLost);
            }
            Some(Capture::List) => {
                self.list.event(tree_table::Event::FocusLost);
            }
            Some(Capture::Transcript) => {
                self.transcript
                    .event(messages::Event::Cancel, &mut td_ui::window::NoClipboard);
            }
            Some(Capture::Composer) => {
                self.composer.event(PaneEvent::CancelPointer);
            }
            None => {}
        }
        // The Messages window holds no capture of the window's: its drag
        // is its own list's.
        if let Some(notes) = self.notes.as_mut() {
            notes.cancel();
        }
        self.apply_focus();
    }

    // --- the menu and the key dialog ---------------------------------

    /// Opens the File menu.
    fn open_menu(&mut self) {
        self.cancel_pointer();
        self.refresh_menu();
        match self.menu.open_bar(0) {
            Ok(()) => self.touch(),
            Err(e) => self.note(format!("the menu: {e}")),
        }
    }

    /// An input while the menu is open, which is the menu's: its keys,
    /// every other chord consumed, the pointer, the wheel over its panel,
    /// and a right press, a focus loss or a resize closing it. True when
    /// the input went no further; a resize and a focus change are the
    /// window's as well.
    fn menu_input(&mut self, input: &Input<'_>) -> bool {
        use td_ui::menus::Event;
        let event = match *input {
            Input::Key { chord, repeat } => menu::event(chord, repeat),
            Input::Pointer {
                phase: PointerPhase::Press,
                x,
                y,
                ..
            } => Event::Press { x, y },
            Input::Pointer {
                phase: PointerPhase::Move,
                x,
                y,
                ..
            } => Event::Move { x, y },
            Input::Pointer {
                phase: PointerPhase::Release,
                ..
            } => Event::Release,
            Input::Wheel { rows, .. } => match self.menu.panel(0) {
                Some(panel) => Event::Wheel {
                    x: panel.x,
                    y: panel.y,
                    rows,
                },
                None => Event::Other,
            },
            Input::Focus(false) => Event::FocusLost,
            // A right press dismisses it, as a press outside it does.
            Input::Context { .. } => Event::Key {
                key: td_ui::menus::Key::Dismiss,
                repeated: false,
            },
            // Under the open menu, nothing shows hover.
            Input::Hover(_) => return true,
            Input::Resize(_)
            | Input::Focus(true)
            | Input::Close
            | Input::Paste(_)
            | Input::CancelPointer => return false,
        };
        self.menu_event(event);
        !matches!(input, Input::Focus(false))
    }

    fn menu_event(&mut self, event: td_ui::menus::Event) {
        use td_ui::menus::Outcome;
        match self.menu.event(Some(self.menu_revision), event) {
            Ok(Outcome::Activated(action)) => {
                self.touch();
                self.menu_action(action);
            }
            Ok(Outcome::Dismissed | Outcome::Stale) => {
                self.menu_row = None;
                self.menu_process = None;
                self.touch();
            }
            Ok(Outcome::Changed) => self.touch(),
            Ok(Outcome::Ignored | Outcome::Consumed) => {}
            Err(e) => {
                self.menu.dismiss();
                self.note(format!("the menu: {e}"));
            }
        }
    }

    /// What a menu item does: what its chord does, or what only the menu
    /// offers.
    fn menu_action(&mut self, action: menu::Action) {
        match action {
            menu::Action::New => self.new_conversation(),
            menu::Action::Messages => self.open_messages(),
            menu::Action::Workspace => self.open_workspace(),
            menu::Action::SetKey => self.open_key_dialog(),
            menu::Action::Export => self.export_diagnostics(),
            menu::Action::Quit => self.requests.push(Request::Quit),
            menu::Action::Model => self.open_picker(),
            menu::Action::Delete => self.open_delete(),
            menu::Action::DefaultModel => self.open_default_picker(),
            menu::Action::AutoMode => {
                if let (true, Some(id), Some(mode)) =
                    (self.live, self.active.clone(), self.workspace_mode())
                {
                    let mode = match mode {
                        Mode::Auto => Mode::Ask,
                        Mode::Ask => Mode::Auto,
                    };
                    self.requests.push(Request::SetMode {
                        conversation: id,
                        mode,
                    });
                }
            }
            menu::Action::Effort(level) => self.choose(self.wanted().0, Some(level.to_string())),
            // The list is the window's: `input_live` reports the choice.
            menu::Action::Keys => self.keys_chosen = true,
            menu::Action::ShowArchived => self.toggle_archived(),
            menu::Action::Archive => self.archive_row(true),
            menu::Action::Unarchive => self.archive_row(false),
            menu::Action::DeleteRow => {
                if let Some(id) = self.menu_row.take() {
                    self.ask_delete(id);
                }
            }
            menu::Action::ShowOutput => {
                if let Some((id, number)) = self.menu_process.take() {
                    self.requests.push(Request::Output { id, number });
                }
            }
            menu::Action::KillProcess => {
                if let Some((id, number)) = self.menu_process.take() {
                    self.requests.push(Request::Kill { id, number });
                    self.note(format!("killing p{number}"));
                }
            }
        }
    }

    /// The list's process row `index`: its conversation and number.
    fn process_item(&self, index: usize) -> Option<&(Id, u64)> {
        self.process_items
            .get(index.checked_sub(self.process_base)?)
    }

    /// What conversation `id`'s event `kind` says of its background
    /// processes: one started, or one ended.
    fn heard_process(&mut self, id: &Id, kind: &Kind) -> bool {
        match kind {
            Kind::Process {
                number, command, ..
            } => {
                let running = self.processes.entry(id.clone()).or_default();
                if !running.iter().any(|(n, _)| n == number) {
                    running.push((*number, command.clone()));
                }
                true
            }
            Kind::Ended { number, .. } => {
                if let Some(running) = self.processes.get_mut(id) {
                    running.retain(|(n, _)| n != number);
                    if running.is_empty() {
                        self.processes.remove(id);
                    }
                }
                true
            }
            _ => false,
        }
    }

    /// Opens background process `number`'s row menu at `x`, `y`: Show
    /// output, then Kill.
    fn open_process_menu(&mut self, id: Id, number: u64, x: i64, y: i64) {
        self.cancel_pointer();
        let revision = self.menu_revision.wrapping_add(1);
        let opened = menu::process(self.surface, revision)
            .and_then(|mut menu| menu.open_context(x, y).map(|()| menu));
        match opened {
            Ok(menu) => {
                self.menu = menu;
                self.menu_revision = revision;
                self.menu_row = None;
                self.menu_process = Some((id, number));
                self.touch();
            }
            Err(e) => self.note(format!("the process's menu: {e}")),
        }
    }

    /// Shows the output of conversation `id`'s background process
    /// `number` as the session read it, read-only, over no modal: its
    /// last `output::READ_BYTES`, each line made visible.
    pub fn show_output(
        &mut self,
        id: &Id,
        number: u64,
        read: Result<crate::output::Output, String>,
    ) {
        if self.modal() {
            return;
        }
        let read = match read {
            Ok(read) => read,
            Err(why) => return self.note(format!("p{number}'s output: {why}")),
        };
        let lines: Vec<String> = read.text.split('\n').map(crate::tools::visible).collect();
        // A list entry holds a bounded run of lines, each headed by them,
        // the range ahead of the first, so a long output scrolls entry by
        // entry.
        let range = format!(
            "bytes {} to {} of {} written{}",
            read.from,
            read.to,
            read.total,
            if read.start > 0 {
                format!(", the first {} dropped", read.start)
            } else {
                String::new()
            }
        );
        let mut entries = Vec::new();
        for (n, chunk) in lines.chunks(OUTPUT_LINES).enumerate() {
            let first = n * OUTPUT_LINES + 1;
            let lines = format!("lines {first} to {}", first + chunk.len() - 1);
            let header = match n {
                0 => format!("{range}; {lines}"),
                _ => lines,
            };
            entries.push((header, chunk.join("\n")));
        }
        let command = self
            .processes
            .get(id)
            .and_then(|running| running.iter().find(|(n, _)| *n == number))
            .map(|(_, command)| {
                let cut: String = command.chars().take(80).collect();
                format!(": {}", crate::tools::visible(&cut))
            })
            .unwrap_or_default();
        match crate::notes::Panel::output(
            self.surface,
            body(self.surface),
            id.clone(),
            format!("p{number} output{command}"),
            &entries,
        ) {
            Ok(panel) => {
                self.notes = Some(panel);
                self.apply_focus();
                self.touch();
            }
            Err(why) => self.note(why),
        }
    }

    /// Archives the row menu's conversation, or brings it back.
    fn archive_row(&mut self, archived: bool) {
        if let Some(id) = self.menu_row.take() {
            self.requests.push(Request::Archive { id, archived });
            self.touch();
        }
    }

    /// A right press: over a conversation's row, that row's menu at the
    /// pointer; anywhere else, nothing.
    fn context(&mut self, x: i64, y: i64) {
        // An open modal or menu has taken the press before this.
        if self.modal() || self.menu.is_open() {
            return;
        }
        let Some(tree_table::Target::Row(index)) = self.list.target(x, y) else {
            return;
        };
        if let Some((id, number)) = self.process_item(index).cloned() {
            return self.open_process_menu(id, number, x, y);
        }
        if let Some(id) = self.rows.get(index).map(|r| r.id.clone()) {
            self.open_row_menu(id, x, y);
        }
    }

    /// Opens a row's menu under its row, as a right press there does:
    /// with the list focused, the selected row's, so the keyboard reaches
    /// an archived one; else the open conversation's. At the list's
    /// corner when the row is out of view.
    pub fn open_key_row_menu(&mut self) {
        if self.modal() {
            return;
        }
        let selected = match self.focus {
            Focus::List => self.list.selected(),
            _ => None,
        };
        let Some(index) = selected.or_else(|| self.shown_active()) else {
            return self.note("no conversation is open or selected");
        };
        let Some(regions) = self.regions else {
            return;
        };
        let under = self
            .list
            .model()
            .find(index)
            .and_then(|at| self.list.geometry()?.row(at))
            .map(|rect| (rect.x, rect.y.saturating_add(i64::from(rect.height))));
        let (x, y) = under.unwrap_or((regions.list.x, regions.list.y));
        if let Some((id, number)) = self.process_item(index).cloned() {
            return self.open_process_menu(id, number, x, y);
        }
        let Some(id) = self.rows.get(index).map(|r| r.id.clone()) else {
            return;
        };
        self.open_row_menu(id, x, y);
    }

    /// Opens conversation `id`'s row menu at `x`, `y`, in the menu's
    /// controller: Archive, or Unarchive, then Delete….
    fn open_row_menu(&mut self, id: Id, x: i64, y: i64) {
        let Some(archived) = self.rows.iter().find(|r| r.id == id).map(|r| r.archived) else {
            return;
        };
        self.cancel_pointer();
        let revision = self.menu_revision.wrapping_add(1);
        let opened = menu::row(self.surface, archived, revision)
            .and_then(|mut row| row.open_context(x, y).map(|()| row));
        match opened {
            Ok(row) => {
                self.menu = row;
                self.menu_revision = revision;
                self.menu_row = Some(id);
                self.touch();
            }
            Err(e) => self.note(format!("the row's menu: {e}")),
        }
    }

    /// Builds the menu again, closed, from the state of the moment and at
    /// a new revision: the open conversation's effort is checked, and its
    /// items are off with no conversation open.
    fn refresh_menu(&mut self) {
        if self.menu.is_open() {
            return;
        }
        let revision = self.menu_revision.wrapping_add(1);
        let model = self.model();
        let state = menu::State {
            open: self.active.is_some(),
            workspace: self.repositories().is_some(),
            effort: self.effort(),
            reasoning: self.reasoning(model),
            show_archived: self.show_archived,
            auto: self.workspace_mode().map(|mode| mode == Mode::Auto),
        };
        match menu::menu(self.surface, state, revision) {
            Ok(menu) => {
                self.menu = menu;
                self.menu_revision = revision;
                self.menu_row = None;
                self.menu_process = None;
            }
            Err(e) => self.note(format!("the menu: {e}")),
        }
    }

    /// The choice the next one starts from: the last asked for, else the
    /// one logged.
    fn wanted(&self) -> (Option<String>, Option<String>) {
        self.asked
            .back()
            .cloned()
            .unwrap_or_else(|| self.choice.clone())
    }

    /// Asks for the open conversation's model and effort to be these.
    fn choose(&mut self, model: Option<String>, effort: Option<String>) {
        if self.active.is_none() {
            return self.note("no conversation is open to choose for");
        }
        self.asked.push_back((model.clone(), effort.clone()));
        self.requests.push(Request::Choose { model, effort });
        self.touch();
    }

    /// Opens the model picker for the open conversation, modal over the
    /// window's body.
    pub fn open_picker(&mut self) {
        if self.modal() {
            return;
        }
        if self.active.is_none() {
            return self.note("no conversation is open to choose a model for");
        }
        let wanted = self.wanted().0;
        let current = wanted.unwrap_or_else(|| self.model().to_string());
        let opened = Picker::open(
            self.surface,
            body(self.surface),
            crate::picker::TITLE,
            &self.offers,
            &current,
        );
        self.show_picker(opened, Picking::Model);
    }

    /// Opens the model picker for the default model, which a conversation
    /// with no model of its own uses (DESIGN.md §4).
    pub fn open_default_picker(&mut self) {
        if self.modal() {
            return;
        }
        let opened = Picker::open(
            self.surface,
            body(self.surface),
            crate::picker::DEFAULT_TITLE,
            &self.offers,
            &self.default_model,
        );
        self.show_picker(opened, Picking::Default);
    }

    /// Starts a new conversation, as `C-n` and File → New conversation…
    /// do: through the template chooser (DESIGN.md §7), Empty selected,
    /// or, when no workspace can be made, at once with none.
    pub fn new_conversation(&mut self) {
        if self.modal() {
            return;
        }
        if self.no_workspaces.is_some() {
            self.requests.push(Request::New);
            return self.touch();
        }
        let mut rows = vec![
            (crate::config::EMPTY.to_string(), "scratch".to_string()),
            (crate::config::DIRECTORY.to_string(), "folder".to_string()),
        ];
        rows.extend(self.templates.iter().map(|(name, repos)| {
            let meta = if *repos { "repositories" } else { "scratch" };
            (name.clone(), meta.to_string())
        }));
        let note = if self.templates.iter().any(|(_, repos)| *repos) {
            "a repository template's worktrees are checked out in the background; a remote not yet admitted is asked about first"
        } else {
            "Empty is a private scratch directory; Directory\u{2026} is a folder of yours"
        };
        let opened = Picker::templates(self.surface, body(self.surface), &rows, note);
        self.show_picker(opened, Picking::Template);
    }

    /// Whether a modal is open: the picker, the Messages window, the
    /// chooser, the key dialog or the question.
    fn modal(&self) -> bool {
        self.picker.is_some()
            || self.notes.is_some()
            || self.chooser.is_some()
            || self.dialog.is_some()
            || self.confirm.is_some()
    }

    /// Opens the Messages window, as `C-S-m` and File → Messages… do:
    /// every note kept, whole and with its time, and none unread after.
    pub fn open_messages(&mut self) {
        if self.modal() {
            return;
        }
        self.cancel_pointer();
        self.menu.dismiss();
        match crate::notes::Panel::open(self.surface, body(self.surface), &self.log) {
            Ok(panel) => {
                self.notes = Some(panel);
                self.log.read();
                self.apply_focus();
                self.touch();
            }
            Err(why) => self.note(why),
        }
    }

    /// Asks for the open conversation's workspace card, as `C-S-w` and
    /// Conversation → Workspace card… do: the session reads what it
    /// shows and answers through `show_workspace`.
    pub fn open_workspace(&mut self) {
        if self.modal() {
            return;
        }
        let Some((id, _)) = self.repositories() else {
            return self.note("the open conversation has no repository workspace, so no card");
        };
        let id = id.clone();
        self.cancel_pointer();
        self.menu.dismiss();
        self.requests.push(Request::Workspace(id));
    }

    /// Shows conversation `id`'s workspace card from `record`, as the
    /// session read it: only while it is the open conversation, and over
    /// no modal. It is what was read then, opened again to read again.
    pub fn show_workspace(&mut self, id: &Id, record: &crate::card::Record) {
        let Some((open, repositories)) = self.repositories() else {
            return;
        };
        if open != id || self.modal() {
            return;
        }
        let (title, entries) = (
            crate::card::title(repositories),
            crate::card::entries(repositories, record),
        );
        match crate::notes::Panel::card(
            self.surface,
            body(self.surface),
            id.clone(),
            title,
            &entries,
        ) {
            Ok(panel) => {
                self.notes = Some(panel);
                self.apply_focus();
                self.touch();
            }
            Err(why) => self.note(why),
        }
    }

    fn close_messages(&mut self) {
        self.notes = None;
        self.apply_focus();
        self.touch();
        self.offer();
    }

    /// An input while the Messages window is open, which is its own, as
    /// the picker's are.
    fn notes_input(&mut self, input: Input<'_>, clipboard: &mut dyn Clipboard) {
        match input {
            Input::Resize(surface) => return self.resize(surface),
            Input::Focus(focused) => {
                self.focused = focused;
                if !focused {
                    self.cancel_pointer();
                }
                return self.apply_focus();
            }
            _ => {}
        }
        let clock = self.clock;
        let Some(notes) = self.notes.as_mut() else {
            return;
        };
        match notes.input(&input, clipboard, clock) {
            crate::notes::Reply::Stay(changed) => {
                if changed {
                    self.touch();
                }
            }
            crate::notes::Reply::Closed => self.close_messages(),
            crate::notes::Reply::Refused(why) => self.note(format!("copy: {why}")),
        }
    }

    /// Opens the directory chooser, as the template chooser's Directory…
    /// does: modal over the window's body, on the folder it last chose
    /// from, else the home directory.
    fn open_chooser(&mut self) {
        if self.modal() {
            return;
        }
        if let Some(why) = self.no_workspaces.clone() {
            return self.note(why);
        }
        self.cancel_pointer();
        self.menu.dismiss();
        // Where it last chose from, else the home directory, else the
        // root: a folder gone since is not a dead end.
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .filter(|home| home.is_absolute());
        let mut opened = Err("nowhere to start".to_string());
        for start in [
            Some(self.chooser_start.clone()),
            home,
            Some(PathBuf::from("/")),
        ]
        .into_iter()
        .flatten()
        {
            opened = Chooser::open(self.surface, body(self.surface), &start);
            if opened.is_ok() {
                break;
            }
        }
        match opened {
            Ok(chooser) => {
                self.chooser = Some(chooser);
                self.apply_focus();
                self.touch();
            }
            Err(e) => self.note(format!("the directory chooser: {e}")),
        }
    }

    fn close_chooser(&mut self) {
        self.chooser = None;
        self.apply_focus();
        self.touch();
        self.offer();
    }

    /// An input while the chooser is open, which is the chooser's, as the
    /// picker's are.
    fn chooser_input(&mut self, input: Input<'_>) {
        match input {
            Input::Resize(surface) => return self.resize(surface),
            Input::Focus(focused) => {
                self.focused = focused;
                if !focused {
                    self.cancel_pointer();
                }
                return self.apply_focus();
            }
            _ => {}
        }
        let Some(chooser) = self.chooser.as_mut() else {
            return;
        };
        match chooser.input(&input) {
            crate::chooser::Reply::Stay(changed) => {
                if changed {
                    self.touch();
                }
            }
            crate::chooser::Reply::Closed => self.close_chooser(),
            crate::chooser::Reply::Chosen(folder) => {
                if let Some(parent) = folder.parent() {
                    self.chooser_start = parent.to_path_buf();
                }
                self.close_chooser();
                self.requests.push(Request::NewIn(folder));
            }
        }
    }

    fn show_picker(&mut self, opened: Result<Picker, String>, picking: Picking) {
        if self.modal() {
            return;
        }
        self.cancel_pointer();
        self.menu.dismiss();
        match opened {
            Ok(picker) => {
                self.picker = Some(picker);
                self.picking = picking;
                self.apply_focus();
                self.touch();
            }
            Err(e) => self.note(e),
        }
    }

    fn close_picker(&mut self) {
        self.picker = None;
        self.picking = Picking::Model;
        self.apply_focus();
        self.touch();
        self.offer();
    }

    /// An input while the picker is open, which is the picker's: every
    /// key and the pointer. A resize and the window's focus are the
    /// window's too.
    fn picker_input(&mut self, input: Input<'_>) {
        match input {
            Input::Resize(surface) => return self.resize(surface),
            Input::Focus(focused) => {
                self.focused = focused;
                if !focused {
                    self.cancel_pointer();
                }
                return self.apply_focus();
            }
            _ => {}
        }
        let Some(picker) = self.picker.as_mut() else {
            return;
        };
        match picker.input(&input) {
            crate::picker::Reply::Stay(changed) => {
                if changed {
                    self.touch();
                }
            }
            crate::picker::Reply::Closed => self.close_picker(),
            crate::picker::Reply::Chosen(name) => {
                let picking = self.picking;
                if picking == Picking::Template && name == crate::config::DIRECTORY {
                    self.picker = None;
                    self.picking = Picking::Model;
                    // Focus back where it was, should the chooser not open.
                    self.apply_focus();
                    self.touch();
                } else {
                    self.close_picker();
                }
                match picking {
                    Picking::Default => {
                        self.requests.push(Request::SetDefault(name));
                        self.touch();
                    }
                    Picking::Model => self.choose(Some(name), self.wanted().1),
                    Picking::Template if name == crate::config::EMPTY => {
                        self.requests.push(Request::NewScratch);
                        self.touch();
                    }
                    // Straight on to the folder chooser: a card that waited
                    // behind the picker waits behind the chooser too, rather
                    // than open first and leave Directory… undone.
                    Picking::Template if name == crate::config::DIRECTORY => {
                        self.open_chooser();
                        if self.chooser.is_none() {
                            self.offer();
                        }
                    }
                    Picking::Template => {
                        self.requests.push(Request::NewFrom(name));
                        self.touch();
                    }
                }
            }
        }
    }

    /// Asks whether to delete the open conversation, as Conversation →
    /// Delete conversation… does: modal over the window's body, `Cancel`
    /// first.
    pub fn open_delete(&mut self) {
        if self.modal() {
            return;
        }
        let Some(id) = self.active.clone() else {
            return self.note("no conversation is open to delete");
        };
        self.ask_delete(id);
    }

    /// Asks whether to delete conversation `id`, open or not, as its row
    /// menu's Delete… does.
    fn ask_delete(&mut self, id: Id) {
        if self.modal() {
            return;
        }
        let Some(row) = self.rows.iter().find(|r| r.id == id) else {
            return;
        };
        let (title, workspace) = (row.title.clone(), row.workspace.clone());
        self.cancel_pointer();
        self.menu.dismiss();
        self.confirm_revision = self.confirm_revision.wrapping_add(1);
        match Confirm::open(
            self.surface,
            body(self.surface),
            id,
            &title,
            workspace.as_ref(),
            self.confirm_revision,
        ) {
            Ok(confirm) => {
                self.confirm = Some(confirm);
                self.apply_focus();
                self.touch();
            }
            Err(e) => self.note(e),
        }
    }

    /// Closes the question or card; a card waiting is shown next.
    fn close_confirm(&mut self) {
        self.confirm = None;
        self.apply_focus();
        self.touch();
        self.offer();
    }

    /// An input while the question is open, which is the question's: every
    /// key and the pointer. A resize and the window's focus are the
    /// window's too.
    fn confirm_input(&mut self, input: Input<'_>) {
        match input {
            Input::Resize(surface) => return self.resize(surface),
            Input::Focus(focused) => {
                self.focused = focused;
                if !focused {
                    self.cancel_pointer();
                    // td-ui's question is closed when the window loses
                    // the keyboard: nothing is deleted, and a card is
                    // set aside, to be shown again when it comes back.
                    let closed = self.confirm.as_mut().and_then(|c| {
                        matches!(
                            c.input(&input),
                            confirm::Reply::Closed | confirm::Reply::SetAside
                        )
                        .then(|| c.purpose().clone())
                    });
                    if let Some(purpose) = closed {
                        self.confirm = None;
                        // An admission card is asked again when the
                        // keyboard comes back, as a tool's card is.
                        if let confirm::Purpose::Admit { template, remotes } = purpose {
                            self.admission = Some((template, remotes));
                        }
                        self.touch();
                    }
                }
                return self.apply_focus();
            }
            _ => {}
        }
        let Some(confirm) = self.confirm.as_mut() else {
            return;
        };
        let purpose = confirm.purpose().clone();
        // A card just shown takes no key or press yet: one meant for
        // what was there before is not a decision.
        let settling = matches!(
            purpose,
            confirm::Purpose::Approve { .. }
                | confirm::Purpose::Admit { .. }
                | confirm::Purpose::Remove(_)
        ) && self
            .card_shown
            .is_some_and(|shown| shown.elapsed() < self.settle);
        if settling
            && matches!(
                input,
                Input::Key { .. }
                    | Input::Pointer {
                        phase: PointerPhase::Press,
                        ..
                    }
            )
        {
            // Settled only once nothing has come for that long.
            self.card_shown = Some(Instant::now());
            return;
        }
        match confirm.input(&input) {
            confirm::Reply::Stay(changed) => {
                if changed {
                    self.touch();
                }
            }
            confirm::Reply::SetAside => self.close_confirm(),
            // Cancel, or Escape, refuses a card's call.
            confirm::Reply::Closed => {
                match purpose {
                    confirm::Purpose::Approve { conversation, call } => {
                        self.decide(conversation, call, None)
                    }
                    confirm::Purpose::Admit { template, .. } => self.note(format!(
                        "no conversation from template {template:?}: its remotes were not admitted"
                    )),
                    confirm::Purpose::Delete(_) => {}
                    confirm::Purpose::Remove(id) => self.answer_removal(id, false),
                }
                self.close_confirm();
            }
            confirm::Reply::Confirmed(confirm::Purpose::Remove(id), _) => {
                self.answer_removal(id, true);
                self.close_confirm();
            }
            confirm::Reply::Confirmed(confirm::Purpose::Delete(id), _) => {
                self.close_confirm();
                self.requests.push(Request::Delete(id));
            }
            confirm::Reply::Confirmed(confirm::Purpose::Approve { conversation, call }, act) => {
                self.decide(conversation, call, Some(act));
                self.close_confirm();
            }
            confirm::Reply::Confirmed(confirm::Purpose::Admit { template, remotes }, _) => {
                self.close_confirm();
                self.requests.push(Request::Admit { template, remotes });
            }
        }
    }

    /// The human's answer to conversation `id`'s loss card, sent and the
    /// card forgotten.
    fn answer_removal(&mut self, id: Id, remove: bool) {
        self.removals.retain(|(of, _, _, _)| of != &id);
        self.requests.push(Request::Remove { id, remove });
    }

    /// Asks whether `remotes`, which template `template` names and no
    /// admission covers, are admitted (DESIGN.md §7): a card, modal, its
    /// `Admit` making the workspace (`Request::Admit`). With something
    /// else modal, or no room for it, nothing is made and that is said.
    pub fn ask_admission(&mut self, template: String, remotes: Vec<String>) {
        // The latest choice is the one asked; one set aside is dropped.
        self.admission = None;
        if self.modal() {
            return self.note(format!(
                "no conversation from template {template:?}: its remotes are not admitted, and another question is open"
            ));
        }
        self.cancel_pointer();
        self.menu.dismiss();
        self.press = None;
        self.confirm_revision = self.confirm_revision.wrapping_add(1);
        let named = format!("no conversation from template {template:?}");
        match Confirm::admit(
            self.surface,
            body(self.surface),
            template,
            remotes,
            self.confirm_revision,
        ) {
            Ok(confirm) => {
                self.confirm = Some(confirm);
                self.card_shown = Some(Instant::now());
                self.apply_focus();
                self.touch();
            }
            Err(e) => self.note(format!("{named}: {e}")),
        }
    }

    /// Asks for the diagnostics archive, as File → Export diagnostics does.
    pub fn export_diagnostics(&mut self) {
        self.requests.push(Request::Export);
        self.touch();
    }

    /// Opens the key dialog, modal over the window.
    pub fn open_key_dialog(&mut self) {
        if self.dialog.is_some() {
            return;
        }
        let Some(path) = self.key_path.clone() else {
            return self.note(
                "there is nowhere to store a key: neither XDG_CONFIG_HOME nor HOME is an absolute path",
            );
        };
        self.cancel_pointer();
        self.menu.dismiss();
        self.press = None;
        match KeyDialog::open(self.surface, &path) {
            Ok(dialog) => {
                // One modal at a time: the dialog replaces the picker, the
                // Messages window, the chooser and the question.
                self.picker = None;
                self.notes = None;
                self.chooser = None;
                self.confirm = None;
                self.dialog = Some(dialog);
                self.apply_focus();
            }
            Err(e) => self.note(e),
        }
    }

    /// Closes the key dialog, its entry cleared first.
    fn close_dialog(&mut self) {
        if let Some(mut dialog) = self.dialog.take() {
            dialog.close();
        }
        self.apply_focus();
        self.offer();
    }

    fn reply(&mut self, reply: Reply) {
        match reply {
            Reply::Stay(true) => self.touch(),
            Reply::Stay(false) => {}
            Reply::Closed => self.close_dialog(),
            Reply::Save { secret, replace } => {
                self.requests.push(Request::SaveKey { secret, replace });
                self.touch();
            }
        }
    }

    /// An input while the key dialog is open, which is the dialog's: every
    /// key, the pointer, and the paste it asked for. A resize and the
    /// window's focus are the window's too.
    fn dialog_input(&mut self, input: Input<'_>, clipboard: &mut dyn Clipboard) {
        let Some(dialog) = self.dialog.as_mut() else {
            return;
        };
        let reply = match input {
            Input::Key { chord, repeat } => {
                self.press = None;
                let idle = !clipboard.pasting();
                let reply = dialog.key(chord, repeat, clipboard);
                // This key asked the clipboard for its text.
                if idle && clipboard.pasting() {
                    self.dialog_paste = true;
                }
                reply
            }
            Input::Pointer {
                phase,
                x,
                y,
                extend,
                ..
            } => {
                if phase == PointerPhase::Press {
                    self.press = Some((x, y));
                }
                dialog.pointer(phase, x, y, extend)
            }
            // Taken first, by `input`.
            Input::Paste(_) => return,
            Input::CancelPointer => {
                dialog.cancel_pointer();
                Reply::Stay(true)
            }
            Input::Focus(focused) => {
                if !focused {
                    dialog.focus_lost();
                }
                self.focused = focused;
                self.apply_focus();
                Reply::Stay(true)
            }
            Input::Resize(surface) => {
                self.resize(surface);
                return;
            }
            // A close never comes here: the window quits on it first.
            Input::Wheel { .. } | Input::Hover(_) | Input::Context { .. } | Input::Close => {
                Reply::Stay(false)
            }
        };
        self.reply(reply);
    }

    /// The key is stored at `path`: the dialog closes, cleared, and the
    /// status row stops asking for one.
    pub fn key_saved(&mut self, path: &str) {
        self.close_dialog();
        self.keyed = true;
        self.note(format!(
            "the key is stored in {path}; every conversation uses it from now on"
        ));
    }

    /// A key is stored already: the dialog asks whether to replace it.
    pub fn key_exists(&mut self) {
        let press = self.press;
        if let Some(dialog) = self.dialog.as_mut() {
            dialog.ask_replace(press);
            self.touch();
        }
    }

    /// The key was not stored, for the reason given, which the dialog
    /// shows; it stays open with its text.
    pub fn key_refused(&mut self, why: String) {
        match self.dialog.as_mut() {
            Some(dialog) => {
                dialog.refused(why);
                self.touch();
            }
            None => self.note(why),
        }
    }

    /// The key list's sections: the window's chords, from the driven
    /// table, then the focused widget's keys and the other widgets'.
    pub fn key_list(&self) -> Vec<Section> {
        let window = Section {
            title: "Global",
            rows: crate::control::BINDINGS
                .iter()
                .filter_map(|binding| {
                    Some(keys::Row {
                        keys: binding.chord?,
                        what: binding.help,
                    })
                })
                .collect(),
        };
        let mut widgets = vec![
            (Focus::List, Section::new("Conversation list", LIST_KEYS)),
            (
                Focus::Transcript,
                Section::new("Transcript", TRANSCRIPT_KEYS),
            ),
            (Focus::Composer, Section::new("Composer", COMPOSER_KEYS)),
        ];
        // Stable: the focused widget's first, the others in order.
        widgets.sort_by_key(|(focus, _)| *focus != self.focus);
        std::iter::once(window)
            .chain(widgets.into_iter().map(|(_, section)| section))
            .collect()
    }

    /// A key, by its chord: the window's own first, then the focused
    /// widget's.
    pub fn key(&mut self, chord: &str, repeat: bool, clipboard: &mut dyn Clipboard) {
        // An open dialog, picker or menu has every key.
        if self.dialog.is_some() {
            return self.dialog_input(Input::Key { chord, repeat }, clipboard);
        }
        if self.picker.is_some() {
            return self.picker_input(Input::Key { chord, repeat });
        }
        if self.notes.is_some() {
            return self.notes_input(Input::Key { chord, repeat }, clipboard);
        }
        if self.chooser.is_some() {
            return self.chooser_input(Input::Key { chord, repeat });
        }
        if self.confirm.is_some() {
            return self.confirm_input(Input::Key { chord, repeat });
        }
        if self.menu.is_open() {
            return self.menu_event(menu::event(chord, repeat));
        }
        match chord {
            menu::OPEN if !repeat => return self.open_menu(),
            menu::ROW_MENU if !repeat => return self.open_key_row_menu(),

            "C-n" if !repeat => return self.new_conversation(),
            "C-S-m" if !repeat => return self.open_messages(),
            crate::card::CHORD if !repeat => return self.open_workspace(),
            "C-t" if !repeat => {
                if self.todo.is_empty() {
                    self.note("the conversation has no todo list");
                } else {
                    self.todo_open = !self.todo_open;
                    self.place();
                }
                return;
            }
            "C-S-t" if !repeat => {
                if !self.todo.is_empty() {
                    self.requests.push(Request::ClearTodo);
                }
                return;
            }
            "C-z" | "C-S-z" if !repeat => return self.restore_step(chord == "C-z"),
            "C-S-p" if !repeat => {
                let paused = self
                    .active
                    .as_ref()
                    .and_then(|id| self.rows.iter().find(|r| &r.id == id))
                    .map(|r| r.paused);
                if let Some(paused) = paused {
                    self.requests.push(Request::Pause(!paused));
                    self.note(if paused { "resuming" } else { "pausing" });
                }
                return;
            }
            "C-PageUp" => return self.switch(-1),
            "C-PageDown" => return self.switch(1),
            "C-Return" if !repeat => return self.send(),
            "C-r" if !repeat => {
                if self.meter.retry {
                    self.meter.retry = false;
                    self.requests.push(Request::Retry);
                    self.touch();
                }
                return;
            }
            // While a turn runs, Escape is the window's; else the
            // focused widget's.
            "Escape" if !repeat && self.running() => {
                self.requests.push(Request::Interrupt);
                self.note("interrupting the turn");
                return;
            }

            "F6" => {
                let next = match self.focus {
                    Focus::List => Focus::Transcript,
                    Focus::Transcript => Focus::Composer,
                    Focus::Composer => Focus::List,
                };
                return self.set_focus(next);
            }
            "S-F6" => {
                let next = match self.focus {
                    Focus::List => Focus::Composer,
                    Focus::Transcript => Focus::List,
                    Focus::Composer => Focus::Transcript,
                };
                return self.set_focus(next);
            }
            _ => {}
        }
        match self.focus {
            Focus::List => {
                let key = match chord {
                    "Up" => tree_table::Key::Up,
                    "Down" => tree_table::Key::Down,
                    "PageUp" => tree_table::Key::PageUp,
                    "PageDown" => tree_table::Key::PageDown,
                    "Home" => tree_table::Key::First,
                    "End" => tree_table::Key::Last,
                    "Left" => tree_table::Key::Left,
                    "Right" => tree_table::Key::Right,
                    "Return" => tree_table::Key::Activate,
                    _ => return,
                };
                match self.list.event(tree_table::Event::Key {
                    key,
                    repeated: repeat,
                }) {
                    tree_table::Outcome::Activate(index) => self.activate(index),
                    tree_table::Outcome::Ignored => {}
                    _ => self.touch(),
                }
            }
            Focus::Transcript => {
                if let Some(key) = messages::Key::from_chord(chord) {
                    match self
                        .transcript
                        .event(messages::Event::Key { key, repeat }, clipboard)
                    {
                        messages::Outcome::Ignored | messages::Outcome::Consumed => {}
                        messages::Outcome::Refused(why) => self.note(format!("copy: {why}")),
                        _ => self.touch(),
                    }
                }
            }
            Focus::Composer => match chord {
                // Return sends, as a chat's composer does; a held one
                // sends once and types nothing.
                "Return" => {
                    if !repeat {
                        self.send();
                    }
                }
                "S-Return" => {
                    if self.composer.chord("Return") {
                        self.touch();
                    }
                }
                "C-c" | "C-Insert" | "C-x" if !repeat => {
                    let Some(snapshot) = self.composer.selection() else {
                        return;
                    };
                    match clipboard.copy(snapshot.text()) {
                        Ok(()) if chord == "C-x" => {
                            self.composer.event(PaneEvent::Cut(snapshot));
                            self.touch();
                        }
                        Ok(()) => {}
                        Err(why) => self.note(format!("copy: {why}")),
                    }
                }
                "C-v" | "S-Insert" if !repeat => {
                    if let Err(why) = clipboard.paste() {
                        self.note(format!("paste: {why}"));
                    }
                }
                _ => {
                    if self.composer.chord(chord) {
                        self.touch();
                    }
                }
            },
        }
    }

    fn pointer(
        &mut self,
        phase: PointerPhase,
        x: i64,
        y: i64,
        extend: bool,
        clipboard: &mut dyn Clipboard,
    ) {
        let Some(regions) = self.regions else {
            return;
        };
        let capture = match phase {
            PointerPhase::Press => {
                if menu::bar(self.surface).rect().contains(x, y) {
                    self.refresh_menu();
                    self.menu_event(td_ui::menus::Event::Press { x, y });
                    return;
                }
                let share = self.split.share();
                if self.split.event(split::Event::Press { x, y }) != Ok(split::Outcome::Ignored) {
                    self.capture = Some(Capture::Split(share));
                    self.touch();
                    return;
                }
                let target = if regions.list.contains(x, y) {
                    Some((Capture::List, Focus::List))
                } else if regions.transcript.contains(x, y) {
                    Some((Capture::Transcript, Focus::Transcript))
                } else if regions.composer.contains(x, y) {
                    Some((Capture::Composer, Focus::Composer))
                } else {
                    None
                };
                let Some((capture, focus)) = target else {
                    return;
                };
                self.capture = Some(capture);
                if self.focus != focus {
                    self.set_focus(focus);
                }
                capture
            }
            _ => match self.capture {
                Some(capture) => capture,
                None => return,
            },
        };
        if phase == PointerPhase::Release {
            self.capture = None;
        }
        match capture {
            Capture::Split(before) => {
                let event = match phase {
                    PointerPhase::Move => split::Event::Move { x, y },
                    _ => split::Event::Release { x, y },
                };
                if self.split.event(event) == Ok(split::Outcome::Changed) {
                    self.place();
                }
                if phase == PointerPhase::Release && self.split.share() != before {
                    let (first, total) = self.split.share().parts();
                    self.requests.push(Request::SaveShare(first, total));
                }
            }
            Capture::List => {
                let event = match phase {
                    PointerPhase::Press => tree_table::Event::Press { x, y },
                    PointerPhase::Move => tree_table::Event::Move { x, y },
                    PointerPhase::Release => tree_table::Event::Release { x, y },
                };
                let before = self.list.selected();
                match self.list.event(event) {
                    // A process row is chosen by a press, and its output
                    // shown by a press once it is: a double press does.
                    tree_table::Outcome::Selected(index)
                        if self.process_item(index).is_some() && before != Some(index) =>
                    {
                        self.touch()
                    }
                    tree_table::Outcome::Selected(index) | tree_table::Outcome::Activate(index) => {
                        self.activate(index)
                    }
                    tree_table::Outcome::Ignored => {}
                    _ => self.touch(),
                }
            }
            Capture::Transcript => {
                let event = match phase {
                    PointerPhase::Press => messages::Event::Press {
                        x,
                        y,
                        extend,
                        at_ms: self.clock,
                    },
                    PointerPhase::Move => messages::Event::Move { x, y },
                    PointerPhase::Release => messages::Event::Release {
                        x,
                        y,
                        at_ms: self.clock,
                    },
                };
                match self.transcript.event(event, clipboard) {
                    messages::Outcome::Ignored | messages::Outcome::Consumed => {}
                    messages::Outcome::Refused(why) => self.note(format!("copy: {why}")),
                    _ => self.touch(),
                }
            }
            Capture::Composer => {
                if self.composer.pointer(phase, x, y, extend) {
                    self.touch();
                }
            }
        }
    }

    // --- drawing ----------------------------------------------------

    /// A conversation's cell in the list.
    fn cell(&self, index: usize, column: usize) -> Cell<'_> {
        if let Some(label) = index
            .checked_sub(self.process_base)
            .and_then(|n| self.process_labels.get(n))
        {
            let text = match column {
                0 => label.as_str(),
                1 => "running",
                _ => "",
            };
            return Cell::new(text).unwrap_or_else(|_| Cell::empty());
        }
        let Some(row) = self.rows.get(index) else {
            return Cell::empty();
        };
        let text = match column {
            0 => row.title.as_str(),
            1 if self.removals.iter().any(|(of, _, _, _)| of == &row.id) => ASKS,
            1 if self.closing(&row.id) => self.closing_word(&row.id).unwrap_or_default(),
            1 if self.cards.iter().any(|c| c.conversation == row.id) => ASKS,
            1 => row.word(),
            _ => self.labels.get(index).map_or("", String::as_str),
        };
        Cell::new(text).unwrap_or_else(|_| Cell::empty())
    }

    /// The todo list, a line to an item, over the chrome.
    fn emit_todo(&self, rect: Rect, damage: Rect, sink: &mut dyn FnMut(Draw)) {
        let Some(clip) = rect.intersection(damage) else {
            return;
        };
        let s = self.surface.scale.value();
        sink(Draw {
            clip,
            primitive: Primitive::Fill {
                rect,
                color: CHROME,
            },
        });
        for (at, line) in self.todo_lines().iter().enumerate() {
            raster::text_run(
                self.surface.scale,
                line.chars(),
                (
                    rect.x + (CELL_WIDTH * s) as i64,
                    rect.y + ((at * ROW + 4) * s) as i64,
                ),
                rect,
                GlyphStyle {
                    ink: INK,
                    background: CHROME,
                    weight: Weight::Regular,
                },
                damage,
                sink,
            );
        }
    }

    fn emit_status(&self, rect: Rect, damage: Rect, sink: &mut dyn FnMut(Draw)) {
        let Some(clip) = rect.intersection(damage) else {
            return;
        };
        let s = self.surface.scale.value();
        sink(Draw {
            clip,
            primitive: Primitive::Fill {
                rect,
                color: CHROME,
            },
        });
        sink(Draw {
            clip,
            primitive: Primitive::Fill {
                rect: Rect {
                    height: (s as u32).min(rect.height),
                    ..rect
                },
                color: BORDER,
            },
        });
        let line = self.status_line();
        raster::text_run(
            self.surface.scale,
            line.chars(),
            (rect.x + (CELL_WIDTH * s) as i64, rect.y + (4 * s) as i64),
            rect,
            GlyphStyle {
                ink: INK,
                background: CHROME,
                weight: Weight::Regular,
            },
            damage,
            sink,
        );
    }
}

impl Composition for App {
    fn surface(&self) -> Surface {
        self.surface
    }

    fn emit(&self, damage: Rect, sink: &mut dyn FnMut(Draw)) {
        let bounds = self.surface.bounds();
        let Some(damage) = damage.intersection(bounds) else {
            return;
        };
        sink(Draw {
            clip: damage,
            primitive: Primitive::Fill {
                rect: bounds,
                color: PAPER,
            },
        });
        let Some(regions) = self.regions else {
            // Too small for the split: say so rather than draw over itself,
            // and still draw a modal that is open, which has the keys.
            raster::text_run(
                self.surface.scale,
                "window too small".chars(),
                (0, 0),
                bounds,
                GlyphStyle {
                    ink: INK,
                    background: PAPER,
                    weight: Weight::Regular,
                },
                damage,
                sink,
            );
            return self.emit_modals(damage, sink);
        };
        self.split.emit(damage, sink);
        self.list.emit(
            None,
            damage,
            &mut |index, column| self.cell(index, column),
            sink,
        );
        self.transcript.emit(damage, sink);
        self.emit_todo(regions.todo, damage, sink);
        if let Some(clip) = regions.rule.intersection(damage) {
            sink(Draw {
                clip,
                primitive: Primitive::Fill {
                    rect: regions.rule,
                    color: BORDER,
                },
            });
        }
        if let Some(clip) = regions.composer.intersection(damage) {
            self.composer.emit(clip, sink);
        }
        self.emit_status(regions.status, damage, sink);
        menu::bar(self.surface).emit(damage, sink);
        self.menu.emit(damage, sink);
        self.emit_modals(damage, sink);
    }
}

impl App {
    /// The modals over everything else, the one with the keys last.
    fn emit_modals(&self, damage: Rect, sink: &mut dyn FnMut(Draw)) {
        if let Some(picker) = &self.picker {
            picker.emit(damage, sink);
        }
        if let Some(chooser) = &self.chooser {
            chooser.emit(damage, sink);
        }
        if let Some(notes) = &self.notes {
            notes.emit(damage, sink);
        }
        if let Some(confirm) = &self.confirm {
            confirm.emit(damage, sink);
        }
        if let Some(dialog) = &self.dialog {
            dialog.emit(self.focused, damage, sink);
        }
    }
}

/// Where the split goes: the surface under the menu bar.
fn body(surface: Surface) -> Rect {
    let bounds = surface.bounds();
    let bar = ((ROW * surface.scale.value()) as u32).min(bounds.height);
    if bar == bounds.height {
        return bounds;
    }
    Rect {
        y: bounds.y + i64::from(bar),
        height: bounds.height - bar,
        ..bounds
    }
}

#[cfg(test)]
pub mod tests {
    #![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
    use super::*;
    use td_ui::raster::Scale;
    use td_ui::window::NoClipboard;

    pub fn id(n: u8) -> Id {
        Id::parse(&format!("{n:032x}")).unwrap()
    }

    fn row(n: u8, activity: u64) -> Row {
        Row {
            id: id(n),
            title: format!("title {n}"),
            activity,
            state: RowState::Closed,
            paused: false,
            workspace: None,
            archived: false,
        }
    }

    pub fn app() -> App {
        let surface = Surface::new(1024, 640, Scale::default()).unwrap();
        let mut app = App::new(surface, None, Mode::Auto).unwrap();
        app.set_models("m/default", vec![("m/default".into(), 200_000)]);
        app.set_offers(Vec::new(), "medium");
        app.set_rows(vec![row(2, 50), row(1, 100), row(3, 90)]);
        app.set_active(id(1));
        app
    }

    fn key(app: &mut App, chord: &str) {
        app.key(chord, false, &mut NoClipboard);
    }

    fn text(app: &App) -> String {
        td_ui::driven::text(app).unwrap().2
    }

    fn user(seq: u64, text: &str) -> Update {
        Update::Up(Up::Event(Event {
            seq,
            time: 0,
            kind: Kind::User {
                delivery: String::new(),
                text: text.into(),
            },
        }))
    }

    #[test]
    fn the_key_list_is_the_driven_table_then_the_focused_widget_first() {
        let mut app = app();
        let sections = app.key_list();
        let titles: Vec<&str> = sections.iter().map(|s| s.title).collect();
        assert_eq!(
            titles,
            ["Global", "Composer", "Conversation list", "Transcript"]
        );
        // Every chord the driven table binds, in its order and words; the
        // chordless menu items, `set-key` and `model`, are no key.
        let chorded: Vec<keys::Row> = crate::control::BINDINGS
            .iter()
            .filter_map(|b| {
                Some(keys::Row {
                    keys: b.chord?,
                    what: b.help,
                })
            })
            .collect();
        assert_eq!(sections[0].rows, chorded);
        let chorded_count = crate::control::BINDINGS
            .iter()
            .filter(|b| b.chord.is_some())
            .count();
        assert_eq!(sections[0].rows.len(), chorded_count);
        assert!(sections[0].rows.iter().any(|r| r.keys == "C-n"));
        assert!(sections[1].rows.iter().any(|r| r.keys == "C-v/S-Insert"));
        key(&mut app, "F6");
        let titles: Vec<&str> = app.key_list().iter().map(|s| s.title).collect();
        assert_eq!(
            titles,
            ["Global", "Conversation list", "Transcript", "Composer"]
        );
    }

    /// Every focus's list is spelled as td-ui's keymap spells chords, and
    /// Help → Keys adds no row: the window's own section lists `F1`.
    #[test]
    fn the_key_list_passes_the_check_under_every_focus() {
        let mut app = app();
        let mut seen = Vec::new();
        for _ in 0..3 {
            let sections = app.key_list();
            let problems = keys::check(&sections);
            assert!(problems.is_empty(), "{:?}: {problems:#?}", app.focus());
            assert!(!sections
                .iter()
                .flat_map(|s| &s.rows)
                .any(|r| r.keys == keys::CHORD || r.what.contains(keys::ITEM)));
            seen.push(app.focus());
            key(&mut app, "F6");
        }
        assert_eq!(
            seen,
            [Focus::Composer, Focus::List, Focus::Transcript],
            "every focus"
        );
    }

    #[test]
    fn the_list_is_the_most_recently_active_first() {
        let app = app();
        let order: Vec<&Id> = app.rows().iter().map(|r| &r.id).collect();
        assert_eq!(order, [&id(1), &id(3), &id(2)]);
        let shown = text(&app);
        let at = |title: &str| shown.find(title).unwrap();
        assert!(
            at("title 1") < at("title 3") && at("title 3") < at("title 2"),
            "{shown}"
        );
        assert_eq!(app.most_recent(), Some(id(1)));
    }

    #[test]
    fn the_status_row_says_the_model_context_cost_and_credit() {
        let mut app = app();
        assert_eq!(
            app.status_line(),
            "starting | m/default medium | ctx 0/200k | cost $0.0000 | mode ask | no limits | 0 background"
        );
        app.update(
            Update::Up(Up::Hello {
                title: "Orchestrator".into(),
                torn: Some(7),
                interrupted: vec![],
                paused: false,
                prefix: Some(String::new()),
            }),
            0,
        );
        app.set_today(cost::ONE / 4);
        app.set_credit(Some("credit $7.5000".into()));
        // The note is counted, not shown: the row keeps a fixed width.
        assert_eq!(
            app.notice(),
            Some("dropped a torn final log line of 7 bytes")
        );
        assert_eq!(
            app.status_line(),
            "idle | 1 new message: C-S-m | m/default medium | ctx 0/200k | cost $0.0000 \
             | today $0.2500 | credit $7.5000 | mode ask | no limits | 0 background"
        );
        // Each total against its limit, where one is set.
        app.set_limits(cost::Limits::default());
        assert!(
            app.status_line()
                .contains("| cost $0.0000/$10.0000 | today $0.2500/$25.0000 |"),
            "{}",
            app.status_line()
        );
        // Another conversation with no model of its own has the default.
        key(&mut app, "C-PageDown");
        assert!(
            app.status_line().contains("| m/default medium |"),
            "{}",
            app.status_line()
        );
        assert!(
            text(&app).contains("| m/default medium |"),
            "{}",
            text(&app)
        );
    }

    #[test]
    fn return_sends_and_empties_the_composer_and_s_return_is_a_newline() {
        let mut app = app();
        for chord in ["h", "i", "S-Return", "t", "h", "e", "r", "e"] {
            key(&mut app, chord);
        }
        assert_eq!(app.composed(), "hi\nthere");
        assert!(app.take_requests().is_empty(), "S-Return sends nothing");
        key(&mut app, "Return");
        assert_eq!(app.take_requests(), [Request::Send("hi\nthere".into())]);
        assert_eq!(app.composed(), "");
        // C-Return sends too; a held Return sends once and types nothing.
        key(&mut app, "x");
        key(&mut app, "C-Return");
        assert_eq!(app.take_requests(), [Request::Send("x".into())]);
        key(&mut app, "y");
        app.key("Return", true, &mut NoClipboard);
        assert!(app.take_requests().is_empty());
        assert_eq!(app.composed(), "y");
        // A held S-Return types a line break each time.
        app.key("S-Return", true, &mut NoClipboard);
        assert_eq!(app.composed(), "y\n");
        // An empty or blank composer sends nothing.
        app.composer.fresh();
        key(&mut app, "Return");
        key(&mut app, "Space");
        assert_eq!(app.composed(), " ");
        key(&mut app, "Return");
        assert!(app.take_requests().is_empty());
    }

    #[test]
    fn local_echo_shows_the_message_and_its_turn() {
        let mut app = app();
        app.update(user(1, "hello there"), 0);
        let started = |seq, of| {
            Update::Up(Up::Event(Event {
                seq,
                time: 0,
                kind: Kind::Started {
                    effect: crate::store::Effect::Turn,
                    of,
                },
            }))
        };
        app.update(started(2, 1), 0);
        assert!(text(&app).contains("running"));
        app.update(
            Update::Up(Up::Event(Event {
                seq: 3,
                time: 0,
                kind: Kind::Finished {
                    started: 2,
                    outcome: "no model".into(),
                    retry: false,
                },
            })),
            0,
        );
        assert_eq!(app.transcript().len(), 1);
        let shown = text(&app);
        assert!(shown.contains("you"), "{shown}");
        assert!(shown.contains("hello there"), "{shown}");
        assert!(!shown.contains("running"), "{shown}");
        // A restart's hello starts the transcript over for the replay.
        app.update(
            Update::Up(Up::Hello {
                title: "Orchestrator".into(),
                torn: None,
                interrupted: vec![2],
                paused: false,
                prefix: Some(String::new()),
            }),
            0,
        );
        assert_eq!(app.transcript().len(), 0);
        assert!(app
            .notice()
            .is_some_and(|n| n.contains("interrupted 1 turn")));
    }

    /// The prefix opens the transcript as one folded system message,
    /// and a `prefix` event adds the one in force from there.
    #[test]
    fn the_system_context_opens_the_transcript_folded() {
        let mut app = app();
        let hello = |prefix: Option<String>| {
            Update::Up(Up::Hello {
                title: "Orchestrator".into(),
                torn: None,
                interrupted: vec![],
                paused: false,
                prefix,
            })
        };
        let prefix = crate::prompt::prefix(0);
        app.update(hello(Some(prefix.clone())), 0);
        assert_eq!(app.transcript().len(), 1);
        let system = app.transcript().message(0).unwrap();
        assert_eq!(system.label(), crate::system::HEADER);
        assert!(system.is_collapsed());
        assert!(system.section_text(0).unwrap().contains("You are td-agent"));
        // Folded, it shows its header and none of its text.
        let shown = text(&app);
        assert!(shown.contains(crate::system::HEADER), "{shown}");
        assert!(!shown.contains("system prompt"), "{shown}");
        app.update(user(1, "hi"), 0);
        app.update(
            at(
                2,
                Kind::Prefix {
                    text: r#"[{"role":"system","content":"be brief"}]"#.into(),
                },
            ),
            0,
        );
        assert_eq!(app.transcript().len(), 3);
        let replaced = app.transcript().message(2).unwrap();
        assert_eq!(replaced.section_text(0), Some("be brief"));
        assert!(text(&app).contains(crate::system::REPLACED));
        // A prefix the process did not send is said to be missing.
        app.update(hello(None), 0);
        assert_eq!(app.transcript().len(), 1);
        assert!(text(&app).contains("the system context is not shown"));
        // That prefix was in force, unshown: the next replaces it.
        app.update(
            at(
                1,
                Kind::Prefix {
                    text: prefix.clone(),
                },
            ),
            0,
        );
        assert!(text(&app).contains(crate::system::REPLACED));
        // The replay of an adopted conversation shows it too, or why not.
        app.replay(Ok(&prefix), vec![]);
        assert_eq!(
            app.transcript().message(0).map(|m| m.label()),
            Some(crate::system::HEADER)
        );
        app.replay(Err("no such file"), vec![]);
        assert!(text(&app).contains("the system context is not shown: no such file"));
        // After an empty prefix, the first prefix event replaces nothing.
        app.update(hello(Some(String::new())), 0);
        assert_eq!(app.transcript().len(), 0);
        app.update(at(1, Kind::Prefix { text: prefix }), 0);
        assert!(!text(&app).contains(crate::system::REPLACED));
    }

    #[test]
    fn c_n_asks_for_a_conversation_and_c_page_keys_switch() {
        let mut app = app();
        app.set_no_workspaces(Some("no workspace jail".into()));
        key(&mut app, "C-n");
        assert_eq!(app.take_requests(), [Request::New]);
        key(&mut app, "C-PageDown");
        assert_eq!(app.take_requests(), [Request::Open(id(3))]);
        assert_eq!(app.active(), Some(&id(3)));
        key(&mut app, "C-PageDown");
        key(&mut app, "C-PageDown");
        assert_eq!(app.take_requests(), [Request::Open(id(2))], "no wrap");
        key(&mut app, "C-PageUp");
        assert_eq!(app.take_requests(), [Request::Open(id(3))]);
        let states: Vec<RowState> = app.rows().iter().map(|r| r.state).collect();
        assert_eq!(
            states,
            [RowState::Closed, RowState::Starting, RowState::Closed]
        );
    }

    #[test]
    fn f6_moves_the_focus_and_the_list_opens_on_activate() {
        let mut app = app();
        assert_eq!(app.focus(), Focus::Composer);
        key(&mut app, "F6");
        assert_eq!(app.focus(), Focus::List);
        key(&mut app, "Down");
        assert!(app.take_requests().is_empty(), "moving opens nothing");
        key(&mut app, "Return");
        assert_eq!(app.take_requests(), [Request::Open(id(3))]);
        key(&mut app, "S-F6");
        assert_eq!(app.focus(), Focus::Composer);
        key(&mut app, "S-F6");
        assert_eq!(app.focus(), Focus::Transcript);
    }

    #[test]
    fn a_click_on_a_row_opens_it() {
        let mut app = app();
        let list = app.regions.unwrap().list;
        // The header's row, then the first, second and third conversation.
        let y = list.y + (ROW as i64) * 2 + 4;
        for phase in [PointerPhase::Press, PointerPhase::Release] {
            app.input(
                Input::Pointer {
                    phase,
                    x: list.x + 20,
                    y,
                    extend: false,
                    follow: false,
                },
                &mut NoClipboard,
            );
        }
        assert_eq!(app.focus(), Focus::List);
        assert_eq!(app.take_requests(), [Request::Open(id(3))]);
    }

    #[test]
    fn dragging_the_divider_asks_for_the_share_to_be_saved() {
        let mut app = app();
        let divider = app.split.layout().unwrap().divider;
        let (x, y) = (divider.x + 2, divider.y + 100);
        let pointer = |app: &mut App, phase, x| {
            app.input(
                Input::Pointer {
                    phase,
                    x,
                    y,
                    extend: false,
                    follow: false,
                },
                &mut NoClipboard,
            )
        };
        // Two moves: the drag goes on past the first one.
        pointer(&mut app, PointerPhase::Press, x);
        pointer(&mut app, PointerPhase::Move, x + 40);
        pointer(&mut app, PointerPhase::Move, x + 100);
        pointer(&mut app, PointerPhase::Release, x + 100);
        let requests = app.take_requests();
        assert!(
            matches!(requests.as_slice(), [Request::SaveShare(first, total)]
                if *first == divider.x as u32 + 100 && *total > *first),
            "{requests:?}"
        );
        // A saved share lays the split out where it was.
        let (first, total) = app.split.share().parts();
        let again = App::new(app.surface, Some((first, total)), Mode::Ask).unwrap();
        assert_eq!(again.split.layout(), app.split.layout());
        assert!(text(&again).contains("mode ask"));
    }

    #[test]
    fn a_message_past_the_bound_is_kept_and_refused() {
        let mut app = app();
        let long = "x".repeat(MAX_TEXT + 1);
        assert!(app.composer.insert(&long).unwrap());
        key(&mut app, "C-Return");
        assert!(app.take_requests().is_empty());
        assert_eq!(app.composed().len(), MAX_TEXT + 1, "nothing is lost");
        assert!(app.notice().is_some_and(|n| n.contains("at most 128 KiB")));
    }

    #[test]
    fn a_failed_conversation_keeps_the_message_and_opens_again() {
        let mut app = app();
        // Opening the open conversation again does nothing while it runs.
        app.open(id(1));
        assert!(app.take_requests().is_empty());
        app.update(Update::Failed { reason: "x".into() }, 0);
        key(&mut app, "h");
        key(&mut app, "C-Return");
        assert!(app.take_requests().is_empty());
        assert_eq!(app.composed(), "h", "nothing is lost");
        assert!(app.notice().is_some_and(|n| n.contains("open it again")));
        // Once failed, opening it again retries it.
        app.open(id(1));
        assert_eq!(app.take_requests(), [Request::Open(id(1))]);
    }

    #[test]
    fn a_message_the_window_could_not_send_comes_back() {
        let mut app = app();
        app.restore("hello", "no conversation is open".into());
        assert_eq!(app.composed(), "hello");
        assert!(app
            .notice()
            .is_some_and(|n| n.contains("back in the composer")));
        // Over a newer draft it goes after it, on a line of its own, and a
        // selection in the draft is left, not replaced.
        key(&mut app, "C-a");
        app.restore("again", "no conversation is open".into());
        assert_eq!(app.composed(), "hello\nagain\n");
    }

    #[test]
    fn a_refused_message_comes_back_with_why() {
        let mut app = app();
        app.update(
            Update::Refused {
                text: Some("long ago".into()),
                reason: "the conversation's log is full".into(),
            },
            0,
        );
        assert_eq!(app.composed(), "long ago");
        assert!(app
            .notice()
            .is_some_and(|n| n.starts_with("refused: the conversation's log is full")));
    }

    #[test]
    fn the_wheel_scrolls_the_focused_composer() {
        let mut app = app();
        assert_eq!(app.focus(), Focus::Composer);
        let lines: Vec<String> = (0..40).map(|n| format!("line {n}")).collect();
        assert!(app.composer.insert(&lines.join("\n")).unwrap());
        let _ = text(&app);
        let before = app.generation();
        app.input(
            Input::Wheel {
                rows: -5,
                columns: 0,
            },
            &mut NoClipboard,
        );
        assert_ne!(app.generation(), before, "the composer scrolled");
    }

    #[test]
    fn a_long_conversation_keeps_its_most_recent_messages_in_view() {
        let mut app = app();
        // Past the list's byte bound: 140 messages of 128 KiB.
        let big = "x".repeat(MAX_TEXT);
        for n in 0..140u64 {
            app.update(user(n * 3 + 1, &big), 0);
            app.update(
                Update::Up(Up::Event(Event {
                    seq: n * 3 + 2,
                    time: 0,
                    kind: Kind::Started {
                        effect: crate::store::Effect::Turn,
                        of: n * 3 + 1,
                    },
                })),
                0,
            );
        }
        let kept = app.transcript.len();
        assert!(kept < 140 && kept > 100, "{kept}");
        assert!(app.notice().is_some_and(|n| n.contains("most recent")));
        // The newest message is the transcript's last, and the indices
        // its turn's records find it by moved with the eviction.
        let last = app.messages.last().copied().unwrap();
        assert_eq!(last, (139 * 3 + 1, kept - 1));
        let first = app.messages.first().copied().unwrap();
        assert_eq!(first, ((140 - kept as u64) * 3 + 1, 0));
        assert_eq!(app.messages.len(), kept);
    }

    fn at(seq: u64, kind: Kind) -> Update {
        Update::Up(Up::Event(Event { seq, time: 0, kind }))
    }

    fn turn(app: &mut App, user: u64, text: &str) {
        app.update(
            at(
                user,
                Kind::User {
                    delivery: String::new(),
                    text: text.into(),
                },
            ),
            0,
        );
        app.update(
            at(
                user + 1,
                Kind::Started {
                    effect: crate::store::Effect::Turn,
                    of: user,
                },
            ),
            0,
        );
    }

    fn request(turn: u64, reserved: u64) -> Kind {
        Kind::Request {
            turn,
            purpose: Purpose::Turn,
            prefix: 0,
            head: String::new(),
            bytes: 0,
            reserved,
        }
    }

    #[test]
    fn a_reply_shows_with_its_reasoning_usage_and_cost() {
        let mut app = app();
        turn(&mut app, 1, "hello");
        assert_eq!(app.rows()[0].state, RowState::Running);
        app.update(at(3, request(2, 900)), 0);
        app.update(
            at(
                4,
                Kind::Assistant {
                    request: 3,
                    content: Some("hi there".into()),
                    reasoning: Some("the person greets".into()),
                    details: Some("[]".into()),
                    finish: "stop".into(),
                    incomplete: false,
                    calls: Vec::new(),
                },
            ),
            0,
        );
        app.update(
            at(
                5,
                Kind::Usage {
                    request: 3,
                    tokens: cost::Tokens {
                        prompt: 12_000,
                        completion: 30,
                        cached: 11_000,
                        cache_write: 0,
                        reasoning: 10,
                    },
                    cost: 12_345_600_000,
                    basis: crate::store::Basis::Reported,
                },
            ),
            0,
        );
        app.update(
            at(
                6,
                Kind::Finished {
                    started: 3,
                    outcome: "stop".into(),
                    retry: false,
                },
            ),
            0,
        );
        app.update(
            at(
                7,
                Kind::Finished {
                    started: 2,
                    outcome: "replied".into(),
                    retry: false,
                },
            ),
            0,
        );
        assert_eq!(app.transcript().len(), 2, "the message and the reply");
        let shown = text(&app);
        assert!(shown.contains("assistant"), "{shown}");
        assert!(shown.contains("hi there"), "{shown}");
        assert!(shown.contains("reasoning"), "{shown}");
        assert!(
            shown.contains("in 12k (cached 11k, written 0) out 30 $0.0123"),
            "{shown}"
        );
        assert!(shown.contains("replied"), "{shown}");
        assert!(app.status_line().contains("ctx 12k/200k | cost $0.0123"));
        assert_eq!(app.rows()[0].state, RowState::Idle);
        // The same events again, as a reopened conversation's process may
        // send what the log already gave: shown once.
        app.update(at(7, Kind::Notice { text: "dup".into() }), 0);
        assert_eq!(app.transcript().len(), 2);
    }

    fn delta(request: u64, reasoning: &str, content: &str) -> Update {
        Update::Up(Up::Delta {
            request,
            reasoning: reasoning.into(),
            content: content.into(),
        })
    }

    fn reply(request: u64, reasoning: &str, content: &str, incomplete: bool) -> Kind {
        Kind::Assistant {
            request,
            content: Some(content.into()),
            reasoning: Some(reasoning.into()),
            details: None,
            finish: if incomplete { "unknown" } else { "stop" }.into(),
            calls: Vec::new(),
            incomplete,
        }
    }

    /// Deltas draw the reply into one message as they come; the logged
    /// reply then settles it, kept as drawn (its reasoning left as the
    /// human opened it) when it matches, replaced when it does not.
    #[test]
    fn a_streamed_reply_is_drawn_as_it_comes_and_settled_once_logged() {
        let mut app = app();
        turn(&mut app, 1, "hello");
        app.update(at(3, request(2, 900)), 0);
        app.update(delta(3, "the person ", ""), 0);
        app.update(delta(3, "greets", "hi "), 0);
        app.update(delta(3, "", "there"), 0);
        assert_eq!(app.transcript().len(), 2, "the message and one reply");
        let drawn = app.transcript().message(1).unwrap();
        assert_eq!(drawn.section_text(0), Some("the person greets"));
        assert_eq!(drawn.section_text(1), Some("hi there"));
        assert!(text(&app).contains("streaming"), "{}", text(&app));
        app.transcript.set_section_collapsed(1, 0, false).unwrap();
        app.update(at(4, reply(3, "the person greets", "hi there", false)), 0);
        assert_eq!(app.transcript().len(), 2);
        let shown = text(&app);
        assert!(!shown.contains("streaming"), "{shown}");
        assert!(shown.contains("the person greets"), "still open: {shown}");
        // Text before reasoning: drawn again with its reasoning first; a
        // reply cut short keeps what was drawn, marked incomplete.
        turn(&mut app, 5, "again");
        app.update(at(7, request(6, 900)), 0);
        app.update(delta(7, "", "par"), 0);
        app.update(delta(7, "late thought", "tial"), 0);
        let drawn = app.transcript().message(3).unwrap();
        assert_eq!(drawn.section_text(0), Some("late thought"));
        assert_eq!(drawn.section_text(1), Some("partial"));
        app.update(at(8, reply(7, "late thought", "partial", true)), 0);
        assert!(text(&app).contains("incomplete"), "{}", text(&app));
        assert_eq!(app.transcript().len(), 4);
        // Opened mid-stream, the window drew only the end: the logged
        // reply replaces it whole.
        turn(&mut app, 9, "once more");
        app.update(at(11, request(10, 900)), 0);
        app.update(delta(11, "", "the end"), 0);
        app.update(
            at(
                12,
                reply(11, "all of it", "from the start to the end", false),
            ),
            0,
        );
        let settled = app.transcript().message(5).unwrap();
        assert_eq!(settled.section_text(1), Some("from the start to the end"));
        // A process that fails mid-stream, not restarted, leaves what was
        // drawn, marked; a later delta of its request starts nothing.
        turn(&mut app, 13, "last");
        app.update(at(15, request(14, 900)), 0);
        app.update(delta(15, "", "half"), 0);
        assert!(text(&app).contains("streaming"), "{}", text(&app));
        app.update(Update::Failed { reason: "x".into() }, 0);
        let shown = text(&app);
        assert!(!shown.contains("streaming"), "{shown}");
        assert!(shown.contains("interrupted"), "{shown}");
        assert_eq!(app.transcript().len(), 8);
    }

    /// A transcript at its limit mid-stream makes room as a pushed message
    /// does, and a reply it still cannot hold is marked, not left streaming.
    #[test]
    fn a_stream_at_the_transcripts_limit_makes_room() {
        let mut app = app();
        // Sixteen messages of just under a mebibyte: the transcript's 16
        // MiB all but full.
        let big = "x".repeat(messages::MAX_TEXT_BYTES - 1024);
        let full = (messages::MAX_TOTAL_BYTES / big.len()) as u64;
        for seq in 1..=full {
            app.update(user(seq, &big), 0);
        }
        assert_eq!(app.transcript().len() as u64, full);
        turn(&mut app, full + 1, "hello");
        let request = full + 3;
        app.update(at(request, self::request(full + 2, 900)), 0);
        let piece = "y".repeat(64 * 1024);
        for _ in 0..8 {
            app.update(delta(request, "", &piece), 0);
        }
        assert!(app.transcript().len() as u64 <= full, "evicted");
        let last = app.transcript().len() - 1;
        let whole = piece.repeat(8);
        assert_eq!(
            app.transcript().message(last).unwrap().section_text(0),
            Some(whole.as_str()),
            "drawn whole"
        );
        app.update(at(request + 1, reply(request, "", &whole, false)), 0);
        assert_eq!(app.transcript().len() - 1, last, "kept as drawn");
        let shown = text(&app);
        assert!(!shown.contains("streaming"), "settled");
        assert!(!shown.contains("cut short"), "settled");
    }

    /// Escape is the window's while a turn runs, asking to interrupt it,
    /// and the focused widget's otherwise.
    #[test]
    fn escape_interrupts_a_running_turn_and_only_then() {
        let mut app = app();
        key(&mut app, "Escape");
        assert!(app.take_requests().is_empty());
        turn(&mut app, 1, "hello");
        key(&mut app, "Escape");
        assert_eq!(app.take_requests(), [Request::Interrupt]);
        assert_eq!(app.notice(), Some("interrupting the turn"));
        app.update(
            at(
                3,
                Kind::Finished {
                    started: 2,
                    outcome: crate::client::INTERRUPTED.into(),
                    retry: true,
                },
            ),
            0,
        );
        key(&mut app, "Escape");
        assert!(app.take_requests().is_empty());
        let shown = text(&app);
        assert!(
            shown.contains("not every provider stops generating"),
            "{shown}"
        );
        assert!(app.status_line().contains("C-r asks again"));
    }

    #[test]
    fn a_failed_turn_says_why_and_c_r_asks_again() {
        let mut app = app();
        turn(&mut app, 1, "hello");
        app.update(at(3, request(2, cost::ONE)), 0);
        app.update(
            at(
                4,
                Kind::Finished {
                    started: 2,
                    outcome: "error 502: bad gateway".into(),
                    retry: true,
                },
            ),
            0,
        );
        let shown = text(&app);
        assert!(shown.contains("not answered"), "{shown}");
        assert!(shown.contains("error 502: bad gateway"), "{shown}");
        assert!(app.status_line().contains("C-r asks again"));
        key(&mut app, "C-r");
        assert_eq!(app.take_requests(), [Request::Retry]);
        key(&mut app, "C-r");
        assert!(app.take_requests().is_empty(), "asked once");
        // An interrupted request counts its whole reservation.
        app.update(at(5, Kind::Interrupted { started: 3 }), 0);
        assert!(
            app.status_line().contains("cost $1.0000"),
            "{}",
            app.status_line()
        );
        // One whose usage was logged before its process died counts that
        // cost alone, not its reservation as well.
        app.update(at(6, request(2, cost::ONE)), 0);
        app.update(
            at(
                7,
                Kind::Usage {
                    request: 6,
                    tokens: cost::Tokens::default(),
                    cost: cost::ONE / 4,
                    basis: crate::store::Basis::Reported,
                },
            ),
            0,
        );
        app.update(at(8, Kind::Interrupted { started: 6 }), 0);
        assert!(
            app.status_line().contains("cost $1.2500"),
            "{}",
            app.status_line()
        );
    }

    #[test]
    fn a_conversation_left_mid_turn_shows_running_until_its_turn_ends() {
        let mut app = app();
        let state = |app: &App, n: u8| app.rows().iter().find(|r| r.id == id(n)).unwrap().state;
        turn(&mut app, 1, "hello");
        app.background(&id(3), &Update::Failed { reason: "x".into() }, 0);
        app.set_active(id(2));
        assert_eq!(state(&app, 1), RowState::Running, "left mid-turn");
        assert_eq!(state(&app, 3), RowState::Failed, "kept across a switch");
        assert_eq!(state(&app, 2), RowState::Starting);
        // A request's finish is not the turn's.
        let finished = |started| {
            at(
                9,
                Kind::Finished {
                    started,
                    outcome: "stop".into(),
                    retry: false,
                },
            )
        };
        app.background(&id(1), &finished(5), 700);
        assert_eq!(state(&app, 1), RowState::Running);
        app.background(&id(1), &finished(2), 800);
        assert_eq!(state(&app, 1), RowState::Closed, "its process is let go");
        // Adopted after its turn ended but before the window heard so:
        // the log as read says idle.
        app.set_active(id(1));
        let event = |seq, kind| Event { seq, time: 0, kind };
        app.replay(
            Ok(""),
            vec![
                event(
                    1,
                    Kind::User {
                        delivery: String::new(),
                        text: "hello".into(),
                    },
                ),
                event(
                    2,
                    Kind::Started {
                        effect: crate::store::Effect::Turn,
                        of: 1,
                    },
                ),
                event(
                    3,
                    Kind::Finished {
                        started: 2,
                        outcome: "replied".into(),
                        retry: false,
                    },
                ),
            ],
        );
        assert_eq!(state(&app, 1), RowState::Idle);
    }

    #[test]
    fn a_conversation_in_the_background_shows_its_state_in_its_row() {
        let mut app = app();
        let started = at(
            9,
            Kind::Started {
                effect: crate::store::Effect::Turn,
                of: 8,
            },
        );
        app.background(&id(2), &started, 500);
        let row = |app: &App| app.rows().iter().find(|r| r.id == id(2)).unwrap().clone();
        assert_eq!(row(&app).state, RowState::Running);
        app.background(
            &id(2),
            &Update::Up(Up::Title {
                title: "named".into(),
            }),
            500,
        );
        app.background(
            &id(2),
            &at(
                10,
                Kind::Finished {
                    started: 9,
                    outcome: "replied".into(),
                    retry: false,
                },
            ),
            600,
        );
        assert_eq!(row(&app).title, "named");
        assert_eq!(row(&app).activity, 600);
        assert_eq!(app.transcript().len(), 0, "nothing in the open transcript");
        // Opened again while it runs: the log as read shows it, running.
        app.set_active(id(2));
        app.replay(
            Ok(""),
            vec![
                Event {
                    seq: 8,
                    time: 0,
                    kind: Kind::User {
                        delivery: String::new(),
                        text: "from the log".into(),
                    },
                },
                Event {
                    seq: 9,
                    time: 0,
                    kind: Kind::Started {
                        effect: crate::store::Effect::Turn,
                        of: 8,
                    },
                },
            ],
        );
        assert_eq!(row(&app).state, RowState::Running);
        assert!(text(&app).contains("from the log"));
        // The process's copy of an event the log gave is not shown twice.
        app.update(
            at(
                8,
                Kind::User {
                    delivery: String::new(),
                    text: "from the log".into(),
                },
            ),
            0,
        );
        assert_eq!(app.transcript().len(), 1);
        app.background(&id(3), &Update::Failed { reason: "x".into() }, 0);
        assert_eq!(
            app.rows().iter().find(|r| r.id == id(3)).unwrap().state,
            RowState::Failed
        );
    }

    fn todo_item(content: &str, status: Status) -> TodoItem {
        TodoItem {
            content: content.into(),
            status,
        }
    }

    #[test]
    fn the_todo_list_shows_above_the_composer_collapsed_to_its_item_in_progress() {
        let mut app = app();
        let before = app.regions.unwrap().transcript.height;
        key(&mut app, "C-t");
        assert!(app.notice().unwrap().contains("no todo list"));
        app.update(
            at(
                1,
                Kind::Todo {
                    items: vec![
                        todo_item("Read the design", Status::Done),
                        todo_item("Write the tests", Status::InProgress),
                        todo_item("Land it", Status::Pending),
                    ],
                    cleared: false,
                },
            ),
            0,
        );
        let regions = app.regions.unwrap();
        assert!(regions.todo.height > 0 && regions.transcript.height < before);
        assert_eq!(
            regions.todo.y + i64::from(regions.todo.height),
            regions.rule.y
        );
        assert_eq!(
            app.todo_lines(),
            ["todo 1/3: [>] Write the tests | C-t shows all, C-S-t clears"]
        );
        assert!(text(&app).contains("[>] Write the tests"), "drawn");
        key(&mut app, "C-t");
        assert_eq!(
            app.todo_lines(),
            ["[x] Read the design", "[>] Write the tests", "[ ] Land it"]
        );
        assert!(app.regions.unwrap().todo.height > regions.todo.height);
        key(&mut app, "C-S-t");
        assert_eq!(app.take_requests(), [Request::ClearTodo]);
        app.update(
            at(
                2,
                Kind::Todo {
                    items: Vec::new(),
                    cleared: true,
                },
            ),
            0,
        );
        assert!(app.todo().is_empty());
        assert_eq!(app.regions.unwrap().todo.height, 0);
        assert_eq!(app.regions.unwrap().transcript.height, before);
        assert!(text(&app).contains("you cleared the todo list"));
        // Nothing to clear asks nothing.
        key(&mut app, "C-S-t");
        assert!(app.take_requests().is_empty());
    }

    #[test]
    fn c_z_undoes_the_latest_step_and_c_s_z_redoes_it_between_turns() {
        let mut app = app();
        key(&mut app, "C-z");
        assert!(app.take_requests().is_empty());
        assert!(app.notice().unwrap().contains("no step to undo"));
        let snapshot = |seq: u64| {
            at(
                seq,
                Kind::Snapshot {
                    reply: seq - 1,
                    background: if seq == 5 { vec![1, 3] } else { Vec::new() },
                    worktrees: vec![crate::store::Snapped {
                        checkout: "/w/td".into(),
                        before: "a".repeat(40),
                        after: "b".repeat(40),
                        changed: vec!["src/x\u{1b}.rs".into()],
                        more: 2,
                    }],
                },
            )
        };
        app.update(snapshot(3), 0);
        app.update(snapshot(5), 0);
        assert!(text(&app).contains("this step changed /w/td"));
        assert!(text(&app).contains("background processes were running (p1, p3)"));
        key(&mut app, "C-z");
        assert_eq!(app.take_requests(), [Request::Undo(5)]);
        // Only between turns.
        app.update(
            at(
                6,
                Kind::Started {
                    effect: crate::store::Effect::Turn,
                    of: 1,
                },
            ),
            0,
        );
        key(&mut app, "C-z");
        assert!(app.take_requests().is_empty());
        assert!(app.notice().unwrap().contains("between turns"));
        app.update(
            at(
                7,
                Kind::Finished {
                    started: 6,
                    outcome: "replied".into(),
                    retry: false,
                },
            ),
            0,
        );
        app.update(
            at(
                8,
                Kind::Restore {
                    step: 5,
                    undo: true,
                },
            ),
            0,
        );
        assert!(text(&app).contains("you undid the step snapshotted at #5"));
        // A background process's start is its call's result, and its
        // row under its conversation's; its end is a notice.
        let shown = app.transcript.len();
        app.update(
            at(
                9,
                Kind::Process {
                    number: 1,
                    call: 2,
                    command: "make".into(),
                },
            ),
            0,
        );
        assert_eq!(app.transcript.len(), shown);
        assert!(text(&app).contains("p1 make"));
        assert!(app.status_line().ends_with("| 1 background"));
        app.update(
            at(
                10,
                Kind::Ended {
                    number: 1,
                    how: "exit status 2".into(),
                    tail: None,
                    held: None,
                },
            ),
            0,
        );
        assert!(text(&app).contains("background process p1 ended: exit status 2"));
        assert!(!text(&app).contains("p1 make"));
        assert!(app.status_line().ends_with("| 0 background"));
        key(&mut app, "C-z");
        assert_eq!(app.take_requests(), [Request::Undo(3)]);
        key(&mut app, "C-S-z");
        assert_eq!(app.take_requests(), [Request::Redo(5)]);
        // A new step leaves nothing to redo.
        app.update(snapshot(11), 0);
        key(&mut app, "C-S-z");
        assert!(app.take_requests().is_empty());
        assert!(app.notice().unwrap().contains("no step to redo"));
    }

    #[test]
    fn c_s_p_pauses_and_resumes_and_the_list_says_paused() {
        let mut app = app();
        app.update(
            Update::Up(Up::Hello {
                title: "t".into(),
                torn: None,
                interrupted: Vec::new(),
                paused: false,
                prefix: Some(String::new()),
            }),
            0,
        );
        key(&mut app, "C-S-p");
        assert_eq!(app.take_requests(), [Request::Pause(true)]);
        app.update(at(1, Kind::Pause { paused: true }), 0);
        assert!(
            app.status_line().starts_with("paused"),
            "{}",
            app.status_line()
        );
        assert_eq!(app.directory()[0].state, "paused");
        assert!(text(&app).contains("messages from other conversations wait"));
        key(&mut app, "C-S-p");
        assert_eq!(app.take_requests(), [Request::Pause(false)]);
        app.update(at(2, Kind::Pause { paused: false }), 0);
        assert!(
            app.status_line().starts_with("idle"),
            "{}",
            app.status_line()
        );
        // A conversation in the background that pauses shows it in its row.
        app.background(&id(2), &at(5, Kind::Pause { paused: true }), 0);
        let row = app.rows().iter().find(|r| r.id == id(2)).unwrap();
        assert_eq!(row.word(), "paused");
        let entry = app.directory().into_iter().find(|e| e.id == id(3)).unwrap();
        assert_eq!(
            entry.state, "idle",
            "a closed conversation is idle to the tools"
        );
    }

    #[test]
    fn messages_tool_calls_and_results_show_in_the_transcript() {
        let mut app = app();
        app.update(
            at(
                1,
                Kind::Message {
                    delivery: "d".into(),
                    from: id(3),
                    role: Role::Conversation,
                    text: "the build is green".into(),
                    status: Some("done".into()),
                    held: Some(Held::Budget),
                },
            ),
            0,
        );
        app.update(
            at(
                2,
                Kind::Started {
                    effect: crate::store::Effect::Turn,
                    of: 1,
                },
            ),
            0,
        );
        app.update(at(3, request(2, 0)), 0);
        app.update(
            at(
                4,
                Kind::Assistant {
                    request: 3,
                    content: None,
                    reasoning: None,
                    details: None,
                    finish: "tool_calls".into(),
                    incomplete: false,
                    calls: vec![crate::store::Call {
                        id: "c1".into(),
                        name: "history_search".into(),
                        arguments: "{\"query\":\"green\"}".into(),
                    }],
                },
            ),
            0,
        );
        app.update(
            at(
                5,
                Kind::ToolResult {
                    reply: 4,
                    id: "c1".into(),
                    name: "history_search".into(),
                    call: 0,
                    content: "No event matches.".into(),
                    error: true,
                    kept: None,
                    digest: None,
                },
            ),
            0,
        );
        let shown = text(&app);
        for said in [
            "report (done) from title 3",
            "the build is green",
            "held: wake budget",
            "running",
            "(tool calls)",
            "history_search({\"query\":\"green\"})",
            "tool history_search",
            "No event matches.",
            "error",
        ] {
            assert!(shown.contains(said), "{said} in {shown}");
        }
        // A notice in the background is said in the status row.
        app.background(
            &id(2),
            &at(
                9,
                Kind::Notice {
                    text: crate::wake::notice(),
                },
            ),
            0,
        );
        assert!(app
            .notice()
            .unwrap()
            .starts_with("title 2: messages from other"));
    }

    #[test]
    fn a_streamed_reply_that_calls_tools_shows_its_calls_once_logged() {
        let mut app = app();
        turn(&mut app, 1, "plan");
        app.update(at(3, request(2, 0)), 0);
        app.update(delta(3, "", "I will plan."), 0);
        app.update(
            at(
                4,
                Kind::Assistant {
                    request: 3,
                    content: Some("I will plan.".into()),
                    reasoning: None,
                    details: None,
                    finish: "tool_calls".into(),
                    incomplete: false,
                    calls: vec![crate::store::Call {
                        id: "c1".into(),
                        name: "todo_write".into(),
                        arguments: "{\"items\":[]}".into(),
                    }],
                },
            ),
            0,
        );
        let shown = text(&app);
        assert!(shown.contains("todo_write({\"items\":[]})"), "{shown}");
        assert_eq!(shown.matches("I will plan.").count(), 1, "{shown}");
    }

    #[test]
    fn a_window_too_small_for_the_split_says_so() {
        let surface = Surface::new(200, 200, Scale::default()).unwrap();
        let app = App::new(surface, None, Mode::Auto).unwrap();
        assert!(text(&app).contains("too small"));
    }

    const KEY: &str = crate::keydialog::tests::KEY;
    const PATH: &str = "/home/me/.config/td-agent/openrouter.key";

    fn press(app: &mut App, x: i64, y: i64) {
        for phase in [PointerPhase::Press, PointerPhase::Release] {
            app.input(
                Input::Pointer {
                    phase,
                    x,
                    y,
                    extend: false,
                    follow: false,
                },
                &mut NoClipboard,
            );
        }
    }

    /// The app with the key dialog open from the File menu.
    fn keyless() -> App {
        let mut app = app();
        app.set_key_path(Some(PATH.into()));
        app.set_keyed(false);
        key(&mut app, "F10");
        // Set OpenRouter key…, below New conversation….
        key(&mut app, "Down");
        key(&mut app, "Return");
        assert!(app.dialog().is_some());
        app
    }

    /// Everything the window shows or says of itself.
    fn said(app: &App) -> String {
        format!(
            "{}\n{}\n{}\n{:?}",
            text(app),
            app.status_line(),
            app.log.texts().collect::<Vec<_>>().join("\n"),
            app.dialog()
        )
    }

    #[test]
    fn the_bar_shows_file_and_the_split_lies_under_it() {
        let app = app();
        assert!(text(&app).starts_with(" File"), "{}", text(&app));
        let regions = app.regions.unwrap();
        assert_eq!(regions.list.y, ROW as i64);
        assert_eq!(
            regions.status.y + i64::from(regions.status.height),
            app.surface.height as i64
        );
    }

    /// Where conversation `n`'s row is drawn in the list.
    fn row_at(app: &App, n: u8) -> (i64, i64) {
        let index = app.rows().iter().position(|r| r.id == id(n)).unwrap();
        let at = app.list.model().find(index).unwrap();
        let rect = app.list.geometry().unwrap().row(at).unwrap();
        (rect.x + 8, rect.y + 4)
    }

    fn context(app: &mut App, (x, y): (i64, i64)) {
        app.input(Input::Context { x, y }, &mut NoClipboard);
    }

    /// The conversations the list shows, in its order.
    fn shown(app: &App) -> Vec<Id> {
        app.list
            .model()
            .rows()
            .iter()
            .map(|r| app.rows()[r.id].id.clone())
            .collect()
    }

    /// A conversation's background processes are rows under its own,
    /// counted on the status row, each with a menu to show its output or
    /// kill it; its output shows read-only (DESIGN.md §12).
    #[test]
    fn background_processes_are_listed_under_their_conversation() {
        let mut app = app();
        let process = |seq, number, command: &str| {
            at(
                seq,
                Kind::Process {
                    number,
                    call: 1,
                    command: command.into(),
                },
            )
        };
        app.background(&id(3), &process(4, 1, "make\tall"), 0);
        app.background(&id(3), &process(5, 2, "cargo test"), 0);
        app.update(process(6, 1, "sleep 60"), 0);
        assert!(app.status_line().ends_with("| 3 background"));
        let three = app.rows().iter().position(|r| r.id == id(3)).unwrap();
        let rows = app.list.model().rows();
        let place = rows.iter().position(|r| r.id == three).unwrap();
        let children: Vec<usize> = rows
            .get(place + 1..place + 3)
            .unwrap()
            .iter()
            .map(|r| {
                assert_eq!((r.parent, r.depth), (Some(three), 1));
                r.id
            })
            .collect();
        let first = *children.first().unwrap();
        assert_eq!(app.cell(first, 0).text(), "p1 make<U+0009>all");
        assert_eq!(app.cell(first, 1).text(), "running");
        // Its menu from the keyboard: Show output, then Kill.
        app.set_focus(Focus::List);
        app.list.select(Some(first), true);
        key(&mut app, "S-F10");
        assert!(app.menu_open());
        assert!(text(&app).contains(menu::SHOW_OUTPUT) && text(&app).contains(menu::KILL_PROCESS));
        key(&mut app, "Down");
        key(&mut app, "Return");
        assert_eq!(
            app.take_requests(),
            [Request::Kill {
                id: id(3),
                number: 1
            }]
        );
        // Return on it shows its output; no conversation opens.
        key(&mut app, "Return");
        assert_eq!(
            app.take_requests(),
            [Request::Output {
                id: id(3),
                number: 1
            }]
        );
        app.show_output(
            &id(3),
            1,
            Ok(crate::output::Output {
                text: "built\n\u{1b}[0m".into(),
                from: 4,
                to: 15,
                start: 4,
                total: 15,
            }),
        );
        let shown = text(&app);
        assert!(shown.contains("p1 output: make<U+0009>all"), "{shown}");
        assert!(shown.contains("bytes 4 to 15 of 15 written, the first 4 dropped; lines 1 to 2"));
        assert_eq!(app.notes.as_ref().unwrap().name(), "process output");
        assert!(app.workspace_card().is_none());
        assert!(shown.contains("<U+001B>[0m"), "{shown}");
        key(&mut app, "Escape");
        assert!(!text(&app).contains("p1 output"));
        // A long output shows to its last line, entry by entry.
        let long: Vec<String> = (1..=100).map(|n| format!("line {n}")).collect();
        app.show_output(
            &id(3),
            1,
            Ok(crate::output::Output {
                text: long.join("\n"),
                ..crate::output::Output::default()
            }),
        );
        assert_eq!(app.notes.as_ref().unwrap().len(), 3);
        key(&mut app, "Escape");
        // By the pointer: a press selects it, a second shows its output.
        let (x, y) = {
            let at = app.list.model().find(first).unwrap();
            let rect = app.list.geometry().unwrap().row(at).unwrap();
            (rect.x + 8, rect.y + 4)
        };
        app.list.select(None, true);
        press(&mut app, x, y);
        assert!(app.take_requests().is_empty());
        assert_eq!(app.active(), Some(&id(1)));
        press(&mut app, x, y);
        assert_eq!(
            app.take_requests(),
            [Request::Output {
                id: id(3),
                number: 1
            }]
        );
        // Its end takes its row away; its process failing takes them all.
        app.background(
            &id(3),
            &at(
                7,
                Kind::Ended {
                    number: 1,
                    how: "killed".into(),
                    tail: None,
                    held: None,
                },
            ),
            0,
        );
        assert!(!text(&app).contains("p1 make"));
        assert!(app.status_line().ends_with("| 2 background"));
        app.background(&id(3), &Update::Failed { reason: "x".into() }, 0);
        assert!(app.status_line().ends_with("| 1 background"));
        // Archiving or deleting a conversation ends its processes, which
        // say nothing more.
        app.background(&id(2), &process(8, 1, "sleep 60"), 0);
        assert!(app.status_line().ends_with("| 2 background"));
        app.set_archived(&id(2), true);
        assert!(app.status_line().ends_with("| 1 background"));
        // A row removed keeps the selected process selected; its end
        // leaves its conversation selected.
        app.background(&id(3), &process(9, 3, "sleep 60"), 0);
        app.set_archived(&id(2), false);
        app.background(&id(2), &process(10, 7, "sleep 60"), 0);
        app.background(&id(2), &process(11, 8, "sleep 60"), 0);
        let seven = app
            .process_items
            .iter()
            .position(|item| *item == (id(2), 7))
            .unwrap()
            + app.process_base;
        app.list.select(Some(seven), true);
        app.remove_row(&id(3));
        let selected = app.list.selected().unwrap();
        assert_eq!(app.process_item(selected), Some(&(id(2), 7)));
        assert!(app.status_line().ends_with("| 3 background"));
        app.background(
            &id(2),
            &at(
                12,
                Kind::Ended {
                    number: 7,
                    how: "killed".into(),
                    tail: None,
                    held: None,
                },
            ),
            0,
        );
        let selected = app.list.selected().unwrap();
        assert_eq!(app.rows().get(selected).map(|r| &r.id), Some(&id(2)));
        // The open one's hello starts its count again from its log; the
        // other's stays.
        assert!(app.status_line().ends_with("| 2 background"));
        app.update(
            Update::Up(Up::Hello {
                title: "t".into(),
                torn: None,
                interrupted: Vec::new(),
                paused: false,
                prefix: Some(String::new()),
            }),
            0,
        );
        assert!(app.status_line().ends_with("| 1 background"));
    }

    #[test]
    fn a_rows_menu_archives_unarchives_and_deletes_its_conversation() {
        let mut app = app();
        assert_eq!(shown(&app), [id(1), id(3), id(2)]);
        // A right press on a row opens its menu at the pointer, and not
        // its conversation.
        let at = row_at(&app, 3);
        context(&mut app, at);
        assert!(app.menu_open());
        assert_eq!(app.menu_row(), Some(&id(3)));
        assert_eq!(app.active(), Some(&id(1)));
        assert!(text(&app).contains(menu::ARCHIVE), "{}", text(&app));
        key(&mut app, "Return");
        assert!(!app.menu_open());
        assert_eq!(
            app.take_requests(),
            [Request::Archive {
                id: id(3),
                archived: true
            }]
        );
        // Archived, the list hides it and C-PageDown passes over it.
        app.set_archived(&id(3), true);
        assert_eq!(shown(&app), [id(1), id(2)]);
        key(&mut app, "C-PageDown");
        assert_eq!(app.take_requests(), [Request::Open(id(2))]);
        // Show archived shows it, archived, and opening it is refused.
        app.menu_action(menu::Action::ShowArchived);
        assert!(app.shows_archived());
        assert_eq!(shown(&app), [id(1), id(3), id(2)]);
        let at = app.rows().iter().position(|r| r.id == id(3)).unwrap();
        assert_eq!(app.cell(at, 1).text(), "archived");
        let (x, y) = row_at(&app, 3);
        press(&mut app, x, y);
        assert!(app.take_requests().is_empty());
        assert!(app.notice().unwrap().contains("is archived"));
        assert_eq!(app.active(), Some(&id(2)));
        assert_eq!(app.list.selected(), app.shown_active());
        // From the keyboard, S-F10 in the list opens the selected row's,
        // archived or not, and closes it again.
        app.set_focus(Focus::List);
        let three = app.rows().iter().position(|r| r.id == id(3)).unwrap();
        app.list.select(Some(three), true);
        // An update to the list keeps the selection where it is.
        app.set_rows(app.rows().to_vec());
        key(&mut app, "S-F10");
        assert_eq!(app.menu_row(), Some(&id(3)));
        assert!(text(&app).contains(menu::UNARCHIVE));
        key(&mut app, "S-F10");
        assert!(!app.menu_open());
        // Its menu unarchives it, or asks whether to delete it.
        let at = row_at(&app, 3);
        context(&mut app, at);
        assert!(text(&app).contains(menu::UNARCHIVE));
        key(&mut app, "Return");
        assert_eq!(
            app.take_requests(),
            [Request::Archive {
                id: id(3),
                archived: false
            }]
        );
        let at = row_at(&app, 3);
        context(&mut app, at);
        key(&mut app, "Down");
        key(&mut app, "Return");
        assert!(matches!(
            app.confirm().map(Confirm::purpose),
            Some(confirm::Purpose::Delete(of)) if *of == id(3)
        ));
        key(&mut app, "Escape");
        assert!(app.confirm().is_none() && app.take_requests().is_empty());
        // The bar's menu is back at F10.
        key(&mut app, "F10");
        assert!(app.menu_row().is_none());
        assert!(text(&app).contains("New conversation"));
        key(&mut app, "Escape");
        // Away from the list, S-F10 opens the open conversation's under
        // its row.
        app.set_focus(Focus::Composer);
        key(&mut app, "S-F10");
        assert_eq!(app.menu_row(), Some(&id(2)));
        assert!(app.menu.panel(0).unwrap().y > row_at(&app, 2).1);
        key(&mut app, "Escape");
        // A right press off the rows opens nothing.
        let list = app.regions.unwrap().list;
        context(&mut app, (list.x + 8, list.y + list.height as i64 - 4));
        assert!(!app.menu_open());
        // Archiving the open one closes it, and the most recent is the
        // most recent not archived.
        app.set_archived(&id(3), false);
        app.set_archived(&id(1), true);
        app.set_archived(&id(2), true);
        assert_eq!(app.active(), None);
        assert_eq!(app.most_recent(), Some(id(3)));
    }

    #[test]
    fn f10_or_a_press_on_file_opens_the_menu_and_its_items_act() {
        let mut app = app();
        key(&mut app, "F10");
        assert!(app.menu_open());
        assert!(text(&app).contains("New conversation"), "{}", text(&app));
        assert!(text(&app).contains("Set OpenRouter key\u{2026}"));
        // The open menu takes every key: C-n is consumed, not a new
        // conversation.
        key(&mut app, "C-n");
        assert!(app.take_requests().is_empty());
        key(&mut app, "Return");
        assert!(!app.menu_open());
        assert_eq!(app.picking(), "template");
        key(&mut app, "Escape");
        assert!(app.take_requests().is_empty());
        // The chord the item shows, with the menu closed, does the same.
        key(&mut app, "C-n");
        assert_eq!(app.picking(), "template");
        key(&mut app, "Escape");
        // A press on the header opens it; one on the third row exports,
        // and one on the fifth quits.
        for (row, request) in [(2, Request::Export), (4, Request::Quit)] {
            press(&mut app, CELL_WIDTH as i64 + 4, 4);
            assert!(app.menu_open());
            let panel = app.menu.panel(0).unwrap();
            press(&mut app, panel.x + 8, panel.y + (row * ROW) as i64 + 4);
            assert_eq!(app.take_requests(), [request]);
        }
        // A right press closes it and goes no further.
        key(&mut app, "F10");
        app.input(Input::Context { x: 600, y: 300 }, &mut NoClipboard);
        assert!(!app.menu_open());
        assert!(app.take_requests().is_empty());
        // Escape and F10 close it, and a press outside closes it and
        // goes no further.
        for chord in ["Escape", "F10"] {
            key(&mut app, "F10");
            key(&mut app, chord);
            assert!(!app.menu_open(), "{chord}");
        }
        key(&mut app, "F10");
        let transcript = app.regions.unwrap().transcript;
        press(&mut app, transcript.x + 40, transcript.y + 40);
        assert!(!app.menu_open());
        assert_eq!(app.focus(), Focus::Composer, "the press went no further");
        let requests = app.take_requests();
        assert!(
            requests.is_empty(),
            "the press opened nothing: {requests:?}"
        );
    }

    /// Whether any of a live press and release at `x`, `y` chose Help →
    /// Keys.
    fn press_live(app: &mut App, x: i64, y: i64) -> bool {
        [PointerPhase::Press, PointerPhase::Release]
            .into_iter()
            .fold(false, |chosen, phase| {
                let input = Input::Pointer {
                    phase,
                    x,
                    y,
                    extend: false,
                    follow: false,
                };
                app.input_live(input, &mut NoClipboard) | chosen
            })
    }

    fn key_live(app: &mut App, chord: &str) -> bool {
        let input = Input::Key {
            chord,
            repeat: false,
        };
        app.input_live(input, &mut NoClipboard)
    }

    #[test]
    fn help_keys_by_the_live_pointer_or_keyboard_asks_for_the_key_list() {
        let mut app = app();
        let header = menu::bar(app.surface).header(2).unwrap();
        assert!(!press_live(&mut app, header.x + 4, header.y + 4));
        assert!(app.menu_open());
        assert!(text(&app).contains(keys::ITEM), "{}", text(&app));
        let panel = app.menu.panel(0).unwrap();
        assert!(press_live(&mut app, panel.x + 8, panel.y + 4));
        assert!(!app.menu_open());
        assert!(app.take_requests().is_empty());
        // The keyboard's way: F10, Right past Conversation to Help, Return.
        assert!(!key_live(&mut app, "F10"));
        assert!(!key_live(&mut app, "Right"));
        assert!(!key_live(&mut app, "Right"));
        assert!(key_live(&mut app, "Return"));
        assert!(!app.menu_open());
        // Asked once: the next input asks nothing.
        assert!(!key_live(&mut app, "Right"));
    }

    /// Conversation → Auto mode in this workspace by the live keyboard
    /// asks for the mode; through the control seam's keys, nothing.
    #[test]
    fn only_the_live_keyboard_sets_a_workspaces_mode() {
        use td_ui::driven;
        let mut app = app();
        app.add_row(Row {
            workspace: Some(Workspace::Scratch),
            ..row(9, 1)
        });
        app.set_active(id(9));
        const CHORDS: &[&str] = &["F10", "Right", "Down", "Down", "Return"];
        let hex = |text: &str| -> String { text.bytes().map(|b| format!("{b:02x}")).collect() };
        let mut remote = crate::control::Remote { app: &mut app };
        for (n, chord) in CHORDS.iter().enumerate() {
            let line = format!("1\t{n}\tkey\t{}", hex(chord));
            driven::request(&mut remote, line.as_bytes());
        }
        assert!(!app.menu_open());
        assert!(app.take_requests().is_empty());
        for chord in CHORDS {
            key_live(&mut app, chord);
        }
        assert_eq!(
            app.take_requests(),
            [Request::SetMode {
                conversation: id(9),
                mode: Mode::Ask
            }]
        );
    }

    /// Help → Keys through the control seam's keys or pointer chooses the
    /// item, but the window's next input reports no choice: only one made
    /// inside `input_live` is, and the seam delivers through `input`.
    #[test]
    fn help_keys_through_the_control_seam_asks_for_no_key_list() {
        use td_ui::driven;
        let mut app = app();
        let hex = |text: &str| -> String { text.bytes().map(|b| format!("{b:02x}")).collect() };
        let mut remote = crate::control::Remote { app: &mut app };
        for (n, chord) in ["F10", "Right", "Right", "Return"].iter().enumerate() {
            let line = format!("1\t{n}\tkey\t{}", hex(chord));
            assert!(driven::request(&mut remote, line.as_bytes()).ends_with("changed"));
        }
        assert!(app.keys_chosen, "the seam's keys chose the item");
        assert!(!app.menu_open());
        assert!(!app.input_live(Input::Focus(true), &mut NoClipboard));
        let header = menu::bar(app.surface).header(2).unwrap();
        let mut lines = vec![(header.x + 4, header.y + 4)];
        let mut opened = menu::menu(app.surface, menu::State::default(), 1).unwrap();
        opened.open_bar(2).unwrap();
        let panel = opened.panel(0).unwrap();
        lines.push((panel.x + 8, panel.y + 4));
        let mut remote = crate::control::Remote { app: &mut app };
        for (n, (x, y)) in lines.into_iter().enumerate() {
            for phase in ["press", "release"] {
                let line = format!("1\t{}\tpointer\t{phase}\t{x}\t{y}", 10 + n);
                driven::request(&mut remote, line.as_bytes());
            }
        }
        assert!(app.keys_chosen, "the seam's pointer chose the item");
        assert!(!app.input_live(Input::Focus(true), &mut NoClipboard));
        assert!(app.take_requests().is_empty());
    }

    /// The open conversation's repository workspace has its card behind
    /// the status row: `C-S-w` and the Conversation menu ask the session
    /// for what it shows, the answer opens it modal over the body, notes
    /// stay out of it, a prepared repository asks again, and `C-S-w`
    /// closes it. A conversation without one says so, and an answer
    /// for another conversation shows nothing.
    #[test]
    fn a_repository_workspace_has_its_card_behind_the_status_row() {
        use crate::repo::Instructions;
        use crate::workspace::{Entry, Repositories};
        let mut app = app();
        // None for a conversation with no repository workspace.
        assert!(!app.status_line().contains("workspace"));
        key(&mut app, "C-S-w");
        assert!(app.take_requests().is_empty());
        assert!(app.notice().unwrap().contains("no repository workspace"));
        let repositories = Repositories {
            template: "td".into(),
            name: "td-1".into(),
            entries: vec![Entry {
                remote: "https://github.com/timmydo/td".into(),
                base: "main".into(),
                branch: "td-agent/td-1".into(),
                sparse: None,
                store: "/s/td".into(),
                repository: "/r/td".into(),
                id: "td".into(),
                checkout: "/w/td-1/td".into(),
            }],
        };
        let mut rows = app.rows().to_vec();
        for row in &mut rows {
            if row.id == id(1) {
                row.workspace = Some(Workspace::Repositories(repositories.clone()));
            }
        }
        app.set_rows(rows);
        assert!(
            app.status_line().contains(" | workspace: C-S-w |"),
            "{}",
            app.status_line()
        );
        key(&mut app, "C-S-w");
        assert_eq!(app.take_requests(), [Request::Workspace(id(1))]);
        let record = crate::card::Record {
            instructions: Ok(vec![crate::store::Instructed {
                checkout: "/w/td-1/td".into(),
                base: "a".repeat(40),
                read: Instructions::Found {
                    name: "AGENTS.md".into(),
                    text: "Read DESIGN.md first.".into(),
                },
                rules: Default::default(),
            }]),
            prepared: Ok(Vec::new()),
            removed: false,
        };
        // Another conversation's answer shows nothing.
        app.show_workspace(&id(2), &record);
        assert!(app.workspace_card().is_none());
        app.show_workspace(&id(1), &record);
        let card = app.workspace_card().unwrap();
        assert_eq!(card.len(), 2);
        assert!(app.messages_window().is_none());
        let shown = text(&app);
        assert!(shown.contains("Workspace td-1:"), "{shown}");
        assert!(shown.contains("Read DESIGN.md first."), "{shown}");
        // A note is counted, not added to the card.
        let unread = app.unread();
        app.note("a note while the card is open");
        assert_eq!(app.workspace_card().unwrap().len(), 2);
        assert_eq!(app.unread(), unread + 1);
        // It is what was read when it opened: a prepared repository asks
        // nothing, and an answer over it shows nothing.
        app.update(
            Update::Up(Up::Prepared {
                remote: "https://github.com/timmydo/td".into(),
            }),
            0,
        );
        assert!(app.take_requests().is_empty());
        let prepared = crate::card::Record {
            prepared: Ok(vec!["/r/td".into()]),
            ..record
        };
        app.show_workspace(&id(1), &prepared);
        assert!(text(&app).contains("not checked out yet"));
        // A paste is dropped, naming it.
        app.input(Input::Paste("pasted"), &mut NoClipboard);
        assert!(app
            .notice()
            .unwrap()
            .contains("while the workspace card was open"));
        // Its chord closes it; C-S-m does not.
        key(&mut app, "C-S-m");
        assert!(app.workspace_card().is_some());
        key(&mut app, "C-S-w");
        assert!(app.workspace_card().is_none());
        assert!(app.take_requests().is_empty());
        // Opened again, it reads again.
        key(&mut app, "C-S-w");
        assert_eq!(app.take_requests(), [Request::Workspace(id(1))]);
        app.show_workspace(&id(1), &prepared);
        assert!(text(&app).contains("/w/td-1/td (main on branch td-agent/td-1): checked out"));
        key(&mut app, "Escape");
        assert!(app.workspace_card().is_none());
        // The Conversation menu's item asks for it too.
        app.menu_action(menu::Action::Workspace);
        assert_eq!(app.take_requests(), [Request::Workspace(id(1))]);
        // Over another modal, an answer shows nothing.
        key(&mut app, "C-S-m");
        app.show_workspace(&id(1), &prepared);
        assert!(app.messages_window().is_some());
        assert!(app.workspace_card().is_none());
    }

    #[test]
    fn notes_are_counted_in_the_row_and_read_whole_in_the_messages_window() {
        let mut app = app();
        let long = format!("a refusal said by name: {}", "why ".repeat(200));
        app.note(long.clone());
        app.note("a second note");
        assert_eq!(app.unread(), 2);
        assert!(app.status_line().contains("| 2 new messages: C-S-m |"));
        assert!(!app.status_line().contains("refusal"));
        // C-S-m opens it, modal: every note whole, none unread after.
        key(&mut app, "C-S-m");
        let window = app.messages_window().unwrap();
        assert_eq!(window.len(), 2);
        assert_eq!(app.unread(), 0);
        assert!(!app.status_line().contains("new message"));
        assert!(text(&app).contains(crate::notes::TITLE), "{}", text(&app));
        // The window has every key: C-n starts nothing.
        key(&mut app, "C-n");
        assert!(app.take_requests().is_empty() && app.picker().is_none());
        // A note while it is open joins it and is read.
        app.note("a third note");
        assert_eq!(app.messages_window().unwrap().len(), 3);
        assert_eq!(app.unread(), 0);
        // A paste is not the window's: dropped, and said.
        app.input(Input::Paste("x"), &mut NoClipboard);
        assert!(app.notice().unwrap().contains("Messages window was open"));
        assert_eq!(app.messages_window().unwrap().len(), 4);
        key(&mut app, "Escape");
        assert!(app.messages_window().is_none());
        assert_eq!(app.focus(), Focus::Composer);
        // File → Messages… opens it too, and C-S-m closes it.
        app.menu_action(menu::Action::Messages);
        assert!(app.messages_window().is_some());
        key(&mut app, "C-S-m");
        assert!(app.messages_window().is_none());
        // Over another modal it does not open.
        key(&mut app, "C-n");
        key(&mut app, "C-S-m");
        assert!(app.messages_window().is_none() && app.picker().is_some());
        key(&mut app, "Escape");
        // Held, its chord opens it once and closes nothing.
        key(&mut app, "C-S-m");
        app.input(
            Input::Key {
                chord: "C-S-m",
                repeat: true,
            },
            &mut NoClipboard,
        );
        assert!(app.messages_window().is_some());
        // A window too narrow for the split still shows it, and it has
        // the keys.
        let narrow = Surface::new(400, 640, Scale::default()).unwrap();
        app.resize(narrow);
        assert!(app.regions.is_none());
        assert!(text(&app).contains("Messages:"), "{}", text(&app));
        key(&mut app, "Escape");
        assert!(app.messages_window().is_none());
        app.resize(Surface::new(1024, 640, Scale::default()).unwrap());
        // The key dialog replaces it: one modal at a time.
        app.set_key_path(Some(PATH.into()));
        key(&mut app, "C-S-m");
        app.open_key_dialog();
        assert!(app.dialog().is_some() && app.messages_window().is_none());
    }

    #[test]
    fn a_note_from_a_conversation_in_the_background_is_kept_with_its_title() {
        let mut app = app();
        app.background(
            &id(2),
            &Update::Up(Up::Event(Event {
                seq: 9,
                time: 0,
                kind: Kind::Notice {
                    text: crate::wake::notice(),
                },
            })),
            0,
        );
        assert_eq!(app.unread(), 1);
        assert!(
            app.notice().unwrap().starts_with("title 2: "),
            "{:?}",
            app.notice()
        );
    }

    #[test]
    fn without_a_key_the_status_row_says_so_until_one_is_stored() {
        let mut app = keyless();
        assert!(app.status_line().contains("| no key: F10 |"));
        assert!(text(&app).contains("Set OpenRouter key"));
        // The dialog is modal: the window's chords are its, consumed.
        for chord in ["C-n", "C-PageDown", "F6", "C-t"] {
            key(&mut app, chord);
        }
        assert!(app.take_requests().is_empty());
        assert_eq!(app.focus(), Focus::Composer);
        for c in KEY.chars() {
            key(&mut app, &c.to_string());
        }
        assert!(app.composed().is_empty(), "the composer took nothing");
        key(&mut app, "Return");
        let requests = app.take_requests();
        assert_eq!(
            requests,
            [Request::SaveKey {
                secret: Secret::new(KEY.into()),
                replace: false
            }]
        );
        assert!(!format!("{requests:?}").contains(KEY), "{requests:?}");
        // Refused: said in the dialog, which keeps its text.
        app.key_refused("the directory /x is writable by its group (mode 0775)".into());
        assert_eq!(app.dialog().unwrap().length(), KEY.len());
        assert!(text(&app).contains("writable by its group"));
        // Stored: the dialog closes and the row stops asking.
        app.key_saved(PATH);
        assert!(app.dialog().is_none());
        assert!(!app.status_line().contains("no key"));
        assert!(app.notice().unwrap().contains("the key is stored in"));
        assert!(!said(&app).contains(KEY));
    }

    #[test]
    fn a_stored_key_is_replaced_only_once_confirmed() {
        let mut app = keyless();
        for c in KEY.chars() {
            key(&mut app, &c.to_string());
        }
        key(&mut app, "Return");
        app.take_requests();
        app.key_exists();
        assert_eq!(app.dialog().unwrap().part(), "replace");
        assert!(text(&app).contains("A key is already stored; replace it?"));
        key(&mut app, "Tab");
        key(&mut app, "Return");
        assert_eq!(
            app.take_requests(),
            [Request::SaveKey {
                secret: Secret::new(KEY.into()),
                replace: true
            }]
        );
        assert!(!said(&app).contains(KEY));
    }

    #[test]
    fn a_paste_reaches_the_dialog_that_asked_and_cancelling_clears_it() {
        use crate::keydialog::tests::Recorder;
        let mut app = keyless();
        let mut clipboard = Recorder::default();
        app.input(
            Input::Key {
                chord: "C-v",
                repeat: false,
            },
            &mut clipboard,
        );
        assert_eq!(clipboard.pastes, 1);
        // td-ui stops saying a paste is in flight before it hands it over.
        clipboard.inflight = false;
        app.input(Input::Paste(&format!("{KEY}\n")), &mut clipboard);
        assert_eq!(app.dialog().unwrap().length(), KEY.len());
        assert!(text(&app).contains(&"\u{2022}".repeat(KEY.len())));
        // Copying is refused, and the clipboard never offered the key.
        for chord in ["C-a", "C-c", "C-x"] {
            app.input(
                Input::Key {
                    chord,
                    repeat: false,
                },
                &mut clipboard,
            );
        }
        assert!(clipboard.copies.is_empty());
        assert!(text(&app).contains("a masked entry does not copy"));
        assert!(!said(&app).contains(KEY));
        key(&mut app, "Escape");
        assert!(app.dialog().is_none());
        assert!(app.take_requests().is_empty());
        // A paste the dialog asked for that comes after it closed is
        // dropped, never the composer's.
        let chord = |app: &mut App, chord, clipboard: &mut Recorder| {
            app.input(
                Input::Key {
                    chord,
                    repeat: false,
                },
                clipboard,
            )
        };
        let mut clipboard = Recorder::default();
        let mut app = keyless();
        chord(&mut app, "C-v", &mut clipboard);
        chord(&mut app, "Escape", &mut clipboard);
        assert!(app.dialog().is_none());
        clipboard.inflight = false;
        app.input(Input::Paste(KEY), &mut clipboard);
        assert!(app.composed().is_empty());
        assert!(app.notice().unwrap().contains("dropped"));
        assert!(!said(&app).contains(KEY));
        // One the clipboard gave up on leaves the composer's next paste
        // the composer's.
        let mut clipboard = Recorder::default();
        let mut app = keyless();
        chord(&mut app, "C-v", &mut clipboard);
        chord(&mut app, "Escape", &mut clipboard);
        clipboard.inflight = false;
        chord(&mut app, "x", &mut clipboard);
        app.input(Input::Paste("later"), &mut clipboard);
        assert_eq!(app.composed(), "xlater");
        // The control seam's inputs, with no clipboard, never release the
        // dialog's paste in flight.
        let mut clipboard = Recorder::default();
        let mut app = keyless();
        chord(&mut app, "C-v", &mut clipboard);
        app.input(
            Input::Key {
                chord: "Escape",
                repeat: false,
            },
            &mut NoClipboard,
        );
        app.input(Input::Focus(true), &mut NoClipboard);
        assert!(app.dialog().is_none());
        clipboard.inflight = false;
        app.input(Input::Paste(KEY), &mut clipboard);
        assert!(app.composed().is_empty());
        assert!(app.notice().unwrap().contains("dropped"));
        assert!(!said(&app).contains(KEY));
        // A paste the dialog did not ask for goes nowhere while it is
        // open, and the composer's once it is not.
        let mut app = keyless();
        app.input(Input::Paste("unasked"), &mut NoClipboard);
        assert_eq!(app.dialog().unwrap().length(), 0);
        assert!(app.composed().is_empty());
        key(&mut app, "Escape");
        app.input(Input::Paste("for the composer"), &mut NoClipboard);
        assert_eq!(app.composed(), "for the composer");
    }

    #[test]
    fn with_nowhere_to_store_a_key_the_dialog_does_not_open() {
        let mut app = app();
        key(&mut app, "F10");
        // Set OpenRouter key…, below New conversation….
        key(&mut app, "Down");
        key(&mut app, "Return");
        assert!(app.dialog().is_none());
        assert!(app.notice().unwrap().contains("nowhere to store a key"));
    }

    fn offers() -> Vec<Offer> {
        let offer = |id: &str, reasoning: bool| Offer {
            id: id.into(),
            price: "$1/$2".into(),
            usable: true,
            reasoning,
        };
        vec![
            offer("m/default", true),
            offer("m/conv", true),
            offer("m/plain", false),
        ]
    }

    #[test]
    fn a_new_conversation_chooses_its_workspace_template() {
        let mut app = app();
        // Without the jail there is nothing to choose: C-n starts a
        // conversation with no workspace, as File's item does.
        app.set_no_workspaces(Some("no workspace jail".into()));
        key(&mut app, "C-n");
        app.menu_action(menu::Action::New);
        assert_eq!(app.take_requests(), [Request::New, Request::New]);
        assert!(app.picker().is_none());
        app.set_no_workspaces(None);
        app.set_templates(vec![("notes".into(), false), ("td".into(), true)]);
        // The built-ins, then the templates in the order written, Empty
        // selected; the note says how a repository template prepares.
        key(&mut app, "C-n");
        assert_eq!(app.picking(), "template");
        let picker = app.picker().unwrap();
        let listed: Vec<&str> = picker
            .listing()
            .entries()
            .iter()
            .map(|entry| entry.name())
            .collect();
        assert_eq!(listed, ["Empty", "Directory\u{2026}", "notes", "td"]);
        assert_eq!(picker.selected(), Some("Empty"));
        assert!(
            text(&app).contains("checked out in the background"),
            "{}",
            text(&app)
        );
        assert!(text(&app).contains("repositories"), "{}", text(&app));
        // The picker has every key: C-n starts nothing more.
        key(&mut app, "C-n");
        assert!(app.take_requests().is_empty());
        key(&mut app, "Return");
        assert!(app.picker().is_none());
        assert_eq!(app.take_requests(), [Request::NewScratch]);
        for (downs, name) in [(2, "notes"), (3, "td")] {
            app.menu_action(menu::Action::New);
            for _ in 0..downs {
                key(&mut app, "Down");
            }
            key(&mut app, "Return");
            assert_eq!(app.take_requests(), [Request::NewFrom(name.into())]);
        }
        // Escape chooses nothing.
        key(&mut app, "C-n");
        key(&mut app, "Escape");
        assert!(app.picker().is_none() && app.take_requests().is_empty());
        let base = std::env::temp_dir().join(format!(
            "td-agent-ui-chooser-{}-{}",
            std::process::id(),
            crate::store::random_hex(4).unwrap()
        ));
        std::fs::create_dir_all(base.join("notes")).unwrap();
        let base = std::fs::canonicalize(&base).unwrap();
        app.set_chooser_start(base.clone());
        // Directory… opens the folder chooser.
        let directory = |app: &mut App| {
            key(app, "C-n");
            key(app, "Down");
            key(app, "Return");
        };
        directory(&mut app);
        assert!(app.picker().is_none());
        assert_eq!(app.chooser().unwrap().folder(), base);
        // The chooser has every key: C-n starts nothing.
        key(&mut app, "C-n");
        assert!(app.take_requests().is_empty());
        key(&mut app, "Return");
        assert_eq!(app.chooser().unwrap().folder(), base.join("notes"));
        key(&mut app, "C-Return");
        assert!(app.chooser().is_none());
        assert_eq!(app.take_requests(), [Request::NewIn(base.join("notes"))]);
        // It opens next where it last chose from; Escape chooses nothing.
        directory(&mut app);
        assert_eq!(app.chooser().unwrap().folder(), base);
        key(&mut app, "Escape");
        assert!(app.chooser().is_none() && app.take_requests().is_empty());
        std::fs::remove_dir_all(&base).unwrap();
        // Its folder gone, it opens elsewhere rather than not at all.
        directory(&mut app);
        assert!(app
            .chooser()
            .is_some_and(|chooser| chooser.folder() != base));
        key(&mut app, "Escape");
        // A card that waited behind the picker waits behind the folder
        // chooser too: Directory… is not lost to it.
        key(&mut app, "C-n");
        app.ask(Card {
            conversation: id(1),
            call: 4,
            title: "Run a command".into(),
            details: vec!["make".into()],
            always: None,
        });
        assert!(app.confirm().is_none());
        key(&mut app, "Down");
        key(&mut app, "Return");
        assert!(app.chooser().is_some() && app.confirm().is_none());
        key(&mut app, "Escape");
        assert!(app.chooser().is_none() && app.confirm().is_some());
        app.withdraw(&id(1), None);
        // The list's third column names each conversation's workspace;
        // the status row, of fixed width, does not.
        app.add_row(Row {
            workspace: Some(Workspace::Template("notes".into())),
            ..row(9, 1)
        });
        app.set_active(id(9));
        let at = app.rows().iter().position(|r| r.id == id(9)).unwrap();
        assert_eq!(app.labels[at], "template notes");
        // The columns fit the list at its default share of a window 1024
        // pixels wide: no sideways scrollbar.
        assert!(app.list.geometry().unwrap().horizontal().is_none());
        // A directory by its folder's name, which the column has room for.
        app.add_row(Row {
            workspace: Some(Workspace::Directory("/home/u/src/notes".into())),
            ..row(8, 2)
        });
        let at = app.rows().iter().position(|r| r.id == id(8)).unwrap();
        assert_eq!(app.labels[at], "notes");
        assert!(app.labels.iter().any(|label| label == "none"));
        assert!(
            !app.status_line().contains("notes"),
            "{}",
            app.status_line()
        );
    }

    /// The status row says the open conversation's workspace's mode:
    /// the human's rules', else the configuration's, `ask` while the
    /// rules cannot be read; Conversation > Auto mode in this workspace
    /// asks the window for the other one.
    #[test]
    fn the_status_row_says_the_workspaces_mode_and_the_menu_changes_it() {
        let mut app = app();
        assert!(
            app.status_line().contains("| mode ask |"),
            "{}",
            app.status_line()
        );
        app.add_row(Row {
            workspace: Some(Workspace::Scratch),
            ..row(9, 1)
        });
        app.set_active(id(9));
        let key = Workspace::Scratch.key(&id(9));
        assert!(app.status_line().contains("| mode auto |"));
        app.set_modes(Some(vec![(key.clone(), Mode::Ask)]));
        assert!(
            app.status_line().contains("| mode ask |"),
            "{}",
            app.status_line()
        );
        // Not the control seam's.
        app.menu_action(menu::Action::AutoMode);
        assert!(app.take_requests().is_empty());
        app.live = true;
        app.menu_action(menu::Action::AutoMode);
        assert_eq!(
            app.take_requests(),
            [Request::SetMode {
                conversation: id(9),
                mode: Mode::Auto
            }]
        );
        app.set_modes(Some(vec![(key, Mode::Auto)]));
        app.menu_action(menu::Action::AutoMode);
        assert_eq!(
            app.take_requests(),
            [Request::SetMode {
                conversation: id(9),
                mode: Mode::Ask
            }]
        );
        app.set_modes(None);
        assert!(app.status_line().contains("| mode ask |"));
        app.set_without_jev(true);
        assert!(
            app.status_line()
                .contains("| mode ask, classifier without Jev |"),
            "{}",
            app.status_line()
        );
        app.set_without_jev(false);
        // No workspace: `ask`, and nothing to set.
        app.set_modes(Some(Vec::new()));
        app.add_row(row(8, 2));
        app.set_active(id(8));
        assert!(
            app.status_line().contains("| mode ask |"),
            "{}",
            app.status_line()
        );
        app.menu_action(menu::Action::AutoMode);
        assert!(app.take_requests().is_empty());
    }

    #[test]
    fn the_conversation_menu_chooses_the_effort_and_the_picker_the_model() {
        let mut app = app();
        app.set_offers(offers(), "medium");
        // Conversation > Effort > high, by the keys, from the menu built
        // as it opened.
        key(&mut app, menu::OPEN);
        for chord in ["Right", "Down", "Right", "Down", "Down", "Down", "Down"] {
            key(&mut app, chord);
        }
        key(&mut app, "Return");
        assert!(!app.menu_open());
        assert_eq!(
            app.take_requests(),
            [Request::Choose {
                model: None,
                effort: Some("high".into())
            }]
        );
        // The conversation logs it, and the window shows it.
        app.update(
            at(
                1,
                Kind::Choice {
                    model: None,
                    effort: Some("high".into()),
                },
            ),
            0,
        );
        assert_eq!(app.effort(), "high");
        assert!(
            app.status_line().contains("| m/default high |"),
            "{}",
            app.status_line()
        );
        assert!(text(&app).contains("reasoning effort high, from the next request"));
        // The menu opened again checks it.
        key(&mut app, menu::OPEN);
        let checked: Vec<&str> = (0..)
            .map_while(|n| app.menu.model().node(n).copied())
            .filter(|n| n.row.checked)
            .map(|n| n.row.label)
            .collect();
        assert_eq!(checked, ["high"]);
        key(&mut app, "Escape");
        // Conversation > Model... opens the picker on the conversation's
        // model; typing filters and Return chooses, the effort kept.
        app.menu_action(menu::Action::Model);
        let picker = app.picker().unwrap();
        assert_eq!(picker.selected(), Some("m/default"));
        for c in ["p", "l", "a"] {
            key(&mut app, c);
        }
        assert_eq!(app.picker().unwrap().selected(), Some("m/plain"));
        key(&mut app, "Return");
        assert!(app.picker().is_none());
        assert_eq!(
            app.take_requests(),
            [Request::Choose {
                model: Some("m/plain".into()),
                effort: Some("high".into())
            }]
        );
        app.update(
            at(
                2,
                Kind::Choice {
                    model: Some("m/plain".into()),
                    effort: Some("high".into()),
                },
            ),
            0,
        );
        // A model that takes no effort says so, and its Effort is off.
        assert!(
            app.status_line().contains("| m/plain no reasoning |"),
            "{}",
            app.status_line()
        );
        app.refresh_menu();
        let effort = (0..)
            .map_while(|n| app.menu.model().node(n).copied())
            .find(|n| n.row.label == menu::EFFORT)
            .unwrap();
        assert!(!effort.row.enabled);
        // Escape closes the picker with nothing chosen; opening another
        // conversation closes it too, and shows that one's choice.
        app.open_picker();
        key(&mut app, "Escape");
        assert!(app.picker().is_none() && app.take_requests().is_empty());
        app.open_picker();
        assert!(app.picker().is_some());
        app.set_active(id(2));
        assert!(app.picker().is_none());
        assert_eq!((app.model(), app.effort()), ("m/default", "medium"));
    }

    /// Two choices made before the conversation logs the first: the
    /// second keeps what the first chose.
    #[test]
    fn a_choice_made_before_the_last_is_logged_builds_on_it() {
        let mut app = app();
        app.set_offers(offers(), "medium");
        app.menu_action(menu::Action::Model);
        for c in ["p", "l", "a"] {
            key(&mut app, c);
        }
        key(&mut app, "Return");
        app.menu_action(menu::Action::Effort("high"));
        let plain = Some("m/plain".to_string());
        assert_eq!(
            app.take_requests(),
            [
                Request::Choose {
                    model: plain.clone(),
                    effort: None
                },
                Request::Choose {
                    model: plain.clone(),
                    effort: Some("high".into())
                }
            ]
        );
        // The first logged leaves the second still asked for.
        app.update(
            at(
                1,
                Kind::Choice {
                    model: plain.clone(),
                    effort: None,
                },
            ),
            0,
        );
        // A model that takes no effort is said to have none.
        assert!(text(&app).contains("m/plain, no reasoning, from"));
        app.menu_action(menu::Action::Effort("low"));
        assert_eq!(
            app.take_requests(),
            [Request::Choose {
                model: plain,
                effort: Some("low".into())
            }]
        );
    }

    /// A, then B, then A again, asked during one turn: the logged B does
    /// not hide the A asked after it.
    #[test]
    fn choices_asked_are_matched_to_their_events_in_order() {
        let mut app = app();
        app.set_offers(offers(), "medium");
        let pick = |model: &str| (Some(model.to_string()), None);
        for model in ["m/conv", "m/plain", "m/conv"] {
            let (model, effort) = pick(model);
            app.choose(model, effort);
        }
        app.take_requests();
        for (seq, model) in [(1, "m/conv"), (2, "m/plain")] {
            let (model, effort) = pick(model);
            app.update(at(seq, Kind::Choice { model, effort }), 0);
        }
        app.menu_action(menu::Action::Effort("high"));
        assert_eq!(
            app.take_requests(),
            [Request::Choose {
                model: Some("m/conv".into()),
                effort: Some("high".into())
            }]
        );
        // The picker opens on the model asked for last.
        app.open_picker();
        assert_eq!(app.picker().unwrap().selected(), Some("m/conv"));
    }

    #[test]
    fn the_key_dialog_replaces_the_picker() {
        let mut app = app();
        app.set_offers(offers(), "medium");
        app.open_picker();
        assert!(app.picker().is_some());
        // With nowhere to store a key, no dialog, and the picker stays.
        app.open_key_dialog();
        assert!(app.picker().is_some() && app.dialog().is_none());
        app.set_key_path(Some(PATH.into()));
        app.open_key_dialog();
        assert!(app.picker().is_none() && app.dialog().is_some());
    }

    /// A conversation being deleted while its workspace is asked neither
    /// opens nor takes a message, and the list says so; what its workspace
    /// reports is asked on a loss card, Cancel focused, until answered:
    /// set aside when the window loses the keyboard and asked again when
    /// it comes back, Cancel keeping both, the action deleting both.
    #[test]
    fn a_deletion_that_would_lose_work_is_asked_until_answered() {
        let mut app = app();
        app.settle = Duration::ZERO;
        app.set_closing(&id(1), Some("deleting"));
        assert!(app.active().is_none(), "the open one is closed");
        assert!(app.closing(&id(1)));
        assert_eq!(app.most_recent(), Some(id(3)));
        let entry = app.directory().into_iter().find(|e| e.id == id(1)).unwrap();
        assert!(entry.failed, "nothing is delivered to it meanwhile");
        let index = app.rows().iter().position(|r| r.id == id(1)).unwrap();
        assert_eq!(app.cell(index, 1).text(), "deleting");
        // Previous and next pass over it: above the next, it is not.
        app.set_active(id(3));
        key(&mut app, "C-PageUp");
        assert!(app.take_requests().is_empty());
        assert!(app
            .notice()
            .is_none_or(|n| !n.contains("while td-agent is deleting it")));
        app.open(id(1));
        assert!(app.take_requests().is_empty());
        assert!(app
            .notice()
            .unwrap()
            .contains("while td-agent is deleting it"));
        let lost =
            vec!["/w/td-1/td (branch td-agent/td-1): 2 changed or untracked files".to_string()];
        app.ask_removal(id(1), "title 1".into(), lost.clone(), false);
        let asked = |app: &App| matches!(app.confirm().map(Confirm::purpose), Some(confirm::Purpose::Remove(of)) if *of == id(1));
        assert!(asked(&app));
        assert_eq!(app.confirm().unwrap().focus(), "cancel");
        let shown = text(&app);
        assert!(shown.contains(confirm::REMOVE_TITLE), "{shown}");
        assert!(shown.contains("2 changed or untracked files"), "{shown}");
        assert_eq!(app.cell(index, 1).text(), ASKS);
        // Set aside with the keyboard, and asked again when it is back.
        app.input(Input::Focus(false), &mut NoClipboard);
        assert!(app.confirm().is_none());
        app.input(Input::Focus(true), &mut NoClipboard);
        assert!(asked(&app));
        // Cancel keeps both.
        key(&mut app, "Return");
        assert_eq!(
            app.take_requests(),
            [Request::Remove {
                id: id(1),
                remove: false
            }]
        );
        assert!(app.confirm().is_none());
        // Asked again, the action deletes both.
        app.ask_removal(id(1), "title 1".into(), lost, false);
        key(&mut app, "Tab");
        assert_eq!(app.confirm().unwrap().focus(), "remove");
        key(&mut app, "Return");
        assert_eq!(
            app.take_requests(),
            [Request::Remove {
                id: id(1),
                remove: true
            }]
        );
        app.set_closing(&id(1), None);
        assert!(!app.closing(&id(1)));
        app.open(id(1));
        assert_eq!(app.take_requests(), [Request::Open(id(1))]);
        // Archiving says so in the list and on its card.
        app.set_closing(&id(2), Some("archiving"));
        let index = app.rows().iter().position(|r| r.id == id(2)).unwrap();
        assert_eq!(app.cell(index, 1).text(), "archiving");
        app.open(id(2));
        assert!(app
            .notice()
            .unwrap()
            .contains("while td-agent is archiving it"));
        app.ask_removal(id(2), "title 2".into(), vec!["/w/x: 1 commit".into()], true);
        let shown = text(&app);
        assert!(
            shown.contains(confirm::ARCHIVE_TITLE) && shown.contains("Archiving"),
            "{shown}"
        );
        key(&mut app, "Tab");
        key(&mut app, "Return");
        assert_eq!(
            app.take_requests(),
            [Request::Remove {
                id: id(2),
                remove: true
            }]
        );
    }

    /// A template's remotes no admission covers are asked about on a
    /// card, Cancel focused: Admit asks the window to admit them and make
    /// the workspace, Cancel or Escape makes nothing and says so, a key
    /// that comes as it is shown decides nothing, and nothing is asked
    /// over another question.
    #[test]
    fn a_template_whose_remotes_are_not_admitted_asks_on_a_card() {
        let mut app = app();
        app.settle = Duration::ZERO;
        let remotes = vec![
            "https://example.org/a/td".to_string(),
            "git@example.org:a/b".to_string(),
        ];
        let asked = |app: &App| match app.confirm().map(Confirm::purpose) {
            Some(confirm::Purpose::Admit { template, remotes }) => {
                Some((template.clone(), remotes.clone()))
            }
            _ => None,
        };
        app.ask_admission("td".into(), remotes.clone());
        assert_eq!(asked(&app), Some(("td".to_string(), remotes.clone())));
        assert_eq!(app.confirm().unwrap().focus(), "cancel");
        let shown = text(&app);
        assert!(shown.contains("Admit remotes"), "{shown}");
        assert!(shown.contains("https://example.org/a/td"), "{shown}");
        key(&mut app, "Return");
        assert!(app.take_requests().is_empty());
        assert!(app.confirm().is_none());
        // Said in a note: nothing was made.
        assert_eq!(app.unread(), 1);
        app.ask_admission("td".into(), remotes.clone());
        key(&mut app, "Escape");
        assert!(app.take_requests().is_empty() && app.confirm().is_none());
        app.ask_admission("td".into(), remotes.clone());
        key(&mut app, "Tab");
        assert_eq!(app.confirm().unwrap().focus(), "admit");
        key(&mut app, "Return");
        assert_eq!(
            app.take_requests(),
            [Request::Admit {
                template: "td".into(),
                remotes: remotes.clone()
            }]
        );
        assert!(app.confirm().is_none());
        // Just shown, it takes no key: one meant for the chooser.
        app.settle = Duration::from_secs(60);
        app.ask_admission("td".into(), remotes.clone());
        key(&mut app, "Tab");
        key(&mut app, "Return");
        assert!(app.take_requests().is_empty());
        assert_eq!(app.confirm().unwrap().focus(), "cancel");
        // Set aside with the keyboard, decided nothing, and asked again
        // when it comes back.
        app.settle = Duration::ZERO;
        key(&mut app, "Escape");
        app.take_requests();
        app.ask_admission("td".into(), remotes.clone());
        key(&mut app, "Tab");
        let unread = app.unread();
        app.input(Input::Focus(false), &mut NoClipboard);
        assert!(app.confirm().is_none() && app.take_requests().is_empty());
        assert_eq!(app.unread(), unread);
        app.input(Input::Focus(true), &mut NoClipboard);
        assert_eq!(asked(&app), Some(("td".to_string(), remotes.clone())));
        assert_eq!(app.confirm().unwrap().focus(), "cancel");
        // Over another question, nothing is asked, and that is said.
        let unread = app.unread();
        app.ask_admission("other".into(), remotes);
        assert_eq!(asked(&app).map(|(template, _)| template), Some("td".into()));
        assert_eq!(app.unread(), unread + 1);
    }

    /// A card is shown for the open conversation whenever nothing else is
    /// modal, Cancel focused: Return or Escape refuses its call, Allow
    /// lets it run, and the decision goes to the conversation that asked.
    /// Losing the keyboard sets it aside; another conversation's waits,
    /// its row saying it asks; a withdrawn one closes.
    #[test]
    fn a_card_asks_the_person_and_their_answer_goes_to_its_conversation() {
        let mut app = app();
        app.settle = Duration::ZERO;
        app.set_active(id(2));
        let card = |conversation: u8, call: u64| Card {
            conversation: id(conversation),
            call,
            title: "Run a command".into(),
            details: vec!["In the workspace:".into(), "make".into()],
            always: None,
        };
        let decided = |conversation: u8, call: u64, allow: bool| Request::Decide {
            conversation: id(conversation),
            call,
            answer: Answer::Once(allow),
        };
        let shown = |app: &App| match app.confirm().map(Confirm::purpose) {
            Some(confirm::Purpose::Approve { conversation, call }) => {
                Some((conversation.clone(), *call))
            }
            _ => None,
        };
        let says = |app: &App, of: u8| {
            let at = app.rows().iter().position(|r| r.id == id(of)).unwrap();
            app.cell(at, 1).text().to_string()
        };
        app.ask(card(3, 7));
        assert!(app.confirm().is_none(), "another conversation's waits");
        assert_eq!(says(&app, 3), ASKS);
        app.ask(card(2, 5));
        assert_eq!(shown(&app), Some((id(2), 5)));
        assert_eq!(app.confirm().unwrap().focus(), "cancel");
        assert!(text(&app).contains("Run a command"));
        key(&mut app, "C-n");
        assert!(app.take_requests().is_empty(), "keys are the card's");
        key(&mut app, "Return");
        assert_eq!(app.take_requests(), [decided(2, 5, false)]);
        assert!(app.confirm().is_none());
        app.ask(card(2, 6));
        key(&mut app, "Escape");
        assert_eq!(app.take_requests(), [decided(2, 6, false)]);
        app.ask(card(2, 8));
        assert_eq!(app.confirm().unwrap().focus(), "cancel");
        key(&mut app, "Tab");
        assert_eq!(app.confirm().unwrap().focus(), "allow");
        key(&mut app, "Return");
        assert_eq!(app.take_requests(), [decided(2, 8, true)]);
        // Two cards come one after the other.
        app.ask(card(2, 10));
        app.ask(card(2, 11));
        key(&mut app, "Escape");
        assert_eq!(shown(&app), Some((id(2), 11)));
        key(&mut app, "Escape");
        assert_eq!(
            app.take_requests(),
            [decided(2, 10, false), decided(2, 11, false)]
        );
        // Set aside with the keyboard, armed or not, and decided nothing.
        app.ask(card(2, 9));
        key(&mut app, "Tab");
        app.input(Input::Focus(false), &mut NoClipboard);
        assert!(app.confirm().is_none());
        assert!(app.take_requests().is_empty());
        app.input(Input::Focus(true), &mut NoClipboard);
        assert_eq!(shown(&app), Some((id(2), 9)));
        assert_eq!(app.confirm().unwrap().focus(), "cancel");
        // Withdrawn, it closes, and nothing is decided.
        app.withdraw(&id(2), Some(9));
        assert!(app.confirm().is_none());
        assert!(app.take_requests().is_empty());
        // Opening the conversation that asked shows its card; its process
        // ending withdraws them all.
        app.set_active(id(3));
        assert_eq!(shown(&app), Some((id(3), 7)));
        app.withdraw(&id(3), None);
        assert!(app.confirm().is_none() && app.cards().is_empty());
        assert_ne!(says(&app, 3), ASKS);
        // A modal open first goes first.
        app.open_delete();
        app.ask(card(3, 12));
        assert!(matches!(
            app.confirm().map(Confirm::purpose),
            Some(confirm::Purpose::Delete(_))
        ));
        key(&mut app, "Escape");
        assert_eq!(shown(&app), Some((id(3), 12)));
    }

    /// A card's "always" answers go to the window with what they would
    /// remember: each deny here or everywhere, and the allow here.
    #[test]
    fn a_cards_always_answers_carry_what_they_remember() {
        let bodies = vec!["shell cargo test".to_string()];
        for (tabs, answer) in [
            (0, Answer::Once(false)),
            (
                1,
                Answer::Always {
                    allow: false,
                    everywhere: false,
                    bodies: bodies.clone(),
                },
            ),
            (
                2,
                Answer::Always {
                    allow: false,
                    everywhere: true,
                    bodies: bodies.clone(),
                },
            ),
            (
                3,
                Answer::Always {
                    allow: true,
                    everywhere: false,
                    bodies: bodies.clone(),
                },
            ),
            (4, Answer::Once(true)),
        ] {
            let mut app = app();
            app.settle = Duration::ZERO;
            app.set_active(id(2));
            app.ask(Card {
                conversation: id(2),
                call: 4,
                title: "Run a command".into(),
                details: vec!["cargo test".into()],
                always: Some(crate::rules::Offer::Rules(crate::rules::Always {
                    allow: true,
                    bodies: bodies.clone(),
                })),
            });
            for _ in 0..tabs {
                key(&mut app, "Tab");
            }
            key(&mut app, "Return");
            assert_eq!(
                app.take_requests(),
                [Request::Decide {
                    conversation: id(2),
                    call: 4,
                    answer
                }],
                "{tabs}"
            );
        }
    }

    /// A crossing card's "always" answers go to the window as its pair
    /// and way.
    #[test]
    fn a_crossing_cards_always_answers_name_its_pair_and_way() {
        let to = "b".repeat(32);
        for (tabs, answer) in [
            (
                1,
                Answer::Crossing {
                    allow: false,
                    op: crate::rules::Crossed::Message,
                    to: to.clone(),
                },
            ),
            (
                2,
                Answer::Crossing {
                    allow: true,
                    op: crate::rules::Crossed::Message,
                    to: to.clone(),
                },
            ),
            (3, Answer::Once(true)),
        ] {
            let mut app = app();
            app.settle = Duration::ZERO;
            app.set_active(id(2));
            app.ask(Card {
                conversation: id(2),
                call: 4,
                title: "Send a message to another conversation".into(),
                details: vec!["hello".into()],
                always: Some(crate::rules::Offer::Crossing {
                    op: crate::rules::Crossed::Message,
                    to: to.clone(),
                }),
            });
            for _ in 0..tabs {
                key(&mut app, "Tab");
            }
            key(&mut app, "Return");
            assert_eq!(
                app.take_requests(),
                [Request::Decide {
                    conversation: id(2),
                    call: 4,
                    answer
                }],
                "{tabs}"
            );
        }
    }

    /// A card that comes while the human types takes no key at first: what
    /// was meant for the composer neither refuses nor allows it.
    #[test]
    fn a_card_just_shown_takes_no_key() {
        let mut app = app();
        app.settle = Duration::from_secs(3600);
        app.set_active(id(2));
        app.ask(Card {
            conversation: id(2),
            call: 4,
            title: "Run a command".into(),
            details: vec!["make".into()],
            always: None,
        });
        for chord in ["Tab", "Space", "Return", "Escape"] {
            key(&mut app, chord);
        }
        assert!(app.take_requests().is_empty());
        assert_eq!(app.confirm().unwrap().focus(), "cancel");
        app.settle = Duration::ZERO;
        key(&mut app, "Escape");
        assert_eq!(
            app.take_requests(),
            [Request::Decide {
                conversation: id(2),
                call: 4,
                answer: Answer::Once(false)
            }]
        );
    }

    /// Deletion asks first, Cancel focused, and takes any conversation,
    /// one that was the orchestrator included; the session carries it out
    /// and the row goes.
    #[test]
    fn a_conversation_is_deleted_only_after_the_question() {
        let mut app = app();
        app.remove_row(&id(1));
        assert!(app.active().is_none());
        app.open_delete();
        assert!(app.confirm().is_none());
        assert!(app
            .notice()
            .unwrap()
            .contains("no conversation is open to delete"));
        app.refresh_menu();
        let delete = |app: &App| {
            (0..)
                .map_while(|n| app.menu.model().node(n).copied())
                .find(|n| n.row.label == menu::DELETE)
                .unwrap()
                .row
                .enabled
        };
        assert!(!delete(&app));
        app.set_active(id(2));
        app.refresh_menu();
        assert!(delete(&app));
        // Return alone is Cancel; Escape too.
        for chord in ["Return", "Escape"] {
            app.menu_action(menu::Action::Delete);
            assert_eq!(app.confirm().unwrap().focus(), "cancel");
            key(&mut app, "C-n");
            assert!(app.take_requests().is_empty(), "keys are the question's");
            key(&mut app, chord);
            assert!(app.confirm().is_none(), "{chord}");
            assert!(app.take_requests().is_empty(), "{chord}");
        }
        // Tab to Delete, and Return asks the session.
        app.open_delete();
        key(&mut app, "Tab");
        key(&mut app, "Return");
        assert_eq!(app.take_requests(), [Request::Delete(id(2))]);
        // Losing the keyboard cancels it, an armed Delete with it.
        app.open_delete();
        key(&mut app, "Tab");
        app.input(Input::Focus(false), &mut NoClipboard);
        assert!(app.confirm().is_none());
        app.input(Input::Focus(true), &mut NoClipboard);
        key(&mut app, "Return");
        assert!(app
            .take_requests()
            .iter()
            .all(|r| !matches!(r, Request::Delete(_))));
        // Opening another conversation closes the question.
        app.open_delete();
        app.set_active(id(3));
        assert!(app.confirm().is_none());
        app.set_active(id(2));
        app.remove_row(&id(2));
        assert!(app.active().is_none());
        assert!(app.rows().iter().all(|r| r.id != id(2)));
        // An orchestrator from an older state directory is asked about
        // like any other.
        app.add_row(row(4, 200));
        app.set_active(id(4));
        app.open_delete();
        key(&mut app, "Tab");
        key(&mut app, "Return");
        assert_eq!(app.take_requests(), [Request::Delete(id(4))]);
    }

    /// Conversation → Default model… opens the picker on the default,
    /// with or without a conversation open, and its choice asks the
    /// session to make it the default, not the open conversation's.
    #[test]
    fn the_default_model_is_chosen_through_its_own_picker() {
        let mut app = app();
        let offer = |id: &str| crate::picker::Offer {
            id: id.into(),
            price: String::new(),
            usable: true,
            reasoning: true,
        };
        app.set_offers(vec![offer("m/conv"), offer("m/new")], "medium");
        app.set_active(id(2));
        app.menu_action(menu::Action::DefaultModel);
        assert_eq!(app.picking(), "default");
        let picker = app.picker().unwrap();
        assert_eq!(picker.selected(), Some("m/conv"));
        assert!(text(&app).contains("Default model for new conversations"));
        key(&mut app, "Down");
        key(&mut app, "Return");
        assert!(app.picker().is_none());
        assert_eq!(app.take_requests(), [Request::SetDefault("m/new".into())]);
        // The open conversation's own picker still chooses for it.
        app.open_picker();
        key(&mut app, "Return");
        assert!(matches!(
            app.take_requests().as_slice(),
            [Request::Choose { .. }]
        ));
        // Opening another conversation closes it, nothing chosen.
        app.open_default_picker();
        app.set_active(id(3));
        assert_eq!(app.picking(), "none");
        assert!(app
            .take_requests()
            .iter()
            .all(|r| !matches!(r, Request::SetDefault(_))));
        // Escape leaves nothing of the default's picker behind.
        app.open_default_picker();
        key(&mut app, "Escape");
        assert!(app.take_requests().is_empty());
        app.open_picker();
        key(&mut app, "Return");
        assert!(matches!(
            app.take_requests().as_slice(),
            [Request::Choose { .. }]
        ));
        app.set_default_model("m/new");
        assert_eq!(app.default_model(), "m/new");
    }

    #[test]
    fn with_no_models_known_the_picker_says_so() {
        let mut app = app();
        app.open_picker();
        assert!(app.picker().is_none());
        assert!(app.notice().unwrap().contains("no models are known yet"));
    }
}

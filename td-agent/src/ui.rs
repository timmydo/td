//! The window's state and drawing (DESIGN.md §4), apart from the
//! compositor and the processes so it is tested on its own: a horizontal
//! split with the conversation list on the left, a td-ui tree table, and
//! on the right the active conversation's transcript, a td-ui message
//! list, over the composer, td-ui's editor pane, and a status row.
//!
//! The list holds the orchestrator first and then every other
//! conversation, most recently active first. There are no workspaces yet,
//! so each conversation is a row of its own, a workspace of one.
//!
//! What the window must do outside itself (open a conversation, start a
//! new one, send a message, save the split) it asks through `Request`s,
//! which the session takes after every input.

use td_ui::chrome::ROW;
use td_ui::editor::{self, Controller as Pane, Event as PaneEvent, Outcome as PaneOutcome};
use td_ui::editor_clipboard::{Paste, Snapshot};
use td_ui::editor_model::TabId;
use td_ui::messages::{self, Error as MessagesError, Message, Tone};
use td_ui::raster::{
    self, Composition, Draw, GlyphStyle, Primitive, Rect, Surface, Weight, BORDER, CHROME, INK,
    PAPER,
};
use td_ui::split;
use td_ui::tree_table::{self, Cell, Column, Model, Row as TreeRow};
use td_ui::window::{Clipboard, Input, PointerPhase};
use td_ui::{CELL_HEIGHT, CELL_WIDTH};

use crate::config::Mode;
use crate::protocol::{Up, MAX_TEXT};
use crate::store::{Event, Id, Kind, Role};
use crate::supervisor::Update;

/// The composer's text rows when the window has room for them.
const COMPOSER_ROWS: usize = 6;
/// The split's child minima, in logical pixels.
const LIST_MIN: u32 = 160;
const CONVERSATION_MIN: u32 = 320;
/// The list's columns.
const COLUMNS: [Column<'static>; 2] = [
    Column {
        title: "Conversation",
        minimum: 120,
        preferred: 200,
        numeric: false,
    },
    Column {
        title: "State",
        minimum: 72,
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
    Restarting,
    Failed,
}

impl RowState {
    pub fn word(self) -> &'static str {
        match self {
            Self::Closed => "",
            Self::Starting => "starting",
            Self::Idle => "idle",
            Self::Restarting => "restarting",
            Self::Failed => "failed",
        }
    }
}

/// One conversation in the list.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Row {
    pub id: Id,
    pub role: Role,
    pub title: String,
    /// When it was last active, in seconds since the epoch: what the list
    /// orders the conversations by.
    pub activity: u64,
    pub state: RowState,
}

/// What the window asks of the session.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Request {
    /// Open this conversation in its own process; the one open before is
    /// closed.
    Open(Id),
    /// Start a new conversation and open it.
    New,
    /// Send the human's message to the open conversation.
    Send(String),
    /// Save the split's preferred share.
    SaveShare(u32, u32),
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
    /// The rule above the composer.
    rule: Rect,
    composer: Rect,
    status: Rect,
}

/// The composer: one editable document in td-ui's editor pane. `C-Return`
/// sends it; `Return` is the pane's own newline.
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
    mode: Mode,
    notice: Option<String>,
    /// Each user message's sequence number and its index in the
    /// transcript, and each started turn's and its user message's.
    messages: Vec<(u64, usize)>,
    turns: Vec<(u64, u64)>,
    requests: Vec<Request>,
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
            .unwrap_or_else(|| split::Share::new(2, 7).unwrap_or_default());
        let split = split::Controller::new(
            split::Config {
                axis: split::Axis::Horizontal,
                first_min: LIST_MIN,
                second_min: CONVERSATION_MIN,
            },
            share,
            surface,
            surface.bounds(),
        )
        .map_err(|e| e.to_string())?;
        let model = Model::new(&[], &COLUMNS).map_err(|e| e.to_string())?;
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
            notice: None,
            messages: Vec::new(),
            turns: Vec::new(),
            requests: Vec::new(),
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

    pub fn focus(&self) -> Focus {
        self.focus
    }

    pub fn composed(&self) -> &str {
        self.composer.text()
    }

    pub fn transcript(&self) -> &messages::Controller {
        &self.transcript
    }

    pub fn notice(&self) -> Option<&str> {
        self.notice.as_deref()
    }

    /// The status row's line.
    pub fn status_line(&self) -> String {
        let state = self
            .active
            .as_ref()
            .and_then(|id| self.rows.iter().find(|r| &r.id == id))
            .map_or("no conversation", |r| r.state.word());
        // A notice goes next to the state, where a narrow row still
        // shows it.
        let notice = self
            .notice
            .as_deref()
            .map(|n| format!(" | {n}"))
            .unwrap_or_default();
        format!(
            "{state}{notice} | no model | mode {} | no limits | 0 background",
            self.mode.word()
        )
    }

    // --- what the session tells it ----------------------------------

    /// Shows `message` in the status row until the next one.
    pub fn note(&mut self, message: impl Into<String>) {
        self.notice = Some(message.into());
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
        for row in &mut self.rows {
            row.state = if row.id == id {
                RowState::Starting
            } else {
                RowState::Closed
            };
        }
        self.active = Some(id);
        self.clear_transcript();
        self.refresh_list();
    }

    fn clear_transcript(&mut self) {
        self.transcript.remove_first(self.transcript.len());
        self.messages.clear();
        self.turns.clear();
        self.touch();
    }

    fn active_row(&mut self) -> Option<&mut Row> {
        let id = self.active.as_ref()?;
        self.rows.iter_mut().find(|r| &r.id == id)
    }

    /// What the open conversation's process said, or what became of it.
    pub fn update(&mut self, update: Update, now: u64) {
        match update {
            Update::Up(Up::Hello {
                title,
                torn,
                interrupted,
                ..
            }) => {
                self.clear_transcript();
                if let Some(row) = self.active_row() {
                    row.title = crate::store::title(&title);
                    row.state = RowState::Idle;
                }
                if let Some(bytes) = torn {
                    self.note(format!("dropped a torn final log line of {bytes} bytes"));
                }
                if !interrupted.is_empty() {
                    self.note(format!(
                        "a restart interrupted {} turn(s); none is repeated",
                        interrupted.len()
                    ));
                }
                self.refresh_list();
            }
            Update::Up(Up::Event(event)) => self.event(event),
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
            Update::Restarting { reason } => {
                if let Some(row) = self.active_row() {
                    row.state = RowState::Restarting;
                }
                self.note(format!(
                    "the conversation's process failed ({reason}); restarting it from its log"
                ));
                self.refresh_list();
            }
            Update::Failed { reason } => {
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
                if let Some(index) = message_of(&self.messages, &self.turns, event.seq) {
                    let _ = self
                        .transcript
                        .set_status(index, Some(("running", Tone::Neutral)));
                }
            }
            Kind::Finished { started, outcome } => {
                if let Some(index) = message_of(&self.messages, &self.turns, started) {
                    let _ = self
                        .transcript
                        .set_status(index, Some((&outcome, Tone::Neutral)));
                }
            }
            Kind::Interrupted { started } => {
                if let Some(index) = message_of(&self.messages, &self.turns, started) {
                    let _ = self
                        .transcript
                        .set_status(index, Some(("interrupted", Tone::Bad)));
                }
            }
            Kind::Notice { text } => {
                let pushed = Message::new("td-agent")
                    .and_then(|m| m.status("notice", Tone::Neutral))
                    .and_then(|m| m.text(&text))
                    .map_err(|e| e.to_string())
                    .and_then(|m| self.push_message(m));
                if let Err(e) = pushed {
                    self.note(format!("the transcript refused a notice: {e}"));
                }
            }
        }
        self.touch();
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
    /// turns find their messages by.
    fn evict(&mut self) {
        let count = (self.transcript.len() / 8).max(1);
        self.transcript.remove_first(count);
        self.messages.retain(|(_, index)| *index >= count);
        for (_, index) in &mut self.messages {
            *index = index.saturating_sub(count);
        }
        self.note("the transcript shows the most recent messages; the log keeps every one");
    }

    /// Orders the rows, the orchestrator first and then the most recently
    /// active, and gives the list a model of them.
    fn refresh_list(&mut self) {
        self.rows.sort_by(|a, b| {
            (b.role == Role::Orchestrator)
                .cmp(&(a.role == Role::Orchestrator))
                .then(b.activity.cmp(&a.activity))
                .then(a.id.cmp(&b.id))
        });
        let tree: Vec<TreeRow<usize>> = (0..self.rows.len())
            .map(|id| TreeRow {
                id,
                parent: None,
                depth: 0,
                children: false,
                expanded: false,
            })
            .collect();
        match Model::new(&tree, &COLUMNS) {
            Ok(model) => {
                if let Err(e) = self.list.replace(model) {
                    self.note(format!("the list: {e}"));
                }
            }
            Err(e) => self.note(format!("the list: {e}")),
        }
        let selected = self
            .active
            .as_ref()
            .and_then(|id| self.rows.iter().position(|r| &r.id == id));
        self.list.select(selected, true);
        self.touch();
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
        let bounds = self.surface.bounds();
        let _ = self.split.event(split::Event::Resize {
            surface: self.surface,
            rect: bounds,
        });
        self.place();
    }

    /// The panes where the split's layout puts them. A drag of the
    /// divider comes here alone: a resize of the split would end it.
    fn place(&mut self) {
        self.regions = self.split.layout().map(|layout| {
            let s = self.surface.scale.value();
            let right = layout.second;
            let status_height = ((ROW * s) as u32).min(right.height);
            let rest = right.height - status_height;
            let wanted = (COMPOSER_ROWS * CELL_HEIGHT * s + 2 * s) as u32;
            let composer_height = wanted.min(rest / 2);
            let rule_height = (s as u32).min(rest - composer_height);
            let transcript_height = rest - composer_height - rule_height;
            let at = |y: u32, height: u32| Rect {
                x: right.x,
                y: right.y + i64::from(y),
                width: right.width,
                height,
            };
            Regions {
                list: layout.first,
                transcript: at(0, transcript_height),
                rule: at(transcript_height, rule_height),
                composer: at(transcript_height + rule_height, composer_height),
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
        let on = |f: Focus| self.focused && self.focus == f;
        let list_focus = if on(Focus::List) {
            tree_table::Focus::Rows
        } else {
            tree_table::Focus::None
        };
        let (list, transcript, composer) = (list_focus, on(Focus::Transcript), on(Focus::Composer));
        let _ = self.list.set_focus(list);
        self.transcript.event(
            messages::Event::Focus(transcript),
            &mut td_ui::window::NoClipboard,
        );
        self.composer.event(PaneEvent::Focus(composer));
        self.touch();
    }

    /// The conversation `step` rows from the active one, opened.
    fn switch(&mut self, step: isize) {
        let at = self
            .active
            .as_ref()
            .and_then(|id| self.rows.iter().position(|r| &r.id == id));
        let next = match at {
            Some(at) => at.checked_add_signed(step),
            None => Some(0),
        };
        if let Some(row) = next.and_then(|i| self.rows.get(i)) {
            let id = row.id.clone();
            self.open(id);
        }
    }

    /// Opens conversation `id`; opening the one already open again
    /// retries it when its process failed.
    fn open(&mut self, id: Id) {
        if self.active.as_ref() != Some(&id) || self.active_failed() {
            self.set_active(id.clone());
            self.requests.push(Request::Open(id));
        }
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

    /// One input from the window, with its clipboard.
    pub fn input(&mut self, input: Input<'_>, clipboard: &mut dyn Clipboard) {
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
            Input::Paste(text) => {
                if self.focus == Focus::Composer {
                    match self.composer.insert(text) {
                        Ok(true) => self.touch(),
                        Ok(false) => {}
                        Err(e) => self.note(e),
                    }
                }
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
        self.apply_focus();
    }

    /// A key, by its chord: the window's own first, then the focused
    /// widget's.
    pub fn key(&mut self, chord: &str, repeat: bool, clipboard: &mut dyn Clipboard) {
        match chord {
            "C-n" if !repeat => {
                self.requests.push(Request::New);
                return;
            }
            "C-PageUp" => return self.switch(-1),
            "C-PageDown" => return self.switch(1),
            "C-Return" if !repeat => return self.send(),
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
                    tree_table::Outcome::Activate(index) => {
                        if let Some(row) = self.rows.get(index) {
                            let id = row.id.clone();
                            self.open(id);
                        }
                    }
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
                match self.list.event(event) {
                    tree_table::Outcome::Selected(index) | tree_table::Outcome::Activate(index) => {
                        if let Some(row) = self.rows.get(index) {
                            let id = row.id.clone();
                            self.open(id);
                        }
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
        let text = self.rows.get(index).map_or("", |row| match column {
            0 => row.title.as_str(),
            _ => row.state.word(),
        });
        Cell::new(text).unwrap_or_else(|_| Cell::empty())
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
            // Too small for the split: say so rather than draw over itself.
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
            return;
        };
        self.split.emit(damage, sink);
        self.list.emit(
            None,
            damage,
            &mut |index, column| self.cell(index, column),
            sink,
        );
        self.transcript.emit(damage, sink);
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

    fn row(n: u8, role: Role, activity: u64) -> Row {
        Row {
            id: id(n),
            role,
            title: format!("title {n}"),
            activity,
            state: RowState::Closed,
        }
    }

    pub fn app() -> App {
        let surface = Surface::new(1024, 640, Scale::default()).unwrap();
        let mut app = App::new(surface, None, Mode::Auto).unwrap();
        app.set_rows(vec![
            row(2, Role::Conversation, 50),
            row(1, Role::Orchestrator, 10),
            row(3, Role::Conversation, 90),
        ]);
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
    fn the_orchestrator_is_first_then_the_most_recently_active() {
        let app = app();
        let order: Vec<&Id> = app.rows().iter().map(|r| &r.id).collect();
        assert_eq!(order, [&id(1), &id(3), &id(2)]);
        let shown = text(&app);
        let (first, rest) = shown.split_once("title 1").unwrap();
        assert!(first.contains("Conversation"), "{shown}");
        assert!(rest.find("title 3") < rest.find("title 2"), "{shown}");
    }

    #[test]
    fn the_status_row_says_what_is_not_there_yet() {
        let mut app = app();
        assert!(text(&app).contains("starting | no model | mode auto | no limits"));
        app.update(
            Update::Up(Up::Hello {
                role: Role::Orchestrator,
                title: "Orchestrator".into(),
                torn: Some(7),
                interrupted: vec![],
            }),
            0,
        );
        let shown = text(&app);
        assert!(
            shown.contains("idle | dropped a torn final log line of 7 bytes | no model"),
            "{shown}"
        );
    }

    #[test]
    fn return_is_a_newline_and_c_return_sends_and_empties_the_composer() {
        let mut app = app();
        for chord in ["h", "i", "Return", "t", "h", "e", "r", "e"] {
            key(&mut app, chord);
        }
        assert_eq!(app.composed(), "hi\nthere");
        assert!(app.take_requests().is_empty(), "Return sends nothing");
        key(&mut app, "C-Return");
        assert_eq!(app.take_requests(), [Request::Send("hi\nthere".into())]);
        assert_eq!(app.composed(), "");
        // An empty or blank composer sends nothing.
        key(&mut app, "C-Return");
        key(&mut app, "space");
        key(&mut app, "C-Return");
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
                role: Role::Orchestrator,
                title: "Orchestrator".into(),
                torn: None,
                interrupted: vec![2],
            }),
            0,
        );
        assert_eq!(app.transcript().len(), 0);
        assert!(app
            .notice()
            .is_some_and(|n| n.contains("interrupted 1 turn")));
    }

    #[test]
    fn c_n_asks_for_a_conversation_and_c_page_keys_switch() {
        let mut app = app();
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

    #[test]
    fn a_window_too_small_for_the_split_says_so() {
        let surface = Surface::new(200, 200, Scale::default()).unwrap();
        let app = App::new(surface, None, Mode::Auto).unwrap();
        assert!(text(&app).contains("too small"));
    }
}

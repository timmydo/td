//! The window's state: the image's applications in a list, the selected
//! one's details and actions, the question Remove and Delete data ask, and
//! the status row. Pure: it hands `Job`s out through `take_jobs` and takes
//! the worker's `Reply`s, so a test drives it whole.

use td_ui::chrome::{Buttons, Item, List, Status as StatusRow, ROW};
use td_ui::confirmations::{self, Choice};
use td_ui::keys::{self, Section};
use td_ui::raster::{
    self, Composition, Draw, GlyphStyle, Primitive, Raster, Rect, Surface, INK, LINE_NUMBER, PAPER,
    WARNING,
};
use td_ui::window::{Input, PointerPhase};
use td_ui::{CELL_HEIGHT, CELL_WIDTH};

use crate::registry::Application;
use crate::snapshot::{
    CacheStatus, GenerationStatus, Operation, Part, Progress, Record, Snapshot, Stage, Status,
};
use crate::worker::{Job, Reply};

/// How often a Flathub application's status is asked: while something
/// changes the cache, while nothing does, when root's record says the
/// image ships it after all, and after a failure. One the registry does
/// not declare from Flathub is never asked: the image ships it.
pub const POLL_ACTIVE_MS: u64 = 1000;
pub const POLL_IDLE_MS: u64 = 15_000;
pub const POLL_IMAGE_MS: u64 = 60_000;
pub const POLL_RETRY_MS: u64 = 5_000;

/// The most rows the list shows at once.
const LIST_ROWS: usize = 6;
/// A detail line's step below the last, in font pixels.
const LINE_STEP: usize = CELL_HEIGHT + 4;

/// What the selected application offers.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Action {
    Launch,
    Install,
    Update,
    Remove,
    DeleteData,
}

impl Action {
    pub fn label(self) -> &'static str {
        match self {
            Self::Launch => "Launch",
            Self::Install => "Install",
            Self::Update => "Check now",
            Self::Remove => "Remove",
            Self::DeleteData => "Delete data",
        }
    }

    fn operation(self) -> Option<Operation> {
        match self {
            Self::Launch => None,
            Self::Install => Some(Operation::Install),
            Self::Update => Some(Operation::Update),
            Self::Remove => Some(Operation::Remove),
            Self::DeleteData => Some(Operation::DeleteData),
        }
    }
}

/// What a question confirms, for the entry it was asked about.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Act {
    Remove(usize),
    DeleteData(usize),
}

type Dialog = confirmations::Controller<Act, u64, ()>;

/// The window's own keys.
pub const KEYS: &[(&str, &str)] = &[
    ("Up/Down", "select the application above or below"),
    ("Home/End", "select the first or last application"),
    ("Return", "launch the application, or install it"),
    ("l", "launch"),
    ("i", "install"),
    ("c", "check for an update now"),
    ("x", "remove, keeping its data"),
    ("D", "delete a removed application's data"),
    ("F5", "read every status again"),
    ("C-q", "quit"),
];
const KEYS_TITLE: &str = "Software";

/// How the selected application stands, as its status says.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Standing {
    /// No status has been read yet.
    Unknown,
    /// The image ships it; it updates with the system.
    BuiltIn,
    NotInstalled,
    /// Installed; `stale` when the deployment changed what it is admitted
    /// under, so a launch revalidates it.
    Installed {
        stale: bool,
    },
}

struct Entry {
    application: Application,
    snapshot: Option<Snapshot>,
    /// The last record root read: kept while later reads fail, so one
    /// failed read neither hides its actions nor closes a question.
    known: Option<Status>,
    /// The last request's failure.
    error: Option<String>,
    /// A status request is out.
    asking: bool,
    next_poll: u64,
    /// An operation asked for whose start no status has shown yet.
    pending: Option<Operation>,
    /// Root started `pending`, or may have; the next status shows it.
    started: bool,
    /// The request for `pending` got no `ok` that said root did not act
    /// on it, so the next status decides whether it started.
    uncertain: bool,
    /// Asked to read again while a status request was out.
    again: bool,
    /// `last=` when `pending` was asked for, so an outcome an uncertain
    /// request did not cause is not taken for its own.
    last_before: Option<(Operation, Option<String>)>,
}

impl Entry {
    fn standing(&self) -> Standing {
        if self.application.flathub.is_none() {
            return Standing::BuiltIn;
        }
        let Some(status) = &self.known else {
            return Standing::Unknown;
        };
        match &status.cache {
            None => Standing::BuiltIn,
            Some(cache) => match &cache.current {
                None => Standing::NotInstalled,
                Some(current) => Standing::Installed {
                    stale: !current.serves,
                },
            },
        }
    }

    /// The cache as root last read it.
    fn cache(&self) -> Option<&CacheStatus> {
        self.known.as_ref()?.cache.as_ref()
    }

    /// Whether root's every read so far failed: a damaged cache, which
    /// `remove` clears.
    fn unreadable(&self) -> bool {
        self.known.is_none()
            && self
                .snapshot
                .as_ref()
                .is_some_and(|snapshot| matches!(snapshot.record, Record::Failed))
    }

    /// What runs on it: an operation asked for, root's, or another
    /// path's writer.
    fn running(&self) -> Option<Option<Operation>> {
        if let Some(pending) = self.pending {
            return Some(Some(pending));
        }
        let snapshot = self.snapshot.as_ref()?;
        if let Some(operation) = snapshot.operation {
            return Some(Some(operation));
        }
        snapshot.busy().then_some(None)
    }

    /// Whether its actions wait: something writes the cache, or root's
    /// record predates the operation that just ended.
    fn busy(&self) -> bool {
        self.running().is_some()
            || self
                .snapshot
                .as_ref()
                .is_some_and(|snapshot| matches!(snapshot.record, Record::Older(_)))
    }

    fn actions(&self) -> Vec<(Action, bool)> {
        let busy = self.busy();
        match self.standing() {
            Standing::Unknown if self.unreadable() => vec![(Action::Remove, !busy)],
            Standing::Unknown | Standing::BuiltIn => Vec::new(),
            Standing::NotInstalled => {
                let cache = self.cache();
                // Delete data is for a removed application: one never
                // checked has kept nothing, and td-app refuses while a
                // previous version is kept.
                let removed = cache.is_some_and(|cache| {
                    cache.previous.is_none() && (cache.checked.is_some() || cache.refused.is_some())
                });
                let mut actions = vec![(Action::Install, !busy)];
                if removed {
                    actions.push((Action::DeleteData, !busy));
                }
                actions
            }
            Standing::Installed { .. } => {
                let removing = matches!(self.running(), Some(Some(Operation::Remove)));
                vec![
                    (Action::Launch, !removing),
                    (Action::Update, !busy),
                    (Action::Remove, !busy),
                ]
            }
        }
    }

    /// The list's right column.
    fn meta(&self) -> &'static str {
        if self.error.is_some() {
            return "unreachable";
        }
        match self.running() {
            Some(Some(Operation::Install)) => return "installing",
            Some(Some(Operation::Update)) => return "checking",
            Some(Some(Operation::Remove)) => return "removing",
            Some(Some(Operation::DeleteData)) => return "deleting data",
            Some(None) => return "busy",
            None => {}
        }
        match self.standing() {
            Standing::Unknown if self.unreadable() => "unreadable",
            Standing::Unknown => "...",
            Standing::BuiltIn => "built in",
            Standing::NotInstalled => "not installed",
            Standing::Installed { stale: true } => "needs checking",
            Standing::Installed { stale: false } => "installed",
        }
    }

    /// What changes the dialog's question: its standing and whether it
    /// is busy.
    fn key(&self) -> (Standing, bool) {
        (self.standing(), self.busy())
    }
}

pub struct App {
    surface: Surface,
    entries: Vec<Entry>,
    selected: usize,
    first: usize,
    dialog: Option<Dialog>,
    /// Bumped whenever the selected application's standing changes, so a
    /// question asked about an older state closes unanswered.
    revision: u64,
    /// A press armed this action's button; its release on it activates
    /// it, if the action is still offered there.
    armed: Option<Action>,
    /// A launch request is out.
    launching: bool,
    jobs: Vec<Job>,
    message: String,
    now_ms: u64,
    redraw: bool,
    quitting: bool,
    show_keys: bool,
}

/// Where a dialog may go: centred over the window's body.
fn dialog_rect(surface: Surface) -> Option<Rect> {
    let s = surface.scale.value() as i64;
    let cell = CELL_WIDTH as i64 * s;
    let row = ROW as i64 * s;
    let body = surface.height as i64 - row;
    let width = (surface.width as i64 - 2 * cell).min(72 * cell);
    let height = (10 * row).min(body);
    (width > 0 && height > 0).then(|| Rect {
        x: (surface.width as i64 - width) / 2,
        y: (body - height).max(0) / 2,
        width: width as u32,
        height: height as u32,
    })
}

/// The parts of the window, from the surface alone, so a press lands on
/// what was painted.
struct Layout {
    list: Option<List>,
    details: Rect,
    buttons_y: i64,
}

impl App {
    /// A window over `applications`; `problem` says why there are none
    /// when the tables could not be read.
    pub fn new(applications: Vec<Application>, problem: Option<String>, surface: Surface) -> Self {
        let entries = applications
            .into_iter()
            .map(|application| Entry {
                application,
                snapshot: None,
                known: None,
                error: None,
                asking: false,
                next_poll: 0,
                pending: None,
                started: false,
                uncertain: false,
                again: false,
                last_before: None,
            })
            .collect::<Vec<_>>();
        let message = match problem {
            Some(problem) => problem,
            None if entries.is_empty() => "The image declares no applications".into(),
            None => String::new(),
        };
        Self {
            surface,
            entries,
            selected: 0,
            first: 0,
            dialog: None,
            revision: 0,
            armed: None,
            launching: false,
            jobs: Vec::new(),
            message,
            now_ms: 0,
            redraw: true,
            quitting: false,
            show_keys: false,
        }
    }

    pub fn title(&self) -> &str {
        "Software"
    }
    pub fn quitting(&self) -> bool {
        self.quitting
    }
    pub fn needs_redraw(&self) -> bool {
        self.redraw
    }
    pub fn dialog_open(&self) -> bool {
        self.dialog.is_some()
    }
    /// The status row's message, without the key hint.
    pub fn message(&self) -> &str {
        &self.message
    }
    /// The applications' names, in the list's order.
    pub fn names(&self) -> Vec<&str> {
        self.entries
            .iter()
            .map(|entry| entry.application.name.as_str())
            .collect()
    }
    pub fn selected(&self) -> Option<&str> {
        Some(self.entries.get(self.selected)?.application.name.as_str())
    }
    /// The selected application's standing.
    pub fn standing(&self) -> Option<Standing> {
        Some(self.entries.get(self.selected)?.standing())
    }
    /// The list's right column for each application.
    pub fn metas(&self) -> Vec<&'static str> {
        self.entries.iter().map(Entry::meta).collect()
    }
    /// The selected application's actions and whether each is enabled.
    pub fn actions(&self) -> Vec<(Action, bool)> {
        self.entries
            .get(self.selected)
            .map(Entry::actions)
            .unwrap_or_default()
    }
    /// How long the loop may wait: briefly while anything is busy.
    pub fn wait_ms(&self) -> u64 {
        if self
            .entries
            .iter()
            .any(|entry| entry.busy() || entry.asking)
        {
            100
        } else {
            1000
        }
    }

    pub fn key_list(&self) -> Vec<Section> {
        let question = Section::new("Question", confirmations::KEYS);
        let window = Section::new(KEYS_TITLE, KEYS);
        if self.dialog_open() {
            vec![question, window]
        } else {
            vec![window, question]
        }
    }

    pub fn take_show_keys(&mut self) -> bool {
        std::mem::take(&mut self.show_keys)
    }

    pub fn take_jobs(&mut self) -> Vec<Job> {
        std::mem::take(&mut self.jobs)
    }

    fn say(&mut self, message: impl Into<String>) {
        self.message = message.into();
        self.redraw = true;
    }

    /// Asks every status that is due.
    pub fn tick(&mut self, now_ms: u64) {
        self.now_ms = now_ms;
        for entry in &mut self.entries {
            // An application the image ships has no cache to read.
            let flathub = entry.application.flathub.is_some();
            if flathub && !entry.asking && now_ms >= entry.next_poll {
                entry.asking = true;
                self.jobs.push(Job::Status(entry.application.name.clone()));
            }
        }
    }

    fn refresh(&mut self) {
        for entry in &mut self.entries {
            entry.next_poll = 0;
            entry.again = entry.asking;
        }
        self.tick(self.now_ms);
        self.say("Reading every status again");
    }

    pub fn reply(&mut self, reply: Reply) {
        self.redraw = true;
        let before = self.entries.get(self.selected).map(Entry::key);
        match reply {
            Reply::Status(name, result) => self.status(&name, *result),
            Reply::Operation(operation, name, result) => {
                let now = self.now_ms;
                let display = self.display(&name);
                let Some(entry) = self.entry_mut(&name) else {
                    return;
                };
                entry.next_poll = now;
                match result {
                    Ok(()) => {
                        entry.started = true;
                        self.say(format!("{} {display}", operation.doing()));
                    }
                    Err(failure) if failure.definite => {
                        entry.pending = None;
                        self.say(format!(
                            "Root did not start the {}: {}",
                            operation.noun(),
                            failure.reason
                        ));
                    }
                    // It may have reached root: the next status decides.
                    Err(failure) => {
                        entry.started = true;
                        entry.uncertain = true;
                        self.say(format!(
                            "The {} of {display} may not have started: {}",
                            operation.noun(),
                            failure.reason
                        ));
                    }
                }
            }
            Reply::Launch(name, result) => {
                self.launching = false;
                let display = self.display(&name);
                match result {
                    Ok(()) => self.say(format!("Launching {display}")),
                    Err(error) => self.say(format!("Cannot launch {display}: {error}")),
                }
            }
        }
        if self.entries.get(self.selected).map(Entry::key) != before {
            self.revision += 1;
            self.armed = None;
            // A question asked about the older state closes now, unanswered.
            self.dialog_event(confirmations::Event::Other);
        }
    }

    fn display(&self, name: &str) -> String {
        self.entries
            .iter()
            .find(|entry| entry.application.name == name)
            .map_or_else(
                || name.to_string(),
                |entry| entry.application.display.clone(),
            )
    }

    fn entry_mut(&mut self, name: &str) -> Option<&mut Entry> {
        self.entries
            .iter_mut()
            .find(|entry| entry.application.name == name)
    }

    fn status(&mut self, name: &str, result: Result<Snapshot, String>) {
        let now = self.now_ms;
        let display = self.display(name);
        let Some(entry) = self.entry_mut(name) else {
            return;
        };
        entry.asking = false;
        let again = std::mem::take(&mut entry.again);
        let snapshot = match result {
            Ok(snapshot) => snapshot,
            Err(error) => {
                entry.error = Some(error);
                entry.next_poll = now.saturating_add(POLL_RETRY_MS);
                return;
            }
        };
        entry.error = None;
        // The first status after root started an operation, or may have,
        // shows it running, already ended, or never started.
        let mut ended = None;
        let mut unstarted = None;
        if entry.started {
            if let Some(pending) = entry.pending.take() {
                if snapshot.operation.is_none() {
                    ended = snapshot.last.clone().filter(|(verb, _)| *verb == pending);
                    if entry.uncertain && snapshot.last == entry.last_before {
                        ended = None;
                    }
                    if ended.is_none() && entry.uncertain {
                        unstarted = Some(pending);
                    }
                }
            }
            entry.started = false;
            entry.uncertain = false;
        }
        if let Some(Some(running)) = entry.snapshot.as_ref().map(|old| old.operation) {
            if snapshot.operation.is_none() {
                ended = snapshot.last.clone().filter(|(verb, _)| *verb == running);
            }
        }
        let image =
            matches!(snapshot.record, Record::Current(ref status) if status.cache.is_none());
        let active = again
            || snapshot.busy()
            || entry.pending.is_some()
            || matches!(
                snapshot.record,
                Record::Unread | Record::Older(_) | Record::Failed
            );
        entry.next_poll = now.saturating_add(if again {
            0
        } else if active {
            POLL_ACTIVE_MS
        } else if image {
            POLL_IMAGE_MS
        } else {
            POLL_IDLE_MS
        });
        if let Some(status) = snapshot.status() {
            entry.known = Some(status.clone());
        }
        entry.snapshot = Some(snapshot);
        if let Some(operation) = unstarted {
            self.say(format!(
                "The {} of {display} did not start",
                operation.noun()
            ));
        }
        if let Some((operation, refusal)) = ended {
            match refusal {
                None => self.say(match operation {
                    Operation::Install => format!("{display} is installed"),
                    Operation::Update => format!("Checked {display} for an update"),
                    Operation::Remove => format!("Removed {display}; its data is kept"),
                    Operation::DeleteData => format!("Deleted {display}'s data"),
                }),
                Some(reason) => self.say(format!(
                    "The {} of {display} failed: {reason}",
                    operation.noun()
                )),
            }
        }
    }

    // ---- actions

    fn act(&mut self, action: Action) {
        let Some(entry) = self.entries.get(self.selected) else {
            return;
        };
        if !entry
            .actions()
            .iter()
            .any(|(offered, enabled)| *offered == action && *enabled)
        {
            return;
        }
        let name = entry.application.name.clone();
        let display = entry.application.display.clone();
        match action {
            // One launch at a time: the compositor answers each.
            Action::Launch if self.launching => {}
            Action::Launch => {
                self.launching = true;
                self.jobs.push(Job::Launch(name));
                self.say(format!("Asking for {display}"));
            }
            Action::Remove => self.ask(Act::Remove(self.selected), &display),
            Action::DeleteData => self.ask(Act::DeleteData(self.selected), &display),
            Action::Install | Action::Update => self.start(action, self.selected),
        }
    }

    fn start(&mut self, action: Action, index: usize) {
        let Some(operation) = action.operation() else {
            return;
        };
        let Some(entry) = self.entries.get_mut(index) else {
            return;
        };
        if !entry.actions().contains(&(action, true)) {
            return self.say("The application changed; ask again");
        }
        entry.last_before = entry.snapshot.as_ref().and_then(|s| s.last.clone());
        entry.pending = Some(operation);
        entry.started = false;
        entry.uncertain = false;
        let name = entry.application.name.clone();
        let display = entry.application.display.clone();
        self.jobs.push(Job::Operation(operation, name));
        self.revision += 1;
        let asked = match operation {
            Operation::Install => "install",
            Operation::Update => "check for an update to",
            Operation::Remove => "remove",
            Operation::DeleteData => "delete the data of",
        };
        self.say(format!("Asking root to {asked} {display}"));
    }

    fn ask(&mut self, act: Act, display: &str) {
        let (title, label, details) = match act {
            Act::Remove(_) => (
                format!("Remove {display}?"),
                "Remove",
                vec![
                    format!("{display}'s download is deleted; installing it again fetches it again."),
                    format!("Its data stays: {display}'s profile and settings are kept, and Delete data removes them afterwards."),
                ],
            ),
            Act::DeleteData(_) => (
                format!("Delete {display}'s data?"),
                "Delete data",
                vec![
                    format!("{display}'s profile, settings and everything else it kept are deleted for good."),
                    "This cannot be undone.".to_string(),
                ],
            ),
        };
        let details: Vec<&str> = details.iter().map(String::as_str).collect();
        let Some(rect) = dialog_rect(self.surface) else {
            return self.say("The window is too small for the question");
        };
        let dialog = confirmations::Model::new(&title, label, &details, act, self.revision)
            .and_then(|model| Dialog::new(model, self.surface, rect, None));
        match dialog {
            Ok(dialog) => {
                self.dialog = Some(dialog);
                self.armed = None;
                self.say(format!(
                    "Tab to {label} and press Return, or Escape to keep it"
                ));
            }
            Err(error) => self.say(format!("Cannot ask: {error}")),
        }
    }

    fn dialog_event(&mut self, event: confirmations::Event) {
        let Some(dialog) = &mut self.dialog else {
            return;
        };
        match dialog.event(Some(self.revision), false, event) {
            confirmations::Outcome::Ignored | confirmations::Outcome::Consumed => {}
            confirmations::Outcome::Changed => self.redraw = true,
            confirmations::Outcome::Closed { choice, .. } => {
                self.dialog = None;
                self.redraw = true;
                match choice {
                    Choice::Confirmed(Act::Remove(index)) => self.start(Action::Remove, index),
                    Choice::Confirmed(Act::DeleteData(index)) => {
                        self.start(Action::DeleteData, index);
                    }
                    Choice::Cancelled => self.say("Nothing changed"),
                    Choice::Stale => self.say("The application changed; ask again"),
                    Choice::Unavailable(_) => self.say("The window is too small for the question"),
                }
            }
        }
    }

    /// The default action: launch an installed application, else install.
    fn activate(&mut self) {
        let actions = self.actions();
        for wanted in [Action::Launch, Action::Install] {
            if actions.contains(&(wanted, true)) {
                return self.act(wanted);
            }
        }
    }

    fn select(&mut self, index: usize) {
        let index = index.min(self.entries.len().saturating_sub(1));
        if index != self.selected {
            self.selected = index;
            self.revision += 1;
            self.armed = None;
            self.redraw = true;
        }
        if let Some(list) = self.layout().list {
            self.first = list.reveal(self.entries.len(), self.selected, self.first);
        }
    }

    // ---- input

    pub fn input(&mut self, input: Input<'_>) {
        match input {
            Input::Resize(surface) => {
                self.surface = surface;
                self.relayout();
            }
            Input::Close => self.quitting = true,
            Input::Focus(false) => {
                self.armed = None;
                self.dialog_event(confirmations::Event::FocusLost);
                self.redraw = true;
            }
            Input::Key { chord, repeat } => self.key(chord, repeat),
            Input::Pointer { phase, x, y, .. } => self.pointer(phase, x, y),
            Input::CancelPointer => {
                self.armed = None;
                self.redraw = true;
                self.dialog_event(confirmations::Event::Other);
            }
            Input::Wheel { rows, .. } if self.dialog.is_none() => {
                let target = if rows < 0 {
                    self.selected.saturating_sub(rows.unsigned_abs())
                } else {
                    self.selected.saturating_add(rows.unsigned_abs())
                };
                self.select(target);
            }
            _ => {}
        }
    }

    fn relayout(&mut self) {
        let surface = self.surface;
        if let Some(dialog) = &mut self.dialog {
            match dialog_rect(surface) {
                Some(rect) => {
                    let outcome = dialog.event(
                        Some(self.revision),
                        false,
                        confirmations::Event::Resize { surface, rect },
                    );
                    if matches!(outcome, confirmations::Outcome::Closed { .. }) {
                        self.dialog = None;
                        self.say("The window is too small for the question; nothing changed");
                    }
                }
                None => {
                    self.dialog = None;
                    self.say("The window is too small for the question; nothing changed");
                }
            }
        }
        self.select(self.selected);
        self.redraw = true;
    }

    fn key(&mut self, chord: &str, repeat: bool) {
        if chord == "C-q" {
            self.quitting = true;
            return;
        }
        if self.dialog.is_some() {
            let event = match confirmations::Key::from_chord(chord) {
                Some(key) => confirmations::Event::Key {
                    key,
                    repeated: repeat,
                },
                None => confirmations::Event::Other,
            };
            return self.dialog_event(event);
        }
        match chord {
            "Up" => self.select(self.selected.saturating_sub(1)),
            "Down" => self.select(self.selected.saturating_add(1)),
            "Home" => self.select(0),
            "End" => self.select(self.entries.len()),
            _ if repeat => {}
            "Return" => self.activate(),
            "l" => self.act(Action::Launch),
            "i" => self.act(Action::Install),
            "c" => self.act(Action::Update),
            "x" => self.act(Action::Remove),
            "D" => self.act(Action::DeleteData),
            "F5" => self.refresh(),
            _ => {}
        }
    }

    fn pointer(&mut self, phase: PointerPhase, x: i64, y: i64) {
        if self.dialog.is_some() {
            return self.dialog_event(match phase {
                PointerPhase::Press => confirmations::Event::Press { x, y },
                PointerPhase::Move => confirmations::Event::Move { x, y },
                PointerPhase::Release => confirmations::Event::Release { x, y },
            });
        }
        // The status row is painted over everything below it, so it
        // takes its own presses first.
        let status = StatusRow::new(self.surface).rect().contains(x, y);
        let layout = self.layout();
        let actions = self.actions();
        let labels: Vec<&str> = actions.iter().map(|(action, _)| action.label()).collect();
        let hit = (!status)
            .then(|| Buttons::new(self.surface, layout.buttons_y, &labels).hit(x, y))
            .flatten()
            .and_then(|index| actions.get(index))
            .map(|(action, _)| *action);
        match phase {
            PointerPhase::Press => {
                if self.armed.take().is_some() {
                    self.redraw = true;
                }
                if let Some(action) = hit {
                    if actions.contains(&(action, true)) {
                        self.armed = Some(action);
                        self.redraw = true;
                    }
                    return;
                }
                if status {
                    self.show_keys = true;
                    return;
                }
                if let Some(row) = layout.list.and_then(|list| list.hit(x, y)) {
                    self.select(self.first.saturating_add(row));
                }
            }
            PointerPhase::Move => {}
            PointerPhase::Release => {
                let armed = self.armed.take();
                if armed.is_some() {
                    self.redraw = true;
                }
                if let Some(action) = armed.filter(|action| hit == Some(*action)) {
                    self.act(action);
                }
            }
        }
    }

    // ---- paint

    fn layout(&self) -> Layout {
        let surface = self.surface;
        let s = surface.scale.value();
        let margin = (CELL_WIDTH * s) as i64;
        let row = (ROW * s) as i64;
        let status = StatusRow::new(surface).rect();
        // As many rows as the strip wraps to on this width.
        let actions = self.actions();
        let labels: Vec<&str> = actions.iter().map(|(action, _)| action.label()).collect();
        let strip = Buttons::new(surface, 0, &labels).rows() as i64;
        let buttons_y = (status.y - strip * row - margin / 2).max(0);
        let rows = self.entries.len().clamp(1, LIST_ROWS) as i64;
        let width = surface.width as i64 - 2 * margin;
        let list = (width > 0)
            .then(|| Rect {
                x: margin,
                y: margin,
                width: width as u32,
                height: (rows * row) as u32,
            })
            .and_then(|rect| List::new(surface, rect));
        let top = list.map_or(margin, |list| {
            let rect = list.rect();
            rect.y + i64::from(rect.height) + margin
        });
        Layout {
            list,
            details: Rect {
                x: margin,
                y: top,
                width: width.max(0) as u32,
                height: (buttons_y - top).max(0) as u32,
            },
            buttons_y,
        }
    }

    /// The selected application's details, line by line, with each
    /// line's ink.
    pub fn details(&self) -> Vec<(String, u32)> {
        let Some(entry) = self.entries.get(self.selected) else {
            return Vec::new();
        };
        let display = &entry.application.display;
        let mut lines = Vec::new();
        let mut line = |text: String, ink: u32| lines.push((text, ink));
        match entry.application.flathub {
            Some(source) => line(format!("{display}, from Flathub ({source})"), INK),
            None => line(display.clone(), INK),
        }
        match entry.standing() {
            Standing::Unknown => match &entry.error {
                Some(error) => line(format!("Its status cannot be read: {error}"), WARNING),
                None if entry.unreadable() => line(
                    "Root cannot read its status, which a damaged download can cause: \
                     Remove deletes the download and keeps its data."
                        .into(),
                    WARNING,
                ),
                None if entry.snapshot.is_some() => {}
                None => line("Reading its status".into(), LINE_NUMBER),
            },
            Standing::BuiltIn => {
                line(
                    format!("Built in: {display} ships with the system and updates with it."),
                    INK,
                );
                line("It cannot be removed here.".into(), LINE_NUMBER);
            }
            Standing::NotInstalled => line(
                "Not installed. Install downloads it from Flathub; nothing is fetched until you ask."
                    .into(),
                INK,
            ),
            Standing::Installed { stale: false } => line("Installed.".into(), INK),
            Standing::Installed { stale: true } => line(
                "Installed, but a system update changed what it is admitted under: \
                 launching it checks the files it has again, and Check now fetches it again."
                    .into(),
                WARNING,
            ),
        }
        // What is changing comes first, so a short window keeps it.
        if let Some(error) = entry.error.as_ref().filter(|_| entry.snapshot.is_some()) {
            line(
                format!("Its status cannot be read now, so what follows may be old: {error}"),
                WARNING,
            );
        }
        // Progress only as just read: a writer killed mid-download can
        // leave the cache unreadable, and Remove must then stay on.
        let fresh = entry.snapshot.as_ref().and_then(Snapshot::cache);
        if let Some(progress) = fresh.and_then(|cache| cache.progress.as_ref()) {
            progressing(&mut line, progress);
        }
        let cache = entry.cache();
        if let Some(snapshot) = &entry.snapshot {
            match (snapshot.operation, &snapshot.last) {
                (Some(operation), _) => line(
                    format!(
                        "Root is running the {} as the application's account.",
                        operation.noun()
                    ),
                    INK,
                ),
                (None, Some((operation, None))) => line(
                    format!("The last {} finished.", operation.noun()),
                    LINE_NUMBER,
                ),
                (None, Some((operation, Some(reason)))) => line(
                    format!("The last {} failed: {reason}", operation.noun()),
                    WARNING,
                ),
                (None, None) => {}
            }
            if matches!(snapshot.record, Record::Failed) {
                line("Root could not read its status this time.".into(), WARNING);
            }
        }
        if let Some(cache) = cache {
            if let Some(current) = &cache.current {
                generation(&mut line, "Version", current);
            }
            if let Some(previous) = &cache.previous {
                let commit = short(&previous.application.row.commit);
                line(
                    format!(
                        "The previous version, application commit {commit}, is kept beside it."
                    ),
                    LINE_NUMBER,
                );
            }
            line(
                match cache.checked {
                    Some(time) => format!("Last checked: {}", when(time)),
                    None => "Last checked: never".into(),
                },
                INK,
            );
            if let Some(refused) = &cache.refused {
                line(
                    format!("Last refusal, {}: {}", when(refused.time), refused.reason),
                    WARNING,
                );
            }
        }
        lines
    }

    /// The status row's line.
    pub fn status_line(&self) -> String {
        let hint = format!("{}: keys", keys::CHORD);
        let shown = if self.message.is_empty() {
            "Return: launch or install  c: check now  x: remove  D: delete data"
        } else {
            self.message.as_str()
        };
        format!("{hint}  {shown}")
    }

    pub fn frame(&self) -> impl Composition + '_ {
        Frame { app: self }
    }

    pub fn paint(&mut self, raster: &mut Raster<'_, '_>, surface: Surface) -> Result<(), String> {
        if surface != self.surface {
            self.surface = surface;
            self.relayout();
        }
        self.redraw = false;
        raster
            .paint(&Frame { app: self }, surface.bounds())
            .map_err(|error| error.to_string())
    }
}

/// A commit as a person compares it: its first twelve digits.
fn short(commit: &str) -> &str {
    commit.get(..12).unwrap_or(commit)
}

/// A time as the details show it.
fn when(seconds: u64) -> String {
    i64::try_from(seconds)
        .ok()
        .and_then(td_civil::unix_to_civil_utc_checked)
        .map_or_else(
            || "an unreadable time".into(),
            |civil| format!("{} UTC", td_civil::format_ymd_hms(&civil)),
        )
}

/// Bytes in decimal units, as a download is sized.
pub fn size(bytes: u64) -> String {
    const UNITS: &[&str] = &["kB", "MB", "GB", "TB"];
    if bytes < 1000 {
        return format!("{bytes} bytes");
    }
    let mut value = bytes as f64 / 1000.0;
    let mut unit = 0;
    while value >= 1000.0 && unit + 1 < UNITS.len() {
        value /= 1000.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS.get(unit).copied().unwrap_or("TB"))
}

/// A count with its thousands apart.
pub fn count(value: u64) -> String {
    let digits = value.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

/// A ref's last two parts, `org.freedesktop.Platform 25.08`.
fn named(exact_ref: &str) -> String {
    let parts: Vec<&str> = exact_ref.split('/').collect();
    match (parts.get(1), parts.get(3)) {
        (Some(id), Some(branch)) => format!("{id} {branch}"),
        _ => exact_ref.to_string(),
    }
}

fn generation(line: &mut impl FnMut(String, u32), label: &str, current: &GenerationStatus) {
    let application = &current.application;
    let runtime = &current.runtime;
    line(
        format!(
            "{label}: application commit {} of {}, on {} commit {}",
            short(&application.row.commit),
            when(application.row.timestamp),
            named(&runtime.row.exact_ref),
            short(&runtime.row.commit),
        ),
        INK,
    );
    let measured = |bytes: Option<u64>| bytes.map_or_else(|| "unmeasured".to_string(), size);
    line(
        format!(
            "Size on disk: {} for the application, {} for its runtime",
            measured(application.bytes),
            measured(runtime.bytes)
        ),
        INK,
    );
}

fn progressing(line: &mut impl FnMut(String, u32), progress: &Progress) {
    let doing = match progress.stage {
        Stage::Busy => "Starting",
        Stage::Resolve => "Asking Flathub what to fetch",
        Stage::Fetch => "Fetching",
        Stage::Materialize => "Writing the files",
        Stage::Validate => "Checking the files",
        Stage::Publish => "Publishing",
        Stage::Collect => "Collecting old versions",
        Stage::Revalidate => "Checking the files it has again",
        Stage::Remove => "Removing",
        Stage::DeleteData => "Deleting its data",
    };
    let part = match progress.part {
        Some(Part::Application) => " the application",
        Some(Part::Runtime) => " the runtime",
        None => "",
    };
    line(format!("Working: {doing}{part}."), INK);
    if progress.objects_known > 0 || progress.bytes > 0 {
        line(
            format!(
                "{} of the {} objects found so far (never more than {}), {} downloaded (never more than {}).",
                count(progress.objects),
                count(progress.objects_known),
                count(progress.objects_bound),
                size(progress.bytes),
                size(progress.bytes_bound),
            ),
            INK,
        );
    }
    if progress.stage == Stage::Fetch {
        line(
            "Flathub names no total in advance: the objects found grow as each tree is read."
                .into(),
            LINE_NUMBER,
        );
    }
}

/// `text` cut into lines of at most `columns` characters, at spaces where
/// it has them.
fn wrap(text: &str, columns: usize) -> Vec<String> {
    let columns = columns.max(1);
    let mut lines = Vec::new();
    let mut current = String::new();
    for word in text.split(' ') {
        let mut word = word.to_string();
        loop {
            let used = current.chars().count();
            let length = word.chars().count();
            let gap = usize::from(used > 0);
            if used + gap + length <= columns {
                if gap == 1 {
                    current.push(' ');
                }
                current.push_str(&word);
                break;
            }
            if used > 0 {
                lines.push(std::mem::take(&mut current));
                continue;
            }
            let head: String = word.chars().take(columns).collect();
            word = word.chars().skip(columns).collect();
            lines.push(head);
            if word.is_empty() {
                break;
            }
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines
}

struct Frame<'a> {
    app: &'a App,
}

impl Composition for Frame<'_> {
    fn surface(&self) -> Surface {
        self.app.surface
    }

    fn emit(&self, damage: Rect, sink: &mut dyn FnMut(Draw)) {
        let app = self.app;
        let surface = app.surface;
        let s = surface.scale;
        let bounds = surface.bounds();
        if let Some(clip) = bounds.intersection(damage) {
            sink(Draw {
                clip,
                primitive: Primitive::Fill {
                    rect: bounds,
                    color: PAPER,
                },
            });
        }
        let layout = app.layout();
        if let Some(list) = layout.list {
            let metas: Vec<&str> = app.entries.iter().map(Entry::meta).collect();
            let items = app
                .entries
                .iter()
                .zip(&metas)
                .skip(app.first)
                .map(|(entry, meta)| Item {
                    label: &entry.application.display,
                    meta,
                    enabled: true,
                    marked: false,
                });
            list.emit(
                items,
                app.first,
                app.selected,
                app.entries.len(),
                damage,
                sink,
            );
        }
        let cell = (CELL_WIDTH * s.value()) as i64;
        let step = (LINE_STEP * s.value()) as i64;
        let columns = usize::try_from(i64::from(layout.details.width) / cell.max(1)).unwrap_or(0);
        let mut y = layout.details.y;
        let bottom = layout.details.y + i64::from(layout.details.height);
        'lines: for (index, (text, ink)) in app.details().iter().enumerate() {
            let style = GlyphStyle::medium(*ink, PAPER);
            let style = if index == 0 { style.bold() } else { style };
            for piece in wrap(text, columns) {
                if y + step > bottom {
                    break 'lines;
                }
                raster::text_run(
                    s,
                    piece.chars(),
                    (layout.details.x, y),
                    layout.details,
                    style,
                    damage,
                    sink,
                );
                y += step;
            }
        }
        let actions = app.actions();
        let labels: Vec<&str> = actions.iter().map(|(action, _)| action.label()).collect();
        if !labels.is_empty() {
            Buttons::new(surface, layout.buttons_y, &labels).emit(
                actions
                    .iter()
                    .map(|(action, enabled)| (app.armed == Some(*action), *enabled)),
                damage,
                sink,
            );
        }
        StatusRow::new(surface).emit(app.status_line().chars(), damage, sink);
        if let Some(dialog) = &app.dialog {
            dialog.emit(damage, sink);
        }
    }
}

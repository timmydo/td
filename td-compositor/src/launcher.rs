use crate::text::{Text, CELL_HEIGHT, CELL_WIDTH};
use crate::ui::{border, fill, intersect};
use crate::{filter, socket};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

const CARD_WIDTH: usize = 480;
const CARD_HEIGHT: usize = 546;
const CARD: [u8; 4] = [0x20, 0x18, 0x28, 0];
const HIGHLIGHT: [u8; 4] = [0x58, 0x30, 0x70, 0];
const CARD_PADDING: usize = 24;
const MAX_LAUNCHED_CLIENTS: usize = 16;
/// Where the first row's text starts below the card's top, how far apart
/// rows are, and the highlight drawn behind the selected one. One set of
/// numbers for the paint and the pointer, so a click lands on the row it
/// was drawn over.
const FIRST_ROW_TOP: usize = 92;
const ROW_STEP: usize = 42;
const ROW_INSET: usize = 14;
const ROW_RISE: usize = 8;
const ROW_HEIGHT: usize = 32;
/// A row's label is one row of text cells, so this tall.
const LABEL_HEIGHT: usize = CELL_HEIGHT;
/// The most rows a full-size card shows at once; the window of rows
/// scrolls with the selection over every match.
const CARD_ROWS: usize = (CARD_HEIGHT - FIRST_ROW_TOP - LABEL_HEIGHT) / ROW_STEP + 1;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LauncherAction {
    Open,
    Next,
    Previous,
    Close,
    Activate,
    /// The pointer's Activate: the `n`th row SHOWN, which is a position in
    /// the window of the filtered list rather than in the table.
    Choose(usize),
    Insert(char),
    Backspace,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LaunchRequest {
    UiDemo,
    Terminal,
    TaskManager,
    Editor,
    Photo,
    Review,
    Dua,
    Agent,
    Software,
    /// The launcher table's application at this position in the card's
    /// order (`Launcher::application`): raise its window or start it.
    Application(usize),
    /// Open td-software at that application, which is not installed.
    Install(usize),
}

impl LaunchRequest {
    /// The fixed authority program a request names, when it names one the
    /// authority starts as the human: the desktop programs the image ships.
    fn authority_program(self) -> Option<crate::authority::Program> {
        match self {
            Self::TaskManager => Some(crate::authority::Program::TaskManager),
            Self::Editor => Some(crate::authority::Program::Editor),
            Self::Photo => Some(crate::authority::Program::Photo),
            Self::Review => Some(crate::authority::Program::Review),
            Self::Dua => Some(crate::authority::Program::Dua),
            Self::Agent => Some(crate::authority::Program::Agent),
            Self::Software => Some(crate::authority::Program::Software),
            Self::UiDemo | Self::Terminal | Self::Application(_) | Self::Install(_) => None,
        }
    }
}

#[derive(Clone, Copy)]
struct Entry<'a> {
    label: &'a str,
    search: &'a str,
    request: Option<LaunchRequest>,
}

// First because it is the one a person came here for; the monitor below it
// is a diagnostic that happened to be the only client there was.
const TERMINAL_ENTRY: Entry = Entry {
    label: "NEW TERMINAL",
    search: "new terminal shell console prompt",
    request: Some(LaunchRequest::Terminal),
};

const CLOSE_ENTRY: Entry = Entry {
    label: "CLOSE LAUNCHER",
    search: "close launcher",
    request: None,
};

const SOFTWARE_ENTRY: Entry = Entry {
    label: "SOFTWARE",
    search: "software install remove applications apps flathub",
    request: Some(LaunchRequest::Software),
};

// The image's desktop programs, each a fixed authority request: shown only
// when the compositor has that authority, as the task manager is.
const AUTHORITY_ENTRIES: &[Entry] = &[
    Entry {
        label: "TASK MANAGER",
        search: "task manager processes cpu memory network disk",
        request: Some(LaunchRequest::TaskManager),
    },
    Entry {
        label: "TEXT EDITOR",
        search: "text editor edit write notes file",
        request: Some(LaunchRequest::Editor),
    },
    Entry {
        label: "PHOTOS",
        search: "photos photo image picture viewer",
        request: Some(LaunchRequest::Photo),
    },
    Entry {
        label: "CODE REVIEW",
        search: "code review git branch repository land merge",
        request: Some(LaunchRequest::Review),
    },
    Entry {
        label: "DISK USAGE",
        search: "disk usage space storage analyzer size clean up",
        request: Some(LaunchRequest::Dua),
    },
    Entry {
        label: "CODING AGENT",
        search: "coding agent ai assistant model llm openrouter",
        request: Some(LaunchRequest::Agent),
    },
];

const DIRECT_UI_ENTRY: Entry = Entry {
    label: "NEW INPUT MONITOR",
    search: "new input monitor demo wayland",
    request: Some(LaunchRequest::UiDemo),
};

/// The card's fixed rows, in its order: with the authority the terminal,
/// td-software, the desktop programs and close (td-software/DESIGN.md
/// §5); without it, the development launcher's terminal, monitor and
/// close.
fn fixed_entry(authority: bool, index: usize) -> Option<Entry<'static>> {
    if authority {
        match index {
            0 => Some(TERMINAL_ENTRY),
            1 => Some(SOFTWARE_ENTRY),
            index if index == AUTHORITY_ENTRIES.len() + 2 => Some(CLOSE_ENTRY),
            index => index
                .checked_sub(2)
                .and_then(|at| AUTHORITY_ENTRIES.get(at))
                .copied(),
        }
    } else {
        [TERMINAL_ENTRY, DIRECT_UI_ENTRY, CLOSE_ENTRY]
            .get(index)
            .copied()
    }
}

fn fixed_count(authority: bool) -> usize {
    if authority {
        AUTHORITY_ENTRIES.len() + 3
    } else {
        3
    }
}

/// A launcher table application, as the card shows it.
#[derive(Clone)]
struct Application {
    name: String,
    label: String,
    search: String,
    install_label: String,
    /// The search text with `install`, so a query naming the row's verb
    /// finds it too.
    install_search: String,
    /// Root's last word on it, `None` until the first.
    installed: Option<bool>,
}

/// A row the filter left: a fixed entry, a table application, or the one
/// Install row.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Item {
    Fixed(usize),
    Application(usize),
    Install(usize),
}

/// The card, centred and shrunk to fit an output smaller than it.
fn card(width: usize, height: usize) -> (usize, usize, usize, usize) {
    let card_width = CARD_WIDTH.min(width.saturating_sub(CARD_PADDING.saturating_mul(2)));
    let card_height = CARD_HEIGHT.min(height.saturating_sub(CARD_PADDING.saturating_mul(2)));
    let left = width.saturating_sub(card_width) / 2;
    let top = height.saturating_sub(card_height) / 2;
    (left, top, card_width, card_height)
}

fn row_text_top(card_top: usize, row: usize) -> usize {
    card_top
        .saturating_add(FIRST_ROW_TOP)
        .saturating_add(row.saturating_mul(ROW_STEP))
}

/// A row's highlight, which is also what a press must land in to choose
/// it; clipped to the card, so a row the card cut off cannot be chosen.
fn row_area(card: (usize, usize, usize, usize), row: usize) -> (usize, usize, usize, usize) {
    let (left, top, card_width, _) = card;
    intersect(
        (
            left.saturating_add(ROW_INSET),
            row_text_top(top, row).saturating_sub(ROW_RISE),
            card_width.saturating_sub(ROW_INSET.saturating_mul(2)),
            ROW_HEIGHT,
        ),
        card,
    )
}

/// The middle of a shown row's press area, for tests that drive a press.
#[cfg(test)]
pub(crate) fn row_centre(width: usize, height: usize, row: usize) -> (usize, usize) {
    let (left, top, row_width, row_height) = row_area(card(width, height), row);
    (left + row_width / 2, top + row_height / 2)
}

/// Whether a row is on the card at all: its whole label fits. A row the
/// card cut off is not drawn and the pointer cannot choose it, so nothing
/// the operator cannot read can be clicked.
fn row_shown(card: (usize, usize, usize, usize), row: usize) -> bool {
    let (_, top, _, height) = card;
    row_text_top(top, row).saturating_add(LABEL_HEIGHT) <= top.saturating_add(height)
}

fn contains(rect: (usize, usize, usize, usize), x: usize, y: usize) -> bool {
    let (left, top, width, height) = rect;
    x >= left && x < left.saturating_add(width) && y >= top && y < top.saturating_add(height)
}

#[derive(Clone)]
pub struct Launcher {
    visible: bool,
    /// The selected match, and the first match the window of rows shows.
    selected: usize,
    scroll: usize,
    query: String,
    matches: Vec<Item>,
    /// The launcher table's applications, by display name.
    applications: Vec<Application>,
    authority: bool,
}

pub struct LaunchOptions {
    pub socket: PathBuf,
    pub client: PathBuf,
    pub terminal: PathBuf,
}

pub(crate) trait ChildProcess {
    fn state(&mut self) -> Result<ChildState, String>;
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) enum ChildState {
    Running,
    Exited,
    Failed(String),
}

pub(crate) trait ProcessSpawner {
    type Child: ChildProcess;

    fn spawn(&mut self, program: &Path, arguments: &[OsString]) -> Result<Self::Child, String>;
}

pub(crate) struct CommandSpawner;

pub struct LaunchProcesses<S = CommandSpawner>
where
    S: ProcessSpawner,
{
    options: LaunchOptions,
    spawner: S,
    children: Vec<LaunchedChild<S::Child>>,
    sequence: u64,
}

pub(crate) enum LaunchBackend {
    Direct(LaunchProcesses),
    Authority(crate::authority::Launcher),
}

impl LaunchBackend {
    pub(crate) fn task_launcher(&self) -> Option<crate::authority::Launcher> {
        match self {
            Self::Authority(authority) => Some(authority.clone()),
            Self::Direct(_) => None,
        }
    }

    pub fn unlock(
        &self,
        attempt: std::sync::Arc<crate::secret_client::Attempt>,
    ) -> Result<(), String> {
        match self {
            Self::Authority(authority) => authority.unlock(attempt),
            Self::Direct(_) => Err("secret unlock requires the paired authority".into()),
        }
    }

    /// The application `name` has no window: ask root to start its unit,
    /// which does nothing if it is already running.
    pub fn start_application(&self, name: &str) -> Result<(), String> {
        match self {
            Self::Authority(authority) => {
                authority.supervise(crate::authority::Supervised::Start(name.to_string()))
            }
            Self::Direct(_) => Err("starting the application requires the paired authority".into()),
        }
    }

    /// The application `name` is not installed: open td-software at it.
    pub fn install_application(&self, name: &str) -> Result<(), String> {
        match self {
            Self::Authority(authority) => authority.launch_software_at(name),
            Self::Direct(_) => Err("installing requires the paired authority".into()),
        }
    }

    pub fn launch(&mut self, request: LaunchRequest) -> Result<Vec<String>, String> {
        match self {
            Self::Direct(processes) => processes.launch(request),
            Self::Authority(authority) if request == LaunchRequest::Terminal => {
                authority.launch()?;
                Ok(Vec::new())
            }
            Self::Authority(authority) => match request.authority_program() {
                Some(program) => {
                    authority.launch_selected(program)?;
                    Ok(Vec::new())
                }
                None => Err(format!("{request:?} has no authority program")),
            },
        }
    }
}

struct LaunchedChild<C> {
    process: C,
    ready_socket: PathBuf,
}

impl LaunchOptions {
    fn validate(&self) -> Result<(), String> {
        for (what, path) in [
            ("Wayland socket", &self.socket),
            ("launcher terminal", &self.terminal),
            ("launcher client", &self.client),
        ] {
            if !path.is_absolute() {
                return Err(format!("{what} {} is not absolute", path.display()));
            }
        }
        Ok(())
    }
}

impl Launcher {
    pub fn new() -> Launcher {
        let mut launcher = Launcher {
            visible: false,
            selected: 0,
            scroll: 0,
            query: String::with_capacity(filter::MAX_QUERY_BYTES),
            matches: Vec::new(),
            applications: Vec::new(),
            authority: false,
        };
        launcher.refresh_matches();
        launcher
    }

    pub(crate) fn set_authority(&mut self, available: bool) {
        self.authority = available;
        self.refresh_matches();
    }

    /// The launcher table's applications, which the card orders by display
    /// name; none is shown until root says it is installed.
    pub(crate) fn set_table(&mut self, rows: &[crate::launcher_table::Row]) {
        let mut applications: Vec<Application> = rows
            .iter()
            .map(|row| {
                let label = row.display.to_uppercase();
                let mut search = row.display.to_lowercase();
                for word in row.terms.iter().chain(std::iter::once(&row.name)) {
                    search.push(' ');
                    search.push_str(&word.to_lowercase());
                }
                Application {
                    name: row.name.clone(),
                    install_label: format!("INSTALL {label}"),
                    install_search: format!("install {search}"),
                    label,
                    search,
                    installed: None,
                }
            })
            .collect();
        applications.sort_by(|one, other| {
            one.label
                .cmp(&other.label)
                .then_with(|| one.name.cmp(&other.name))
        });
        self.applications = applications;
        self.refresh_matches();
    }

    /// Root's installed set: which table applications the card shows.
    pub(crate) fn set_installed(&mut self, installed: impl Fn(&str) -> Option<bool>) {
        for application in &mut self.applications {
            application.installed = installed(&application.name);
        }
        let selected = self.matches.get(self.selected).copied();
        let scroll = self.scroll;
        self.refresh_matches();
        // A set that arrives while the card is up keeps the selection and,
        // as far as the shorter list allows, the window.
        if let Some(at) = selected.and_then(|item| self.matches.iter().position(|m| *m == item)) {
            self.selected = at;
            self.scroll = scroll.min(self.matches.len().saturating_sub(CARD_ROWS));
            self.scroll_to_selection();
        }
    }

    /// The table application a request names, by its position.
    pub(crate) fn application(&self, index: usize) -> Option<&str> {
        self.applications.get(index).map(|a| a.name.as_str())
    }

    pub fn apply(&mut self, action: LauncherAction) -> Option<LaunchRequest> {
        match action {
            LauncherAction::Open => {
                self.visible = true;
                self.query.clear();
                self.refresh_matches();
            }
            LauncherAction::Next if self.visible && !self.matches.is_empty() => {
                self.selected = self.selected.saturating_add(1) % self.matches.len();
                self.scroll_to_selection();
            }
            LauncherAction::Previous if self.visible && !self.matches.is_empty() => {
                self.selected = if self.selected == 0 {
                    self.matches.len().saturating_sub(1)
                } else {
                    self.selected.saturating_sub(1)
                };
                self.scroll_to_selection();
            }
            LauncherAction::Close => self.visible = false,
            LauncherAction::Choose(row) if self.visible => {
                let at = self.scroll.saturating_add(row);
                if row < CARD_ROWS && at < self.matches.len() {
                    self.selected = at;
                    return self.apply(LauncherAction::Activate);
                }
            }
            LauncherAction::Activate if self.visible => {
                let request = self
                    .matches
                    .get(self.selected)
                    .and_then(|item| self.entry(*item))
                    .map(|entry| entry.request);
                let request = request?;
                self.visible = false;
                return request;
            }
            LauncherAction::Insert(character)
                if self.visible && filter::insert(&mut self.query, character) =>
            {
                self.refresh_matches();
            }
            LauncherAction::Backspace if self.visible && !self.query.is_empty() => {
                self.query.pop();
                self.refresh_matches();
            }
            LauncherAction::Next
            | LauncherAction::Previous
            | LauncherAction::Activate
            | LauncherAction::Choose(_)
            | LauncherAction::Insert(_)
            | LauncherAction::Backspace => {}
        }
        None
    }

    fn entry(&self, item: Item) -> Option<Entry<'_>> {
        match item {
            Item::Fixed(index) => fixed_entry(self.authority, index),
            Item::Application(index) => self.applications.get(index).map(|a| Entry {
                label: &a.label,
                search: &a.search,
                request: Some(LaunchRequest::Application(index)),
            }),
            Item::Install(index) => self.applications.get(index).map(|a| Entry {
                label: &a.install_label,
                search: &a.install_search,
                request: Some(LaunchRequest::Install(index)),
            }),
        }
    }

    /// The fixed entries, then each installed application, that the query
    /// matches. A query matching nothing else but an application root says
    /// is not installed shows one row, the first such: install it in
    /// td-software. An application updates only from td-software.
    fn refresh_matches(&mut self) {
        self.matches.clear();
        let query = self.query.as_str();
        for index in 0..fixed_count(self.authority) {
            if fixed_entry(self.authority, index).is_some_and(|e| filter::matches(e.search, query))
            {
                self.matches.push(Item::Fixed(index));
            }
        }
        if self.authority {
            for (index, application) in self.applications.iter().enumerate() {
                if application.installed != Some(true) {
                    continue;
                }
                if filter::matches(&application.search, query) {
                    self.matches.push(Item::Application(index));
                }
            }
            if self.matches.is_empty() && !query.is_empty() {
                if let Some(index) = self.applications.iter().position(|application| {
                    application.installed == Some(false)
                        && filter::matches(&application.install_search, query)
                }) {
                    self.matches.push(Item::Install(index));
                }
            }
        }
        self.selected = 0;
        self.scroll = 0;
    }

    /// Moves the window of rows so the selection is in it.
    fn scroll_to_selection(&mut self) {
        if self.selected < self.scroll {
            self.scroll = self.selected;
        } else if self.selected >= self.scroll.saturating_add(CARD_ROWS) {
            self.scroll = self.selected.saturating_add(1).saturating_sub(CARD_ROWS);
        }
    }

    /// The first match a card of this size shows: the stored window, moved
    /// further when a card shrunk below its full size holds fewer rows.
    fn first_shown(&self, card: (usize, usize, usize, usize)) -> usize {
        let rows = (0..CARD_ROWS)
            .take_while(|row| row_shown(card, *row))
            .count();
        if rows == 0 {
            return self.scroll;
        }
        if self.selected >= self.scroll.saturating_add(rows) {
            self.selected.saturating_add(1).saturating_sub(rows)
        } else {
            self.scroll
        }
    }

    fn visible_query(&self, columns: usize) -> &str {
        let start = self.query.len().saturating_sub(columns);
        self.query.get(start..).unwrap_or_default()
    }

    /// What a press at `(x, y)` asks of the open overlay. A row's highlight
    /// area chooses that row; a press off the card closes it, as Escape
    /// does; the card's title, filter and the air between rows ask nothing.
    pub fn press_at(
        &self,
        x: usize,
        y: usize,
        width: usize,
        height: usize,
    ) -> Option<LauncherAction> {
        if !self.visible {
            return None;
        }
        let card = card(width, height);
        if !contains(card, x, y) {
            return Some(LauncherAction::Close);
        }
        let first = self.first_shown(card);
        let shown = self.matches.len().saturating_sub(first).min(CARD_ROWS);
        (0..shown)
            .position(|row| row_shown(card, row) && contains(row_area(card, row), x, y))
            .map(|row| {
                LauncherAction::Choose(row.saturating_add(first).saturating_sub(self.scroll))
            })
    }

    pub fn paint(&self, frame: &mut [u8], width: usize, height: usize, stride: usize, text: &Text) {
        if !self.visible {
            return;
        }
        let card = card(width, height);
        let (left, top, card_width, _) = card;
        fill(frame, width, height, stride, card, CARD);
        border(frame, width, height, stride, card, [0xc0, 0x70, 0xf0, 0]);
        text.draw(
            frame,
            width,
            height,
            stride,
            (left.saturating_add(20), top.saturating_add(18)),
            "TD LAUNCHER",
            ([0xff, 0xff, 0xff, 0], CARD),
            card,
        );
        text.draw(
            frame,
            width,
            height,
            stride,
            (left.saturating_add(20), top.saturating_add(48)),
            "FILTER:",
            ([0xd8, 0xb0, 0xf0, 0], CARD),
            card,
        );
        let query_columns = card_width.saturating_sub(136) / CELL_WIDTH;
        let visible_query = self.visible_query(query_columns);
        text.draw(
            frame,
            width,
            height,
            stride,
            (left.saturating_add(116), top.saturating_add(48)),
            visible_query,
            ([0xff, 0xff, 0xff, 0], CARD),
            card,
        );
        if self.matches.is_empty() {
            text.draw(
                frame,
                width,
                height,
                stride,
                (left.saturating_add(24), top.saturating_add(92)),
                "NO MATCHES",
                ([0xa8, 0xa0, 0xb0, 0], CARD),
                card,
            );
        }
        let first = self.first_shown(card);
        for (row, item) in self.matches.iter().skip(first).take(CARD_ROWS).enumerate() {
            let Some(entry) = self.entry(*item) else {
                continue;
            };
            if !row_shown(card, row) {
                break;
            }
            let selected = first.saturating_add(row) == self.selected;
            if selected {
                let highlight = row_area(card, row);
                fill(frame, width, height, stride, highlight, HIGHLIGHT);
            }
            text.draw(
                frame,
                width,
                height,
                stride,
                (left.saturating_add(24), row_text_top(top, row)),
                entry.label,
                if selected {
                    ([0xff, 0xff, 0xff, 0], HIGHLIGHT)
                } else {
                    ([0xa8, 0xa0, 0xb0, 0], CARD)
                },
                card,
            );
        }
    }

    pub fn visible(&self) -> bool {
        self.visible
    }

    #[cfg(test)]
    fn selected(&self) -> usize {
        self.selected
    }

    #[cfg(test)]
    pub(crate) fn query(&self) -> &str {
        &self.query
    }

    #[cfg(test)]
    fn matched_labels(&self) -> Vec<&str> {
        self.matches
            .iter()
            .filter_map(|item| self.entry(*item).map(|entry| entry.label))
            .collect()
    }
}

impl ChildProcess for Child {
    fn state(&mut self) -> Result<ChildState, String> {
        match self.try_wait().map_err(|error| error.to_string())? {
            None => Ok(ChildState::Running),
            Some(status) if status.success() => Ok(ChildState::Exited),
            Some(status) => Ok(ChildState::Failed(status.to_string())),
        }
    }
}

impl ProcessSpawner for CommandSpawner {
    type Child = Child;

    fn spawn(&mut self, program: &Path, arguments: &[OsString]) -> Result<Self::Child, String> {
        Command::new(program)
            .args(arguments)
            .spawn()
            .map_err(|error| format!("launch {}: {error}", program.display()))
    }
}

impl LaunchProcesses<CommandSpawner> {
    pub fn new(options: LaunchOptions) -> Result<LaunchProcesses, String> {
        options.validate()?;
        Ok(LaunchProcesses {
            options,
            spawner: CommandSpawner,
            children: Vec::new(),
            sequence: 0,
        })
    }
}

impl<S> LaunchProcesses<S>
where
    S: ProcessSpawner,
{
    #[cfg(test)]
    fn with_spawner(options: LaunchOptions, spawner: S) -> Self {
        Self {
            options,
            spawner,
            children: Vec::new(),
            sequence: 0,
        }
    }

    pub fn launch(&mut self, request: LaunchRequest) -> Result<Vec<String>, String> {
        let failures = self.reap()?;
        if let Err(error) = self.launch_one(request) {
            return Err(failures_and_error(&failures, error));
        }
        Ok(failures)
    }

    fn launch_one(&mut self, request: LaunchRequest) -> Result<(), String> {
        ensure_capacity(self.children.len())?;
        let sequence = self.sequence;
        let next_sequence = self
            .sequence
            .checked_add(1)
            .ok_or_else(|| "launcher sequence exhausted".to_string())?;
        let (program, arguments, ready_socket) = launch_command(&self.options, request, sequence)?;
        let child = self.spawner.spawn(&program, &arguments)?;
        self.children.push(LaunchedChild {
            process: child,
            ready_socket,
        });
        self.sequence = next_sequence;
        Ok(())
    }

    fn reap(&mut self) -> Result<Vec<String>, String> {
        let mut error = None;
        let mut failures = Vec::new();
        self.children
            .retain_mut(|child| match child.process.state() {
                Ok(ChildState::Running) => true,
                Ok(ChildState::Exited) => {
                    if let Err(candidate) =
                        socket::remove_stale(&child.ready_socket, "launcher readiness")
                    {
                        failures.push(candidate);
                    }
                    false
                }
                Ok(ChildState::Failed(status)) => {
                    failures.push(format!("launched UI client exited with {status}"));
                    if let Err(candidate) =
                        socket::remove_stale(&child.ready_socket, "launcher readiness")
                    {
                        failures.push(candidate);
                    }
                    false
                }
                Err(candidate) => {
                    if error.is_none() {
                        error = Some(candidate);
                    }
                    true
                }
            });
        match error {
            Some(error) => Err(failures_and_error(
                &failures,
                format!("reap launched UI client: {error}"),
            )),
            None => Ok(failures),
        }
    }
}

fn failures_and_error(failures: &[String], error: String) -> String {
    if failures.is_empty() {
        error
    } else {
        format!("{}; {error}", failures.join("; "))
    }
}

fn ensure_capacity(active: usize) -> Result<(), String> {
    if active >= MAX_LAUNCHED_CLIENTS {
        return Err(format!(
            "launcher retains {MAX_LAUNCHED_CLIENTS} live clients"
        ));
    }
    Ok(())
}

pub(crate) fn launch_command(
    options: &LaunchOptions,
    request: LaunchRequest,
    sequence: u64,
) -> Result<(PathBuf, Vec<OsString>, PathBuf), String> {
    options.validate()?;
    let directory = options
        .socket
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .ok_or_else(|| format!("Wayland socket {} has no parent", options.socket.display()))?;
    let ready = directory.join(format!(
        "td-launcher-{}-{sequence}.ready",
        std::process::id()
    ));
    let program = match request {
        LaunchRequest::TaskManager
        | LaunchRequest::Editor
        | LaunchRequest::Photo
        | LaunchRequest::Review
        | LaunchRequest::Dua
        | LaunchRequest::Agent
        | LaunchRequest::Software => {
            return Err("desktop programs are not configured for this development launcher".into());
        }
        LaunchRequest::Application(_) | LaunchRequest::Install(_) => {
            return Err("applications require the paired authority".into());
        }
        LaunchRequest::UiDemo => options.client.clone(),
        LaunchRequest::Terminal => options.terminal.clone(),
    };
    let arguments = vec![
        OsString::from("run"),
        OsString::from("--socket"),
        options.socket.as_os_str().to_os_string(),
        OsString::from("--ready-socket"),
        ready.as_os_str().to_os_string(),
    ];
    Ok((program, arguments, ready))
}

/// How long a failed launch's state stays on the bar.
const LAUNCH_FAILED_TIME: Duration = Duration::from_secs(30);
/// How long a launch may stay pending before the bar stops waiting on it:
/// far past any start, so only a window that never matches reaches it.
const LAUNCH_LIMIT: Duration = Duration::from_secs(30 * 60);

/// The configured application's launch from its card, from the request
/// until its window maps or the start fails (DESIGN.md, the launcher).
/// It shows what root last read of the application's unit and how long
/// the launch has taken, and never claims more: what the unit is doing
/// inside its run is not on the wire.
pub(crate) struct PendingLaunch {
    label: String,
    /// The worker's start count before this request: an outcome counts
    /// only once the count moved past it.
    base: u64,
    sent: bool,
    since: Instant,
    /// When the shown state became a failure; cleared if the unit is seen
    /// going up again, as a supervised restart does.
    ended: Option<Instant>,
}

impl PendingLaunch {
    /// `sent` says the request reached the worker's queue.
    pub(crate) fn begin(label: &str, base: u64, sent: bool, now: Instant) -> Self {
        Self {
            label: label.to_string(),
            base,
            sent,
            since: now,
            ended: None,
        }
    }

    /// Whether another activation must not ask again: the launch is under
    /// way and has not failed.
    pub(crate) fn blocks_start(&self) -> bool {
        self.ended.is_none()
    }

    /// The bar's text at `now` for `outcome`, or `None` once the launch
    /// is over without a window: a failure shown its time, or the limit.
    pub(crate) fn text(
        &mut self,
        outcome: crate::authority::StartOutcome,
        now: Instant,
    ) -> Option<String> {
        use crate::authority::UnitState;
        let elapsed = now.saturating_duration_since(self.since);
        if elapsed >= LAUNCH_LIMIT {
            return None;
        }
        let answered = outcome.sent != self.base;
        let phase = match (self.sent, answered, outcome.refused, outcome.state) {
            // The worker's queue held another request, or there is no
            // authority: nothing was asked, so nothing failed.
            (false, _, _, _) => Err("NOT STARTED"),
            (true, true, true, _) => Err("FAILED TO START"),
            (true, true, false, Some(UnitState::Failed)) => Err("FAILED TO START"),
            (true, true, false, Some(UnitState::Idle)) => Err("EXITED"),
            (true, true, false, Some(UnitState::Running)) => Ok("LOADING"),
            (true, false, _, _)
            | (true, true, false, None | Some(UnitState::Starting | UnitState::Unknown)) => {
                Ok("STARTING")
            }
        };
        match phase {
            Ok(phase) => {
                self.ended = None;
                let seconds = elapsed.as_secs();
                Some(format!(
                    "{} {phase} {}:{:02}",
                    self.label,
                    seconds / 60,
                    seconds % 60
                ))
            }
            Err(ending) => {
                let at = *self.ended.get_or_insert(now);
                (now.saturating_duration_since(at) < LAUNCH_FAILED_TIME)
                    .then(|| format!("{} {ending}", self.label))
            }
        }
    }
}

#[cfg(test)]
mod pending_launch_tests {
    use super::*;
    use crate::authority::{StartOutcome, UnitState};

    fn outcome(sent: u64, refused: bool, state: Option<UnitState>) -> StartOutcome {
        StartOutcome {
            sent,
            refused,
            state,
        }
    }

    #[test]
    fn a_launch_names_its_phase_and_how_long_it_has_taken() {
        let start = Instant::now();
        let mut launch = PendingLaunch::begin("FIREFOX", 3, true, start);
        // Before the worker sent it, and before root read anything.
        assert_eq!(
            launch.text(outcome(3, false, Some(UnitState::Running)), start),
            Some("FIREFOX STARTING 0:00".into())
        );
        let later = start + Duration::from_secs(7);
        assert_eq!(
            launch.text(outcome(4, false, None), later),
            Some("FIREFOX STARTING 0:07".into())
        );
        for state in [UnitState::Starting, UnitState::Unknown] {
            assert_eq!(
                launch.text(outcome(4, false, Some(state)), later),
                Some("FIREFOX STARTING 0:07".into())
            );
        }
        let minutes = start + Duration::from_secs(125);
        assert_eq!(
            launch.text(outcome(4, false, Some(UnitState::Running)), minutes),
            Some("FIREFOX LOADING 2:05".into())
        );
        assert!(launch.blocks_start());
        assert_eq!(
            launch.text(
                outcome(4, false, Some(UnitState::Running)),
                start + LAUNCH_LIMIT
            ),
            None
        );
    }

    #[test]
    fn a_failure_shows_for_its_time_unless_the_unit_comes_back() {
        let start = Instant::now();
        let mut launch = PendingLaunch::begin("FIREFOX", 0, true, start);
        let failed = start + Duration::from_secs(5);
        assert_eq!(
            launch.text(outcome(1, false, Some(UnitState::Failed)), failed),
            Some("FIREFOX FAILED TO START".into())
        );
        assert!(!launch.blocks_start());
        // A supervised restart takes it back up.
        let back = failed + Duration::from_secs(2);
        assert_eq!(
            launch.text(outcome(1, false, Some(UnitState::Running)), back),
            Some("FIREFOX LOADING 0:07".into())
        );
        assert!(launch.blocks_start());
        let exited = back + Duration::from_secs(1);
        assert_eq!(
            launch.text(outcome(1, false, Some(UnitState::Idle)), exited),
            Some("FIREFOX EXITED".into())
        );
        let shown = exited + LAUNCH_FAILED_TIME - Duration::from_millis(1);
        assert_eq!(
            launch.text(outcome(1, false, Some(UnitState::Idle)), shown),
            Some("FIREFOX EXITED".into())
        );
        assert_eq!(
            launch.text(
                outcome(1, false, Some(UnitState::Idle)),
                exited + LAUNCH_FAILED_TIME
            ),
            None
        );
    }

    /// The bar draws every name the registry grammar admits, uppercased,
    /// the image's display names and every phase, with Unifont behind the
    /// outline face. A table display name is any text without control
    /// characters; one with a character neither face has draws that
    /// character as the text renderer draws any it lacks.
    #[test]
    fn every_character_a_launch_can_spell_is_in_the_font() {
        let start = Instant::now();
        let name = "ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789._- FIREFOX MAIL NEWS";
        let states = [
            outcome(1, false, None),
            outcome(1, false, Some(UnitState::Running)),
            outcome(1, false, Some(UnitState::Idle)),
            outcome(1, true, None),
        ];
        for (sent, state) in states
            .into_iter()
            .map(|state| (true, state))
            .chain([(false, outcome(0, false, None))])
        {
            let mut launch = PendingLaunch::begin(name, 0, sent, start);
            let text = launch
                .text(state, start + Duration::from_secs(599))
                .unwrap();
            for character in text.chars() {
                assert!(
                    crate::text::covered(character),
                    "{character:?} in {text:?} has no glyph"
                );
            }
        }
    }

    #[test]
    fn a_request_never_sent_or_refused_fails_at_once() {
        let start = Instant::now();
        let mut unsent = PendingLaunch::begin("FIREFOX", 0, false, start);
        assert_eq!(
            unsent.text(outcome(0, false, None), start),
            Some("FIREFOX NOT STARTED".into())
        );
        let mut refused = PendingLaunch::begin("FIREFOX", 0, true, start);
        assert_eq!(
            refused.text(outcome(1, true, Some(UnitState::Running)), start),
            Some("FIREFOX FAILED TO START".into())
        );
        // Nothing read for an earlier start answers for this one.
        let mut fresh = PendingLaunch::begin("FIREFOX", 1, true, start);
        assert_eq!(
            fresh.text(outcome(1, true, Some(UnitState::Failed)), start),
            Some("FIREFOX STARTING 0:00".into())
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::fs;
    use std::os::unix::net::UnixListener;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_SEQ: AtomicU64 = AtomicU64::new(0);

    #[derive(Default)]
    struct FakeChild {
        states: VecDeque<Result<ChildState, String>>,
    }

    #[derive(Default)]
    struct FakeSpawner {
        calls: Vec<(PathBuf, Vec<OsString>)>,
        children: VecDeque<FakeChild>,
        failures: usize,
    }

    impl ChildProcess for FakeChild {
        fn state(&mut self) -> Result<ChildState, String> {
            self.states.pop_front().unwrap_or(Ok(ChildState::Running))
        }
    }

    impl ProcessSpawner for FakeSpawner {
        type Child = FakeChild;

        fn spawn(&mut self, program: &Path, arguments: &[OsString]) -> Result<Self::Child, String> {
            self.calls.push((program.to_path_buf(), arguments.to_vec()));
            if self.failures > 0 {
                self.failures = self.failures.saturating_sub(1);
                return Err("injected spawn failure".to_string());
            }
            Ok(self.children.pop_front().unwrap_or_default())
        }
    }

    fn launch_options() -> LaunchOptions {
        LaunchOptions {
            socket: std::env::temp_dir()
                .join(format!("td-launcher-fake-{}-missing", std::process::id()))
                .join("wayland-0"),
            client: PathBuf::from("/bin/td-ui-demo"),
            terminal: PathBuf::from("/bin/td-term"),
        }
    }

    #[test]
    fn navigation_wraps_and_activation_is_explicit() {
        let mut launcher = Launcher::new();
        assert_eq!(launcher.apply(LauncherAction::Next), None);
        assert!(!launcher.visible());
        assert_eq!(launcher.apply(LauncherAction::Open), None);
        assert!(launcher.visible());
        assert_eq!(launcher.selected(), 0);
        assert_eq!(launcher.apply(LauncherAction::Previous), None);
        assert_eq!(launcher.selected(), 2);
        assert_eq!(launcher.apply(LauncherAction::Activate), None);
        assert!(!launcher.visible());

        launcher.apply(LauncherAction::Open);
        assert_eq!(
            launcher.apply(LauncherAction::Activate),
            Some(LaunchRequest::Terminal)
        );
        assert!(!launcher.visible());
    }

    #[test]
    fn a_press_chooses_the_row_painted_under_it_and_off_the_card_closes() {
        let (width, height) = (640usize, 480usize);
        let stride = width * 4;
        let highlight = [0x58, 0x30, 0x70, 0];
        let mut launcher = Launcher::new();
        assert_eq!(launcher.press_at(0, 0, width, height), None, "closed");
        launcher.apply(LauncherAction::Open);
        let card = card(width, height);
        let rows = launcher.matches.len();
        assert!(rows > 1);
        for row in 0..rows {
            let (x, y) = row_centre(width, height, row);
            assert_eq!(
                launcher.press_at(x, y, width, height),
                Some(LauncherAction::Choose(row))
            );
            // The press area IS the highlight the paint draws behind the
            // selected row: its corners are highlight, the rows just outside
            // it are not.
            launcher.selected = row;
            let mut frame = vec![0u8; stride * height];
            launcher.paint(&mut frame, width, height, stride, &Text::default());
            let at = |x: usize, y: usize| frame.get(y * stride + x * 4..y * stride + x * 4 + 4);
            let (left, top, w, h) = row_area(card, row);
            assert_eq!(at(left, top), Some(&highlight[..]), "row {row}");
            assert_eq!(at(left + w - 1, top + h - 1), Some(&highlight[..]));
            assert_ne!(at(left, top - 1), Some(&highlight[..]));
            assert_ne!(at(left, top + h), Some(&highlight[..]));
            assert_eq!(launcher.press_at(left, top - 1, width, height), None);
            assert_eq!(launcher.press_at(left, top + h, width, height), None);
        }
        // Off the card is Escape; the title on it asks nothing.
        assert_eq!(
            launcher.press_at(0, 0, width, height),
            Some(LauncherAction::Close)
        );
        assert_eq!(
            launcher.press_at(card.0 + card.2, card.1, width, height),
            Some(LauncherAction::Close)
        );
        assert_eq!(
            launcher.press_at(card.0 + 24, card.1 + 20, width, height),
            None
        );
        // A row the filter took away is not there to press.
        for character in "close".chars() {
            launcher.apply(LauncherAction::Insert(character));
        }
        let (x, y) = row_centre(width, height, 1);
        assert_eq!(launcher.press_at(x, y, width, height), None);

        // An output too short for every row: a row the card cut off is off
        // the card, so a press there closes rather than choosing it.
        let short = 160usize;
        launcher.apply(LauncherAction::Open);
        let (x, y) = row_centre(width, short, 1);
        assert_eq!(
            launcher.press_at(x, y, width, short),
            Some(LauncherAction::Close)
        );
        // Shorter still by the right amount, the card keeps a sliver of the
        // next row's highlight below a label it cannot hold: that row is not
        // drawn, and the sliver chooses nothing.
        let sliver = 180usize;
        let cut = super::card(width, sliver);
        assert!(row_shown(cut, 0) && !row_shown(cut, 1));
        let (left, top, w, h) = row_area(cut, 1);
        assert!(h > 0, "no sliver to press: {:?}", row_area(cut, 1));
        assert_eq!(launcher.press_at(left + w / 2, top, width, sliver), None);
        launcher.selected = 1;
        let mut frame = vec![0u8; stride * sliver];
        launcher.paint(&mut frame, width, sliver, stride, &Text::default());
        let at = |x: usize, y: usize| frame.get(y * stride + x * 4..y * stride + x * 4 + 4);
        assert_ne!(at(left, top), Some(&highlight[..]), "a hidden row drawn");
        assert!((top..top + h)
            .all(|y| (left..left + w).all(|x| at(x, y) != Some(&[0xff, 0xff, 0xff, 0][..]))));
    }

    #[test]
    fn choosing_a_row_activates_that_row_and_only_a_shown_one() {
        let mut launcher = Launcher::new();
        assert_eq!(launcher.apply(LauncherAction::Choose(0)), None);
        assert!(!launcher.visible(), "a closed launcher opened");
        launcher.apply(LauncherAction::Open);
        assert_eq!(
            launcher.apply(LauncherAction::Choose(0)),
            Some(LaunchRequest::Terminal)
        );
        assert!(!launcher.visible());
        // Past the rows shown asks nothing and leaves it up.
        launcher.apply(LauncherAction::Open);
        assert_eq!(launcher.apply(LauncherAction::Choose(9)), None);
        assert!(launcher.visible());
        // A position in the FILTERED list: the only row left is the close.
        for character in "close".chars() {
            launcher.apply(LauncherAction::Insert(character));
        }
        assert_eq!(launcher.matched_labels(), ["CLOSE LAUNCHER"]);
        assert_eq!(launcher.apply(LauncherAction::Choose(0)), None);
        assert!(!launcher.visible());
    }

    #[test]
    fn close_resets_visibility_and_open_resets_selection() {
        let mut launcher = Launcher::new();
        launcher.apply(LauncherAction::Open);
        launcher.apply(LauncherAction::Next);
        assert_eq!(launcher.selected(), 1);
        launcher.apply(LauncherAction::Insert('i'));
        assert_eq!(launcher.query(), "i");
        launcher.apply(LauncherAction::Close);
        assert!(!launcher.visible());
        launcher.apply(LauncherAction::Open);
        assert_eq!(launcher.selected(), 0);
        assert_eq!(launcher.query(), "");
        assert_eq!(
            launcher.matched_labels(),
            ["NEW TERMINAL", "NEW INPUT MONITOR", "CLOSE LAUNCHER"]
        );
    }

    #[test]
    fn filtering_is_ascii_bounded_and_matches_all_terms() {
        let mut launcher = Launcher::new();
        launcher.apply(LauncherAction::Open);
        for character in "NeW TeRm".chars() {
            launcher.apply(LauncherAction::Insert(character));
        }
        assert_eq!(launcher.query(), "new term");
        assert_eq!(launcher.matched_labels(), ["NEW TERMINAL"]);
        assert_eq!(
            launcher.apply(LauncherAction::Activate),
            Some(LaunchRequest::Terminal)
        );
        launcher.apply(LauncherAction::Open);
        launcher.apply(LauncherAction::Insert('é'));
        launcher.apply(LauncherAction::Insert('\n'));
        assert_eq!(launcher.query(), "");
        for _ in 0..filter::MAX_QUERY_BYTES.saturating_add(8) {
            launcher.apply(LauncherAction::Insert('x'));
        }
        assert_eq!(launcher.query().len(), filter::MAX_QUERY_BYTES);
        assert!(launcher.matched_labels().is_empty());
        assert_eq!(launcher.visible_query(8), "xxxxxxxx");
        assert_eq!(
            launcher.visible_query(filter::MAX_QUERY_BYTES),
            launcher.query()
        );
    }

    #[test]
    fn empty_filter_keeps_the_launcher_open_and_backspace_recovers() {
        let mut launcher = Launcher::new();
        launcher.apply(LauncherAction::Open);
        launcher.apply(LauncherAction::Insert('z'));
        launcher.apply(LauncherAction::Insert('z'));
        assert!(launcher.matched_labels().is_empty());
        assert_eq!(launcher.apply(LauncherAction::Next), None);
        assert_eq!(launcher.apply(LauncherAction::Previous), None);
        assert_eq!(launcher.apply(LauncherAction::Activate), None);
        assert!(launcher.visible());
        launcher.apply(LauncherAction::Backspace);
        launcher.apply(LauncherAction::Backspace);
        assert_eq!(
            launcher.matched_labels(),
            ["NEW TERMINAL", "NEW INPUT MONITOR", "CLOSE LAUNCHER"]
        );
        launcher.apply(LauncherAction::Backspace);
        assert_eq!(launcher.query(), "");
    }

    #[test]
    fn painter_is_hidden_bounded_and_stride_aware() {
        let mut launcher = Launcher::new();
        let width = 120;
        let height = 100;
        let stride = width * 4 + 8;
        let mut hidden = vec![7u8; stride * height];
        launcher.paint(&mut hidden, width, height, stride, &Text::default());
        assert!(hidden.iter().all(|byte| *byte == 7));

        launcher.apply(LauncherAction::Open);
        let mut visible = hidden.clone();
        launcher.paint(&mut visible, width, height, stride, &Text::default());
        assert_ne!(visible, hidden);
        for row in visible.chunks_exact(stride) {
            assert!(row
                .get(width * 4..)
                .is_some_and(|padding| padding.iter().all(|byte| *byte == 7)));
        }

        let mut tiny = vec![0u8; 4];
        launcher.paint(&mut tiny, 1, 1, 4, &Text::default());
        assert_eq!(tiny, [0, 0, 0, 0]);
        launcher.paint(&mut [], 0, 0, 0, &Text::default());
    }

    #[test]
    fn painter_clips_every_overlay_pixel_to_a_shrunken_card() {
        let mut launcher = Launcher::new();
        launcher.apply(LauncherAction::Open);
        let width = 800;
        let height = 150;
        let stride = width * 4 + 8;
        let mut frame = vec![7u8; stride * height];
        launcher.paint(&mut frame, width, height, stride, &Text::default());

        let card_width = CARD_WIDTH.min(width.saturating_sub(CARD_PADDING.saturating_mul(2)));
        let card_height = CARD_HEIGHT.min(height.saturating_sub(CARD_PADDING.saturating_mul(2)));
        let left = width.saturating_sub(card_width) / 2;
        let right = left.saturating_add(card_width);
        let top = height.saturating_sub(card_height) / 2;
        let bottom = top.saturating_add(card_height);
        for (y, row) in frame.chunks_exact(stride).enumerate() {
            if y < top || y >= bottom {
                assert!(row.iter().all(|byte| *byte == 7));
                continue;
            }
            assert!(row
                .get(..left.saturating_mul(4))
                .is_some_and(|pixels| pixels.iter().all(|byte| *byte == 7)));
            assert!(row
                .get(right.saturating_mul(4)..)
                .is_some_and(|pixels| pixels.iter().all(|byte| *byte == 7)));
        }
    }

    #[test]
    fn fixed_entries_are_searchable_by_their_labels() {
        for entry in [TERMINAL_ENTRY, CLOSE_ENTRY, DIRECT_UI_ENTRY, SOFTWARE_ENTRY]
            .into_iter()
            .chain(AUTHORITY_ENTRIES.iter().copied())
        {
            assert!(entry.search.is_ascii());
            assert_eq!(entry.search, entry.search.to_ascii_lowercase());
            for word in entry.label.split_ascii_whitespace() {
                assert!(entry.search.contains(&word.to_ascii_lowercase()));
            }
        }
    }

    #[test]
    fn native_launch_commands_are_explicit_unique_and_socket_local() {
        let direct = LaunchOptions {
            socket: PathBuf::from("/run/user/1000/wayland-0"),
            client: PathBuf::from("/bin/td-ui-demo"),
            terminal: PathBuf::from("/bin/td-term"),
        };
        let (terminal, terminal_arguments, _) =
            launch_command(&direct, LaunchRequest::Terminal, 9).unwrap();
        assert_eq!(terminal, PathBuf::from("/bin/td-term"));
        assert_eq!(terminal_arguments.first(), Some(&OsString::from("run")));
        let (_, direct_arguments, direct_ready) =
            launch_command(&direct, LaunchRequest::UiDemo, 10).unwrap();
        assert_eq!(
            direct_arguments.last().map(OsString::as_os_str),
            Some(direct_ready.as_os_str())
        );
        assert_eq!(direct_ready.parent(), Some(Path::new("/run/user/1000")));
        // Table applications are the authority's alone.
        for request in [LaunchRequest::Application(0), LaunchRequest::Install(0)] {
            assert!(launch_command(&direct, request, 1).is_err(), "{request:?}");
        }

        let relative = LaunchOptions {
            socket: PathBuf::from("wayland-0"),
            client: PathBuf::from("/bin/td-ui-demo"),
            terminal: PathBuf::from("/bin/td-term"),
        };
        assert!(launch_command(&relative, LaunchRequest::UiDemo, 0).is_err());
        let relative = LaunchOptions {
            socket: PathBuf::from("/run/user/1000/wayland-0"),
            client: PathBuf::from("td-ui-demo"),
            terminal: PathBuf::from("/bin/td-term"),
        };
        assert!(launch_command(&relative, LaunchRequest::UiDemo, 0).is_err());
        assert!(LaunchProcesses::new(relative).is_err());
        // Both paths are refused, whichever request asks: the terminal is
        // spawned by the same `Command::new` the demo is.
        let relative = LaunchOptions {
            socket: PathBuf::from("/run/user/1000/wayland-0"),
            client: PathBuf::from("/bin/td-ui-demo"),
            terminal: PathBuf::from("td-term"),
        };
        assert!(launch_command(&relative, LaunchRequest::Terminal, 0).is_err());
        assert!(launch_command(&relative, LaunchRequest::UiDemo, 0).is_err());
        assert!(LaunchProcesses::new(relative).is_err());
    }

    #[test]
    fn launch_capacity_is_bounded() {
        assert!(ensure_capacity(MAX_LAUNCHED_CLIENTS.saturating_sub(1)).is_ok());
        assert!(ensure_capacity(MAX_LAUNCHED_CLIENTS).is_err());
        assert!(ensure_capacity(MAX_LAUNCHED_CLIENTS.saturating_add(1)).is_err());
    }

    #[test]
    fn process_adapter_reaps_exits_and_numbers_literal_commands() {
        let mut first_states = VecDeque::new();
        first_states.push_back(Ok(ChildState::Running));
        first_states.push_back(Ok(ChildState::Exited));
        let mut spawner = FakeSpawner::default();
        spawner.children.push_back(FakeChild {
            states: first_states,
        });
        spawner.children.push_back(FakeChild::default());
        spawner.children.push_back(FakeChild::default());
        let mut processes = LaunchProcesses::with_spawner(launch_options(), spawner);

        processes.launch(LaunchRequest::UiDemo).unwrap();
        processes.launch(LaunchRequest::UiDemo).unwrap();
        assert_eq!(processes.children.len(), 2);
        processes.launch(LaunchRequest::UiDemo).unwrap();
        assert_eq!(processes.children.len(), 2);
        assert_eq!(processes.spawner.calls.len(), 3);

        let first = processes.spawner.calls.first().unwrap().1.last().unwrap();
        let second = processes.spawner.calls.get(1).unwrap().1.last().unwrap();
        let third = processes.spawner.calls.get(2).unwrap().1.last().unwrap();
        assert!(first.to_string_lossy().ends_with("-0.ready"));
        assert!(second.to_string_lossy().ends_with("-1.ready"));
        assert!(third.to_string_lossy().ends_with("-2.ready"));
    }

    #[test]
    fn process_adapter_retains_a_child_after_reap_failure() {
        let mut states = VecDeque::new();
        states.push_back(Err("injected reap failure".to_string()));
        states.push_back(Ok(ChildState::Exited));
        let mut spawner = FakeSpawner::default();
        spawner.children.push_back(FakeChild { states });
        spawner.children.push_back(FakeChild::default());
        let mut processes = LaunchProcesses::with_spawner(launch_options(), spawner);

        processes.launch(LaunchRequest::UiDemo).unwrap();
        assert!(processes.launch(LaunchRequest::UiDemo).is_err());
        assert_eq!(processes.children.len(), 1);
        assert_eq!(processes.spawner.calls.len(), 1);
        processes.launch(LaunchRequest::UiDemo).unwrap();
        assert_eq!(processes.spawner.calls.len(), 2);
        let retried = processes.spawner.calls.get(1).unwrap().1.last().unwrap();
        assert!(retried.to_string_lossy().ends_with("-1.ready"));
    }

    #[test]
    fn process_adapter_retries_a_failed_spawn_without_consuming_its_name() {
        let spawner = FakeSpawner {
            failures: 1,
            ..FakeSpawner::default()
        };
        let mut processes = LaunchProcesses::with_spawner(launch_options(), spawner);

        assert!(processes.launch(LaunchRequest::UiDemo).is_err());
        processes.launch(LaunchRequest::UiDemo).unwrap();
        let failed = processes.spawner.calls.first().unwrap().1.last().unwrap();
        let retried = processes.spawner.calls.get(1).unwrap().1.last().unwrap();
        assert_eq!(failed, retried);
        assert!(retried.to_string_lossy().ends_with("-0.ready"));
    }

    #[test]
    fn process_adapter_reports_failed_children_and_continues_launching() {
        let mut states = VecDeque::new();
        states.push_back(Ok(ChildState::Failed("exit status: 2".to_string())));
        let mut spawner = FakeSpawner::default();
        spawner.children.push_back(FakeChild { states });
        spawner.children.push_back(FakeChild::default());
        let mut processes = LaunchProcesses::with_spawner(launch_options(), spawner);

        assert!(processes.launch(LaunchRequest::UiDemo).unwrap().is_empty());
        assert_eq!(
            processes.launch(LaunchRequest::UiDemo).unwrap(),
            ["launched UI client exited with exit status: 2"]
        );
        assert_eq!(processes.children.len(), 1);
        assert_eq!(processes.spawner.calls.len(), 2);
    }

    #[test]
    fn reaping_an_abnormal_exit_removes_its_stale_readiness_socket() {
        let directory = std::env::temp_dir().join(format!(
            "td-launcher-stale-ready-{}-{}",
            std::process::id(),
            TEST_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&directory).unwrap();
        let options = LaunchOptions {
            socket: directory.join("wayland-0"),
            client: PathBuf::from("/bin/td-ui-demo"),
            terminal: PathBuf::from("/bin/td-term"),
        };
        let mut states = VecDeque::new();
        states.push_back(Ok(ChildState::Failed("signal: 9".to_string())));
        let mut spawner = FakeSpawner::default();
        spawner.children.push_back(FakeChild { states });
        spawner.children.push_back(FakeChild::default());
        let mut processes = LaunchProcesses::with_spawner(options, spawner);
        processes.launch(LaunchRequest::UiDemo).unwrap();
        let ready = processes.children.first().unwrap().ready_socket.clone();
        drop(UnixListener::bind(&ready).unwrap());

        let failures = processes.launch(LaunchRequest::UiDemo).unwrap();
        assert_eq!(failures, ["launched UI client exited with signal: 9"]);
        assert!(!ready.exists());
        drop(processes);
        fs::remove_dir(&directory).unwrap();
    }

    #[test]
    fn readiness_cleanup_refusal_is_reported() {
        let directory = std::env::temp_dir().join(format!(
            "td-launcher-ready-refusal-{}-{}",
            std::process::id(),
            TEST_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&directory).unwrap();
        let options = LaunchOptions {
            socket: directory.join("wayland-0"),
            client: PathBuf::from("/bin/td-ui-demo"),
            terminal: PathBuf::from("/bin/td-term"),
        };
        let mut states = VecDeque::new();
        states.push_back(Ok(ChildState::Exited));
        let mut spawner = FakeSpawner::default();
        spawner.children.push_back(FakeChild { states });
        spawner.children.push_back(FakeChild::default());
        let mut processes = LaunchProcesses::with_spawner(options, spawner);
        processes.launch(LaunchRequest::UiDemo).unwrap();
        let ready = processes.children.first().unwrap().ready_socket.clone();
        fs::write(&ready, b"not a socket").unwrap();

        let failures = processes.launch(LaunchRequest::UiDemo).unwrap();
        assert_eq!(failures.len(), 1);
        assert!(failures
            .first()
            .unwrap()
            .contains("refusing to replace non-socket launcher readiness"));
        fs::remove_file(&ready).unwrap();
        drop(processes);
        fs::remove_dir(&directory).unwrap();
    }

    #[test]
    fn failed_child_diagnostic_survives_a_following_spawn_error() {
        let mut states = VecDeque::new();
        states.push_back(Ok(ChildState::Failed("exit status: 2".to_string())));
        let mut spawner = FakeSpawner::default();
        spawner.children.push_back(FakeChild { states });
        let mut processes = LaunchProcesses::with_spawner(launch_options(), spawner);

        processes.launch(LaunchRequest::UiDemo).unwrap();
        processes.spawner.failures = 1;
        let error = processes.launch(LaunchRequest::UiDemo).unwrap_err();
        assert!(error.contains("launched UI client exited with exit status: 2"));
        assert!(error.contains("injected spawn failure"));
        assert!(processes.children.is_empty());
        assert_eq!(processes.spawner.calls.len(), 2);
    }

    #[test]
    fn failed_child_diagnostic_survives_another_childs_reap_error() {
        let mut first_states = VecDeque::new();
        first_states.push_back(Ok(ChildState::Running));
        first_states.push_back(Ok(ChildState::Failed("signal: 9".to_string())));
        let mut second_states = VecDeque::new();
        second_states.push_back(Err("injected reap failure".to_string()));
        let mut spawner = FakeSpawner::default();
        spawner.children.push_back(FakeChild {
            states: first_states,
        });
        spawner.children.push_back(FakeChild {
            states: second_states,
        });
        let mut processes = LaunchProcesses::with_spawner(launch_options(), spawner);

        processes.launch(LaunchRequest::UiDemo).unwrap();
        processes.launch(LaunchRequest::UiDemo).unwrap();
        let error = processes.launch(LaunchRequest::UiDemo).unwrap_err();
        assert!(error.contains("launched UI client exited with signal: 9"));
        assert!(error.contains("reap launched UI client: injected reap failure"));
        assert_eq!(processes.children.len(), 1);
        assert_eq!(processes.spawner.calls.len(), 2);
    }

    #[test]
    fn process_adapter_rejects_sequence_exhaustion_before_spawning() {
        let mut processes = LaunchProcesses::with_spawner(launch_options(), FakeSpawner::default());
        processes.sequence = u64::MAX;

        assert!(processes.launch(LaunchRequest::UiDemo).is_err());
        assert!(processes.spawner.calls.is_empty());
        assert!(processes.children.is_empty());
    }
}

#[cfg(test)]
mod authority_entry_tests {
    use super::*;
    use crate::launcher_table::Row;

    fn row(name: &str, display: &str, terms: &[&str], app_id: &str) -> Row {
        Row {
            name: name.into(),
            display: display.into(),
            terms: terms.iter().map(|term| term.to_string()).collect(),
            app_id: app_id.into(),
        }
    }

    fn image() -> Vec<Row> {
        vec![
            row("news", "News", &["feeds"], "td-news"),
            row("firefox", "Firefox", &["browser"], "org.mozilla.firefox"),
            row("mail", "Mail", &["email"], "td-mail"),
        ]
    }

    fn authority_card(installed: &[&str]) -> Launcher {
        let mut launcher = Launcher::new();
        launcher.set_authority(true);
        launcher.set_table(&image());
        launcher.set_installed(|name| Some(installed.contains(&name)));
        launcher
    }

    const FIXED: &[&str] = &[
        "NEW TERMINAL",
        "SOFTWARE",
        "TASK MANAGER",
        "TEXT EDITOR",
        "PHOTOS",
        "CODE REVIEW",
        "DISK USAGE",
        "CODING AGENT",
        "CLOSE LAUNCHER",
    ];

    /// td-software/DESIGN.md §5's order: the fixed entries, then the
    /// installed applications by display name; nothing of the table
    /// before root has answered.
    #[test]
    fn installed_applications_follow_the_fixed_entries_by_display_name() {
        let mut launcher = Launcher::new();
        launcher.set_authority(true);
        launcher.set_table(&image());
        launcher.apply(LauncherAction::Open);
        assert_eq!(launcher.matched_labels(), FIXED);
        launcher.set_installed(|name| Some(name != "firefox"));
        let labels = launcher.matched_labels();
        assert_eq!(labels.get(..FIXED.len()), Some(FIXED));
        assert_eq!(labels.get(FIXED.len()..), Some(&["MAIL", "NEWS"][..]));
        launcher.set_installed(|_| Some(true));
        assert_eq!(
            launcher.matched_labels().get(FIXED.len()..),
            Some(&["FIREFOX", "MAIL", "NEWS"][..])
        );
        // A row asks for its application by position, which names it.
        for (term, name) in [("browser", "firefox"), ("email", "mail"), ("news", "news")] {
            launcher.apply(LauncherAction::Open);
            for character in term.chars() {
                launcher.apply(LauncherAction::Insert(character));
            }
            let Some(LaunchRequest::Application(index)) = launcher.apply(LauncherAction::Activate)
            else {
                panic!("{term} asked no application");
            };
            assert_eq!(launcher.application(index), Some(name));
        }
        // Without the authority the table is not on the card.
        launcher.set_authority(false);
        launcher.apply(LauncherAction::Open);
        assert_eq!(
            launcher.matched_labels(),
            ["NEW TERMINAL", "NEW INPUT MONITOR", "CLOSE LAUNCHER"]
        );
    }

    /// A search matching nothing but an application root says is not
    /// installed shows one row, which opens td-software at it.
    #[test]
    fn a_search_for_only_an_uninstalled_application_offers_to_install_it() {
        let mut launcher = authority_card(&["mail", "news"]);
        launcher.apply(LauncherAction::Open);
        for character in "fire".chars() {
            launcher.apply(LauncherAction::Insert(character));
        }
        assert_eq!(launcher.matched_labels(), ["INSTALL FIREFOX"]);
        let Some(LaunchRequest::Install(index)) = launcher.apply(LauncherAction::Activate) else {
            panic!("no install row");
        };
        assert_eq!(launcher.application(index), Some("firefox"));
        // Installed, the row is the application's own.
        launcher.set_installed(|_| Some(true));
        launcher.apply(LauncherAction::Open);
        for character in "fire".chars() {
            launcher.apply(LauncherAction::Insert(character));
        }
        assert_eq!(launcher.matched_labels(), ["FIREFOX"]);
        // A query that matches a fixed entry too offers no install.
        let mut launcher = authority_card(&[]);
        launcher.apply(LauncherAction::Open);
        launcher.apply(LauncherAction::Insert('e'));
        assert!(!launcher
            .matched_labels()
            .iter()
            .any(|label| label.starts_with("INSTALL")));
        // Before root has answered nothing is offered.
        let mut unknown = Launcher::new();
        unknown.set_authority(true);
        unknown.set_table(&image());
        unknown.apply(LauncherAction::Open);
        for character in "fire".chars() {
            unknown.apply(LauncherAction::Insert(character));
        }
        assert!(unknown.matched_labels().is_empty());
    }

    /// An application updates only from td-software (td-software/DESIGN.md
    /// §7): the card offers no update row, and a search for one finds
    /// nothing; an application not installed is offered to install.
    #[test]
    fn the_card_offers_no_update_and_follows_the_installed_set() {
        let mut launcher = authority_card(&["firefox", "mail"]);
        launcher.apply(LauncherAction::Open);
        assert_eq!(
            launcher.matched_labels().get(FIXED.len()..),
            Some(&["FIREFOX", "MAIL"][..])
        );
        for query in ["update", "check"] {
            launcher.apply(LauncherAction::Open);
            for character in query.chars() {
                launcher.apply(LauncherAction::Insert(character));
            }
            assert!(launcher.matched_labels().is_empty(), "{query}");
        }
        launcher.set_installed(|name| Some(name == "mail"));
        launcher.apply(LauncherAction::Open);
        assert_eq!(
            launcher.matched_labels().get(FIXED.len()..),
            Some(&["MAIL"][..])
        );
        for (query, expected) in [
            ("firefox", &["INSTALL FIREFOX"][..]),
            ("install fire", &["INSTALL FIREFOX"][..]),
            ("install", &["SOFTWARE"][..]),
            ("update", &[][..]),
        ] {
            launcher.apply(LauncherAction::Open);
            for character in query.chars() {
                launcher.apply(LauncherAction::Insert(character));
            }
            assert_eq!(launcher.matched_labels(), expected, "{query}");
        }
        // A live boot installs nothing, so it offers no Install row either.
        launcher.set_installed(|_| None);
        launcher.apply(LauncherAction::Open);
        for character in "firefox".chars() {
            launcher.apply(LauncherAction::Insert(character));
        }
        assert!(launcher.matched_labels().is_empty());
    }

    /// The card's start and the install row each queue one request for
    /// the worker.
    #[test]
    fn the_backend_asks_root_for_the_applications_units() {
        let (authority, queued) = crate::authority::Queued::launcher();
        let mut backend = LaunchBackend::Authority(authority);
        backend.start_application("mail").unwrap();
        assert_eq!(
            queued.supervised(),
            Some(crate::authority::Supervised::Start("mail".into()))
        );
        backend.install_application("firefox").unwrap();
        assert_eq!(queued.software_at().as_deref(), Some("firefox"));
        assert!(backend.install_application("Firefox").is_err());
        for request in [LaunchRequest::Application(0), LaunchRequest::Install(0)] {
            assert!(backend.launch(request).is_err());
        }
    }

    #[test]
    fn desktop_program_entries_are_enabled_only_for_the_fixed_authority() {
        let mut launcher = Launcher::new();
        launcher.apply(LauncherAction::Open);
        assert_eq!(
            launcher.matched_labels(),
            ["NEW TERMINAL", "NEW INPUT MONITOR", "CLOSE LAUNCHER"]
        );
        launcher.set_authority(true);
        launcher.apply(LauncherAction::Open);
        assert_eq!(launcher.matched_labels(), FIXED);
        for (word, label, request) in [
            ("process", "TASK MANAGER", LaunchRequest::TaskManager),
            ("editor", "TEXT EDITOR", LaunchRequest::Editor),
            ("picture", "PHOTOS", LaunchRequest::Photo),
            ("git", "CODE REVIEW", LaunchRequest::Review),
            ("storage", "DISK USAGE", LaunchRequest::Dua),
            ("coding", "CODING AGENT", LaunchRequest::Agent),
            ("flathub", "SOFTWARE", LaunchRequest::Software),
        ] {
            launcher.apply(LauncherAction::Open);
            for character in word.chars() {
                launcher.apply(LauncherAction::Insert(character));
            }
            assert_eq!(launcher.matched_labels(), [label], "{word}");
            assert_eq!(launcher.apply(LauncherAction::Activate), Some(request));
            assert!(request.authority_program().is_some());
        }
        launcher.set_authority(false);
        launcher.apply(LauncherAction::Open);
        assert_eq!(launcher.matched_labels().len(), 3);
        for request in [LaunchRequest::UiDemo, LaunchRequest::Terminal] {
            assert!(request.authority_program().is_none());
        }
    }

    /// The card shows at most CARD_ROWS rows, which a full card holds;
    /// with the table's 256 applications installed every entry is reached
    /// by the selection, shown when selected and pressable where shown,
    /// and by a filter on its name.
    #[test]
    fn the_window_fits_and_every_entry_is_reachable() {
        assert_eq!(CARD_ROWS, 11);
        const {
            assert!(
                FIRST_ROW_TOP + (CARD_ROWS - 1) * ROW_STEP + LABEL_HEIGHT <= CARD_HEIGHT
                    && FIRST_ROW_TOP + CARD_ROWS * ROW_STEP + LABEL_HEIGHT > CARD_HEIGHT
            )
        };
        let rows: Vec<Row> = (0..crate::launcher_table::MAX_ROWS)
            .map(|n| {
                row(
                    &format!("app{n:03}"),
                    &format!("App {n:03}"),
                    &[],
                    &format!("id{n}"),
                )
            })
            .collect();
        let mut launcher = Launcher::new();
        launcher.set_authority(true);
        launcher.set_table(&rows);
        launcher.set_installed(|_| Some(true));
        launcher.apply(LauncherAction::Open);
        let total = FIXED.len() + rows.len();
        assert_eq!(launcher.matches.len(), total);
        let (width, height) = (1024usize, 768usize);
        let full = card(width, height);
        let short = card(width, 300);
        for at in 0..total {
            assert_eq!(launcher.selected(), at);
            for shape in [full, short] {
                let first = launcher.first_shown(shape);
                let shown = (0..CARD_ROWS).take_while(|r| row_shown(shape, *r)).count();
                assert!(first <= at && at < first + shown, "{at} not shown");
            }
            // The selected row's press chooses it.
            let first = launcher.first_shown(full);
            let (x, y) = row_centre(width, height, at - first);
            let Some(LauncherAction::Choose(row)) = launcher.press_at(x, y, width, height) else {
                panic!("no row under {at}");
            };
            let mut pressed = launcher.clone();
            let request = pressed.apply(LauncherAction::Choose(row));
            let expected = launcher
                .matches
                .get(at)
                .and_then(|item| launcher.entry(*item));
            assert_eq!(request, expected.and_then(|entry| entry.request));
            launcher.apply(LauncherAction::Next);
        }
        assert_eq!(launcher.selected(), 0, "wraps");
        launcher.apply(LauncherAction::Previous);
        assert_eq!(launcher.selected(), total - 1);
        for n in [0usize, 117, 255] {
            launcher.apply(LauncherAction::Open);
            for character in format!("app{n:03}").chars() {
                launcher.apply(LauncherAction::Insert(character));
            }
            assert_eq!(launcher.matched_labels(), [format!("APP {n:03}")]);
        }
    }

    #[test]
    fn the_development_launcher_refuses_every_desktop_program() {
        let options = LaunchOptions {
            socket: PathBuf::from("/run/user/1000/wayland-0"),
            client: PathBuf::from("/bin/td-ui-demo"),
            terminal: PathBuf::from("/bin/td-term"),
        };
        for request in [
            LaunchRequest::TaskManager,
            LaunchRequest::Editor,
            LaunchRequest::Photo,
            LaunchRequest::Review,
            LaunchRequest::Dua,
            LaunchRequest::Agent,
            LaunchRequest::Software,
        ] {
            assert!(launch_command(&options, request, 1).is_err(), "{request:?}");
        }
    }
}

use crate::ui::{border, fill, intersect};
use crate::{socket, ui};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};

const CARD_WIDTH: usize = 480;
const CARD_HEIGHT: usize = 210;
const CARD_PADDING: usize = 24;
const MAX_QUERY_BYTES: usize = 64;
const MAX_LAUNCHED_CLIENTS: usize = 16;
const MAX_APPLICATION_NAME_BYTES: usize = 32;
const RESERVED_APPLICATION_NAMES: &[&str] = &["td-jail", "td-jail-reaper-probe"];
const UI_ENTRY_INDEX: usize = 1;
const ENTRY_COUNT: usize = 3;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LauncherAction {
    Open,
    Next,
    Previous,
    Close,
    Activate,
    Insert(char),
    Backspace,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LaunchRequest {
    UiDemo,
    Terminal,
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

const DIRECT_UI_ENTRY: Entry = Entry {
    label: "NEW INPUT MONITOR",
    search: "new input monitor demo wayland",
    request: Some(LaunchRequest::UiDemo),
};

#[derive(Clone)]
struct ApplicationEntry {
    label: String,
    search: String,
}

fn entry_at(application: Option<&ApplicationEntry>, index: usize) -> Option<Entry<'_>> {
    match index {
        0 => Some(TERMINAL_ENTRY),
        UI_ENTRY_INDEX => match application {
            Some(application) => Some(Entry {
                label: &application.label,
                search: &application.search,
                request: Some(LaunchRequest::UiDemo),
            }),
            None => Some(DIRECT_UI_ENTRY),
        },
        2 => Some(CLOSE_ENTRY),
        _ => None,
    }
}

#[derive(Clone)]
pub struct Launcher {
    visible: bool,
    selected: usize,
    query: String,
    matches: Vec<usize>,
    application: Option<ApplicationEntry>,
}

pub struct LaunchOptions {
    pub socket: PathBuf,
    pub client: Option<PathBuf>,
    pub terminal: PathBuf,
    pub application: Option<ApplicationLaunch>,
}

#[derive(Clone)]
pub struct ApplicationLaunch {
    pub name: String,
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

struct LaunchedChild<C> {
    process: C,
    ready_socket: PathBuf,
}

impl LaunchOptions {
    fn validate(&self) -> Result<(), String> {
        if !self.socket.is_absolute() {
            return Err(format!(
                "Wayland socket {} is not absolute",
                self.socket.display()
            ));
        }
        if !self.terminal.is_absolute() {
            return Err(format!(
                "launcher terminal {} is not absolute",
                self.terminal.display()
            ));
        }
        match (&self.client, &self.application) {
            (Some(client), None) if !client.is_absolute() => {
                return Err(format!(
                    "launcher client {} is not absolute",
                    client.display()
                ));
            }
            (Some(_), Some(_)) | (None, None) => {
                return Err(
                    "exactly one launcher client or launcher application is required".into(),
                );
            }
            _ => {}
        }
        if let Some(application) = &self.application {
            if application.name.is_empty()
                || application.name.len() > MAX_APPLICATION_NAME_BYTES
                || application.name.starts_with('-')
                || application.name == "."
                || application.name.contains("..")
                || RESERVED_APPLICATION_NAMES.contains(&application.name.as_str())
                || !application.name.bytes().all(|byte| {
                    byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-')
                })
            {
                return Err("launcher application name is outside the image grammar".into());
            }
        }
        Ok(())
    }
}

impl Launcher {
    pub fn new() -> Launcher {
        Launcher {
            visible: false,
            selected: 0,
            query: String::with_capacity(MAX_QUERY_BYTES),
            matches: (0..ENTRY_COUNT).collect(),
            application: None,
        }
    }

    pub(crate) fn set_application(&mut self, application: Option<&str>) {
        self.application = application.map(|name| ApplicationEntry {
            label: name.to_ascii_uppercase(),
            search: name.to_ascii_lowercase(),
        });
        self.refresh_matches();
    }

    pub fn apply(&mut self, action: LauncherAction) -> Option<LaunchRequest> {
        match action {
            LauncherAction::Open => {
                self.visible = true;
                self.selected = 0;
                self.query.clear();
                self.refresh_matches();
            }
            LauncherAction::Next if self.visible && !self.matches.is_empty() => {
                self.selected = self.selected.saturating_add(1) % self.matches.len();
            }
            LauncherAction::Previous if self.visible && !self.matches.is_empty() => {
                self.selected = if self.selected == 0 {
                    self.matches.len().saturating_sub(1)
                } else {
                    self.selected.saturating_sub(1)
                };
            }
            LauncherAction::Close => self.visible = false,
            LauncherAction::Activate if self.visible => {
                let request = self
                    .matches
                    .get(self.selected)
                    .and_then(|index| entry_at(self.application.as_ref(), *index))
                    .map(|entry| entry.request);
                let request = request?;
                self.visible = false;
                return request;
            }
            LauncherAction::Insert(character)
                if self.visible
                    && character.is_ascii()
                    && !character.is_ascii_control()
                    && self.query.len() < MAX_QUERY_BYTES =>
            {
                self.query.push(character.to_ascii_lowercase());
                self.refresh_matches();
            }
            LauncherAction::Backspace if self.visible && !self.query.is_empty() => {
                self.query.pop();
                self.refresh_matches();
            }
            LauncherAction::Next
            | LauncherAction::Previous
            | LauncherAction::Activate
            | LauncherAction::Insert(_)
            | LauncherAction::Backspace => {}
        }
        None
    }

    fn refresh_matches(&mut self) {
        self.matches.clear();
        let terms = self.query.split_ascii_whitespace();
        for index in 0..ENTRY_COUNT {
            let Some(entry) = entry_at(self.application.as_ref(), index) else {
                continue;
            };
            if terms.clone().all(|term| entry.search.contains(term)) {
                self.matches.push(index);
            }
        }
        self.selected = 0;
    }

    fn visible_query(&self, columns: usize) -> &str {
        let start = self.query.len().saturating_sub(columns);
        self.query.get(start..).unwrap_or_default()
    }

    pub fn paint(&self, frame: &mut [u8], width: usize, height: usize, stride: usize) {
        if !self.visible {
            return;
        }
        let card_width = CARD_WIDTH.min(width.saturating_sub(CARD_PADDING.saturating_mul(2)));
        let card_height = CARD_HEIGHT.min(height.saturating_sub(CARD_PADDING.saturating_mul(2)));
        let left = width.saturating_sub(card_width) / 2;
        let top = height.saturating_sub(card_height) / 2;
        let card = (left, top, card_width, card_height);
        fill(frame, width, height, stride, card, [0x20, 0x18, 0x28, 0]);
        border(frame, width, height, stride, card, [0xc0, 0x70, 0xf0, 0]);
        ui::draw_text_clipped(
            frame,
            width,
            height,
            stride,
            left.saturating_add(20),
            top.saturating_add(18),
            2,
            "TD LAUNCHER",
            [0xff, 0xff, 0xff, 0],
            card,
        );
        ui::draw_text_clipped(
            frame,
            width,
            height,
            stride,
            left.saturating_add(20),
            top.saturating_add(48),
            2,
            "FILTER:",
            [0xd8, 0xb0, 0xf0, 0],
            card,
        );
        let query_columns = card_width.saturating_sub(136) / 12;
        let visible_query = self.visible_query(query_columns);
        ui::draw_text_clipped(
            frame,
            width,
            height,
            stride,
            left.saturating_add(116),
            top.saturating_add(48),
            2,
            visible_query,
            [0xff, 0xff, 0xff, 0],
            card,
        );
        if self.matches.is_empty() {
            ui::draw_text_clipped(
                frame,
                width,
                height,
                stride,
                left.saturating_add(24),
                top.saturating_add(92),
                2,
                "NO MATCHES",
                [0xa8, 0xa0, 0xb0, 0],
                card,
            );
        }
        for (match_index, entry_index) in self.matches.iter().enumerate() {
            let Some(entry) = entry_at(self.application.as_ref(), *entry_index) else {
                continue;
            };
            let row_top = top
                .saturating_add(92)
                .saturating_add(match_index.saturating_mul(42));
            if match_index == self.selected {
                let highlight = intersect(
                    (
                        left.saturating_add(14),
                        row_top.saturating_sub(8),
                        card_width.saturating_sub(28),
                        32,
                    ),
                    card,
                );
                fill(
                    frame,
                    width,
                    height,
                    stride,
                    highlight,
                    [0x58, 0x30, 0x70, 0],
                );
            }
            ui::draw_text_clipped(
                frame,
                width,
                height,
                stride,
                left.saturating_add(24),
                row_top,
                2,
                entry.label,
                if match_index == self.selected {
                    [0xff, 0xff, 0xff, 0]
                } else {
                    [0xa8, 0xa0, 0xb0, 0]
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
            .filter_map(|index| {
                entry_at(self.application.as_ref(), *index).map(|entry| entry.label)
            })
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
    pub fn activates_application(&self) -> bool {
        self.options.application.is_some()
    }

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
    // A configured application is owned by its supervised service. The card
    // activates the observed surface through Runtime and must never create a
    // second process over the same persistent profile.
    let (program, published_ready, tracked_ready) = match (request, &options.application) {
        (LaunchRequest::UiDemo, Some(_)) => {
            return Err("configured launcher application is activation-only".to_string());
        }
        (LaunchRequest::UiDemo, None) => (
            options
                .client
                .clone()
                .ok_or_else(|| "launcher client is not configured".to_string())?,
            ready.clone(),
            ready,
        ),
        (LaunchRequest::Terminal, _) => (options.terminal.clone(), ready.clone(), ready),
    };
    let arguments = vec![
        OsString::from("run"),
        OsString::from("--socket"),
        options.socket.as_os_str().to_os_string(),
        OsString::from("--ready-socket"),
        published_ready.as_os_str().to_os_string(),
    ];
    Ok((program, arguments, tracked_ready))
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
            client: Some(PathBuf::from("/bin/td-ui-demo")),
            terminal: PathBuf::from("/bin/td-term"),
            application: None,
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
    fn application_configuration_names_only_the_ui_entry() {
        assert_eq!(
            entry_at(None, UI_ENTRY_INDEX).map(|entry| entry.label),
            Some("NEW INPUT MONITOR")
        );
        let mut launcher = Launcher::new();
        launcher.apply(LauncherAction::Open);
        assert_eq!(
            launcher.matched_labels(),
            ["NEW TERMINAL", "NEW INPUT MONITOR", "CLOSE LAUNCHER"]
        );
        launcher.set_application(Some("firefox"));
        assert_eq!(
            launcher.matched_labels(),
            ["NEW TERMINAL", "FIREFOX", "CLOSE LAUNCHER"]
        );
    }

    #[test]
    fn filtering_is_ascii_bounded_and_matches_all_terms() {
        let mut launcher = Launcher::new();
        launcher.set_application(Some("firefox"));
        launcher.apply(LauncherAction::Open);
        for character in "FiReFoX".chars() {
            launcher.apply(LauncherAction::Insert(character));
        }
        assert_eq!(launcher.query(), "firefox");
        assert_eq!(launcher.matched_labels(), ["FIREFOX"]);
        assert_eq!(
            launcher.apply(LauncherAction::Activate),
            Some(LaunchRequest::UiDemo)
        );

        launcher.apply(LauncherAction::Open);
        launcher.apply(LauncherAction::Insert('é'));
        launcher.apply(LauncherAction::Insert('\n'));
        assert_eq!(launcher.query(), "");
        for _ in 0..MAX_QUERY_BYTES.saturating_add(8) {
            launcher.apply(LauncherAction::Insert('x'));
        }
        assert_eq!(launcher.query().len(), MAX_QUERY_BYTES);
        assert!(launcher.matched_labels().is_empty());
        assert_eq!(launcher.visible_query(8), "xxxxxxxx");
        assert_eq!(launcher.visible_query(MAX_QUERY_BYTES), launcher.query());
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
        launcher.paint(&mut hidden, width, height, stride);
        assert!(hidden.iter().all(|byte| *byte == 7));

        launcher.apply(LauncherAction::Open);
        let mut visible = hidden.clone();
        launcher.paint(&mut visible, width, height, stride);
        assert_ne!(visible, hidden);
        for row in visible.chunks_exact(stride) {
            assert!(row
                .get(width * 4..)
                .is_some_and(|padding| padding.iter().all(|byte| *byte == 7)));
        }

        let mut tiny = vec![0u8; 4];
        launcher.paint(&mut tiny, 1, 1, 4);
        assert_eq!(tiny, [0, 0, 0, 0]);
        launcher.paint(&mut [], 0, 0, 0);
    }

    #[test]
    fn painter_clips_every_overlay_pixel_to_a_shrunken_card() {
        let mut launcher = Launcher::new();
        launcher.apply(LauncherAction::Open);
        let width = 800;
        let height = 150;
        let stride = width * 4 + 8;
        let mut frame = vec![7u8; stride * height];
        launcher.paint(&mut frame, width, height, stride);

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
    fn registry_entries_are_searchable_and_fit_the_card() {
        let glyph_height = ui::GLYPH_HEIGHT.saturating_mul(2);
        let final_row = 92usize
            .saturating_add(ENTRY_COUNT.saturating_sub(1).saturating_mul(42))
            .saturating_add(glyph_height);
        assert!(final_row <= CARD_HEIGHT);
        for entry in [TERMINAL_ENTRY, CLOSE_ENTRY, DIRECT_UI_ENTRY] {
            assert!(entry.search.is_ascii());
            assert_eq!(entry.search, entry.search.to_ascii_lowercase());
            for word in entry.label.split_ascii_whitespace() {
                assert!(entry.search.contains(&word.to_ascii_lowercase()));
            }
        }
    }

    #[test]
    fn native_launch_commands_are_explicit_unique_and_socket_local() {
        let options = LaunchOptions {
            socket: PathBuf::from("/run/user/1000/wayland-0"),
            client: None,
            terminal: PathBuf::from("/bin/td-term"),
            application: Some(ApplicationLaunch {
                name: "td-jail-fixture".into(),
            }),
        };
        assert!(launch_command(&options, LaunchRequest::UiDemo, 7).is_err());
        let (terminal, terminal_arguments, _) =
            launch_command(&options, LaunchRequest::Terminal, 9).unwrap();
        assert_eq!(terminal, PathBuf::from("/bin/td-term"));
        assert_eq!(terminal_arguments.first(), Some(&OsString::from("run")));

        let direct = LaunchOptions {
            socket: options.socket.clone(),
            client: Some(PathBuf::from("/bin/td-ui-demo")),
            terminal: options.terminal.clone(),
            application: None,
        };
        let (_, direct_arguments, direct_ready) =
            launch_command(&direct, LaunchRequest::UiDemo, 10).unwrap();
        assert_eq!(
            direct_arguments.last().map(OsString::as_os_str),
            Some(direct_ready.as_os_str())
        );
        assert_eq!(direct_ready.parent(), Some(Path::new("/run/user/1000")));

        let relative = LaunchOptions {
            socket: PathBuf::from("wayland-0"),
            client: Some(PathBuf::from("/bin/td-ui-demo")),
            terminal: PathBuf::from("/bin/td-term"),
            application: None,
        };
        assert!(launch_command(&relative, LaunchRequest::UiDemo, 0).is_err());
        let relative = LaunchOptions {
            socket: PathBuf::from("/run/user/1000/wayland-0"),
            client: Some(PathBuf::from("td-ui-demo")),
            terminal: PathBuf::from("/bin/td-term"),
            application: None,
        };
        assert!(launch_command(&relative, LaunchRequest::UiDemo, 0).is_err());
        assert!(LaunchProcesses::new(relative).is_err());
        let ambiguous = LaunchOptions {
            socket: PathBuf::from("/run/user/1000/wayland-0"),
            client: Some(PathBuf::from("/bin/td-jail-fixture")),
            terminal: PathBuf::from("/bin/td-term"),
            application: Some(ApplicationLaunch {
                name: "td-jail-fixture".into(),
            }),
        };
        assert!(launch_command(&ambiguous, LaunchRequest::UiDemo, 0).is_err());
        assert!(LaunchProcesses::new(ambiguous).is_err());
        for name in RESERVED_APPLICATION_NAMES {
            let reserved = LaunchOptions {
                socket: PathBuf::from("/run/user/1000/wayland-0"),
                client: None,
                terminal: PathBuf::from("/bin/td-term"),
                application: Some(ApplicationLaunch {
                    name: (*name).into(),
                }),
            };
            assert!(launch_command(&reserved, LaunchRequest::UiDemo, 0).is_err());
            assert!(LaunchProcesses::new(reserved).is_err());
        }
        let other_application = LaunchOptions {
            socket: PathBuf::from("/run/user/1000/wayland-0"),
            client: None,
            terminal: PathBuf::from("/bin/td-term"),
            application: Some(ApplicationLaunch {
                name: "other-application".into(),
            }),
        };
        assert!(launch_command(&other_application, LaunchRequest::UiDemo, 0).is_err());
        assert!(LaunchProcesses::new(other_application).is_ok());
        // Both paths are refused, whichever request asks: the terminal is
        // spawned by the same `Command::new` the demo is.
        let relative = LaunchOptions {
            socket: PathBuf::from("/run/user/1000/wayland-0"),
            client: Some(PathBuf::from("/bin/td-ui-demo")),
            terminal: PathBuf::from("td-term"),
            application: None,
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
            client: Some(PathBuf::from("/bin/td-ui-demo")),
            terminal: PathBuf::from("/bin/td-term"),
            application: None,
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
            client: Some(PathBuf::from("/bin/td-ui-demo")),
            terminal: PathBuf::from("/bin/td-term"),
            application: None,
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

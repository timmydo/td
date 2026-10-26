//! Fixed human-session program launches for the separately owned compositor.

use crate::channel::Channel;
use crate::inspection::Inspection;
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{Read, Write};
use std::os::unix::fs::MetadataExt;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const LIMIT: usize = 16;
const CHECK_TIMEOUT: Duration = Duration::from_secs(2);
const VERSION: &[u8] = b"TDLA003\n";
/// Where a live boot mounts its install medium (td-install/MEDIA.md): that
/// session runs only the installer, whose table skips the application.
pub(crate) const LIVE_MEDIUM: &str = "/run/td-media";
/// How long a `td-svc start` client may take, as revocation bounds its own.
const SUPERVISED_TIME: Duration = Duration::from_secs(12);
/// The most `td-svc start` clients in flight at once, over every unit.
const SUPERVISED_LIMIT: usize = 16;
/// `0f`'s bounds: at most one `td-svc status` client in flight, started at
/// most this often, each with this long to answer in at most this many
/// bytes, a status line being under 200.
const STATE_INTERVAL: Duration = Duration::from_secs(1);
const STATE_TIME: Duration = Duration::from_secs(5);
const STATE_REPLY: usize = 256;

pub(crate) struct Config {
    user: String,
    owner: u32,
    compositor: u32,
}

impl Config {
    pub fn parse(arguments: &[String]) -> Result<Self, String> {
        Self::parse_with_primary(arguments, || {
            crate::primary_account::load().map_err(|error| error.to_string())
        })
    }

    fn parse_with_primary(
        arguments: &[String],
        load: impl FnOnce() -> Result<crate::primary_account::PrimaryAccount, String>,
    ) -> Result<Self, String> {
        const USAGE: &str = "terminal-serve requires --primary --peer-uid UID or --user USER --uid UID --peer-uid UID";
        if let [primary, peer_flag, compositor] = arguments {
            if primary != "--primary" || peer_flag != "--peer-uid" {
                return Err(USAGE.into());
            }
            let compositor = number(compositor, 1..=999)?;
            return Ok(Self {
                user: load()?.name().into(),
                owner: crate::primary_account::UID,
                compositor,
            });
        }
        // The named account is the primary one, UID 1000, whose name request
        // `1a` carries: the primary rule, which check-launch-session's wider
        // account grammar contains.
        let [user_flag, user, owner_flag, owner, peer_flag, compositor] = arguments else {
            return Err(USAGE.into());
        };
        if user_flag != "--user"
            || owner_flag != "--uid"
            || peer_flag != "--peer-uid"
            || crate::primary_account::validate_name(user).is_err()
        {
            return Err("invalid terminal launch configuration".into());
        }
        Ok(Self {
            user: user.clone(),
            owner: number(owner, 1000..=1000)?,
            compositor: number(compositor, 1..=999)?,
        })
    }

    pub fn peer_uid(&self) -> u32 {
        self.compositor
    }

    fn checker(&self) -> Command {
        let mut command = Command::new("/bin/td-firstboot");
        command.args([
            "check-launch-session",
            &self.user,
            &self.owner.to_string(),
            &self.compositor.to_string(),
        ]);
        command
    }

    /// Root asks its own supervisor to start an application's unit, which
    /// the image names after the application, or Firefox's update unit:
    /// td-svc runs either as the application's account.
    fn supervised(unit: &Supervised) -> Command {
        let mut command = Command::new("/bin/td-svc");
        command.args(["start", unit.name()]);
        command
    }

    /// `0f`'s client: td-svc's status line for an application's unit.
    fn unit_status(name: &str) -> Result<Inspection, String> {
        let mut command = Command::new("/bin/td-svc");
        command.args(["status", name]);
        Inspection::printing(command, STATE_REPLY, STATE_TIME)
    }

    /// The credential helper's command for a human program, or for
    /// td-software opened at an application, which terminal-exec checks
    /// again.
    fn terminal(&self, generation: &str, handle: u64, launch: &Launch) -> Command {
        let mut command = Command::new("/bin/td-login");
        command.args([
            "exec-as",
            &self.user,
            "--",
            "/bin/td-authd",
            "terminal-exec",
            &self.owner.to_string(),
            generation,
            &handle.to_string(),
        ]);
        if let Some(selection) = launch.program().selection() {
            command.arg(selection);
        }
        if let Launch::SoftwareAt(name) = launch {
            command.arg(name);
        }
        command.process_group(0);
        command
    }
}

fn number(text: &str, range: std::ops::RangeInclusive<u32>) -> Result<u32, String> {
    let value = text
        .parse::<u32>()
        .map_err(|_| "invalid terminal launch uid")?;
    if !range.contains(&value) || value.to_string() != text {
        return Err("noncanonical terminal launch uid".into());
    }
    Ok(value)
}

/// Before Channel duplicates stdin or installs a sender pidfd.
pub(crate) fn require_launch_startup() -> Result<(), String> {
    require_root_status(&bounded_file("/proc/self/status")?)?;
    require_no_terminal(&bounded_file("/proc/self/stat")?)?;
    audit_descriptors(standard_descriptor, inherited_descriptors)
}

/// A root operation whose standard input is no channel (the application
/// cache operation): the same root thread, absent terminal and standard
/// descriptors alone, any of which may be one file such as `/dev/null`.
pub(crate) fn require_operation_startup() -> Result<(), String> {
    require_root_status(&bounded_file("/proc/self/status")?)?;
    require_no_terminal(&bounded_file("/proc/self/stat")?)?;
    audit_operation_descriptors(standard_descriptor, inherited_descriptors)
}

fn audit_operation_descriptors(
    mut standard: impl FnMut(u32) -> Result<(u64, u64), String>,
    enumerate: impl FnOnce() -> Result<Vec<u32>, String>,
) -> Result<(), String> {
    // As audit_descriptors: each standard fd exists before read_dir runs.
    for fd in [0, 1, 2] {
        standard(fd)?;
    }
    require_only_standard(enumerate()?)
}

fn standard_descriptor(fd: u32) -> Result<(u64, u64), String> {
    std::fs::metadata(format!("/proc/self/fd/{fd}"))
        .map(|metadata| (metadata.dev(), metadata.ino()))
        .map_err(|e| format!("missing standard descriptor {fd}: {e}"))
}

fn inherited_descriptors() -> Result<Vec<u32>, String> {
    let mut descriptors = Vec::new();
    for entry in std::fs::read_dir("/proc/self/fd").map_err(|e| e.to_string())? {
        let name = entry.map_err(|e| e.to_string())?.file_name();
        descriptors.push(
            name.to_str()
                .ok_or("invalid inherited descriptor name")?
                .parse::<u32>()
                .map_err(|_| "invalid inherited descriptor")?,
        );
        if descriptors.len() > 4 {
            return Err("root operation inherited extra descriptors".into());
        }
    }
    Ok(descriptors)
}

fn require_no_terminal(stat: &str) -> Result<(), String> {
    if stat
        .rsplit_once(") ")
        .and_then(|(_, fields)| fields.split_whitespace().nth(4))
        != Some("0")
    {
        return Err("root operation must have no controlling terminal".into());
    }
    Ok(())
}

fn require_root_status(status: &str) -> Result<(), String> {
    if status.len() > 8192
        || !["Uid:", "Gid:"].iter().all(|key| {
            status
                .lines()
                .find_map(|line| line.strip_prefix(key))
                .is_some_and(|v| v.split_whitespace().collect::<Vec<_>>() == ["0", "0", "0", "0"])
        })
        || !status.lines().any(|line| {
            line.strip_prefix("Threads:")
                .is_some_and(|v| v.trim() == "1")
        })
    {
        return Err("root operation requires one root thread".into());
    }
    Ok(())
}

fn audit_descriptors(
    mut standard: impl FnMut(u32) -> Result<(u64, u64), String>,
    enumerate: impl FnOnce() -> Result<Vec<u32>, String>,
) -> Result<(), String> {
    // Prove each standard fd exists BEFORE read_dir can reuse a missing one.
    // Single-threaded startup installs no handler that changes descriptors.
    let channel = standard(0)?;
    for fd in [1, 2] {
        if standard(fd)? == channel {
            return Err("terminal authority log aliases its private channel".into());
        }
    }
    require_only_standard(enumerate()?)
}

/// The listing's own descriptor is the fourth.
fn require_only_standard(mut descriptors: Vec<u32>) -> Result<(), String> {
    descriptors.sort_unstable();
    if descriptors != [0, 1, 2, 3] {
        return Err("root operation requires only standard descriptors".into());
    }
    Ok(())
}

fn bounded_file(path: &str) -> Result<String, String> {
    let mut text = String::new();
    File::open(path)
        .map_err(|e| e.to_string())?
        .take(8193)
        .read_to_string(&mut text)
        .map_err(|e| e.to_string())?;
    if text.len() > 8192 {
        return Err("terminal process record is oversized".into());
    }
    Ok(text)
}

fn require_session_process(uid: u32, status: &str, cgroup: &str) -> Result<(), String> {
    let expected = uid.to_string();
    if !["Uid:", "Gid:"].iter().all(|key| {
        status
            .lines()
            .find_map(|line| line.strip_prefix(key))
            .is_some_and(|value| {
                value.split_whitespace().collect::<Vec<_>>() == [expected.as_str(); 4]
            })
    }) || cgroup != format!("0::/td-user-{uid}/session\n")
    {
        return Err("terminal is not in its checked human session".into());
    }
    Ok(())
}

/// The program terminal-exec runs; `select` opens td-software, and only
/// it, at an application, a name in the registry grammar.
fn terminal_command(
    uid: u32,
    generation: &str,
    handle: u64,
    terminal: Program,
    select: Option<&str>,
    account: impl FnOnce() -> std::io::Result<crate::primary_account::PrimaryAccount>,
) -> std::io::Result<Command> {
    if select.is_some_and(|name| terminal != Program::Software || !valid_selection(name)) {
        return Err(std::io::Error::other(
            "only td-software opens at an application, named in the registry grammar",
        ));
    }
    if let Some(program) = terminal.desktop_program() {
        let mut command = Command::new(program);
        command.args(terminal.desktop_arguments());
        if let Some(name) = select {
            command.args(["--select", name]);
        }
        command.env(
            "WAYLAND_DISPLAY",
            format!("/run/td-compositor/{uid}/wayland-0"),
        );
        command.envs(terminal.desktop_environment(uid));
        if terminal.starts_in_account_home() {
            command.current_dir(account()?.home());
        }
        return Ok(command);
    }
    let mut command = Command::new("/bin/td-term");
    command.env(
        "TD_CONTROL_SOCKET",
        format!("/run/td-compositor/{uid}/td-control"),
    );
    command.args([
        "run",
        "--socket",
        &format!("/run/td-compositor/{uid}/wayland-0"),
        "--ready-socket",
        &format!("/run/user/{uid}/td-auth-terminal-{generation}-{handle}.ready"),
    ]);
    if terminal != Program::Home {
        command
            .arg("--working-directory")
            .arg(account()?.home().join("src/td-vm/work"));
    }
    match terminal {
        Program::Home | Program::Task => {}
        // Desktop programs returned above; listed so a new kind is a decision.
        Program::TaskManager
        | Program::Editor
        | Program::Photo
        | Program::Review
        | Program::Dua
        | Program::Agent
        | Program::Software => {}
    }
    Ok(command)
}

/// An application td-software opens at: a name in the registry grammar,
/// which selects nothing but td-software's own view.
fn valid_selection(name: &str) -> bool {
    crate::consent::application_name(name)
}

/// Runs only after td-login dropped credentials, before any terminal code.
pub(crate) fn terminal_exec(arguments: &[String]) -> Result<(), String> {
    let (uid, generation, handle, terminal, select) = terminal_arguments(arguments)?;
    require_session_process(
        uid,
        &bounded_file("/proc/self/status")?,
        &bounded_file("/proc/self/cgroup")?,
    )?;
    let mut command = terminal_command(
        uid,
        generation,
        handle,
        terminal,
        select,
        crate::primary_account::load,
    )
    .map_err(|error| error.to_string())?;
    Err(format!("exec session program: {}", command.exec()))
}

/// terminal-exec's arguments: the human's UID, the generation, the
/// handle, the program and td-software's selection.
fn terminal_arguments(
    arguments: &[String],
) -> Result<(u32, &str, u64, Program, Option<&str>), String> {
    const USAGE: &str = "terminal-exec requires UID GENERATION HANDLE \
                         [task|taskmgr|editor|photo|review|dua|agent|software [APP]]";
    let (uid, generation, handle, terminal, select) = match arguments {
        [uid, generation, handle] => (uid, generation, handle, Some(Program::Home), None),
        [uid, generation, handle, literal] => (
            uid,
            generation,
            handle,
            Program::from_selection(literal),
            None,
        ),
        [uid, generation, handle, literal, name]
            if literal == "software" && valid_selection(name) =>
        {
            (
                uid,
                generation,
                handle,
                Some(Program::Software),
                Some(name.as_str()),
            )
        }
        _ => return Err(USAGE.into()),
    };
    let Some(terminal) = terminal else {
        return Err(USAGE.into());
    };
    let uid = number(uid, 1000..=1000)?;
    if generation.len() != 32
        || !generation
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err("invalid terminal generation".into());
    }
    let value = handle
        .parse::<u64>()
        .map_err(|_| "invalid terminal handle")?;
    if value == 0 || value.to_string() != *handle {
        return Err("noncanonical terminal handle".into());
    }
    Ok((uid, generation.as_str(), value, terminal, select))
}

fn generation() -> Result<String, String> {
    let mut nonce = [0u8; 16];
    File::open("/dev/urandom")
        .map_err(|e| e.to_string())?
        .read_exact(&mut nonce)
        .map_err(|e| e.to_string())?;
    let mut text = String::with_capacity(32);
    use std::fmt::Write;
    for byte in nonce {
        write!(text, "{byte:02x}").map_err(|e| e.to_string())?;
    }
    Ok(text)
}

fn spawn(command: &mut Command) -> Result<Child, String> {
    // The original private endpoint is fd 0, even though Channel's clone is
    // CLOEXEC. Replace all standard descriptors for every child, validator
    // included; no endpoint or root log descriptor crosses either exec.
    command
        .env_clear()
        .current_dir("/")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("start terminal authority child: {e}"))
}

fn check_session(config: &Config) -> Result<(), String> {
    let deadline = Instant::now()
        .checked_add(CHECK_TIMEOUT)
        .ok_or("launch check deadline overflow")?;
    let mut child = spawn(&mut config.checker())?;
    wait_check(&mut child, deadline)
}

fn wait_check(child: &mut Child, deadline: Instant) -> Result<(), String> {
    let result = loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                break if Instant::now() >= deadline {
                    Err("launch session reservation check timed out".into())
                } else if status.success() {
                    Ok(())
                } else {
                    Err(format!("launch session reservation check failed: {status}"))
                }
            }
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
            Ok(None) => break Err("launch session reservation check timed out".into()),
            Err(error) => break Err(format!("wait for launch session check: {error}")),
        }
    };
    if result.is_err() {
        let _ = child.kill();
        let _ = child.wait();
    }
    result
}

#[derive(Debug, PartialEq, Eq)]
enum Request {
    Start(Program),
    Poll(u64),
    Heartbeat,
    Supervised(Supervised),
    /// `0f` and a name: that application unit's state, as root last read
    /// it.
    UnitState(String),
    /// `24`: an application cache request for a validated name.
    Software(crate::software::Request),
    /// `23`: which of the registry's Flathub declarations are installed.
    Installed,
    /// `22` and a name: td-software, opened at that application.
    SoftwareAt(String),
    Secret(crate::session::Request),
}

/// What a start runs: a fixed program, or td-software opened at an
/// application, so only td-software can carry a selection.
#[derive(Debug, PartialEq, Eq)]
enum Launch {
    Fixed(Program),
    SoftwareAt(String),
}

impl Launch {
    fn program(&self) -> Program {
        match self {
            Self::Fixed(program) => *program,
            Self::SoftwareAt(_) => Program::Software,
        }
    }
}

/// What `0d` and `0e` start (APPLICATIONS.md §W.9): an application's unit,
/// named after the application, a registry name the peer chose and root
/// checks against the registry; or the oneshot that checks Firefox's
/// Flathub cache now, which the peer does not name.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Supervised {
    Start(String),
    Update,
}

impl Supervised {
    fn name(&self) -> &str {
        match self {
            Self::Start(name) => name,
            Self::Update => "firefox-update",
        }
    }
}

/// An application `0d` and `0f` name: the rest of the frame, in the
/// registry grammar.
fn application(name: &[u8]) -> Result<String, String> {
    std::str::from_utf8(name)
        .ok()
        .filter(|name| crate::consent::application_name(name))
        .map(str::to_string)
        .ok_or_else(|| "invalid application name".into())
}

/// `0f`'s answer byte: what root last read of the application's unit, so
/// the compositor can say a launch is under way or failed. It describes
/// the unit td-svc supervises, never the application's own output.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum UnitState {
    /// Not running and not failed: never started, stopped, or exited 0.
    Idle,
    /// A start is being asked, or td-svc has the unit on its way up.
    Starting,
    /// td-svc holds a live leader for it, which need not have a window
    /// yet: a first launch's download runs inside it.
    Running,
    /// It failed, which td-svc may yet retry, or is held in backoff, or
    /// root's start request failed.
    Failed,
    /// No start was asked yet and nothing read, or td-svc's line had
    /// another shape.
    Unknown,
}

impl UnitState {
    fn byte(self) -> u8 {
        match self {
            Self::Idle => 0,
            Self::Starting => 1,
            Self::Running => 2,
            Self::Failed => 3,
            Self::Unknown => 4,
        }
    }
}

/// td-svc's `status NAME` reply for `name`, exactly one
/// `NAME STATE pid=PID failures=N[ leaf=unproven]` line (td-svc/DESIGN.md
/// §8), or `None` for any other shape.
fn unit_state(reply: &str, name: &str) -> Option<UnitState> {
    let line = reply.strip_suffix('\n')?;
    let mut fields = line.split(' ');
    if fields.next()? != name {
        return None;
    }
    let state = fields.next()?;
    let digits = |text: &str| !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit());
    let live = match fields.next()?.strip_prefix("pid=")? {
        "-" => false,
        pid if digits(pid) => true,
        _ => return None,
    };
    if !digits(fields.next()?.strip_prefix("failures=")?) {
        return None;
    }
    // td-svc appends the leaf only to a stop in flight.
    match fields.next() {
        None => {}
        Some("leaf=unproven") if state == "stopping" => {}
        Some(_) => return None,
    }
    if fields.next().is_some() {
        return None;
    }
    Some(match (state, live) {
        ("ready", true) => UnitState::Running,
        ("ready" | "skipped" | "stopped", _) => UnitState::Idle,
        ("down" | "starting" | "stopping", _) => UnitState::Starting,
        ("failed" | "held", _) => UnitState::Failed,
        _ => return None,
    })
}

/// `0f`'s reader: one bounded `td-svc status` client at a time, started by
/// a request at most once per STATE_INTERVAL and read without blocking by
/// later ones. A start request opens a new epoch, so a client that began
/// before it cannot answer for the unit after it.
struct UnitProbe {
    status: fn(&str) -> Result<Inspection, String>,
    /// The application it follows: the last a start or a read named.
    name: Option<String>,
    client: Option<(Inspection, u64)>,
    began: Option<Instant>,
    epoch: u64,
    /// A start was asked: until a read answers for it, the unit is
    /// starting.
    asked: bool,
    last: Option<UnitState>,
    /// The last start request's client failed, and no read since has seen
    /// the unit starting or running.
    start_failed: bool,
}

impl UnitProbe {
    fn new(status: fn(&str) -> Result<Inspection, String>) -> Self {
        Self {
            status,
            name: None,
            client: None,
            began: None,
            epoch: 0,
            asked: false,
            last: None,
            start_failed: false,
        }
    }

    /// A start request for `name` is about to be made: nothing read
    /// before it holds. `began` stands, so a start never lets a status
    /// client start sooner.
    fn restart(&mut self, name: &str) {
        self.epoch = self.epoch.wrapping_add(1);
        self.name = Some(name.to_string());
        self.asked = true;
        self.last = None;
        self.start_failed = false;
    }

    /// A read asked of `name`: one for another application than the probe
    /// follows starts over on it, as a start does, but with nothing asked.
    fn follow(&mut self, name: &str) {
        if self.name.as_deref() != Some(name) {
            self.restart(name);
            self.asked = false;
        }
    }

    fn advance(&mut self) {
        let Some((client, epoch)) = self.client.as_mut() else {
            return;
        };
        let reply = match client.answered() {
            Ok(None) => return,
            Ok(Some(Some((bytes, true)))) => Some(bytes),
            // Busy, unreachable, oversized or late: the last read stands.
            Ok(Some(_)) | Err(_) => None,
        };
        let current = *epoch == self.epoch;
        self.client = None;
        let Some(bytes) = reply.filter(|_| current) else {
            return;
        };
        let name = self.name.as_deref().unwrap_or_default();
        let state = std::str::from_utf8(&bytes)
            .ok()
            .and_then(|text| unit_state(text, name));
        if matches!(state, Some(UnitState::Starting | UnitState::Running)) {
            self.start_failed = false;
        }
        self.last = Some(state.unwrap_or(UnitState::Unknown));
    }

    /// The answer for `name`, after starting a client if one is due.
    /// `asking` says a start request's client for it is still in flight.
    fn answer(&mut self, name: &str, asking: bool) -> UnitState {
        self.follow(name);
        self.advance();
        let due = self
            .began
            .is_none_or(|began| began.elapsed() >= STATE_INTERVAL);
        if !asking && self.client.is_none() && due {
            match (self.status)(name) {
                Ok(client) => {
                    self.client = Some((client, self.epoch));
                    self.began = Some(Instant::now());
                }
                Err(why) => {
                    let _ = writeln!(std::io::stderr().lock(), "td-authd: {why}");
                    self.began = Some(Instant::now());
                }
            }
        }
        if asking {
            UnitState::Starting
        } else if self.start_failed {
            UnitState::Failed
        } else {
            self.last.unwrap_or(if self.asked {
                UnitState::Starting
            } else {
                UnitState::Unknown
            })
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Program {
    Home,
    Task,
    TaskManager,
    Editor,
    Photo,
    Review,
    Dua,
    Agent,
    Software,
}

impl Program {
    fn selection(self) -> Option<&'static str> {
        match self {
            Self::Home => None,
            Self::Task => Some("task"),
            Self::TaskManager => Some("taskmgr"),
            Self::Editor => Some("editor"),
            Self::Photo => Some("photo"),
            Self::Review => Some("review"),
            Self::Dua => Some("dua"),
            Self::Agent => Some("agent"),
            Self::Software => Some("software"),
        }
    }

    /// The program `selection` names: its inverse, kept beside it and held
    /// to it by a round-trip test.
    fn from_selection(literal: &str) -> Option<Self> {
        Some(match literal {
            "task" => Self::Task,
            "taskmgr" => Self::TaskManager,
            "editor" => Self::Editor,
            "photo" => Self::Photo,
            "review" => Self::Review,
            "dua" => Self::Dua,
            "agent" => Self::Agent,
            "software" => Self::Software,
            _ => return None,
        })
    }

    /// The fixed system program a desktop request execs directly, with no
    /// terminal around it.
    fn desktop_program(self) -> Option<&'static str> {
        match self {
            Self::TaskManager => Some("/bin/td-taskmgr"),
            Self::Editor => Some("/bin/td-editor"),
            Self::Photo => Some("/bin/td-photo"),
            Self::Review => Some("/bin/td-review"),
            Self::Dua => Some("/bin/td-dua"),
            Self::Agent => Some("/bin/td-agent"),
            Self::Software => Some("/bin/td-software"),
            Self::Home | Self::Task => None,
        }
    }

    /// The fixed arguments a desktop program is started with: none, but
    /// the review window's choice of repository, which it makes itself.
    fn desktop_arguments(self) -> &'static [&'static str] {
        match self {
            Self::Review => &["--choose-repo"],
            Self::Home
            | Self::Task
            | Self::TaskManager
            | Self::Editor
            | Self::Photo
            | Self::Dua
            | Self::Agent
            | Self::Software => &[],
        }
    }

    /// The fixed variables a desktop program is started with beside
    /// `WAYLAND_DISPLAY`: none, but td-agent's runtime directory, where the
    /// fetch service's and egress relay's sockets are, and the image's
    /// td-jail and td-txt, which its tools run through; and td-software's
    /// compositor control socket, which its requests go to.
    fn desktop_environment(self, uid: u32) -> Vec<(&'static str, String)> {
        match self {
            Self::Agent => vec![
                ("XDG_RUNTIME_DIR", format!("/run/user/{uid}")),
                ("TD_AGENT_JAIL", "/bin/td-jail".into()),
                ("TD_AGENT_TXT", "/bin/td-txt".into()),
            ],
            Self::Software => vec![(
                "TD_CONTROL_SOCKET",
                format!("/run/td-compositor/{uid}/td-control"),
            )],
            Self::Home
            | Self::Task
            | Self::TaskManager
            | Self::Editor
            | Self::Photo
            | Self::Review
            | Self::Dua => Vec::new(),
        }
    }

    /// Whether a desktop program starts in the account home, where its file
    /// dialogs begin, the disk usage analyzer scans and td-agent's state
    /// and configuration are found. The task manager and td-software have
    /// no files to open.
    fn starts_in_account_home(self) -> bool {
        match self {
            Self::Editor | Self::Photo | Self::Review | Self::Dua | Self::Agent => true,
            Self::Home | Self::Task | Self::TaskManager | Self::Software => false,
        }
    }
}

fn request(bytes: &[u8]) -> Result<Request, String> {
    match bytes {
        [1] => Ok(Request::Start(Program::Home)),
        [4] => Ok(Request::Start(Program::Task)),
        // 5 started the retired Codex agent and 6 the retired Claude Code;
        // both stay unassigned.
        [7] => Ok(Request::Start(Program::TaskManager)),
        [8] => Ok(Request::Start(Program::Editor)),
        [9] => Ok(Request::Start(Program::Photo)),
        [0x0a] => Ok(Request::Start(Program::Review)),
        [0x0b] => Ok(Request::Start(Program::Dua)),
        [0x0c] => Ok(Request::Start(Program::Agent)),
        [0x0d, name @ ..] if !name.is_empty() => {
            Ok(Request::Supervised(Supervised::Start(application(name)?)))
        }
        [0x0e] => Ok(Request::Supervised(Supervised::Update)),
        [0x0f, name @ ..] if !name.is_empty() => Ok(Request::UnitState(application(name)?)),
        [2, rest @ ..] if rest.len() == 8 => {
            let handle =
                u64::from_be_bytes(rest.try_into().map_err(|_| "invalid terminal handle")?);
            if handle == 0 {
                return Err("zero terminal handle".into());
            }
            Ok(Request::Poll(handle))
        }
        [3] => Ok(Request::Heartbeat),
        [0x10..=0x21, ..] => Ok(Request::Secret(crate::session::Request::decode(bytes)?)),
        [0x24, operands @ ..] => Ok(Request::Software(crate::software::Request::decode(
            operands,
        )?)),
        [0x22] => Ok(Request::Start(Program::Software)),
        [0x22, name @ ..] => {
            let name = std::str::from_utf8(name).map_err(|_| "invalid software selection")?;
            if !valid_selection(name) {
                return Err("invalid software selection".into());
            }
            Ok(Request::SoftwareAt(name.to_string()))
        }
        [0x23] => Ok(Request::Installed),
        _ => Err("invalid terminal authority request".into()),
    }
}

struct Launches {
    children: BTreeMap<u64, Child>,
    next: u64,
    generation: String,
    /// At most one `td-svc start` per unit in flight, and at most
    /// SUPERVISED_LIMIT in all, reaped by a later request (heartbeats arrive every quarter
    /// second) and killed past SUPERVISED_TIME.
    supervised: BTreeMap<Supervised, (Child, Instant)>,
    unit: UnitProbe,
    software: crate::software::Requests,
}

impl Launches {
    /// The client's stdio is null, so its failure is said here, in root's log.
    fn reap_supervised(&mut self) {
        let probe = &mut self.unit;
        self.supervised.retain(|unit, (child, started)| {
            let ended = match child.try_wait() {
                Ok(None) if started.elapsed() < SUPERVISED_TIME => return true,
                Ok(None) => {
                    let _ = child.kill();
                    child
                        .wait()
                        .map(|_| "it did not finish in time".to_string())
                }
                Ok(Some(status)) if status.success() => return false,
                Ok(Some(status)) => Ok(status.to_string()),
                Err(error) => Err(error),
            };
            let detail = ended.unwrap_or_else(|error| format!("wait: {error}"));
            if matches!(unit, Supervised::Start(name) if probe.name.as_ref() == Some(name)) {
                probe.start_failed = true;
            }
            let _ = writeln!(
                std::io::stderr().lock(),
                "td-authd: td-svc start {}: {detail}",
                unit.name()
            );
            false
        });
    }

    fn new(generation: String) -> Self {
        Self::with(
            generation,
            Config::unit_status,
            crate::software::Requests::new(),
        )
    }

    /// With a stand-in `td-svc status` and a registry of Firefox and mail.
    #[cfg(test)]
    fn with_status(generation: String, status: fn(&str) -> Result<Inspection, String>) -> Self {
        Self::with(
            generation,
            status,
            crate::software::Requests::with_registry(|| Ok(vec!["firefox".into(), "mail".into()])),
        )
    }

    fn with(
        generation: String,
        status: fn(&str) -> Result<Inspection, String>,
        software: crate::software::Requests,
    ) -> Self {
        Self {
            children: BTreeMap::new(),
            next: 1,
            generation,
            supervised: BTreeMap::new(),
            unit: UnitProbe::new(status),
            software,
        }
    }

    /// Whether the registry holds `name`; `None` when root cannot read it.
    fn registered(&mut self, name: &str) -> Option<bool> {
        match self.software.registry() {
            Ok(names) => Some(names.iter().any(|known| known == name)),
            Err(why) => {
                let _ = writeln!(std::io::stderr().lock(), "td-authd: {why}");
                None
            }
        }
    }

    fn answer(&mut self, config: &Config, request: Request) -> Result<Vec<u8>, String> {
        self.reap_supervised();
        // Every launch request and heartbeat reaps a finished status
        // client, so one the peer stopped asking about is not left a
        // zombie, and a finished application request.
        self.unit.advance();
        self.software.advance();
        match request {
            Request::Software(request) => Ok(self.software.answer(config.owner, request)),
            Request::Installed => Ok(self.software.installed(config.owner)),
            Request::Secret(_) => Err("secret request reached terminal dispatcher".into()),
            Request::Supervised(unit) => {
                // The live table skips the application; td-svc's `start`
                // would not.
                if std::fs::symlink_metadata(LIVE_MEDIUM).is_ok() {
                    return Ok(vec![0xff, 3]);
                }
                if let Supervised::Start(name) = &unit {
                    // The update unit is `0e`'s, never an application's.
                    if name == Supervised::Update.name() {
                        return Ok(vec![0xff, 4]);
                    }
                    match self.registered(name) {
                        Some(true) => {}
                        Some(false) => return Ok(vec![0xff, 4]),
                        None => return Ok(vec![0xff, 2]),
                    }
                }
                if self.supervised.contains_key(&unit) {
                    return Ok(vec![0xff, 1]);
                }
                if self.supervised.len() >= SUPERVISED_LIMIT {
                    return Ok(vec![0xff, 5]);
                }
                if let Supervised::Start(name) = &unit {
                    self.unit.restart(name);
                }
                match spawn(&mut Config::supervised(&unit)) {
                    Ok(child) => {
                        self.supervised.insert(unit, (child, Instant::now()));
                        Ok(vec![0x84])
                    }
                    Err(why) => {
                        let _ = writeln!(std::io::stderr().lock(), "td-authd: {why}");
                        if matches!(unit, Supervised::Start(_)) {
                            self.unit.start_failed = true;
                        }
                        Ok(vec![0xff, 2])
                    }
                }
            }
            Request::UnitState(name) => {
                if std::fs::symlink_metadata(LIVE_MEDIUM).is_ok() {
                    return Ok(vec![0xff, 3]);
                }
                match self.registered(&name) {
                    Some(true) => {}
                    Some(false) => return Ok(vec![0xff, 4]),
                    None => return Ok(vec![0xff, 2]),
                }
                let asking = self
                    .supervised
                    .contains_key(&Supervised::Start(name.clone()));
                Ok(vec![0x85, self.unit.answer(&name, asking).byte()])
            }
            Request::Heartbeat => Ok(vec![0x83]),
            Request::Start(terminal) => self.start(config, &Launch::Fixed(terminal)),
            Request::SoftwareAt(name) => self.start(config, &Launch::SoftwareAt(name)),
            Request::Poll(handle) => {
                let child = self
                    .children
                    .get_mut(&handle)
                    .ok_or("unknown terminal handle")?;
                let status = child
                    .try_wait()
                    .map_err(|e| format!("wait for terminal: {e}"))?;
                let code = match status {
                    None => 0,
                    Some(status) if status.success() => 1,
                    Some(_) => 2,
                };
                if status.is_some() {
                    self.children.remove(&handle);
                }
                Ok(vec![0x82, code])
            }
        }
    }
}

impl Launches {
    /// A human program, td-software perhaps opened at an application:
    /// `81` and its new handle, `ff 01` with the table full, `ff 02` if
    /// it could not be spawned.
    fn start(&mut self, config: &Config, launch: &Launch) -> Result<Vec<u8>, String> {
        if self.children.len() >= LIMIT {
            return Ok(vec![0xff, 1]);
        }
        let handle = self.next;
        self.next = self
            .next
            .checked_add(1)
            .ok_or("terminal handle space exhausted")?;
        let mut command = config.terminal(&self.generation, handle, launch);
        let child = match spawn(&mut command) {
            Ok(child) => child,
            Err(why) => {
                let _ = writeln!(std::io::stderr().lock(), "td-authd: {why}");
                return Ok(vec![0xff, 2]);
            }
        };
        self.children.insert(handle, child);
        let mut response = vec![0x81];
        response.extend_from_slice(&handle.to_be_bytes());
        Ok(response)
    }
}

pub(crate) fn serve(mut channel: Channel, config: Config) -> Result<(), String> {
    // The channel greeting has already pinned the actual compositor sender.
    channel.send(VERSION).map_err(|e| e.to_string())?;
    if channel.receive().map_err(|e| e.to_string())? != VERSION {
        return Err("unsupported terminal authority protocol".into());
    }
    check_session(&config)?;
    let mut launches = Launches::new(generation()?);
    let mut secrets = crate::session::Session::new(config.owner, &config.user)?;
    channel.send(&[0x80]).map_err(|e| e.to_string())?;
    let result = (|| -> Result<(), String> {
        loop {
            let mut bytes = channel.receive().map_err(|e| e.to_string())?;
            let decoded = request(&bytes);
            // A PIN request's bytes live on only in its clearing owner.
            bytes.fill(0);
            let request = decoded?;
            let answer = match request {
                Request::Secret(request) => secrets.answer(request)?,
                request => {
                    secrets.tick()?;
                    launches.answer(&config, request)?
                }
            };
            channel.send(&answer).map_err(|e| e.to_string())?;
        }
    })();
    let cleanup = secrets.close();
    match (result, cleanup) {
        (Err(reason), Err(cleanup)) => Err(format!("{reason}; {cleanup}")),
        (result, Ok(())) => result,
        (Ok(()), Err(cleanup)) => Err(cleanup),
    }
}

#[cfg(test)]
#[path = "../tests/launch.rs"]
mod tests;

#[cfg(test)]
#[test]
fn enrollment_dispatch_reaches_the_secret_decoder() -> Result<(), String> {
    use crate::consent::Recovery;
    for (tag, recovery) in [(0, Recovery::Unrecoverable), (1, Recovery::SecondToken)] {
        assert_eq!(
            request(&[0x16, tag])?,
            Request::Secret(crate::session::Request::Enroll(recovery))
        );
    }
    assert_eq!(
        request(&[0x17])?,
        Request::Secret(crate::session::Request::Inspect)
    );
    assert_eq!(
        request(&[0x18])?,
        Request::Secret(crate::session::Request::Write)
    );
    assert_eq!(
        request(&[0x19])?,
        Request::Secret(crate::session::Request::Install)
    );
    assert_eq!(
        request(&[0x1b, 7])?,
        Request::Secret(crate::session::Request::Login(
            crate::login::Selection::Unlock
        ))
    );
    assert!(matches!(
        request(&[&[0x1c, 0][..], b"1234"].concat()),
        Err(reason) if !reason.contains("1234")
    ));
    assert_eq!(
        request(&[0x1a])?,
        Request::Secret(crate::session::Request::LoginState)
    );
    assert!(request(&[0x1a, 0]).is_err());
    for choice in [crate::consent::Power::Restart, crate::consent::Power::Off] {
        assert_eq!(
            request(&[0x21, choice.byte()])?,
            Request::Secret(crate::session::Request::Power(choice))
        );
    }
    assert!(request(&[0x21, 2]).is_err());
    assert_eq!(
        request(&[0x1d])?,
        Request::Secret(crate::session::Request::Rollback)
    );
    assert_eq!(
        request(&[0x1e])?,
        Request::Secret(crate::session::Request::Hostname)
    );
    assert_eq!(
        request(&[0x1f])?,
        Request::Secret(crate::session::Request::HostnameState)
    );
    assert_eq!(
        request(&[0x20, 1])?,
        Request::Secret(crate::session::Request::Storage(
            crate::storage::Selection::SelectorUpdate
        ))
    );
    assert_eq!(
        request(&[0x20, 2, 2, 1])?,
        Request::Secret(crate::session::Request::Storage(
            crate::storage::Selection::Upgrade {
                shape: crate::consent::UpgradeShape::Fido2,
                count: 1,
            }
        ))
    );
    assert_eq!(
        request(&[0x20, 3])?,
        Request::Secret(crate::session::Request::Storage(
            crate::storage::Selection::UpgradeCancel
        ))
    );
    {
        use crate::consent::{DiskRequest, DiskRole, Managed, Proof};
        use crate::storage::Selection;
        for (bytes, selection) in [
            (
                &[0x20, 4, 2, 1][..],
                Selection::Protector(Managed::Add {
                    proof: Proof::DiskKey,
                    role: DiskRole::Primary,
                }),
            ),
            (
                &[0x20, 5, 1],
                Selection::Protector(Managed::Remove {
                    proof: Proof::RecoveryKey,
                }),
            ),
            (&[0x20, 6], Selection::Protector(Managed::RecoveryRemove)),
            (
                &[0x20, 9, 2],
                Selection::Protector(Managed::Request {
                    proof: Proof::DiskKey,
                    request: DiskRequest::Reenroll,
                }),
            ),
            (&[0x20, 0x0a], Selection::Later(0x0a)),
        ] {
            assert_eq!(
                request(bytes)?,
                Request::Secret(crate::session::Request::Storage(selection))
            );
        }
        // An operand missing, outside its range or trailing.
        for bad in [
            &[0x20, 4, 2][..],
            &[0x20, 4, 3, 1],
            &[0x20, 5],
            &[0x20, 6, 1],
        ] {
            assert!(request(bad).is_err(), "{bad:?}");
        }
    }
    // None of these takes an operand, `20` takes exactly one but for an
    // upgrade's two, `21` exactly one, `22` and `23` none, and `24` a
    // verb and a name.
    assert!(request(&[0x1d, 0]).is_err());
    assert!(request(&[0x1e, 0]).is_err());
    assert!(request(&[0x1f, 0]).is_err());
    for bad in [
        &[0x20][..],
        &[0x20, 0],
        &[0x20, 0x0b],
        &[0x20, 1, 1],
        &[0x20, 2, 2],
        &[0x20, 2, 2, 0],
        &[0x20, 3, 0],
        &[0x21],
        &[0x21, 0, 0],
        &[0x22, 0],
        &[0x23, 0],
        &[0x24],
        &[0x24, 0],
    ] {
        assert!(request(bad).is_err(), "{bad:?}");
    }
    assert!(request(&[0x16, 2]).is_err());
    assert_eq!(request(&[0x23])?, Request::Installed);
    Ok(())
}

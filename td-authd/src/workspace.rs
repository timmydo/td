//! The workspace launch intake (td-authd/DESIGN.md, "Workspace launch
//! intake"): root's service at `WORKSPACE_INTAKE`, where the primary
//! human's programs create and delete sandboxes, list and change their
//! grants and network, and ask for a shell or a command in one; and create
//! and delete td-agent's workspace slots and start a workspace instance,
//! td-agent's jailed tool host, in one ("Workspace instances"). For a
//! launch root prepares the ledger account, publishes its grant views (a
//! sandbox's from the grant table, a workspace's from the launch's plan),
//! writes a root-owned spec, and starts td-jail's workspace kind as the
//! account, the accepted connection its channel.

use crate::application;
use crate::flathub_app;
use crate::grant_requests::{self, Failed, Written};
use crate::grant_views;
use crate::portal_files::pinned;
use crate::primary_account;
use crate::stderr_tail;
use crate::workload_accounts::{
    self as accounts, canonical_decimal, Kind, WORKSPACE_INTAKE, WORKSPACE_SPECS,
};
use crate::workload_grants::{self, Access, Directory, Network, Role};
use crate::{allocation, message, owned, refused, text};
use std::borrow::Cow;
use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

/// The longest request, its newline included: room for a `run` line's
/// encoded command and a grant's longest path.
const MAX_REQUEST: usize = 8192;
/// The longest workspace instance request, every line of its plan
/// included: its encoded arguments and its directories' paths.
const MAX_INSTANCE_REQUEST: usize = 64 * 1024;
/// A plan's most directories: what an account's manifest lists and
/// td-jail binds.
const MAX_PLAN: usize = workload_grants::MAX_DIRECTORIES;
/// The line that ends a plan.
const PLAN_END: &[u8] = b"end\n";
/// How a workspace instance's request begins.
const INSTANCE: &[u8] = b"workspace launch ";
/// How long a peer has to send its whole request.
const REQUEST_DEADLINE: Duration = Duration::from_secs(2);
/// The most launches at once, over every sandbox, each holding its thread
/// until its shell or command ends.
const MAX_LAUNCHES: usize = 16;
/// The most workspace instances at once, over every slot, and of one
/// slot: td-agent's file tools, each command's and each server's own,
/// each holding its thread until it ends. A pool of their own, so neither
/// sandboxes nor workspaces leave the other none.
const MAX_INSTANCES: usize = 128;
const MAX_SLOT_INSTANCES: usize = 32;
/// The connections served at once: the launches, the instances and room
/// for other verbs, each answered and gone, which neither fills.
const MAX_REQUESTS: usize = MAX_LAUNCHES + MAX_INSTANCES + 4;
/// A terminal type, as td-sandbox's relay admits one.
const MAX_TERM: usize = 64;
const MAX_REASON: usize = 200;
const MAX_PATH: usize = 4096;
/// A `run` command's most words, its program among them.
const MAX_ARGUMENTS: usize = 128;
/// The jail's entry, td-sandbox's `--inside` relay, and the shell `start`
/// runs: td's own, never one a caller names.
const ENTRY: &str = "/bin/td-sandbox";
const SHELL: &str = "/bin/sh";
/// A workspace instance's entry, td-agent, whose jailed tool host the
/// caller's arguments start, and the program td-jail binds beside it:
/// td's own, never ones a caller names.
const AGENT: &str = "/bin/td-agent";
const TXT: &str = "/bin/td-txt";
/// The unified cgroup hierarchy, where a workload's delegation lies.
const CGROUPS: &str = "/sys/fs/cgroup";
/// How long a deletion waits for the processes of the account's
/// delegation to end once killed, and how often it looks.
const STOP_DEADLINE: Duration = Duration::from_secs(10);
const STOP_POLL: Duration = Duration::from_millis(20);
/// The most of a delegation's `cgroup.events` read.
const MAX_EVENTS: u64 = 4096;
/// The answer to an admitted launch, after which the connection is the
/// jail's; a refusal is `refused`, a space and the reason.
const STARTED: &[u8] = b"ok\n";
/// Where everything after a launch's answer is reported, which
/// td-sandbox names when the connection ends before the relay begins.
const LOG: &str = "/var/log/svc/workspace-intake.log";
/// Linux x86-64's `O_NOFOLLOW` and `O_DIRECTORY`.
const O_NOFOLLOW: i32 = 0o400_000;
const O_DIRECTORY: i32 = 0o200_000;
/// And `O_PATH`, a handle that reads nothing.
const O_PATH: i32 = 0o10_000_000;
/// How deep a sandbox's home may nest for its account to empty it: one
/// open directory and one listing per level.
const EMPTY_DEPTH: usize = 128;
/// How often the account walks its home before the home must be empty.
const EMPTY_PASSES: usize = 4;
/// Root's mark in a deleted sandbox's emptied home: a root-owned file the
/// account, which runs only in a launch a mark refuses, never gets to.
const DELETED_MARK: &str = ".td-deleted";
/// The refusals td-sandbox reads by their words: a creation over a
/// sandbox that holds files or grants, and a launch or grant change of a
/// deleted one.
const EXISTS: &str = "exists";
const DELETED: &str = "deleted";
/// Before a refusal asking again cannot change: the writer's or root's
/// validation refusing the request itself, its path, its account or the
/// table's bounds. Every other refusal, a claim's, a busy table's, a
/// failed writer's or a failed allocation's, may pass.
const FINAL: &str = "final: ";
/// The first pause after a failed accept, doubled for each failure in a
/// row up to `ACCEPT_PAUSE_MAX`: a descriptor or memory shortage passes,
/// and leaving would end every shell the service launched.
const ACCEPT_PAUSE: Duration = Duration::from_millis(50);
const ACCEPT_PAUSE_MAX: Duration = Duration::from_secs(1);

/// Root's spec tree: where it lies, the directory below which every
/// directory on the way is made and checked, and the uid and gid that own
/// them all, root's on a running system. A test passes a scratch tree of
/// its own.
struct SpecTree<'a> {
    base: &'a Path,
    root: &'a Path,
    owner: (u32, u32),
}

/// The spec tree root keeps at `WORKSPACE_SPECS`.
fn specs() -> SpecTree<'static> {
    SpecTree {
        base: Path::new(RUN),
        root: Path::new(WORKSPACE_SPECS),
        owner: (0, 0),
    }
}

/// Where root's intakes and specs lie below.
const RUN: &str = "/run";

/// How a launch's relay meets its program: a fresh PTY, or pipes that keep
/// stderr apart and pass stdin's end.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Mode {
    Pty,
    Pipes,
}

impl Mode {
    fn parse(word: &str) -> Option<Self> {
        match word {
            "pty" => Some(Self::Pty),
            "pipes" => Some(Self::Pipes),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Pty => "pty",
            Self::Pipes => "pipes",
        }
    }
}

/// A shell or a command in the sandbox, under the declared runtime
/// `runtime` or td's own trees, through a relay of `mode` on a terminal
/// of type `term`.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Launch {
    pub(crate) runtime: Option<&'static str>,
    pub(crate) mode: Mode,
    pub(crate) term: String,
    /// The program and its arguments; none for td's shell.
    pub(crate) command: Option<Vec<String>>,
}

/// A workspace instance's td-jail profile: `development` shares the
/// host's network, `isolated` has its own with loopback alone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Profile {
    Development,
    Isolated,
}

impl Profile {
    fn parse(word: &str) -> Option<Self> {
        match word {
            "development" => Some(Self::Development),
            "isolated" => Some(Self::Isolated),
            _ => None,
        }
    }
}

/// One directory of a workspace instance's plan: its role, and its path
/// relative to the human's home, admitted by
/// `workload_grants::validate_role`.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Entry {
    pub(crate) role: Role,
    pub(crate) path: String,
}

/// td-agent's jailed tool host in a workspace slot: td-jail's `profile`,
/// the declared runtime `runtime` or td's own trees, the entry's
/// `arguments`, and the plan of the human's directories it binds.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Instance {
    pub(crate) profile: Profile,
    pub(crate) runtime: Option<&'static str>,
    pub(crate) arguments: Vec<String>,
    pub(crate) plan: Vec<Entry>,
}

/// What a request asks of the sandbox or workspace slot it names.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Verb {
    Launch(Launch),
    /// A workspace slot's instance.
    Instance(Instance),
    /// Its account, allocated when the ledger has none.
    Create,
    /// Its processes ended, its grant rows revoked and its home emptied;
    /// its ledger row stays.
    Delete,
    /// Whether the ledger holds it, and whether it holds files or grants.
    Status,
    /// Its directory rows and network listed, or one of them changed,
    /// through the grant table's writer.
    Grants(grant_requests::Verb),
}

/// One request: a verb and a sandbox or a workspace slot, by its kind and
/// ledger name.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Request {
    pub(crate) kind: Kind,
    pub(crate) name: String,
    pub(crate) verb: Verb,
}

impl Request {
    /// One line, single spaces, nothing else:
    ///
    /// - `start NAME RUNTIME TERM`, td's shell on a PTY;
    /// - `run NAME RUNTIME pty|pipes TERM WORD...`, a command, each word
    ///   `x` and its UTF-8 bytes in lowercase hex;
    /// - `create NAME`, `delete NAME` and `grants NAME`;
    /// - `grant NAME ro|rw PATH`, `revoke NAME PATH` and `network NAME
    ///   off|host`, PATH the rest of the line, relative to the human's home.
    ///
    /// NAME is a ledger name (td-sandbox's that begin with a digit or a
    /// hyphen are not), RUNTIME `native` or a declared runtime's td name,
    /// TERM a terminal type of td-sandbox's relay grammar.
    ///
    /// A workspace slot's verbs begin `workspace`: `workspace create
    /// SLOT`, `workspace delete SLOT`, `workspace status SLOT` and
    /// `workspace launch SLOT development|isolated RUNTIME WORD...`, the
    /// entry's arguments encoded as `run`'s are, a runtime only with
    /// `development`. SLOT is one of td-agent's slots
    /// (`workload_accounts::workspace_slot`). A launch's plan follows on
    /// lines of its own (`read_plan`).
    pub(crate) fn parse(line: &[u8]) -> Result<Self, String> {
        let bound = if line.starts_with(INSTANCE) {
            MAX_INSTANCE_REQUEST
        } else {
            MAX_REQUEST
        };
        if line.len() > bound {
            return Err("the workspace request is too long".into());
        }
        let text = std::str::from_utf8(line)
            .ok()
            .and_then(|text| text.strip_suffix('\n'))
            .filter(|text| !text.chars().any(char::is_control))
            .ok_or("the workspace request is not one line")?;
        let malformed = || message(format_args!("the workspace request is malformed"));
        let (verb, rest) = text.split_once(' ').ok_or_else(malformed)?;
        let named = |name: &str| {
            if accounts::valid_name(name) {
                owned(name)
            } else {
                Err(message(format_args!(
                    "the workspace request names no sandbox"
                )))
            }
        };
        let path = |path: &str| {
            if !path.is_empty() && path.len() <= grant_requests::PATH_BYTES {
                allocation::copy(path.as_bytes()).map_err(refused)
            } else {
                Err(message(format_args!("the workspace request names no path")))
            }
        };
        let (name, verb) = match verb {
            "start" => {
                let [name, runtime, term] = words(rest).ok_or_else(malformed)?;
                let launch = Launch {
                    runtime: runtime_of(runtime)?,
                    mode: Mode::Pty,
                    term: term_of(term)?,
                    command: None,
                };
                (name, Verb::Launch(launch))
            }
            "run" => {
                let mut words = rest.split(' ');
                let (Some(name), Some(runtime), Some(mode), Some(term)) =
                    (words.next(), words.next(), words.next(), words.next())
                else {
                    return Err(malformed());
                };
                let launch = Launch {
                    runtime: runtime_of(runtime)?,
                    mode: Mode::parse(mode).ok_or_else(malformed)?,
                    term: term_of(term)?,
                    command: Some(command_of(words)?),
                };
                (name, Verb::Launch(launch))
            }
            "workspace" => return workspace_request(rest),
            "create" | "delete" | "status" | "grants" => {
                let [name] = words(rest).ok_or_else(malformed)?;
                let verb = match verb {
                    "create" => Verb::Create,
                    "delete" => Verb::Delete,
                    "status" => Verb::Status,
                    _ => Verb::Grants(grant_requests::Verb::List { start: 0 }),
                };
                (name, verb)
            }
            "grant" => {
                let mut words = rest.splitn(3, ' ');
                let (Some(name), Some(access), Some(granted)) =
                    (words.next(), words.next(), words.next())
                else {
                    return Err(malformed());
                };
                let access = Access::parse(access).ok_or_else(malformed)?;
                (
                    name,
                    Verb::Grants(grant_requests::Verb::Grant(access, path(granted)?)),
                )
            }
            "revoke" => {
                let (name, revoked) = rest.split_once(' ').ok_or_else(malformed)?;
                (
                    name,
                    Verb::Grants(grant_requests::Verb::Revoke(path(revoked)?)),
                )
            }
            "network" => {
                let [name, network] = words(rest).ok_or_else(malformed)?;
                let network = Network::parse(network).ok_or_else(malformed)?;
                (name, Verb::Grants(grant_requests::Verb::Network(network)))
            }
            _ => return Err(malformed()),
        };
        Ok(Self {
            kind: Kind::Sandbox,
            name: named(name)?,
            verb,
        })
    }
}

/// The words of a command or an entry's arguments, each `x` and its
/// UTF-8 bytes in lowercase hex: at least one, the first not empty, and
/// at most `MAX_ARGUMENTS`.
fn command_of<'a>(words: impl Iterator<Item = &'a str>) -> Result<Vec<String>, String> {
    let mut command = Vec::new();
    for word in words {
        let Some(argument) = argument(word)? else {
            return Err("the workspace request's command is not encoded".into());
        };
        allocation::push(&mut command, argument).map_err(refused)?;
    }
    if command.first().is_none_or(String::is_empty) || command.len() > MAX_ARGUMENTS {
        return Err("the workspace request names no command".into());
    }
    Ok(command)
}

/// A workspace slot's request after its `workspace` (`Request::parse`);
/// a launch's plan is read after it.
fn workspace_request(rest: &str) -> Result<Request, String> {
    let malformed = || message(format_args!("the workspace request is malformed"));
    let (verb, rest) = rest.split_once(' ').ok_or_else(malformed)?;
    let slot = |name: &str| {
        if accounts::workspace_slot(name).is_some() {
            owned(name)
        } else {
            Err(message(format_args!(
                "the workspace request names no workspace slot"
            )))
        }
    };
    let (name, verb) = match verb {
        "create" | "delete" | "status" => {
            let [name] = words(rest).ok_or_else(malformed)?;
            let verb = match verb {
                "create" => Verb::Create,
                "delete" => Verb::Delete,
                _ => Verb::Status,
            };
            (name, verb)
        }
        "launch" => {
            let mut words = rest.split(' ');
            let (Some(name), Some(profile), Some(runtime)) =
                (words.next(), words.next(), words.next())
            else {
                return Err(malformed());
            };
            let profile = Profile::parse(profile).ok_or_else(malformed)?;
            let runtime = runtime_of(runtime)?;
            if runtime.is_some() && profile == Profile::Isolated {
                return Err("a workspace runtime runs only in a development instance".into());
            }
            let instance = Instance {
                profile,
                runtime,
                arguments: command_of(words)?,
                plan: Vec::new(),
            };
            (name, Verb::Instance(instance))
        }
        _ => return Err(malformed()),
    };
    Ok(Request {
        kind: Kind::Workspace,
        name: slot(name)?,
        verb,
    })
}

/// One line of a workspace instance's plan, `ROLE PATH`, its newline
/// taken: a role td-jail binds and a path below the human's home, the
/// rest of the line, which `workload_grants::validate_role` admits in
/// that role. Its refusal is final: asking again cannot change it.
fn plan_entry(line: &[u8]) -> Result<Entry, String> {
    let text = std::str::from_utf8(line)
        .ok()
        .and_then(|text| text.strip_suffix('\n'))
        .ok_or("the workspace plan's line is not one line")?;
    let (role, path) = text
        .split_once(' ')
        .ok_or("the workspace plan's line is malformed")?;
    let role = Role::parse(role).ok_or("the workspace plan names an unknown role")?;
    workload_grants::validate_role(role, path.as_bytes())
        .map_err(|why| message(format_args!("{FINAL}{why}")))?;
    Ok(Entry {
        role,
        path: owned(path)?,
    })
}

/// `entry` beside the plan's earlier entries: at most `MAX_PLAN`, and no
/// two of them one directory or nested, since an account's views never
/// overlap.
fn admit_entry(plan: &mut Vec<Entry>, entry: Entry) -> Result<(), String> {
    if plan.len() >= MAX_PLAN {
        return Err(message(format_args!(
            "{FINAL}the workspace plan names more than {MAX_PLAN} directories"
        )));
    }
    if let Some(other) = plan
        .iter()
        .find(|other| workload_grants::overlapping(&other.path, &entry.path))
    {
        return Err(message(format_args!(
            "{FINAL}the workspace plan's {} and {} overlap",
            other.path, entry.path
        )));
    }
    allocation::push(plan, entry).map_err(refused)
}

/// Exactly `N` words separated by single spaces.
fn words<const N: usize>(text: &str) -> Option<[&str; N]> {
    let mut words = text.split(' ');
    let mut out = [""; N];
    for slot in &mut out {
        *slot = words.next()?;
    }
    words.next().is_none().then_some(out)
}

/// `native`, or a declared runtime's td name.
fn runtime_of(word: &str) -> Result<Option<&'static str>, String> {
    match word {
        "native" => Ok(None),
        named => flathub_app::flathub_runtime(named)
            .map(|runtime| Some(runtime.name))
            .ok_or_else(|| "the workspace request names no declared runtime".into()),
    }
}

fn term_of(word: &str) -> Result<String, String> {
    if valid_term(word) {
        owned(word)
    } else {
        Err("the workspace request names no terminal type".into())
    }
}

/// One word of a `run` command: `x` and its UTF-8 bytes in lowercase hex,
/// with no NUL, so any argument, an empty one included, is one word; none
/// for a word that is not one.
fn argument(word: &str) -> Result<Option<String>, String> {
    let Some(hex) = word.strip_prefix('x') else {
        return Ok(None);
    };
    let (pairs, odd) = hex.as_bytes().as_chunks::<2>();
    if !odd.is_empty() {
        return Ok(None);
    }
    let digit = |byte: u8| match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    };
    let mut bytes = allocation::with_capacity(pairs.len()).map_err(refused)?;
    for [high, low] in pairs {
        let (Some(high), Some(low)) = (digit(*high), digit(*low)) else {
            return Ok(None);
        };
        allocation::push(&mut bytes, (high << 4) | low).map_err(refused)?;
    }
    Ok(String::from_utf8(bytes)
        .ok()
        .filter(|argument| !argument.contains('\0')))
}

/// td-sandbox's relay's terminal type grammar.
fn valid_term(term: &str) -> bool {
    !term.is_empty()
        && term.len() <= MAX_TERM
        && term
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_.+".contains(&byte))
}

/// Only the primary human's programs, by the connecting socket's kernel
/// credentials: never root, an application's or a sandbox's own account.
fn admitted(peer: io::Result<u32>) -> bool {
    matches!(peer, Ok(uid) if uid == primary_account::UID)
}

/// One printable line of at most `MAX_REASON` bytes, never without a
/// reason: a refused allocation, whose error is empty, is said as one.
fn refusal(why: &str) -> Result<Vec<u8>, String> {
    const HEAD: &[u8] = b"refused ";
    let why = if why.is_empty() { "out of memory" } else { why };
    let mut line = allocation::with_capacity(HEAD.len() + MAX_REASON + 1).map_err(refused)?;
    allocation::extend_from_slice(&mut line, HEAD).map_err(refused)?;
    for c in why.chars().take(MAX_REASON) {
        let byte = u8::try_from(c)
            .ok()
            .filter(|byte| (b' '..=b'~').contains(byte))
            .unwrap_or(b'?');
        allocation::push(&mut line, byte).map_err(refused)?;
    }
    allocation::push(&mut line, b'\n').map_err(refused)?;
    Ok(line)
}

/// Writes the refusal of `why` to `stream`; when even its line cannot be
/// had, the requester sees the connection close.
fn refuse(stream: &mut UnixStream, why: &str) {
    if let Ok(line) = refusal(why) {
        let _ = stream.write_all(&line);
    }
}

/// Reads one request: byte by byte to its newline, so nothing after it,
/// which is the channel's, is taken, within `REQUEST_DEADLINE` and
/// `MAX_REQUEST`.
fn read_request(stream: &mut UnixStream) -> Result<Request, String> {
    let deadline = Instant::now() + REQUEST_DEADLINE;
    // Only a workspace instance's line may pass `MAX_REQUEST`.
    let line = read_line(stream, deadline, |line| {
        if line.len() < MAX_REQUEST || line.starts_with(INSTANCE) {
            MAX_INSTANCE_REQUEST
        } else {
            MAX_REQUEST
        }
    })?;
    let mut request = Request::parse(&line)?;
    if let Verb::Instance(instance) = &mut request.verb {
        read_plan(
            stream,
            deadline,
            MAX_INSTANCE_REQUEST.saturating_sub(line.len()),
            &mut instance.plan,
        )?;
    }
    // The timeout is the socket's, which the jail inherits.
    stream
        .set_read_timeout(None)
        .map_err(|e| message(format_args!("{e}")))?;
    Ok(request)
}

/// A workspace instance's plan, after its request's line: a line per
/// directory (`plan_entry`), then `end`, all of it within `budget` bytes
/// and by `deadline`.
fn read_plan(
    stream: &mut UnixStream,
    deadline: Instant,
    mut budget: usize,
    plan: &mut Vec<Entry>,
) -> Result<(), String> {
    loop {
        let line = read_line(stream, deadline, |_| budget)?;
        if line == PLAN_END {
            return Ok(());
        }
        budget = budget.saturating_sub(line.len());
        admit_entry(plan, plan_entry(&line)?)?;
    }
}

/// One line, its newline included: byte by byte, so nothing after it is
/// taken, within the bound `bound` sets for what it has read and by
/// `deadline`.
fn read_line(
    stream: &mut UnixStream,
    deadline: Instant,
    bound: impl Fn(&[u8]) -> usize,
) -> Result<Vec<u8>, String> {
    let mut line = allocation::with_capacity(256).map_err(refused)?;
    let mut byte = [0u8; 1];
    while line.last() != Some(&b'\n') {
        if line.len() >= bound(&line) {
            return Err("the workspace request is too long".into());
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err("the workspace request timed out".into());
        }
        stream
            .set_read_timeout(Some(remaining))
            .map_err(|e| message(format_args!("{e}")))?;
        match stream.read(&mut byte) {
            Ok(0) => return Err("the workspace request ended early".into()),
            Ok(_) => allocation::extend_from_slice(&mut line, &byte).map_err(refused)?,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                return Err("the workspace request timed out".into())
            }
            Err(e) => return Err(message(format_args!("read the workspace request: {e}"))),
        }
    }
    Ok(line)
}

/// The launch spec root writes (APPLICATIONS.md §C, "The `workspace`
/// kind"): the development profile, sharing the host's network only for a
/// sandbox whose network row is `host`, the request's runtime, td-sandbox
/// as the entry and the account's home. Nothing else: the views td-jail
/// binds come from the account's manifest.
fn spec_text(
    network: Network,
    runtime: Option<&str>,
    entry: &Path,
    home: &Path,
) -> Result<String, String> {
    let profile = match network {
        Network::Host => "profile=development\n",
        Network::Off => "profile=development-offline\n",
    };
    let runtime = match runtime {
        Some(runtime) => ["runtime=", runtime, "\n"],
        None => ["", "", ""],
    };
    let mut spec = owned("format=1\n")?;
    for part in [profile]
        .into_iter()
        .chain(runtime)
        .chain(["entry=", spec_line(entry)?, "\n"])
        .chain(["home=", spec_line(home)?, "\n"])
    {
        allocation::push_str(&mut spec, part).map_err(refused)?;
    }
    Ok(spec)
}

/// Where a workspace instance's spec finds its account and the human:
/// the account's home and scratch tree, and the human's home, below which
/// its plan's paths lie and its views are bound.
struct Places<'a> {
    home: &'a Path,
    scratch: &'a Path,
    human: &'a Path,
}

/// A workspace instance's spec (APPLICATIONS.md §C, "The `workspace`
/// kind"): its profile, its runtime, td-agent as the entry with td-txt
/// beside it, the account's home, for `development` the four persistent
/// scratch directories of the account's own tree, and a line per plan
/// entry, by role in the spec's order, naming the human's directory, which
/// td-jail binds from the account's manifest's view of it.
fn instance_spec(
    instance: &Instance,
    entry: &Path,
    program: &Path,
    places: &Places<'_>,
) -> Result<String, String> {
    let mut spec = owned("format=1\n")?;
    let mut put = |parts: &[&str]| {
        parts
            .iter()
            .try_for_each(|part| allocation::push_str(&mut spec, part))
            .map_err(refused)
    };
    if instance.profile == Profile::Development {
        put(&["profile=development\n"])?;
    }
    if let Some(runtime) = instance.runtime {
        put(&["runtime=", runtime, "\n"])?;
    }
    put(&["entry=", spec_line(entry)?, "\n"])?;
    put(&["program=", spec_line(program)?, "\n"])?;
    put(&["home=", spec_line(places.home)?, "\n"])?;
    if instance.profile == Profile::Development {
        for name in accounts::SCRATCH_NAMES {
            let directory = joined(places.scratch, name.as_ref())?;
            put(&["scratch=", spec_line(&directory)?, "\n"])?;
        }
    }
    for role in Role::ALL {
        for entry in instance.plan.iter().filter(|entry| entry.role == *role) {
            let directory = joined(places.human, entry.path.as_ref())?;
            put(&[role.as_str(), "=", spec_line(&directory)?, "\n"])?;
        }
    }
    Ok(spec)
}

/// `path` as one spec line's value.
fn spec_line(path: &Path) -> Result<&str, String> {
    path.to_str()
        .filter(|text| text.len() <= MAX_PATH && !text.contains(['\n', '\0']))
        .ok_or_else(|| message(format_args!("{} cannot be a spec line", path.display())))
}

/// `base` joined with the relative `name`, as `Path::join` joins them, in
/// fallibly grown storage.
fn joined(base: &Path, name: &OsStr) -> Result<PathBuf, String> {
    let (base, name) = (base.as_os_str().as_bytes(), name.as_bytes());
    let slash = !base.is_empty() && !base.ends_with(b"/");
    let mut bytes =
        allocation::with_capacity(base.len() + usize::from(slash) + name.len()).map_err(refused)?;
    allocation::extend_from_slice(&mut bytes, base).map_err(refused)?;
    if slash {
        allocation::push(&mut bytes, b'/').map_err(refused)?;
    }
    allocation::extend_from_slice(&mut bytes, name).map_err(refused)?;
    Ok(PathBuf::from(OsString::from_vec(bytes)))
}

/// `/proc/self/fd/FD` for an open `file`.
fn fd_path(file: &impl AsRawFd) -> Result<PathBuf, String> {
    text(format_args!("/proc/self/fd/{}", file.as_raw_fd())).map(PathBuf::from)
}

/// Launch `id`'s spec for `uid` below root's spec tree: in the account's
/// directory there, root's, of the account's group, exactly mode 0710, so
/// td-jail reaches its spec without listing.
fn spec_path(uid: u32, id: u64) -> Result<PathBuf, String> {
    specs().spec(uid, id)
}

impl SpecTree<'_> {
    fn spec(&self, uid: u32, id: u64) -> Result<PathBuf, String> {
        let account = joined(self.root, text(format_args!("{uid}"))?.as_ref())?;
        joined(&account, text(format_args!("{id}"))?.as_ref())
    }

    /// Writes `text` as launch `id`'s spec for `uid`: a new single-link
    /// file, the tree owner's and the account's group's, mode 0640,
    /// removed again when any step after its creation fails.
    fn write(&self, uid: u32, id: u64, text: &str) -> Result<PathBuf, String> {
        let root = owned_directory(self.root, self.owner)?;
        let directory = grant_views::account_directory_of(&root, uid, self.owner.0)?;
        let name = crate::text(format_args!("{id}"))?;
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(O_NOFOLLOW)
            .open(pinned(&directory, &name))
            .map_err(|e| message(format_args!("create the workspace spec: {e}")))?;
        let written = std::os::unix::fs::fchown(&file, Some(self.owner.0), Some(uid))
            .and_then(|()| file.set_permissions(fs::Permissions::from_mode(0o640)))
            .and_then(|()| file.write_all(text.as_bytes()))
            .map_err(|e| message(format_args!("write the workspace spec: {e}")));
        if let Err(why) = written {
            let _ = fs::remove_file(pinned(&directory, &name));
            return Err(why);
        }
        self.spec(uid, id)
    }

    /// The tree, emptied: a service that starts again has no launch left,
    /// since every jail ends with the service that started it. It and
    /// every directory on its way from `base` are the owner's, with no
    /// other writer; the tree is exactly mode 0755.
    fn prepare(&self) -> Result<(), String> {
        let parent = self.root.parent().ok_or("the spec tree has no parent")?;
        ensure_parents(self.base, parent, self.owner)?;
        match fs::remove_dir_all(self.root) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(message(format_args!("clear the workspace spec tree: {e}"))),
        }
        fs::DirBuilder::new()
            .mode(0o755)
            .create(self.root)
            .map_err(|e| message(format_args!("create the workspace spec tree: {e}")))?;
        let tree = owned_directory(self.root, self.owner)?;
        tree.set_permissions(fs::Permissions::from_mode(0o755))
            .map_err(|e| message(format_args!("{e}")))
    }
}

/// The directory at `path`, reached without following a link at its last
/// name, `owner`'s, with no other writer.
fn owned_directory(path: &Path, owner: (u32, u32)) -> Result<fs::File, String> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(O_NOFOLLOW | O_DIRECTORY)
        .open(path)
        .map_err(|e| message(format_args!("open {}: {e}", path.display())))?;
    let metadata = file.metadata().map_err(|e| message(format_args!("{e}")))?;
    if !metadata.is_dir()
        || (metadata.uid(), metadata.gid()) != owner
        || metadata.mode() & 0o022 != 0
    {
        return Err(message(format_args!(
            "workspace spec tree {} is not root's alone",
            path.display()
        )));
    }
    Ok(file)
}

/// Every directory from `base` down to `through`, made when absent as the
/// other intakes make theirs, `owner`'s with no other writer: `/run`,
/// `/run/td-authd` and the primary human's directory below it.
fn ensure_parents(base: &Path, through: &Path, owner: (u32, u32)) -> Result<(), String> {
    if !through.starts_with(base) {
        return Err("the workspace intake lies outside its base".into());
    }
    // From `base` down: the ancestors below `through`'s depth counted
    // first, each one taken again by its depth, so nothing is collected.
    let depth = through
        .ancestors()
        .take_while(|path| path.starts_with(base))
        .count();
    for below in (0..depth).rev() {
        let Some(path) = through.ancestors().nth(below) else {
            continue;
        };
        match fs::DirBuilder::new().mode(0o755).create(path) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(message(format_args!("{e}"))),
        }
        let metadata = fs::symlink_metadata(path).map_err(|e| message(format_args!("{e}")))?;
        if !metadata.is_dir()
            || (metadata.uid(), metadata.gid()) != owner
            || metadata.mode() & 0o7022 != 0
        {
            return Err(message(format_args!(
                "workspace intake directory {} is not protected",
                path.display()
            )));
        }
    }
    Ok(())
}

/// A declared runtime the request names must be installed: its store's
/// `current` below `store` exists. td-jail holds it at the launch and
/// remains the authority; this only refuses, before the answer, what the
/// human can mend, since a failure after it reaches only root's log.
fn require_runtime(store: &Path, runtime: Option<&str>) -> Result<(), String> {
    let Some(runtime) = runtime else {
        return Ok(());
    };
    let current = joined(
        &joined(store, runtime.as_ref())?,
        flathub_app::CURRENT.as_ref(),
    )?;
    match fs::symlink_metadata(current) {
        Ok(_) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            let installers = Installers(runtime);
            Err(if installers.none() {
                message(format_args!(
                    "the runtime {runtime} is not installed, and no application installs it yet"
                ))
            } else {
                message(format_args!(
                    "the runtime {runtime} is not installed; install an application that uses it ({installers}) in td-software"
                ))
            })
        }
        Err(e) => Err(message(format_args!("inspect the runtime {runtime}: {e}"))),
    }
}

/// The declared applications that install the runtime it names, written
/// as a list separated by commas without collecting them.
struct Installers<'a>(&'a str);

impl Installers<'_> {
    fn names(&self) -> impl Iterator<Item = &'static str> + '_ {
        flathub_app::FLATHUB_SOURCES
            .iter()
            .filter(|source| source.runtime.name == self.0)
            .map(|source| source.name)
    }

    fn none(&self) -> bool {
        self.names().next().is_none()
    }
}

impl fmt::Display for Installers<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, name) in self.names().enumerate() {
            if index > 0 {
                f.write_str(", ")?;
            }
            f.write_str(name)?;
        }
        Ok(())
    }
}

/// The credential helper's command: td-login switches to the account and
/// places it in its delegation, and `workspace-exec` checks both before it
/// runs td-jail with `command`, behind a sandbox's relay of `relay`, or
/// as a workspace instance's entry's arguments.
fn helper(
    uid: u32,
    launcher: u32,
    spec: &Path,
    relay: Option<(Mode, &str)>,
    command: &[String],
) -> Command {
    let mut helper = Command::new("/bin/td-login");
    helper
        .args(["exec-service-as", &format!("tdw{uid}"), "--"])
        .args([
            "/bin/td-authd",
            "workspace-exec",
            &uid.to_string(),
            &launcher.to_string(),
        ])
        .arg(spec);
    if let Some((mode, term)) = relay {
        helper.args([mode.as_str(), term]);
    }
    helper.arg("--").args(command);
    helper
}

/// A copy of `words` in fallibly grown storage.
fn copied(words: &[String]) -> Result<Vec<String>, String> {
    let mut copy = allocation::with_capacity(words.len()).map_err(refused)?;
    for word in words {
        allocation::push(&mut copy, owned(word)?).map_err(refused)?;
    }
    Ok(copy)
}

/// What `workspace-exec` runs: td-jail's workspace kind, tied to root's
/// service as its launcher, with the root-owned spec, and either
/// td-sandbox's relay running `command` through a relay of `mode` of type
/// `term`, or for a workspace instance its entry with `command` as its
/// arguments.
struct Exec {
    uid: u32,
    launcher: u32,
    spec: PathBuf,
    relay: Option<(Mode, String)>,
    command: Vec<String>,
}

impl Exec {
    /// `UID LAUNCHER SPEC [MODE TERM] -- WORD...`: a sandbox's or
    /// workspace's uid, root's service's pid, that uid's spec below root's
    /// tree, a sandbox relay's mode and terminal type, and a command or a
    /// workspace instance's entry's arguments.
    fn parse(arguments: &[String]) -> Result<Self, String> {
        const USAGE: &str = "workspace-exec requires UID LAUNCHER SPEC [MODE TERM] -- WORD...";
        let [uid, launcher, spec, rest @ ..] = arguments else {
            return Err(USAGE.into());
        };
        let (relay, separator, command) = match rest {
            [separator, command @ ..] if separator == "--" => (None, separator, command),
            [mode, term, separator, command @ ..] => {
                let mode = Mode::parse(mode).ok_or("invalid workspace relay mode")?;
                if !valid_term(term) {
                    return Err("invalid workspace terminal type".into());
                }
                (Some((mode, owned(term)?)), separator, command)
            }
            _ => return Err(USAGE.into()),
        };
        let uid = application::workload_uid(uid)?;
        let launcher = launcher
            .parse::<u32>()
            .ok()
            .filter(|pid| *pid > 0 && canonical_decimal(launcher))
            .ok_or("invalid workspace launcher")?;
        let id = spec
            .strip_prefix(WORKSPACE_SPECS)
            .and_then(|rest| rest.strip_prefix('/'))
            .and_then(|rest| rest.split_once('/'))
            .filter(|(account, _)| {
                canonical_decimal(account) && account.parse::<u32>().is_ok_and(|n| n == uid)
            })
            .and_then(|(_, id)| id.parse::<u64>().ok().filter(|_| canonical_decimal(id)))
            .ok_or("the workspace spec is not root's for this account")?;
        if separator != "--"
            || command.first().is_none_or(String::is_empty)
            || command.len() > MAX_ARGUMENTS
            || command.iter().any(|word| word.contains('\0'))
        {
            return Err("invalid workspace command".into());
        }
        Ok(Self {
            uid,
            launcher,
            spec: spec_path(uid, id)?,
            relay,
            command: copied(command)?,
        })
    }

    fn jail(&self) -> Command {
        let mut command = Command::new("/bin/td-jail");
        command
            .args(["--workspace", &self.launcher.to_string()])
            .arg(&self.spec);
        if let Some((mode, term)) = &self.relay {
            command.args(["--inside", mode.as_str(), term]);
        }
        command.args(&self.command).env_clear().current_dir("/");
        command
    }
}

/// `workspace-exec`, run as the sandbox's or workspace's account: its
/// credentials and session leaf checked, then td-jail in its place, the
/// connection still its standard input and output.
pub(crate) fn exec(arguments: &[String]) -> Result<(), String> {
    let launch = Exec::parse(arguments)?;
    require_form(&accounts::load()?, &launch)?;
    application::require_process(
        launch.uid,
        &application::bounded("/proc/self/status")?,
        &application::bounded("/proc/self/cgroup")?,
    )?;
    Err(message(format_args!(
        "exec td-jail: {}",
        launch.jail().exec()
    )))
}

/// A relay is a sandbox's, and only a workspace slot's instance has none:
/// `launch`'s uid must be a ledger row of the kind its form names.
fn require_form(ledger: &accounts::Ledger, launch: &Exec) -> Result<(), String> {
    let kind = match launch.relay {
        Some(_) => Kind::Sandbox,
        None => Kind::Workspace,
    };
    match ledger.name(kind, launch.uid) {
        Some(_) => Ok(()),
        None => Err(message(format_args!(
            "workspace-exec's form is a {}'s, and {} is none",
            kind.as_str(),
            launch.uid
        ))),
    }
}

/// `workspace-empty UID`, run as a deleted sandbox's or workspace's
/// account: its credentials and session leaf checked, then everything in
/// its home removed, the home itself kept, and the same in each directory
/// of its scratch tree, a workspace's. Root never walks a tree the account
/// controls; the account removes only what it may.
pub(crate) fn empty(arguments: &[String]) -> Result<(), String> {
    let [uid] = arguments else {
        return Err("workspace-empty requires UID".into());
    };
    let uid = application::workload_uid(uid)?;
    application::require_process(
        uid,
        &application::bounded("/proc/self/status")?,
        &application::bounded("/proc/self/cgroup")?,
    )?;
    empty_home(&Kind::Sandbox.home(uid), uid)?;
    empty_scratch(Path::new(accounts::DIRECTORY), uid)
}

/// Empties each directory of the account `uid`'s scratch tree below
/// `state`, as its home is emptied, keeping the directories, which only
/// root may add or remove, at the mode 1777 root gave them: the tree a
/// workspace's account holds. A sandbox's account has none, and an absent
/// tree has nothing to empty.
fn empty_scratch(state: &Path, uid: u32) -> Result<(), String> {
    let trees = joined(state, accounts::SCRATCH.as_ref())?;
    let tree = joined(&trees, text(format_args!("{uid}"))?.as_ref())?;
    match fs::symlink_metadata(&tree) {
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(message(format_args!("inspect {}: {e}", tree.display()))),
    }
    for name in accounts::SCRATCH_NAMES {
        let directory = joined(&tree, name.as_ref())?;
        empty_home(&directory, uid)?;
        // The walk opened it to its owner alone; it is a `/tmp` again,
        // through a handle that follows no link.
        let handle = OpenOptions::new()
            .read(true)
            .custom_flags(O_PATH | O_NOFOLLOW | O_DIRECTORY)
            .open(&directory)
            .map_err(|e| message(format_args!("open {}: {e}", directory.display())))?;
        fs::set_permissions(fd_path(&handle)?, fs::Permissions::from_mode(0o1777))
            .map_err(|e| message(format_args!("reopen {}: {e}", directory.display())))?;
    }
    Ok(())
}

/// The command root runs to empty the home of the account `uid`: as the
/// account, through the credential helper that places it in its
/// delegation.
fn emptier(uid: u32) -> Command {
    let mut command = Command::new("/bin/td-login");
    command
        .args(["exec-service-as", &format!("tdw{uid}"), "--"])
        .args(["/bin/td-authd", "workspace-empty", &uid.to_string()]);
    command
}

/// One directory being emptied: its descriptor, its listing, and its name
/// in the directory above, none for the home.
struct Level {
    directory: fs::File,
    entries: fs::ReadDir,
    name: Option<std::ffi::OsString>,
}

impl Level {
    fn open(path: &Path, name: Option<std::ffi::OsString>) -> Result<Self, String> {
        // A handle first, which needs no permission on the directory, so a
        // mode its owner narrowed is restored through it before it is read.
        let handle = OpenOptions::new()
            .read(true)
            .custom_flags(O_PATH | O_NOFOLLOW | O_DIRECTORY)
            .open(path)
            .map_err(|e| message(format_args!("open {}: {e}", path.display())))?;
        let pinned = fd_path(&handle)?;
        fs::set_permissions(&pinned, fs::Permissions::from_mode(0o700))
            .map_err(|e| message(format_args!("open {} to its owner: {e}", path.display())))?;
        let directory = OpenOptions::new()
            .read(true)
            .custom_flags(O_DIRECTORY)
            .open(&pinned)
            .map_err(|e| message(format_args!("open {}: {e}", path.display())))?;
        drop(handle);
        let entries = fs::read_dir(fd_path(&directory)?)
            .map_err(|e| message(format_args!("list {}: {e}", path.display())))?;
        Ok(Self {
            directory,
            entries,
            name,
        })
    }

    fn at(&self, name: &OsStr) -> Result<PathBuf, String> {
        joined(&fd_path(&self.directory)?, name)
    }
}

/// Removes everything below `home`, which must be a directory `uid` owns,
/// reached without following a link: each name by its parent's
/// descriptor, a link removed and never followed, a directory opened to
/// its owner and emptied before it goes, to at most `EMPTY_DEPTH` levels.
/// The home is listed again after each walk, at most `EMPTY_PASSES`
/// times, until it is empty.
fn empty_home(home: &Path, uid: u32) -> Result<(), String> {
    for _ in 0..EMPTY_PASSES {
        empty_pass(home, uid)?;
        if Level::open(home, None)?.entries.next().is_none() {
            return Ok(());
        }
    }
    Err(message(format_args!(
        "{} was not empty after {EMPTY_PASSES} walks",
        home.display()
    )))
}

fn empty_pass(home: &Path, uid: u32) -> Result<(), String> {
    let top = Level::open(home, None)?;
    let owner = top
        .directory
        .metadata()
        .map_err(|e| message(format_args!("{e}")))?
        .uid();
    if owner != uid {
        return Err(message(format_args!(
            "{} is not the account's",
            home.display()
        )));
    }
    let mut levels = allocation::with_capacity(EMPTY_DEPTH).map_err(refused)?;
    allocation::push(&mut levels, top).map_err(refused)?;
    while let Some(level) = levels.last_mut() {
        let Some(entry) = level.entries.next() else {
            let done = levels.pop();
            if let (Some(done), Some(parent)) = (done, levels.last()) {
                if let Some(name) = &done.name {
                    let path = parent.at(name)?;
                    drop(done);
                    fs::remove_dir(&path)
                        .map_err(|e| message(format_args!("remove a directory: {e}")))?;
                }
            }
            continue;
        };
        let name = entry
            .map_err(|e| message(format_args!("list the home: {e}")))?
            .file_name();
        let path = level.at(&name)?;
        let metadata =
            fs::symlink_metadata(&path).map_err(|e| message(format_args!("inspect: {e}")))?;
        if metadata.is_dir() {
            if levels.len() >= EMPTY_DEPTH {
                return Err(message(format_args!(
                    "the home nests deeper than {EMPTY_DEPTH} directories; empty it from its shell"
                )));
            }
            allocation::push(&mut levels, Level::open(&path, Some(name))?).map_err(refused)?;
        } else {
            fs::remove_file(&path).map_err(|e| message(format_args!("remove a file: {e}")))?;
        }
    }
    Ok(())
}

/// The account `uid`'s home, opened by root without following a link and
/// required to be the account's: root reads only its top-level listing
/// and one fixed name in it, and walks nothing.
fn owned_home(home: &Path, uid: u32) -> Result<fs::File, String> {
    let directory = OpenOptions::new()
        .read(true)
        .custom_flags(O_NOFOLLOW | O_DIRECTORY)
        .open(home)
        .map_err(|e| message(format_args!("open {}: {e}", home.display())))?;
    let owner = directory
        .metadata()
        .map_err(|e| message(format_args!("{e}")))?
        .uid();
    if owner != uid {
        return Err(message(format_args!(
            "{} is not the account's",
            home.display()
        )));
    }
    Ok(directory)
}

fn mark_path(directory: &fs::File) -> Result<PathBuf, String> {
    joined(&fd_path(directory)?, DELETED_MARK.as_ref())
}

/// Whether `path` is root's deletion mark: a regular file `marker`, root
/// on a running system, owns, never a link or anything the account made.
fn is_mark(path: &Path, marker: u32) -> bool {
    fs::symlink_metadata(path).is_ok_and(|meta| meta.is_file() && meta.uid() == marker)
}

/// Whether the account `uid`'s home bears the deletion mark `marker` made.
fn marked(home: &Path, uid: u32, marker: u32) -> Result<bool, String> {
    Ok(is_mark(&mark_path(&owned_home(home, uid)?)?, marker))
}

/// Whether the account `uid`'s home holds anything but the deletion mark,
/// from its top-level listing alone.
fn holds_files(home: &Path, uid: u32, marker: u32) -> Result<bool, String> {
    let directory = owned_home(home, uid)?;
    let listing = fd_path(&directory)?;
    for entry in
        fs::read_dir(&listing).map_err(|e| message(format_args!("list {}: {e}", home.display())))?
    {
        let name = entry.map_err(|e| message(format_args!("{e}")))?.file_name();
        if name != DELETED_MARK || !is_mark(&joined(&listing, &name)?, marker) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Marks the emptied home of the deleted account `uid`: a new file, never
/// one followed through a link.
fn mark_deleted(home: &Path, uid: u32, marker: u32) -> Result<(), String> {
    let directory = owned_home(home, uid)?;
    let path = mark_path(&directory)?;
    match OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o400)
        .custom_flags(O_NOFOLLOW)
        .open(&path)
    {
        Ok(file) => file.sync_all().map_err(|e| message(format_args!("{e}")))?,
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
        Err(e) => {
            return Err(message(format_args!(
                "mark {} deleted: {e}",
                home.display()
            )))
        }
    }
    if !is_mark(&path, marker) {
        return Err(message(format_args!(
            "{} holds something in place of root's mark",
            home.display()
        )));
    }
    Ok(())
}

/// Takes root's mark from the home of the account `uid` created again.
fn unmark(home: &Path, uid: u32, marker: u32) -> Result<(), String> {
    let directory = owned_home(home, uid)?;
    let path = mark_path(&directory)?;
    if is_mark(&path, marker) {
        fs::remove_file(&path)
            .map_err(|e| message(format_args!("unmark {}: {e}", home.display())))?;
    }
    Ok(())
}

/// Whether any directory of the account `uid`'s scratch tree below `state`
/// holds anything, from each one's listing alone: root opens each without
/// following a link, requires it to be the account's, and walks nothing.
/// An absent tree holds nothing.
fn scratch_holds(state: &Path, uid: u32) -> Result<bool, String> {
    let trees = joined(state, accounts::SCRATCH.as_ref())?;
    let tree = joined(&trees, text(format_args!("{uid}"))?.as_ref())?;
    match fs::symlink_metadata(&tree) {
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(message(format_args!("inspect {}: {e}", tree.display()))),
    }
    for name in accounts::SCRATCH_NAMES {
        let directory = owned_home(&joined(&tree, name.as_ref())?, uid)?;
        let mut listing = fs::read_dir(fd_path(&directory)?)
            .map_err(|e| message(format_args!("list {}: {e}", tree.display())))?;
        if listing.next().is_some() {
            return Ok(true);
        }
    }
    Ok(false)
}

/// The account `uid`'s delegation below the unified hierarchy at
/// `cgroups`, opened without following a link; none when it is absent,
/// as before a boot's first launch or preparation.
fn delegation_of(cgroups: &Path, uid: u32) -> Result<Option<fs::File>, String> {
    let name = accounts::delegation(uid).ok_or("the account has no delegation")?;
    let path = joined(cgroups, text(format_args!("{name}"))?.as_ref())?;
    match OpenOptions::new()
        .read(true)
        .custom_flags(O_NOFOLLOW | O_DIRECTORY)
        .open(&path)
    {
        Ok(directory) => Ok(Some(directory)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(message(format_args!("open {}: {e}", path.display()))),
    }
}

/// Whether the delegation `directory` holds a process, below it included,
/// by its `cgroup.events`.
fn populated(directory: &fs::File) -> Result<bool, String> {
    let mut events = [0u8; MAX_EVENTS as usize];
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(O_NOFOLLOW)
        .open(pinned(directory, "cgroup.events"))
        .map_err(|e| message(format_args!("open the delegation's events: {e}")))?;
    let mut filled = 0;
    while let Some(free) = events.get_mut(filled..).filter(|free| !free.is_empty()) {
        match file.read(free) {
            Ok(0) => break,
            Ok(count) => filled += count,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(message(format_args!("read the delegation's events: {e}"))),
        }
    }
    let read = events.get(..filled).unwrap_or_default();
    read.split(|byte| *byte == b'\n')
        .find_map(|line| line.strip_prefix(b"populated "))
        .map(|value| value == b"1")
        .ok_or_else(|| "the delegation's events name no population".into())
}

/// Ends every process of the account `uid`'s delegation below `cgroups`,
/// its own subtree included, and waits at most `deadline` for it to hold
/// none: through the delegation's `cgroup.kill`, root's, which the account
/// cannot escape, since it can move no process out of its delegation. An
/// absent or empty delegation has nothing to end.
fn end_processes(cgroups: &Path, uid: u32, deadline: Duration) -> Result<(), String> {
    let Some(directory) = delegation_of(cgroups, uid)? else {
        return Ok(());
    };
    if !populated(&directory)? {
        return Ok(());
    }
    OpenOptions::new()
        .write(true)
        .custom_flags(O_NOFOLLOW)
        .open(pinned(&directory, "cgroup.kill"))
        .and_then(|mut kill| kill.write_all(b"1"))
        .map_err(|e| message(format_args!("kill the account's processes: {e}")))?;
    let until = Instant::now() + deadline;
    while populated(&directory)? {
        if Instant::now() >= until {
            return Err(message(format_args!(
                "the account's processes did not end within {deadline:?}; try again"
            )));
        }
        std::thread::sleep(STOP_POLL);
    }
    Ok(())
}

/// What root finds of a sandbox or workspace slot the ledger holds: its
/// account, whether a deletion marked its home, and whether it holds files
/// in its home or any grant row, and for a workspace anything in its
/// scratch tree or any running process.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct State {
    uid: u32,
    deleted: bool,
    held: bool,
}

/// The uid of the `kind` account `name`'s ledger row: of that kind alone,
/// never an application's or another kind's of the same name, and never
/// one this adds.
fn account_uid(ledger: &accounts::Ledger, kind: Kind, name: &str) -> Result<u32, String> {
    ledger
        .uid_of(kind, name)
        .ok_or_else(|| message(format_args!("{} {name} has no account", kind.as_str())))
}

/// A launch prepared for its account.
struct Prepared {
    uid: u32,
    spec: PathBuf,
    /// A sandbox's relay mode and terminal type; none for a workspace
    /// instance, whose entry speaks on the channel itself.
    relay: Option<(Mode, String)>,
    command: Vec<String>,
    /// A workspace instance's launch id, under which its plan holds its
    /// account's views until it ends.
    held: Option<u64>,
}

/// What root does for each verb: `Root` on a running system, a stand-in
/// in the tests.
trait Serve {
    /// Admits and prepares a sandbox's launch before its answer.
    fn prepare(&self, name: &str, launch: &Launch, id: u64) -> Result<Prepared, String>;
    /// Admits and prepares a workspace slot's instance before its answer.
    fn instance(&self, slot: &str, instance: &Instance, id: u64) -> Result<Prepared, String>;
    /// Runs a prepared launch with `stream` as the jail's channel and
    /// waits for it, returning what td-jail last said.
    fn run(&self, stream: UnixStream, prepared: &Prepared) -> Result<(ExitStatus, String), String>;
    /// Gives back what a prepared launch held once it has ended, `failed`
    /// or not: a workspace instance's plan, whose views no other running
    /// launch names are released.
    fn release(&self, prepared: &Prepared, failed: bool);
    /// Whether a deletion marked the sandbox's or slot's home.
    fn deleted(&self, kind: Kind, name: &str) -> Result<bool, String>;
    /// The sandbox's or slot's state, none without a ledger row.
    fn state(&self, kind: Kind, name: &str) -> Result<Option<State>, String>;
    fn create(&self, kind: Kind, name: &str) -> Result<u32, String>;
    fn delete(&self, kind: Kind, name: &str) -> Result<(), String>;
    /// The sandbox's account, which a listing names.
    fn account(&self, name: &str) -> Result<u32, String>;
    fn grants(&self, request: &grant_requests::Request) -> Result<Written, Failed>;
}

/// What the service keeps of workspace accounts while it runs: the plans
/// their running instances hold, by account and launch, whose union its
/// views are; and which accounts td-firstboot prepared since it started.
#[derive(Default)]
struct Instances {
    plans: Mutex<Plans>,
    prepared: Mutex<HashMap<u32, ()>>,
}

/// Each account's running instances' plans, by launch id.
type Plans = HashMap<u32, Vec<(u64, Vec<Entry>)>>;

impl Instances {
    fn plans(&self) -> std::sync::MutexGuard<'_, Plans> {
        self.plans.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn prepared(&self) -> std::sync::MutexGuard<'_, HashMap<u32, ()>> {
        self.prepared.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Prepares the account `uid` through `prepare` unless it was prepared
    /// since the service started: once a boot, since its delegation lasts
    /// until shutdown, and again after `forget`. Preparation may run twice
    /// for two first launches at once, which is harmless.
    fn prepare_once(
        &self,
        uid: u32,
        prepare: impl FnOnce() -> Result<u32, String>,
    ) -> Result<(), String> {
        if self.prepared().contains_key(&uid) {
            return Ok(());
        }
        if prepare()? != uid {
            return Err("the workspace's account is not the ledger's".into());
        }
        allocation::insert(&mut self.prepared(), uid, ()).map_err(refused)?;
        Ok(())
    }

    /// The account `uid` is prepared again at its next launch: a creation
    /// or a deletion changed its home or its delegation.
    fn forget(&self, uid: u32) {
        self.prepared().remove(&uid);
    }

    /// Holds launch `id`'s `plan` among the account `uid`'s: refused when
    /// it names a directory a running instance holds in another role, so
    /// at another access or under another mount chain (a `repository`'s
    /// read-only hooks beside another's writable tree of them), or nested
    /// with one, or takes the account past `MAX_PLAN` views. Only one
    /// directory in one role is shared.
    fn hold(&self, uid: u32, id: u64, plan: &[Entry]) -> Result<(), String> {
        let mut directories = allocation::with_capacity(plan.len()).map_err(refused)?;
        for entry in plan {
            let entry = Entry {
                role: entry.role,
                path: owned(&entry.path)?,
            };
            allocation::push(&mut directories, entry).map_err(refused)?;
        }
        let mut plans = self.plans();
        let held = plans.get(&uid).map_or(&[][..], Vec::as_slice);
        let all = || held.iter().flat_map(|(_, held)| held);
        // The account's views now: each directory held once.
        let mut views = all()
            .enumerate()
            .filter(|(at, directory)| !all().take(*at).any(|other| other == *directory))
            .count();
        for directory in &directories {
            let mut shared = false;
            for other in all() {
                if other == directory {
                    shared = true;
                } else if workload_grants::overlapping(&other.path, &directory.path) {
                    return Err(message(format_args!(
                        "a running instance of this workspace holds {} otherwise; end it first",
                        other.path
                    )));
                }
            }
            if !shared {
                views = views.saturating_add(1);
            }
        }
        if views > MAX_PLAN {
            return Err(message(format_args!(
                "this workspace's running instances would hold more than {MAX_PLAN} directories"
            )));
        }
        match plans.get_mut(&uid) {
            Some(held) => allocation::push(held, (id, directories)).map_err(refused),
            None => {
                let mut held = allocation::with_capacity(1).map_err(refused)?;
                allocation::push(&mut held, (id, directories)).map_err(refused)?;
                allocation::insert(&mut plans, uid, held).map_err(refused)?;
                Ok(())
            }
        }
    }

    /// Takes launch `id`'s plan from the account `uid`'s.
    fn drop_plan(&self, uid: u32, id: u64) {
        let mut plans = self.plans();
        if let Some(held) = plans.get_mut(&uid) {
            held.retain(|(launch, _)| *launch != id);
            if held.is_empty() {
                plans.remove(&uid);
            }
        }
    }

    /// The directories the account `uid`'s running instances hold, each
    /// once at its role's access: what its views are published from.
    fn union(&self, uid: u32) -> Result<Vec<Directory>, String> {
        let plans = self.plans();
        let mut union: Vec<Directory> = Vec::new();
        for entry in plans
            .get(&uid)
            .into_iter()
            .flatten()
            .flat_map(|(_, held)| held)
        {
            if !union.iter().any(|directory| directory.path == entry.path) {
                let copy = Directory {
                    access: entry.role.access(),
                    path: owned(&entry.path)?,
                };
                allocation::push(&mut union, copy).map_err(refused)?;
            }
        }
        Ok(union)
    }
}

/// Root's own work, with what the service keeps of workspace accounts.
struct Root<'a> {
    instances: &'a Instances,
}

impl Root<'_> {
    /// Releases launch `id`'s hold on the workspace account `uid`'s views:
    /// they are published again from the plans its other running instances
    /// hold, so a view one of them names stays and the rest go. A failure
    /// leaves the views until the account's next launch or shutdown.
    fn release_views(&self, uid: u32, id: u64) {
        self.instances.drop_plan(uid, id);
        if let Err(why) = grant_views::publish_plan(uid, || self.instances.union(uid)) {
            log(format_args!("release the views of tdw{uid}: {why}"));
        }
    }

    /// The instance's views published with its account's other running
    /// instances', each of its plan's directories required among them, and
    /// its spec written.
    fn launch_instance(&self, uid: u32, instance: &Instance, id: u64) -> Result<Prepared, String> {
        let manifest = grant_views::publish_plan(uid, || self.instances.union(uid))?;
        for entry in &instance.plan {
            if !manifest
                .views()
                .iter()
                .any(|view| view.path == entry.path && view.access == entry.role.access())
            {
                return Err(message(format_args!(
                    "the plan's {} {} has no view: it is absent or not the human's own",
                    entry.role.as_str(),
                    entry.path
                )));
            }
        }
        let human = primary_account::load()
            .map_err(|e| message(format_args!("{e}")))?
            .persistent_home();
        let canonical =
            |path: &str| fs::canonicalize(path).map_err(|e| message(format_args!("{path}: {e}")));
        let scratch = joined(
            &joined(Path::new(accounts::DIRECTORY), accounts::SCRATCH.as_ref())?,
            text(format_args!("{uid}"))?.as_ref(),
        )?;
        let places = Places {
            home: &Kind::Workspace.home(uid),
            scratch: &scratch,
            human: &human,
        };
        let text = instance_spec(instance, &canonical(AGENT)?, &canonical(TXT)?, &places)?;
        let spec = specs().write(uid, id, &text)?;
        Ok(Prepared {
            uid,
            spec,
            relay: None,
            command: copied(&instance.arguments)?,
            held: Some(id),
        })
    }
}

impl Serve for Root<'_> {
    /// The sandbox must hold a ledger row, which this never adds;
    /// td-firstboot prepares that row's account again under its lock,
    /// refusing when the row is gone; a named runtime must be installed;
    /// its views are published and its spec written with the network its
    /// grant row names. All of it before the answer, so the requester
    /// hears what refused it.
    fn prepare(&self, name: &str, launch: &Launch, id: u64) -> Result<Prepared, String> {
        let uid = account_uid(&accounts::load()?, Kind::Sandbox, name)?;
        require_runtime(Path::new(flathub_app::RUNTIME_STORE), launch.runtime)?;
        let prepared =
            application::admit(&application::PREPARE_SANDBOX, primary_account::UID, name)?;
        if prepared != uid {
            return Err("the sandbox's account is not the ledger's".into());
        }
        let table = grant_views::publish(Kind::Sandbox, name, uid)?;
        let network = table.network(Kind::Sandbox, name);
        let entry = fs::canonicalize(ENTRY).map_err(|e| message(format_args!("{ENTRY}: {e}")))?;
        let command = match &launch.command {
            Some(command) => copied(command)?,
            None => {
                let shell =
                    fs::canonicalize(SHELL).map_err(|e| message(format_args!("{SHELL}: {e}")))?;
                let shell = shell
                    .to_str()
                    .ok_or_else(|| message(format_args!("{} is not UTF-8", shell.display())))?;
                let mut words = allocation::with_capacity(2).map_err(refused)?;
                allocation::push(&mut words, owned(shell)?).map_err(refused)?;
                allocation::push(&mut words, owned("-i")?).map_err(refused)?;
                words
            }
        };
        let text = spec_text(network, launch.runtime, &entry, &Kind::Sandbox.home(uid))?;
        let spec = specs().write(uid, id, &text)?;
        Ok(Prepared {
            uid,
            spec,
            relay: Some((launch.mode, owned(&launch.term)?)),
            command,
            held: None,
        })
    }

    /// The slot must hold a workspace ledger row, which this never adds; a
    /// named runtime must be installed; td-firstboot prepares the account
    /// once a boot (`Instances::prepare_once`); the plan is held among its
    /// account's, its views published and its spec written. All of it
    /// before the answer, so the requester hears what refused it; a
    /// refusal after the plan is held gives it back.
    fn instance(&self, slot: &str, instance: &Instance, id: u64) -> Result<Prepared, String> {
        let uid = account_uid(&accounts::load()?, Kind::Workspace, slot)?;
        require_runtime(Path::new(flathub_app::RUNTIME_STORE), instance.runtime)?;
        self.instances.prepare_once(uid, || {
            application::admit(&application::PREPARE_WORKSPACE, primary_account::UID, slot)
        })?;
        self.instances.hold(uid, id, &instance.plan)?;
        // Given back on any refusal, and on a panic, until it is prepared.
        let hold = PlanHold {
            root: self,
            uid,
            id,
        };
        let launched = self.launch_instance(uid, instance, id)?;
        std::mem::forget(hold);
        Ok(launched)
    }

    /// This thread lives until the jail ends. A workspace connection is
    /// retained for the independent disconnect check; sandbox relays own
    /// their lifetime. The last helper diagnostic comes back bounded.
    fn run(&self, stream: UnixStream, prepared: &Prepared) -> Result<(ExitStatus, String), String> {
        let monitor = if prepared.held.is_some() {
            Some(
                stream
                    .try_clone()
                    .map_err(|e| message(format_args!("{e}")))?,
            )
        } else {
            None
        };
        let input = stream
            .try_clone()
            .map_err(|e| message(format_args!("{e}")))?;
        let (mut said, said_end) = UnixStream::pair().map_err(|e| message(format_args!("{e}")))?;
        let relay = prepared
            .relay
            .as_ref()
            .map(|(mode, term)| (*mode, term.as_str()));
        let mut command = helper(
            prepared.uid,
            std::process::id(),
            &prepared.spec,
            relay,
            &prepared.command,
        );
        command
            .env_clear()
            .current_dir("/")
            .stdin(Stdio::from(OwnedFd::from(input)))
            .stdout(Stdio::from(OwnedFd::from(stream)))
            .stderr(Stdio::from(OwnedFd::from(said_end)));
        let child = command.spawn();
        drop(command);
        let mut child =
            child.map_err(|e| message(format_args!("start the workspace helper: {e}")))?;
        if let Some(monitor) = monitor {
            return supervise_instance(&mut child, &monitor, &mut said);
        }
        let mut tail = Vec::new();
        let mut buffer = [0u8; 4096];
        loop {
            match said.read(&mut buffer) {
                Ok(0) => break,
                Ok(count) => stderr_tail::push(&mut tail, buffer.get(..count).unwrap_or_default()),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }
        let status = child.wait().map_err(|e| message(format_args!("{e}")))?;
        Ok((status, stderr_tail::last_line(&tail).unwrap_or_default()))
    }

    /// A failed instance has its account prepared again at the next: what
    /// failed may be the account's own home or scratch tree, which its
    /// tools can change and a preparation restores.
    fn release(&self, prepared: &Prepared, failed: bool) {
        if let Some(id) = prepared.held {
            self.release_views(prepared.uid, id);
            if failed {
                self.instances.forget(prepared.uid);
            }
        }
    }

    fn deleted(&self, kind: Kind, name: &str) -> Result<bool, String> {
        match accounts::load()?.uid_of(kind, name) {
            Some(uid) => marked(&kind.home(uid), uid, 0),
            None => Ok(false),
        }
    }

    /// A workspace slot is held, too, while its scratch tree holds anything
    /// or its delegation a process: a conversation that ended without its
    /// slot's deletion left them, and the next must not inherit them.
    fn state(&self, kind: Kind, name: &str) -> Result<Option<State>, String> {
        let Some(uid) = accounts::load()?.uid_of(kind, name) else {
            return Ok(None);
        };
        let home = kind.home(uid);
        let held = grant_views::read_table()?.holds_any(kind, name)
            || holds_files(&home, uid, 0)?
            || (kind.scratch()
                && (scratch_holds(Path::new(accounts::DIRECTORY), uid)?
                    || delegation_of(Path::new(CGROUPS), uid)?
                        .map(|directory| populated(&directory))
                        .transpose()?
                        .unwrap_or(false)));
        Ok(Some(State {
            uid,
            deleted: marked(&home, uid, 0)?,
            held,
        }))
    }

    fn create(&self, kind: Kind, name: &str) -> Result<u32, String> {
        let allocate = match kind {
            Kind::Workspace => &application::ALLOCATE_WORKSPACE,
            _ => &application::ALLOCATE_SANDBOX,
        };
        create(
            self.state(kind, name)?,
            || application::admit(allocate, primary_account::UID, name),
            |uid| unmark(&kind.home(uid), uid, 0),
            |uid| self.instances.forget(uid),
        )
    }

    fn delete(&self, kind: Kind, name: &str) -> Result<(), String> {
        let prepare = match kind {
            Kind::Workspace => &application::PREPARE_WORKSPACE,
            _ => &application::PREPARE_SANDBOX,
        };
        delete(
            kind,
            name,
            || Ok(accounts::load()?.uid_of(kind, name)),
            |uid| {
                self.instances.forget(uid);
                end_processes(Path::new(CGROUPS), uid, STOP_DEADLINE)
            },
            |clear| self.grants(clear),
            || application::admit(prepare, primary_account::UID, name),
            empty_as,
            |uid| mark_deleted(&kind.home(uid), uid, 0),
        )
    }

    fn account(&self, name: &str) -> Result<u32, String> {
        account_uid(&accounts::load()?, Kind::Sandbox, name)
    }

    fn grants(&self, request: &grant_requests::Request) -> Result<Written, Failed> {
        grant_requests::perform(primary_account::UID, request)
    }
}

/// A retained socket observes the client's write-half closing even with
/// unread bytes queued. No root reader consumes the tool-host protocol.
fn supervise_instance(
    child: &mut std::process::Child,
    monitor: &UnixStream,
    stderr: &mut UnixStream,
) -> Result<(ExitStatus, String), String> {
    struct Ending<'a>(&'a mut std::process::Child);
    impl Drop for Ending<'_> {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let ending = Ending(child);
    stderr
        .set_nonblocking(true)
        .map_err(|e| message(format_args!("{e}")))?;
    let mut tail = Vec::new();
    let mut buffer = [0u8; 4096];
    loop {
        // A diagnostic flood cannot starve the lifetime check.
        for _ in 0..16 {
            match stderr.read(&mut buffer) {
                Ok(0) => break,
                Ok(n) => stderr_tail::push(&mut tail, buffer.get(..n).unwrap_or_default()),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
        if let Some(status) = ending
            .0
            .try_wait()
            .map_err(|e| message(format_args!("{e}")))?
        {
            return Ok((status, stderr_tail::last_line(&tail).unwrap_or_default()));
        }
        if !crate::sys::channel_open(monitor).map_err(|e| message(format_args!("{e}")))? {
            ending.0.kill().map_err(|e| message(format_args!("{e}")))?;
            let status = ending.0.wait().map_err(|e| message(format_args!("{e}")))?;
            return Ok((status, stderr_tail::last_line(&tail).unwrap_or_default()));
        }
        std::thread::sleep(STOP_POLL);
    }
}

/// Creates the sandbox whose state root found (`state`), never over one
/// that holds files or grants: that is refused as `exists`, for the human
/// to adopt or delete. Otherwise td-firstboot allocates its account, or
/// returns the one the ledger holds, preparing its home (`allocate`), and
/// a deletion's mark is taken from it (`unmark`): a deleted sandbox or
/// slot, its home empty and its rows cleared, is created again fresh, and
/// its account is prepared again at its next launch (`forget`).
fn create(
    state: Option<State>,
    allocate: impl FnOnce() -> Result<u32, String>,
    unmark: impl FnOnce(u32) -> Result<(), String>,
    forget: impl FnOnce(u32),
) -> Result<u32, String> {
    if state.is_some_and(|state| state.held) {
        return Err(EXISTS.into());
    }
    let uid = allocate()?;
    if state.is_some_and(|state| state.uid != uid) {
        return Err("the account is not the ledger's".into());
    }
    forget(uid);
    unmark(uid)?;
    Ok(uid)
}

/// Deletes the `kind` account `name`. When the ledger holds its account
/// (`account`), every process of its delegation is ended first (`stop`),
/// so nothing it left running writes what follows, and it is prepared
/// again at its next launch. Every grant row its name holds is cleared
/// through the table's writer (`clear`); then td-firstboot prepares the
/// account again (`prepare`), restoring its home's mode and its
/// delegation, the account itself empties its home and scratch tree
/// (`empty`), and root marks the emptied home deleted (`mark`), which
/// refuses its launches and grant changes until it is created again. The
/// ledger row stays, and so does the uid.
#[allow(clippy::too_many_arguments)]
fn delete(
    kind: Kind,
    name: &str,
    account: impl FnOnce() -> Result<Option<u32>, String>,
    stop: impl FnOnce(u32) -> Result<(), String>,
    clear: impl FnOnce(&grant_requests::Request) -> Result<Written, Failed>,
    prepare: impl FnOnce() -> Result<u32, String>,
    empty: impl FnOnce(u32) -> Result<(), String>,
    mark: impl FnOnce(u32) -> Result<(), String>,
) -> Result<(), String> {
    let account = account()?;
    if let Some(uid) = account {
        stop(uid)?;
    }
    let request = grant_requests::Request {
        verb: grant_requests::Verb::Clear,
        kind,
        name: owned(name)?,
    };
    match clear(&request) {
        Ok(Written::Cleared(count)) => log(format_args!(
            "{} {name}: deleting; {count} grant rows revoked",
            kind.as_str()
        )),
        Ok(_) => return Err("the grant table's writer answered a clear amiss".into()),
        Err(failed) => return Err(failure(failed)),
    }
    let Some(uid) = account else {
        return Ok(());
    };
    if prepare()? != uid {
        return Err("the account is not the ledger's".into());
    }
    empty(uid)?;
    mark(uid)
}

/// Has the account `uid` empty its own home, and its scratch tree's
/// directories when it has one, and waits for it: its emptying ends when
/// the trees it owns do. Its deletion ended every process of its
/// delegation first, a workspace's background jobs among them.
fn empty_as(uid: u32) -> Result<(), String> {
    let mut command = emptier(uid);
    command
        .env_clear()
        .current_dir("/")
        .stdin(Stdio::null())
        .stdout(Stdio::null());
    let (status, mut said) = stderr_tail::status(&mut command).map_err(|e| {
        message(format_args!(
            "start the account's emptying of its home: {e}"
        ))
    })?;
    if !status.success() {
        return Err(message(format_args!(
            "the account could not empty its home: {status}{}",
            said.said()
        )));
    }
    Ok(())
}

/// A writer failure as the requester reads it: its refusal, or root's
/// failure, logged, named only by the log.
fn failure(failed: Failed) -> String {
    match failed {
        Failed::Refused(reason) if reason == workload_grants::BUSY => {
            grant_requests::refusal_text(&reason)
        }
        Failed::Refused(reason) => message(format_args!(
            "{FINAL}{}",
            grant_requests::refusal_text(&reason)
        )),
        Failed::Root(why) => {
            log(format_args!("grant table writer: {why}"));
            message(format_args!("the grant table's writer failed; see {LOG}"))
        }
    }
}

/// Writes one line to root's log, formatting it into nothing of its own.
fn log(line: fmt::Arguments<'_>) {
    let _ = writeln!(io::stderr().lock(), "td-authd: {line}");
}

/// Which sandboxes and workspace slots have launches running, grant reads
/// or changes under way, or a creation or deletion: a creation or deletion
/// runs alone on its sandbox or slot, since it reads or empties the home
/// and the rows. Each is keyed by its kind and name, so a sandbox and a
/// slot of one name are apart. Beside them, what the service keeps of
/// workspace accounts.
#[derive(Default)]
struct Active {
    busy: Mutex<Busy>,
    instances: Instances,
}

#[derive(Default)]
struct Busy {
    launches: HashMap<String, usize>,
    changes: HashMap<String, usize>,
    exclusive: HashMap<String, ()>,
    /// Every sandbox's launches together, at most `MAX_LAUNCHES`.
    running: usize,
    /// Every workspace slot's instances together, at most
    /// `MAX_INSTANCES`.
    instances: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Hold {
    Launch,
    Instance,
    Change,
    Exclusive,
}

/// A request's hold on its sandbox or slot, given back when dropped.
struct Claim<'a> {
    active: &'a Active,
    key: String,
    hold: Hold,
}

impl Active {
    fn busy(&self) -> std::sync::MutexGuard<'_, Busy> {
        self.busy.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// What `verb` on the `kind` account `name` may do now: nothing while
    /// it is being created or deleted; a creation or deletion only while no
    /// launch, grant read or change of it runs; a sandbox's launch only
    /// while fewer than `MAX_LAUNCHES` run in all, and a slot's instance
    /// only while fewer than `MAX_INSTANCES` run in all and
    /// `MAX_SLOT_INSTANCES` in the slot.
    fn claim(&self, kind: Kind, name: &str, verb: &Verb) -> Result<Claim<'_>, String> {
        // The claim's own key first: past this point a hold is recorded,
        // and only the claim gives it back.
        let key = text(format_args!("{} {name}", kind.as_str()))?;
        let what = kind.as_str();
        let mut busy = self.busy();
        if busy.exclusive.contains_key(&key) {
            return Err(message(format_args!(
                "{what} {name} is being created or deleted"
            )));
        }
        let hold = match verb {
            Verb::Launch(_) => Hold::Launch,
            Verb::Instance(_) => Hold::Instance,
            Verb::Create | Verb::Delete => Hold::Exclusive,
            Verb::Status | Verb::Grants(_) => Hold::Change,
        };
        match hold {
            Hold::Exclusive => {
                if busy.launches.contains_key(&key) {
                    return Err(message(format_args!(
                        "a shell, command or instance of {what} {name} is running; end it first"
                    )));
                }
                if busy.changes.contains_key(&key) {
                    return Err(message(format_args!(
                        "{what} {name}'s grants are being read or changed; try again"
                    )));
                }
                allocation::insert(&mut busy.exclusive, owned(&key)?, ()).map_err(refused)?;
            }
            Hold::Launch => {
                if busy.running >= MAX_LAUNCHES {
                    return Err("too many sandbox shells and commands are running".into());
                }
                count_up(&mut busy.launches, &key)?;
                busy.running = busy.running.saturating_add(1);
            }
            Hold::Instance => {
                if busy.instances >= MAX_INSTANCES {
                    return Err("too many workspace instances are running".into());
                }
                if busy
                    .launches
                    .get(&key)
                    .is_some_and(|count| *count >= MAX_SLOT_INSTANCES)
                {
                    return Err(message(format_args!(
                        "too many instances of workspace {name} are running"
                    )));
                }
                count_up(&mut busy.launches, &key)?;
                busy.instances = busy.instances.saturating_add(1);
            }
            Hold::Change => count_up(&mut busy.changes, &key)?,
        }
        Ok(Claim {
            active: self,
            key,
            hold,
        })
    }
}

/// One more hold of `name` in `counts`, its first one added in fallibly
/// grown storage.
fn count_up(counts: &mut HashMap<String, usize>, name: &str) -> Result<(), String> {
    match counts.get_mut(name) {
        Some(count) => *count = count.saturating_add(1),
        None => {
            allocation::insert(counts, owned(name)?, 1).map_err(refused)?;
        }
    }
    Ok(())
}

impl Drop for Claim<'_> {
    fn drop(&mut self) {
        let mut busy = self.active.busy();
        let counts = match self.hold {
            Hold::Exclusive => {
                busy.exclusive.remove(&self.key);
                return;
            }
            Hold::Launch => {
                busy.running = busy.running.saturating_sub(1);
                &mut busy.launches
            }
            Hold::Instance => {
                busy.instances = busy.instances.saturating_sub(1);
                &mut busy.launches
            }
            Hold::Change => &mut busy.changes,
        };
        if let Some(count) = counts.get_mut(&self.key) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                counts.remove(&self.key);
            }
        }
    }
}

/// The answer to a verb other than a launch: `ok` and what it found or
/// did, or a refusal. A fixed answer is static, so no allocation can
/// refuse it after the change it reports.
fn answer(request: &Request, serve: &impl Serve) -> Cow<'static, [u8]> {
    let (kind, name) = (request.kind, &request.name);
    let answered: Result<Cow<'static, str>, String> = match &request.verb {
        Verb::Launch(_) | Verb::Instance(_) => Err("a launch is answered by its start".into()),
        Verb::Create => serve
            .create(kind, name)
            .and_then(|uid| text(format_args!("ok {uid}\n")))
            .map(Cow::Owned),
        Verb::Delete => serve.delete(kind, name).map(|()| Cow::Borrowed("ok\n")),
        Verb::Status => serve.state(kind, name).and_then(|state| match state {
            None => Ok(Cow::Borrowed("ok absent\n")),
            Some(State {
                uid, held: true, ..
            }) => text(format_args!("ok held {uid}\n")).map(Cow::Owned),
            Some(State { uid, .. }) => text(format_args!("ok free {uid}\n")).map(Cow::Owned),
        }),
        Verb::Grants(verb) => {
            let account = match verb {
                grant_requests::Verb::List { .. } => serve.account(name).map(Some),
                _ => refuse_deleted(kind, name, serve).map(|()| None),
            };
            account.and_then(|account| {
                let request = grant_requests::Request {
                    verb: verb.copy()?,
                    kind: Kind::Sandbox,
                    name: owned(name)?,
                };
                serve
                    .grants(&request)
                    .map_err(failure)
                    .and_then(|written| grant_text(account, &written))
            })
        }
    };
    match answered {
        Ok(Cow::Borrowed(text)) => Cow::Borrowed(text.as_bytes()),
        Ok(Cow::Owned(text)) => Cow::Owned(text.into_bytes()),
        Err(why) => {
            log(format_args!("{} {name}: {why}", kind.as_str()));
            Cow::Owned(refusal(&why).unwrap_or_default())
        }
    }
}

/// A deleted sandbox or slot takes no launch and no grant change until it
/// is created again.
fn refuse_deleted(kind: Kind, name: &str, serve: &impl Serve) -> Result<(), String> {
    if serve.deleted(kind, name)? {
        return Err(DELETED.into());
    }
    Ok(())
}

/// A workspace instance's plan, held until `launch_instance` prepared
/// it: dropped, as on a refusal or a panic, it is given back.
struct PlanHold<'a> {
    root: &'a Root<'a>,
    uid: u32,
    id: u64,
}

impl Drop for PlanHold<'_> {
    fn drop(&mut self) {
        self.root.release_views(self.uid, self.id);
    }
}

/// A prepared launch, given back when dropped, after its run or on a
/// panic: its hold released and its spec removed.
struct Launched<'a, S: Serve> {
    serve: &'a S,
    prepared: Prepared,
    failed: bool,
}

impl<S: Serve> Drop for Launched<'_, S> {
    fn drop(&mut self) {
        self.serve.release(&self.prepared, self.failed);
        let _ = fs::remove_file(&self.prepared.spec);
    }
}

/// What the writer said, as the intake answers it: a listing as `ok UID
/// NETWORK COUNT` and one `ACCESS<TAB>PATH` line per row, a change as `ok`
/// and what it did, static.
fn grant_text(account: Option<u32>, written: &Written) -> Result<Cow<'static, str>, String> {
    match (account, written) {
        (
            Some(uid),
            Written::Listed {
                network: Some(network),
                directories,
                ..
            },
        ) => {
            let mut listing = text(format_args!(
                "ok {uid} {} {}\n",
                network.as_str(),
                directories.len()
            ))?;
            for (access, path) in directories {
                for part in [access.as_str(), "\t", path, "\n"] {
                    allocation::push_str(&mut listing, part).map_err(refused)?;
                }
            }
            Ok(Cow::Owned(listing))
        }
        (None, Written::Changed(outcome)) => match outcome {
            0 => Ok(Cow::Borrowed("ok added\n")),
            1 => Ok(Cow::Borrowed("ok changed\n")),
            2 => Ok(Cow::Borrowed("ok unchanged\n")),
            3 => Ok(Cow::Borrowed("ok revoked\n")),
            _ => Err("the grant table's writer answered amiss".into()),
        },
        _ => Err("the grant table's writer answered amiss".into()),
    }
}

/// One connection, start to end. A launch is admitted and prepared, then
/// answered `ok` and run: a refusal before the answer goes to the
/// requester and the log; what happens after it, the launch's end or a
/// failure of placement, of `workspace-exec` or of td-jail, reaches only
/// the log, and the requester sees the connection close. The spec goes
/// either way, and a workspace instance's hold on its views once it has
/// ended. Any other verb is answered once.
fn handle(mut stream: UnixStream, id: u64, active: &Active, serve: &impl Serve) {
    let request = match read_request(&mut stream) {
        Ok(request) => request,
        Err(why) => {
            log(format_args!("{why}"));
            refuse(&mut stream, &why);
            return;
        }
    };
    let (kind, name) = (request.kind, &request.name);
    // Declared before the claim so it drops after both the prepared
    // launch and the active claim. EOF then acknowledges their release,
    // not merely the helper's exit. The monitor still sees a peer's
    // write-half close through this retained endpoint.
    let _completion = if matches!(&request.verb, Verb::Instance(_)) {
        match stream.try_clone() {
            Ok(channel) => Some(channel),
            Err(error) => {
                refuse(
                    &mut stream,
                    &message(format_args!("hold workspace completion: {error}")),
                );
                return;
            }
        }
    } else {
        None
    };
    let _claim = match active.claim(kind, name, &request.verb) {
        Ok(claim) => claim,
        Err(why) => {
            log(format_args!("{why}"));
            refuse(&mut stream, &why);
            return;
        }
    };
    let prepared = match &request.verb {
        Verb::Launch(launch) => {
            refuse_deleted(kind, name, serve).and_then(|()| serve.prepare(name, launch, id))
        }
        Verb::Instance(instance) => {
            refuse_deleted(kind, name, serve).and_then(|()| serve.instance(name, instance, id))
        }
        _ => {
            let _ = stream.write_all(&answer(&request, serve));
            return;
        }
    };
    let prepared = match prepared {
        Ok(prepared) => prepared,
        Err(why) => {
            log(format_args!("{} {name}: {why}", kind.as_str()));
            refuse(&mut stream, &why);
            return;
        }
    };
    let mut launched = Launched {
        serve,
        prepared,
        failed: false,
    };
    if stream.write_all(STARTED).is_ok() {
        launched.failed = match serve.run(stream, &launched.prepared) {
            Ok((status, said)) => {
                log(format_args!(
                    "{} {name} launch as tdw{} ended: {status}; it said {said:?}",
                    kind.as_str(),
                    launched.prepared.uid
                ));
                !status.success()
            }
            Err(why) => {
                log(format_args!("{} {name}: {why}", kind.as_str()));
                true
            }
        };
    }
}

/// After a failed accept: retry at once when a signal interrupted it or
/// the peer gave up, and otherwise pause, longer for each failure in a row
/// (`failures`, this one included), up to `ACCEPT_PAUSE_MAX`. The service
/// never leaves over one, since every shell it launched would end with it.
fn accept_pause(kind: io::ErrorKind, failures: u32) -> Option<Duration> {
    match kind {
        io::ErrorKind::Interrupted | io::ErrorKind::ConnectionAborted => None,
        _ => Some(
            ACCEPT_PAUSE
                .saturating_mul(1u32 << failures.saturating_sub(1).min(5))
                .min(ACCEPT_PAUSE_MAX),
        ),
    }
}

/// A connection's place among `MAX_REQUESTS`, given back when dropped.
struct Slot(Arc<AtomicUsize>);

impl Slot {
    fn take(active: &Arc<AtomicUsize>) -> Option<Self> {
        active
            .try_update(Ordering::AcqRel, Ordering::Acquire, |held| {
                (held < MAX_REQUESTS).then_some(held + 1)
            })
            .ok()
            .map(|_| Self(Arc::clone(active)))
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

/// The bound intake, unlinked when the service ends.
struct Intake {
    listener: UnixListener,
    identity: (u64, u64),
}

impl Intake {
    fn bind() -> Result<Self, String> {
        let parent = Path::new(WORKSPACE_INTAKE)
            .parent()
            .ok_or("the workspace intake has no parent")?;
        ensure_parents(Path::new(RUN), parent, (0, 0))?;
        let (listener, identity) =
            crate::deployment::bind_intake(WORKSPACE_INTAKE, primary_account::UID)?;
        let intake = Self { listener, identity };
        intake
            .listener
            .set_nonblocking(false)
            .map_err(|e| message(format_args!("{e}")))?;
        Ok(intake)
    }
}

impl Drop for Intake {
    fn drop(&mut self) {
        crate::deployment::unlink_intake(WORKSPACE_INTAKE, self.identity);
    }
}

/// `workspace-serve`: root's service, run by td-svc. Each admitted
/// connection gets a thread of its own, which answers or launches and
/// waits; a peer other than the primary human, or one past
/// `MAX_REQUESTS`, sees the connection close or a refusal.
pub(crate) fn serve() -> Result<(), String> {
    crate::launch::require_operation_startup()?;
    specs().prepare()?;
    let intake = Intake::bind()?;
    let slots = Arc::new(AtomicUsize::new(0));
    let active = Arc::new(Active::default());
    let launches = AtomicU64::new(0);
    let mut failures = 0u32;
    loop {
        let mut stream = match intake.listener.accept() {
            Ok((stream, _)) => {
                failures = 0;
                stream
            }
            Err(e) => {
                failures = failures.saturating_add(1);
                if let Some(pause) = accept_pause(e.kind(), failures) {
                    log(format_args!("accept a workspace request: {e}"));
                    std::thread::sleep(pause);
                }
                continue;
            }
        };
        if !admitted(crate::secret_sys::peer_uid(&stream)) {
            continue;
        }
        let Some(slot) = Slot::take(&slots) else {
            refuse(
                &mut stream,
                "too many workspace intake requests are running",
            );
            continue;
        };
        let id = launches.fetch_add(1, Ordering::Relaxed);
        let active = Arc::clone(&active);
        let started = std::thread::Builder::new()
            .name("workspace".into())
            .spawn(move || {
                let _slot = slot;
                let root = Root {
                    instances: &active.instances,
                };
                handle(stream, id, &active, &root);
            });
        if let Err(e) = started {
            log(format_args!("start a workspace request: {e}"));
        }
    }
}

#[cfg(test)]
#[path = "../tests/workspace.rs"]
mod tests;

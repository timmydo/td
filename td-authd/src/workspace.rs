//! The workspace launch intake (td-authd/DESIGN.md, "Workspace launch
//! intake"): root's service at `WORKSPACE_INTAKE`, where the primary
//! human's programs create and delete sandboxes, list and change their
//! grants and network, and ask for a shell or a command in one. For a
//! launch root prepares the sandbox's ledger account, publishes its grant
//! views, writes a root-owned spec, and starts td-jail's workspace kind
//! as the account, the accepted connection its channel.

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
use crate::workload_grants::{self, Access, Network};
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
/// How long a peer has to send its whole request.
const REQUEST_DEADLINE: Duration = Duration::from_secs(2);
/// The most launches at once, over every sandbox, each holding its thread
/// until its shell or command ends.
const MAX_LAUNCHES: usize = 16;
/// The connections served at once: the launches and room for other
/// verbs, each answered and gone, which open shells never fill.
const MAX_REQUESTS: usize = MAX_LAUNCHES + 4;
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

/// What a request asks of the sandbox it names.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Verb {
    Launch(Launch),
    /// Its account, allocated when the ledger has none.
    Create,
    /// Its grant rows revoked and its home emptied; its ledger row stays.
    Delete,
    /// Whether the ledger holds it, and whether it holds files or grants.
    Status,
    /// Its directory rows and network listed, or one of them changed,
    /// through the grant table's writer.
    Grants(grant_requests::Verb),
}

/// One request: a verb and a sandbox, by its ledger name.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Request {
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
    pub(crate) fn parse(line: &[u8]) -> Result<Self, String> {
        if line.len() > MAX_REQUEST {
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
                let launch = Launch {
                    runtime: runtime_of(runtime)?,
                    mode: Mode::parse(mode).ok_or_else(malformed)?,
                    term: term_of(term)?,
                    command: Some(command),
                };
                (name, Verb::Launch(launch))
            }
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
            name: named(name)?,
            verb,
        })
    }
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
    let mut line = allocation::with_capacity(256).map_err(refused)?;
    let mut byte = [0u8; 1];
    while line.last() != Some(&b'\n') {
        if line.len() >= MAX_REQUEST {
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
    // The timeout is the socket's, which the jail inherits.
    stream
        .set_read_timeout(None)
        .map_err(|e| message(format_args!("{e}")))?;
    Request::parse(&line)
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

/// The credential helper's command: td-login switches to the sandbox's
/// account and places it in its delegation, and `workspace-exec` checks
/// both before it runs td-jail with `command` behind the relay.
fn helper(
    uid: u32,
    launcher: u32,
    spec: &Path,
    mode: Mode,
    term: &str,
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
        .arg(spec)
        .args([mode.as_str(), term, "--"])
        .args(command);
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
/// service as its launcher, with the root-owned spec and td-sandbox's
/// relay running `command` through a relay of `mode` of type `term`.
struct Exec {
    uid: u32,
    launcher: u32,
    spec: PathBuf,
    mode: Mode,
    term: String,
    command: Vec<String>,
}

impl Exec {
    /// `UID LAUNCHER SPEC MODE TERM -- PROGRAM [ARG...]`: a sandbox's or
    /// workspace's uid, root's service's pid, that uid's spec below root's
    /// tree, the relay's mode, a terminal type and a command.
    fn parse(arguments: &[String]) -> Result<Self, String> {
        let [uid, launcher, spec, mode, term, separator, command @ ..] = arguments else {
            return Err(
                "workspace-exec requires UID LAUNCHER SPEC MODE TERM -- PROGRAM [ARG...]".into(),
            );
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
        let mode = Mode::parse(mode).ok_or("invalid workspace relay mode")?;
        if !valid_term(term) {
            return Err("invalid workspace terminal type".into());
        }
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
            mode,
            term: owned(term)?,
            command: copied(command)?,
        })
    }

    fn jail(&self) -> Command {
        let mut command = Command::new("/bin/td-jail");
        command
            .args(["--workspace", &self.launcher.to_string()])
            .arg(&self.spec)
            .args(["--inside", self.mode.as_str(), &self.term])
            .args(&self.command)
            .env_clear()
            .current_dir("/");
        command
    }
}

/// `workspace-exec`, run as the sandbox's account: its credentials and
/// session leaf checked, then td-jail in its place, the connection still
/// its standard input and output.
pub(crate) fn exec(arguments: &[String]) -> Result<(), String> {
    let launch = Exec::parse(arguments)?;
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

/// `workspace-empty UID`, run as a deleted sandbox's account: its
/// credentials and session leaf checked, then everything in its home
/// removed, the home itself kept. Root never walks a tree the account
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
    empty_home(&Kind::Sandbox.home(uid), uid)
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

/// What root finds of a sandbox the ledger holds: its account, whether a
/// deletion marked its home, and whether it holds files in its home or
/// any grant row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct State {
    uid: u32,
    deleted: bool,
    held: bool,
}

/// The uid of the sandbox `name`'s ledger row: a sandbox's alone, never an
/// application's or a workspace's of the same name, and never one this
/// adds.
fn sandbox_uid(ledger: &accounts::Ledger, name: &str) -> Result<u32, String> {
    ledger
        .uid_of(Kind::Sandbox, name)
        .ok_or_else(|| message(format_args!("sandbox {name} has no account")))
}

/// A launch prepared for its account.
struct Prepared {
    uid: u32,
    spec: PathBuf,
    command: Vec<String>,
}

/// What root does for each verb: `Root` on a running system, a stand-in
/// in the tests.
trait Serve {
    /// Admits and prepares a launch before its answer.
    fn prepare(&self, name: &str, launch: &Launch, id: u64) -> Result<Prepared, String>;
    /// Runs a prepared launch with `stream` as the jail's channel and
    /// waits for it, returning what td-jail last said.
    fn run(
        &self,
        stream: UnixStream,
        prepared: &Prepared,
        launch: &Launch,
    ) -> Result<(ExitStatus, String), String>;
    /// The sandbox's state, none without a ledger row.
    fn state(&self, name: &str) -> Result<Option<State>, String>;
    fn create(&self, name: &str) -> Result<u32, String>;
    fn delete(&self, name: &str) -> Result<(), String>;
    /// The sandbox's account, which a listing names.
    fn account(&self, name: &str) -> Result<u32, String>;
    fn grants(&self, request: &grant_requests::Request) -> Result<Written, Failed>;
}

/// Root's own work.
struct Root;

impl Serve for Root {
    /// The sandbox must hold a ledger row, which this never adds;
    /// td-firstboot prepares that row's account again under its lock,
    /// refusing when the row is gone; a named runtime must be installed;
    /// its views are published and its spec written with the network its
    /// grant row names. All of it before the answer, so the requester
    /// hears what refused it.
    fn prepare(&self, name: &str, launch: &Launch, id: u64) -> Result<Prepared, String> {
        let uid = sandbox_uid(&accounts::load()?, name)?;
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
        Ok(Prepared { uid, spec, command })
    }

    /// Root keeps no copy of the channel, and this thread, td-jail's
    /// launcher, lives until the jail ends. What td-jail last wrote on its
    /// stderr comes back, bounded.
    fn run(
        &self,
        stream: UnixStream,
        prepared: &Prepared,
        launch: &Launch,
    ) -> Result<(ExitStatus, String), String> {
        let input = stream
            .try_clone()
            .map_err(|e| message(format_args!("{e}")))?;
        let (mut said, said_end) = UnixStream::pair().map_err(|e| message(format_args!("{e}")))?;
        let mut command = helper(
            prepared.uid,
            std::process::id(),
            &prepared.spec,
            launch.mode,
            &launch.term,
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

    fn state(&self, name: &str) -> Result<Option<State>, String> {
        let Some(uid) = accounts::load()?.uid_of(Kind::Sandbox, name) else {
            return Ok(None);
        };
        let home = Kind::Sandbox.home(uid);
        let held = grant_views::read_table()?.holds_any(Kind::Sandbox, name)
            || holds_files(&home, uid, 0)?;
        Ok(Some(State {
            uid,
            deleted: marked(&home, uid, 0)?,
            held,
        }))
    }

    fn create(&self, name: &str) -> Result<u32, String> {
        create(
            self.state(name)?,
            || application::admit(&application::ALLOCATE_SANDBOX, primary_account::UID, name),
            |uid| unmark(&Kind::Sandbox.home(uid), uid, 0),
        )
    }

    fn delete(&self, name: &str) -> Result<(), String> {
        delete(
            name,
            |clear| self.grants(clear),
            || Ok(accounts::load()?.uid_of(Kind::Sandbox, name)),
            || application::admit(&application::PREPARE_SANDBOX, primary_account::UID, name),
            empty_as,
            |uid| mark_deleted(&Kind::Sandbox.home(uid), uid, 0),
        )
    }

    fn account(&self, name: &str) -> Result<u32, String> {
        sandbox_uid(&accounts::load()?, name)
    }

    fn grants(&self, request: &grant_requests::Request) -> Result<Written, Failed> {
        grant_requests::perform(primary_account::UID, request)
    }
}

/// Creates the sandbox whose state root found (`state`), never over one
/// that holds files or grants: that is refused as `exists`, for the human
/// to adopt or delete. Otherwise td-firstboot allocates its account, or
/// returns the one the ledger holds, preparing its home (`allocate`), and
/// a deletion's mark is taken from it (`unmark`): a deleted sandbox, its
/// home empty and its rows cleared, is created again fresh.
fn create(
    state: Option<State>,
    allocate: impl FnOnce() -> Result<u32, String>,
    unmark: impl FnOnce(u32) -> Result<(), String>,
) -> Result<u32, String> {
    if state.is_some_and(|state| state.held) {
        return Err(EXISTS.into());
    }
    let uid = allocate()?;
    if state.is_some_and(|state| state.uid != uid) {
        return Err("the sandbox's account is not the ledger's".into());
    }
    unmark(uid)?;
    Ok(uid)
}

/// Deletes the sandbox `name`. Every grant row its name holds is cleared
/// through the table's writer first (`clear`); then, when the ledger holds
/// its account (`account`), td-firstboot prepares that account again
/// (`prepare`), restoring its home's mode and its delegation, the account
/// itself empties its home (`empty`), and root marks the emptied home
/// deleted (`mark`), which refuses its launches and grant changes until it
/// is created again. The ledger row stays, and so does the uid.
fn delete(
    name: &str,
    clear: impl FnOnce(&grant_requests::Request) -> Result<Written, Failed>,
    account: impl FnOnce() -> Result<Option<u32>, String>,
    prepare: impl FnOnce() -> Result<u32, String>,
    empty: impl FnOnce(u32) -> Result<(), String>,
    mark: impl FnOnce(u32) -> Result<(), String>,
) -> Result<(), String> {
    let request = grant_requests::Request {
        verb: grant_requests::Verb::Clear,
        kind: Kind::Sandbox,
        name: owned(name)?,
    };
    match clear(&request) {
        Ok(Written::Cleared(count)) => log(format_args!(
            "sandbox {name}: deleting; {count} grant rows revoked"
        )),
        Ok(_) => return Err("the grant table's writer answered a clear amiss".into()),
        Err(failed) => return Err(failure(failed)),
    }
    let Some(uid) = account()? else {
        return Ok(());
    };
    if prepare()? != uid {
        return Err("the sandbox's account is not the ledger's".into());
    }
    empty(uid)?;
    mark(uid)
}

/// Has the account `uid` empty its own home, and waits for it: its
/// emptying ends when the tree it owns does, and nothing else runs as it
/// while no launch of its sandbox does.
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

/// Which sandboxes have launches running, grant reads or changes under
/// way, or a creation or deletion: a creation or deletion runs alone on
/// its sandbox, since it reads or empties the home and the rows.
#[derive(Default)]
struct Active {
    busy: Mutex<Busy>,
}

#[derive(Default)]
struct Busy {
    launches: HashMap<String, usize>,
    changes: HashMap<String, usize>,
    exclusive: HashMap<String, ()>,
    /// Every sandbox's launches together, at most `MAX_LAUNCHES`.
    running: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Hold {
    Launch,
    Change,
    Exclusive,
}

/// A request's hold on its sandbox, given back when dropped.
struct Claim<'a> {
    active: &'a Active,
    name: String,
    hold: Hold,
}

impl Active {
    fn busy(&self) -> std::sync::MutexGuard<'_, Busy> {
        self.busy.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// What `verb` on `name` may do now: nothing while the sandbox is
    /// being created or deleted; a creation or deletion only while no
    /// launch, grant read or change of it runs; a launch only while fewer
    /// than `MAX_LAUNCHES` run in all.
    fn claim(&self, name: &str, verb: &Verb) -> Result<Claim<'_>, String> {
        // The claim's own copy of the name first: past this point a hold is
        // recorded, and only the claim gives it back.
        let claimed = owned(name)?;
        let mut busy = self.busy();
        if busy.exclusive.contains_key(name) {
            return Err(message(format_args!(
                "sandbox {name} is being created or deleted"
            )));
        }
        let hold = match verb {
            Verb::Launch(_) => Hold::Launch,
            Verb::Create | Verb::Delete => Hold::Exclusive,
            Verb::Status | Verb::Grants(_) => Hold::Change,
        };
        match hold {
            Hold::Exclusive => {
                if busy.launches.contains_key(name) {
                    return Err(message(format_args!(
                        "a shell or command of sandbox {name} is running; end it first"
                    )));
                }
                if busy.changes.contains_key(name) {
                    return Err(message(format_args!(
                        "sandbox {name}'s grants are being read or changed; try again"
                    )));
                }
                allocation::insert(&mut busy.exclusive, owned(name)?, ()).map_err(refused)?;
            }
            Hold::Launch => {
                if busy.running >= MAX_LAUNCHES {
                    return Err("too many sandbox shells and commands are running".into());
                }
                count_up(&mut busy.launches, name)?;
                busy.running = busy.running.saturating_add(1);
            }
            Hold::Change => count_up(&mut busy.changes, name)?,
        }
        Ok(Claim {
            active: self,
            name: claimed,
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
                busy.exclusive.remove(&self.name);
                return;
            }
            Hold::Launch => {
                busy.running = busy.running.saturating_sub(1);
                &mut busy.launches
            }
            Hold::Change => &mut busy.changes,
        };
        if let Some(count) = counts.get_mut(&self.name) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                counts.remove(&self.name);
            }
        }
    }
}

/// The answer to a verb other than a launch: `ok` and what it found or
/// did, or a refusal. A fixed answer is static, so no allocation can
/// refuse it after the change it reports.
fn answer(request: &Request, serve: &impl Serve) -> Cow<'static, [u8]> {
    let name = &request.name;
    let answered: Result<Cow<'static, str>, String> = match &request.verb {
        Verb::Launch(_) => Err("a launch is answered by its start".into()),
        Verb::Create => serve
            .create(name)
            .and_then(|uid| text(format_args!("ok {uid}\n")))
            .map(Cow::Owned),
        Verb::Delete => serve.delete(name).map(|()| Cow::Borrowed("ok\n")),
        Verb::Status => serve.state(name).and_then(|state| match state {
            None => Ok(Cow::Borrowed("ok absent\n")),
            Some(State {
                uid, held: true, ..
            }) => text(format_args!("ok held {uid}\n")).map(Cow::Owned),
            Some(State { uid, .. }) => text(format_args!("ok free {uid}\n")).map(Cow::Owned),
        }),
        Verb::Grants(verb) => {
            let account = match verb {
                grant_requests::Verb::List { .. } => serve.account(name).map(Some),
                _ => refuse_deleted(name, serve).map(|()| None),
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
            log(format_args!("sandbox {name}: {why}"));
            Cow::Owned(refusal(&why).unwrap_or_default())
        }
    }
}

/// A deleted sandbox takes no launch and no grant change until it is
/// created again.
fn refuse_deleted(name: &str, serve: &impl Serve) -> Result<(), String> {
    match serve.state(name)? {
        Some(State { deleted: true, .. }) => Err(DELETED.into()),
        _ => Ok(()),
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
/// either way. Any other verb is answered once.
fn handle(mut stream: UnixStream, id: u64, active: &Active, serve: &impl Serve) {
    let request = match read_request(&mut stream) {
        Ok(request) => request,
        Err(why) => {
            log(format_args!("{why}"));
            refuse(&mut stream, &why);
            return;
        }
    };
    let _claim = match active.claim(&request.name, &request.verb) {
        Ok(claim) => claim,
        Err(why) => {
            log(format_args!("{why}"));
            refuse(&mut stream, &why);
            return;
        }
    };
    let Verb::Launch(launch) = &request.verb else {
        let _ = stream.write_all(&answer(&request, serve));
        return;
    };
    let prepared = match refuse_deleted(&request.name, serve)
        .and_then(|()| serve.prepare(&request.name, launch, id))
    {
        Ok(prepared) => prepared,
        Err(why) => {
            log(format_args!("sandbox {}: {why}", request.name));
            refuse(&mut stream, &why);
            return;
        }
    };
    if stream.write_all(STARTED).is_ok() {
        match serve.run(stream, &prepared, launch) {
            Ok((status, said)) => log(format_args!(
                "sandbox {} launch as tdw{} ended: {status}; it said {said:?}",
                request.name, prepared.uid
            )),
            Err(why) => log(format_args!("sandbox {}: {why}", request.name)),
        }
    }
    let _ = fs::remove_file(&prepared.spec);
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
            refuse(&mut stream, "too many sandbox requests are running");
            continue;
        };
        let id = launches.fetch_add(1, Ordering::Relaxed);
        let active = Arc::clone(&active);
        let started = std::thread::Builder::new()
            .name("workspace".into())
            .spawn(move || {
                let _slot = slot;
                handle(stream, id, &active, &Root);
            });
        if let Err(e) = started {
            log(format_args!("start a workspace request: {e}"));
        }
    }
}

#[cfg(test)]
#[path = "../tests/workspace.rs"]
mod tests;

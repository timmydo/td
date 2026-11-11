//! The workspace launch intake (td-authd/DESIGN.md, "Workspace launch
//! intake"): root's service at `WORKSPACE_INTAKE`, where the primary
//! human's programs ask for a sandbox's shell. For each request root
//! prepares the sandbox's ledger account, publishes its grant views,
//! writes a root-owned spec, and starts td-jail's workspace kind as the
//! account, the accepted connection its channel.

use crate::application;
use crate::flathub_app;
use crate::grant_views;
use crate::portal_files::pinned;
use crate::primary_account;
use crate::stderr_tail;
use crate::workload_accounts::{
    self as accounts, canonical_decimal, Kind, WORKSPACE_INTAKE, WORKSPACE_SPECS,
};
use crate::workload_grants::Network;
use crate::{allocation, message, owned, refused, text};
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::OwnedFd;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// The longest request, its newline included.
const MAX_REQUEST: usize = 256;
/// How long a peer has to send its whole request.
const REQUEST_DEADLINE: Duration = Duration::from_secs(2);
/// The most launches at once, over every sandbox: each holds its thread
/// until its shell ends.
const MAX_LAUNCHES: usize = 16;
/// A terminal type, as td-sandbox's relay admits one.
const MAX_TERM: usize = 64;
const MAX_REASON: usize = 200;
const MAX_PATH: usize = 4096;
/// The jail's entry, td-sandbox's `--inside` relay, and the shell it runs:
/// td's own, never one a caller names.
const ENTRY: &str = "/bin/td-sandbox";
const SHELL: &str = "/bin/sh";
/// The answer to an admitted request, after which the connection is the
/// jail's; a refusal is `refused`, a space and the reason.
const STARTED: &[u8] = b"ok\n";
/// Linux x86-64's `O_NOFOLLOW` and `O_DIRECTORY`.
const O_NOFOLLOW: i32 = 0o400_000;
const O_DIRECTORY: i32 = 0o200_000;
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

/// One request: a shell in the sandbox `name`, under the declared runtime
/// `runtime` or td's own trees, on a terminal of type `term`.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Request {
    pub(crate) name: String,
    pub(crate) runtime: Option<&'static str>,
    pub(crate) term: String,
}

impl Request {
    /// `start NAME RUNTIME TERM` and a newline, single spaces, nothing
    /// else: a ledger name, `native` or a declared runtime's td name, and a
    /// terminal type.
    pub(crate) fn parse(line: &[u8]) -> Result<Self, String> {
        if line.len() > MAX_REQUEST {
            return Err("the workspace request is too long".into());
        }
        let text = std::str::from_utf8(line)
            .ok()
            .and_then(|text| text.strip_suffix('\n'))
            .ok_or("the workspace request is not one line")?;
        let Some(["start", name, runtime, term]) = words(text) else {
            return Err("the workspace request is not `start NAME RUNTIME TERM`".into());
        };
        if !accounts::valid_name(name) {
            return Err("the workspace request names no sandbox".into());
        }
        let runtime = match runtime {
            "native" => None,
            named => Some(
                flathub_app::flathub_runtime(named)
                    .ok_or("the workspace request names no declared runtime")?
                    .name,
            ),
        };
        if !valid_term(term) {
            return Err("the workspace request names no terminal type".into());
        }
        Ok(Self {
            name: owned(name)?,
            runtime,
            term: owned(term)?,
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
    let mut line = allocation::with_capacity(MAX_REQUEST).map_err(refused)?;
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
/// both before it runs td-jail.
fn helper(uid: u32, launcher: u32, spec: &Path, term: &str, shell: &Path) -> Command {
    let mut command = Command::new("/bin/td-login");
    command
        .args(["exec-service-as", &format!("tdw{uid}"), "--"])
        .args([
            "/bin/td-authd",
            "workspace-exec",
            &uid.to_string(),
            &launcher.to_string(),
        ])
        .arg(spec)
        .arg(term)
        .arg(shell);
    command
}

/// What `workspace-exec` runs: td-jail's workspace kind, tied to root's
/// service as its launcher, with the root-owned spec and td-sandbox's
/// relay running td's shell on a fresh PTY of type `term`.
struct Exec {
    uid: u32,
    launcher: u32,
    spec: PathBuf,
    term: String,
    shell: PathBuf,
}

impl Exec {
    /// `UID LAUNCHER SPEC TERM SHELL`: a sandbox's or workspace's uid,
    /// root's service's pid, that uid's spec below root's tree, a terminal
    /// type and an absolute shell.
    fn parse(arguments: &[String]) -> Result<Self, String> {
        let [uid, launcher, spec, term, shell] = arguments else {
            return Err("workspace-exec requires UID LAUNCHER SPEC TERM SHELL".into());
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
        if !valid_term(term) {
            return Err("invalid workspace terminal type".into());
        }
        if !shell.starts_with('/') || shell.len() > MAX_PATH || shell.contains('\0') {
            return Err("invalid workspace shell".into());
        }
        Ok(Self {
            uid,
            launcher,
            spec: spec_path(uid, id)?,
            term: owned(term)?,
            shell: PathBuf::from(owned(shell)?),
        })
    }

    fn jail(&self) -> Command {
        let mut command = Command::new("/bin/td-jail");
        command
            .args(["--workspace", &self.launcher.to_string()])
            .arg(&self.spec)
            .args(["--inside", "pty", &self.term])
            .arg(&self.shell)
            .arg("-i")
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
    shell: PathBuf,
}

/// Admits and prepares `request`: the sandbox must hold a ledger row,
/// which this never adds; td-firstboot prepares that row's account again
/// under its lock, refusing when the row is gone; a named runtime must be
/// installed; its views are published and its spec written with the
/// network its grant row names. All of it before the answer, so the
/// requester hears what refused it.
fn prepare(request: &Request, id: u64) -> Result<Prepared, String> {
    let uid = sandbox_uid(&accounts::load()?, &request.name)?;
    require_runtime(Path::new(flathub_app::RUNTIME_STORE), request.runtime)?;
    let prepared = application::admit(
        &application::PREPARE_SANDBOX,
        primary_account::UID,
        &request.name,
    )?;
    if prepared != uid {
        return Err("the sandbox's account is not the ledger's".into());
    }
    let table = grant_views::publish(Kind::Sandbox, &request.name, uid)?;
    let network = table.network(Kind::Sandbox, &request.name);
    let entry = fs::canonicalize(ENTRY).map_err(|e| message(format_args!("{ENTRY}: {e}")))?;
    let shell = fs::canonicalize(SHELL).map_err(|e| message(format_args!("{SHELL}: {e}")))?;
    let text = spec_text(network, request.runtime, &entry, &Kind::Sandbox.home(uid))?;
    let spec = specs().write(uid, id, &text)?;
    Ok(Prepared { uid, spec, shell })
}

/// Runs the prepared launch with `stream` as the jail's channel and waits
/// for it: root keeps no copy of the channel, and this thread, td-jail's
/// launcher, lives until the jail ends. What td-jail last wrote on its
/// stderr comes back, bounded.
fn run(
    stream: UnixStream,
    prepared: &Prepared,
    term: &str,
) -> Result<(ExitStatus, String), String> {
    let input = stream
        .try_clone()
        .map_err(|e| message(format_args!("{e}")))?;
    let (mut said, said_end) = UnixStream::pair().map_err(|e| message(format_args!("{e}")))?;
    let mut command = helper(
        prepared.uid,
        std::process::id(),
        &prepared.spec,
        term,
        &prepared.shell,
    );
    command
        .env_clear()
        .current_dir("/")
        .stdin(Stdio::from(OwnedFd::from(input)))
        .stdout(Stdio::from(OwnedFd::from(stream)))
        .stderr(Stdio::from(OwnedFd::from(said_end)));
    let child = command.spawn();
    drop(command);
    let mut child = child.map_err(|e| message(format_args!("start the workspace helper: {e}")))?;
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

/// Writes one line to root's log, formatting it into nothing of its own.
fn log(line: fmt::Arguments<'_>) {
    let _ = writeln!(io::stderr().lock(), "td-authd: {line}");
}

/// One connection's launch, start to end, through `prepare` and `run`:
/// a refusal before the answer goes to the requester and the log; what
/// happens after it, the launch's end or a failure of placement, of
/// `workspace-exec` or of td-jail, reaches only the log, and the
/// requester sees the connection close. The spec goes either way.
fn handle(
    mut stream: UnixStream,
    id: u64,
    prepare: impl FnOnce(&Request, u64) -> Result<Prepared, String>,
    run: impl FnOnce(UnixStream, &Prepared, &str) -> Result<(ExitStatus, String), String>,
) {
    let request = match read_request(&mut stream) {
        Ok(request) => request,
        Err(why) => {
            log(format_args!("{why}"));
            refuse(&mut stream, &why);
            return;
        }
    };
    let prepared = match prepare(&request, id) {
        Ok(prepared) => prepared,
        Err(why) => {
            log(format_args!("sandbox {}: {why}", request.name));
            refuse(&mut stream, &why);
            return;
        }
    };
    if stream.write_all(STARTED).is_ok() {
        match run(stream, &prepared, &request.term) {
            Ok((status, said)) => log(format_args!(
                "sandbox {} shell as tdw{} ended: {status}; it said {said:?}",
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

/// A launch's place among `MAX_LAUNCHES`, given back when dropped.
struct Slot(Arc<AtomicUsize>);

impl Slot {
    fn take(active: &Arc<AtomicUsize>) -> Option<Self> {
        active
            .try_update(Ordering::AcqRel, Ordering::Acquire, |held| {
                (held < MAX_LAUNCHES).then_some(held + 1)
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
/// connection gets a thread of its own, which launches and waits; a peer
/// other than the primary human, or one past `MAX_LAUNCHES`, sees the
/// connection close or a refusal.
pub(crate) fn serve() -> Result<(), String> {
    crate::launch::require_operation_startup()?;
    specs().prepare()?;
    let intake = Intake::bind()?;
    let active = Arc::new(AtomicUsize::new(0));
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
        let Some(slot) = Slot::take(&active) else {
            refuse(&mut stream, "too many sandbox shells are running");
            continue;
        };
        let id = launches.fetch_add(1, Ordering::Relaxed);
        let started = std::thread::Builder::new()
            .name("workspace".into())
            .spawn(move || {
                let _slot = slot;
                handle(stream, id, prepare, run);
            });
        if let Err(e) = started {
            log(format_args!("start a workspace launch: {e}"));
        }
    }
}

#[cfg(test)]
#[path = "../tests/workspace.rs"]
mod tests;

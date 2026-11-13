//! The `workspace` launch kind (APPLICATIONS.md §C, td-agent/DESIGN.md §8):
//! a launch on a development host, or on td by td's account or by a
//! sandbox's or workspace's ledger account that root's intake started,
//! whose policy is a spec file its launcher writes, for an agent's tools or
//! a sandbox's shell. It runs a program the launcher names, not
//! a package; binds the launcher's worktrees read-write and executable at
//! their real paths, its git repositories through the git mount chain,
//! shared directories at theirs, a private home, and the host's own system
//! trees read-only, on td optionally with a declared Flathub runtime at
//! `/usr` in place of the host's `/usr`, `/lib` and `/lib64`; and grants
//! no Wayland, bus, audio, fetch, tty or
//! network. Its entry's standard input and output are one stream socket
//! the launcher holds the other end of.
//!
//! This module reads and admits the spec, as `authority` does a package's;
//! `transition` performs it.

use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, Read};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use crate::authority::{
    self, path_is_same_or_child, paths_overlap, FilesystemGrant, FilesystemSourceKind,
};
use crate::flathub_app::{self, FlathubRuntime};
use crate::workload_accounts::canonical_decimal;
use crate::workload_accounts::{Kind, WORKLOAD_UIDS, WORKSPACE_INTAKE, WORKSPACE_SPECS};

/// The argument after the exact `td-jail` argv[0] that selects this kind.
pub(crate) const WORKSPACE_ARG: &str = "--workspace";
/// Where the spec's programs are bound, each under its own file name.
pub(crate) const PROGRAM_DIR: &str = "/opt/workspace/bin";
pub(crate) const ALIAS_DIR: &str = "/opt/workspace/path";
/// The entry's `PATH`, after the spec's own directories, and its
/// `TMPDIR` and `LANG`; `HOME` is the spec's home. The system profiles
/// of store-based hosts (Guix, NixOS) follow the ordinary directories;
/// on another host they are absent and cost a lookup.
pub(crate) const PATH: &str =
    "/usr/local/bin:/usr/bin:/bin:/run/current-system/profile/bin:/run/current-system/sw/bin";
/// `PATH`'s tail with a runtime at `/usr`: td's own `/bin` first, then
/// the runtime's.
pub(crate) const RUNTIME_PATH: &str = "/bin:/usr/bin";
pub(crate) const TMPDIR: &str = "/tmp";
/// Persistent development mounts, ordered as the spec's scratch lines.
pub(crate) const SCRATCH_TARGETS: &[&str] = &["/tmp", "/var/tmp", "/dev/shm", "/run"];
pub(crate) const LANG: &str = "C.UTF-8";
pub(crate) const TERM: &str = "dumb";
/// The host's trees bound read-only and executable, as the host has them:
/// a link stays the same link, and a tree the host lacks is absent. `gnu`
/// and `nix` are the stores where Guix and NixOS keep every program, and
/// `td` td's, which its `/bin` links into.
pub(crate) const SYSTEM_TREES: &[&str] = &[
    "bin", "gnu", "lib", "lib32", "lib64", "libx32", "nix", "sbin", "td", "usr",
];
/// The system trees one of which the host must have as a directory:
/// where its programs are. td has no `/usr`; another host has no `/td`.
pub(crate) const PROGRAM_TREES: &[&str] = &["td", "usr"];
/// The system trees a spec's runtime replaces: its tree at `/usr`, and
/// `/lib` and `/lib64` links into it. Nothing of the host's is bound there.
/// One list with the launchers that leave them out of a spec.
pub(crate) const RUNTIME_TREES: &[&str] = flathub_app::WORKSPACE_RUNTIME_TREES;
/// td's one account, the human's: where the product configuration is
/// installed, the kind serves it alone, whose td-agent launches it
/// (APPLICATIONS.md §X.8).
const TD_ACCOUNT_UID: u32 = 1000;
/// The caller's user namespace's map of ids.
const UID_MAP: &str = "/proc/self/uid_map";
/// More than any map the kernel writes: at most 340 lines of three ids.
const MAX_UID_MAP_BYTES: u64 = 16 * 1024;
/// The host's links under `/run` repeated in the jail's otherwise empty
/// one: a store-based host's current system, which its `/etc` entries and
/// `PATH` name.
pub(crate) const RUN_LINKS: &[&str] = &["current-system"];
/// The host `/etc` entries bound read-only beside the synthesized account,
/// group, host and name-service files: what a compiler, git and a shell
/// read, and no credential.
pub(crate) const HOST_ETC: &[&str] = &[
    "alternatives",
    "ca-certificates",
    "ca-certificates.conf",
    "gitconfig",
    "inputrc",
    "ld.so.cache",
    "ld.so.conf",
    "ld.so.conf.d",
    "localtime",
    "mime.types",
    "os-release",
    "pki",
    "profile",
    "protocols",
    "services",
    "shells",
    "ssl",
    "td-rust.tsv",
    "terminfo",
    "timezone",
];
/// The files the plan writes into its `/etc`.
pub(crate) const SYNTHESIZED_ETC: &[&str] =
    &["group", "hostname", "hosts", "nsswitch.conf", "passwd"];
/// What no admitted directory may be, contain or lie inside: the jail's
/// own mount points and the host trees bound for it.
const RESERVED: &[&str] = &[
    "/bin",
    "/boot",
    "/dev",
    "/etc",
    "/gnu",
    "/lib",
    "/lib32",
    "/lib64",
    "/libx32",
    "/nix",
    "/opt",
    "/proc",
    "/run",
    "/sbin",
    "/sys",
    "/td",
    "/tmp",
    "/usr",
    "/var/tmp",
    // td-svc's logs, which the human account's svc-log group may read.
    "/var/log/svc",
];
const FORMAT_LINE: &str = "format=1";
const O_NOFOLLOW: i32 = 0o400_000;
const O_NONBLOCK: i32 = 0o4000;
/// `/proc/net/unix`'s type and state for a connected stream socket.
const UNIX_STREAM: &str = "0001";
const UNIX_CONNECTED: &str = "03";
const MAX_SPEC_BYTES: u64 = 64 * 1024;
const MAX_PATH_BYTES: usize = 4096;
pub(crate) const MAX_PROGRAMS: usize = 4;
/// The most `path` directories a spec names.
pub(crate) const MAX_PATH_DIRECTORIES: usize = 16;
pub(crate) const MAX_TREES: usize = 32;
/// The most mounts the git chains add, past the directories: a
/// repository takes 11 and 4 for each of at most `MAX_TREES` linked
/// worktrees, 139 at most, so this holds 7 such repositories.
pub(crate) const MAX_CHAIN: usize = 1024;
/// The most bytes a plan's paths take in stage 2's argv, each counted
/// with `ARGV_WORD` for its pointer, its terminator and the word beside
/// it, so a plan too large is refused by name at admission rather than
/// failing to start. The kernel allows a quarter of the stack limit,
/// 2 MiB at the usual 8 MiB; this assumes at least 2 MiB of stack.
const MAX_PLAN_BYTES: usize = 384 * 1024;
const ARGV_WORD: usize = 32;
/// A workspace repository's entries that say what git executes or where
/// it looks, bound read-only over it (td-agent/DESIGN.md §8, The git
/// mount chain): files, then directories.
pub(crate) const REPOSITORY_FILES: &[&str] = &["commondir", "config", "config.worktree", "shallow"];
pub(crate) const REPOSITORY_DIRECTORIES: &[&str] = &["branches", "hooks", "info", "remotes"];
/// A linked worktree's entries under `worktrees/<id>/`, bound read-only.
pub(crate) const LINKED_FILES: &[&str] = &["commondir", "config.worktree", "gitdir"];
/// Filesystems no admitted tree may carry in below it: each reaches past
/// the jail (procfs's `/proc/<pid>/root` and `environ` of the caller's own
/// processes, the same uid) or the kernel.
const PSEUDO_FILESYSTEMS: &[&str] = &[
    "autofs",
    "binfmt_misc",
    "bpf",
    "cgroup",
    "cgroup2",
    "configfs",
    "debugfs",
    "devpts",
    "devtmpfs",
    "efivarfs",
    "fusectl",
    "mqueue",
    "nsfs",
    "proc",
    "pstore",
    "securityfs",
    "sysfs",
    "tracefs",
];
const MAX_PROGRAM_NAME: usize = 64;

/// A program bound read-only and executable under `PROGRAM_DIR`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Program {
    pub(crate) name: String,
    pub(crate) source: PathBuf,
    pub(crate) device: u64,
    pub(crate) inode: u64,
}

impl Program {
    pub(crate) fn target(&self) -> PathBuf {
        Path::new(PROGRAM_DIR).join(&self.name)
    }
}

/// An admitted spec: what `transition::launch_workspace` performs.
#[derive(Debug)]
pub(crate) struct WorkspacePlan {
    pub(crate) development: bool,
    pub(crate) shared_network: bool,
    pub(crate) scratch: Vec<FilesystemGrant>,
    pub(crate) uid: u32,
    pub(crate) gid: u32,
    /// The first program, which is the entry.
    pub(crate) programs: Vec<Program>,
    /// Directories of the bound system trees put first on the entry's
    /// `PATH`, in order.
    pub(crate) path: Vec<PathBuf>,
    pub(crate) aliases: Vec<crate::host_path::Alias>,
    /// Read-write, not executable.
    pub(crate) home: FilesystemGrant,
    /// Read-write and executable: what the tools build and run. A
    /// checkout's are last.
    pub(crate) worktrees: Vec<FilesystemGrant>,
    /// Workspace git repositories: read-write, not executable.
    pub(crate) repositories: Vec<FilesystemGrant>,
    /// The git mount chain, in mount order, each inside a repository or a
    /// checkout and bound over it: read-only or read-write, not
    /// executable.
    pub(crate) chain: Vec<FilesystemGrant>,
    /// Read-only or read-write, not executable.
    pub(crate) shared: Vec<FilesystemGrant>,
    pub(crate) arguments: Vec<OsString>,
    /// The spec's runtime, held; none for a native workspace.
    pub(crate) runtime: Option<WorkspaceRuntime>,
    /// Whether the channel is a connection root's intake accepted, a
    /// ledger caller's, rather than its launcher's unnamed socket pair.
    pub(crate) intake: bool,
}

/// Who may launch the kind, which decides whose spec, home and grants an
/// instance takes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Caller {
    /// td's account, the human, launching itself (td-agent's workspaces,
    /// until WORKLOADS.md step 8 gives them accounts), or any nonzero
    /// identity on a development host: its own spec, its passwd home, the
    /// spec's directories, and a socket pair to its launcher. Its spec may
    /// not name the offline profile, which only a sandbox's names: root's
    /// intake starts a sandbox as its account, and td-sandbox calls no
    /// td-jail.
    Human,
    /// A sandbox's or workspace's ledger account, which root's workspace
    /// intake started (td-authd/DESIGN.md, "Workspace launch intake"):
    /// root's spec, the account's own home, the views its manifest lists,
    /// and the intake's connection.
    Account(Kind),
}

/// A runtime store's `current`, held for the launch, and its tree, which
/// the plan binds at `/usr`.
#[derive(Debug)]
pub(crate) struct WorkspaceRuntime {
    pub(crate) files: PathBuf,
    /// The shared lock stage 1 keeps through its descriptor sweep.
    pub(crate) hold: fs::File,
}

impl WorkspacePlan {
    /// Where the entry starts: the first worktree, else the home.
    pub(crate) fn working_directory(&self) -> &Path {
        self.worktrees
            .first()
            .map_or(&self.home.target, |tree| &tree.target)
    }
}

/// The entry's whole environment: `path` first on its `PATH`, and with a
/// `runtime` td's `/bin` before the runtime's. No `LD_LIBRARY_PATH`: td's
/// programs find their libraries by their own store interpreter and
/// `RUNPATH`.
pub(crate) fn environment(
    home: &Path,
    path: &[PathBuf],
    runtime: bool,
) -> Vec<(OsString, OsString)> {
    let mut search = OsString::new();
    search.push(ALIAS_DIR);
    search.push(":");
    for directory in path {
        search.push(directory.as_os_str());
        search.push(":");
    }
    search.push(PROGRAM_DIR);
    search.push(":");
    search.push(if runtime { RUNTIME_PATH } else { PATH });
    vec![
        ("HOME".into(), home.as_os_str().to_os_string()),
        ("LANG".into(), LANG.into()),
        ("PATH".into(), search),
        ("TERM".into(), TERM.into()),
        ("TMPDIR".into(), TMPDIR.into()),
    ]
}

/// Whether `path` may be one of the entry's `PATH` directories, by its
/// name: absolute and canonical in spelling, inside one of the system
/// trees every instance binds, and holding no `:`, which would split it.
pub(crate) fn path_directory_named(path: &Path) -> bool {
    authority::validate_filesystem_target(path).is_ok()
        && !path.as_os_str().as_bytes().contains(&b':')
        && SYSTEM_TREES
            .iter()
            .any(|tree| path_is_same_or_child(path, &Path::new("/").join(tree)))
}

/// Whether `path` lies in a tree a runtime replaces, where a host path
/// would name the runtime's file inside the jail.
pub(crate) fn in_runtime_tree(path: &Path) -> bool {
    RUNTIME_TREES
        .iter()
        .any(|tree| path_is_same_or_child(path, &Path::new("/").join(tree)))
}

pub(crate) fn is_workspace_argument(argument: &OsStr) -> bool {
    argument == WORKSPACE_ARG
}

/// The spec as written, before admission.
#[derive(Debug, Default, Eq, PartialEq)]
struct Spec {
    /// Set only after a ledger caller's root-owned spec has passed
    /// `require_account_spec`; never parsed from a spec line.
    account_private: bool,
    development: bool,
    shared_network: bool,
    runtime: Option<FlathubRuntime>,
    scratch: Vec<PathBuf>,
    programs: Vec<PathBuf>,
    aliases: Vec<crate::host_path::Alias>,
    path: Vec<PathBuf>,
    home: PathBuf,
    worktrees: Vec<PathBuf>,
    checkouts: Vec<PathBuf>,
    repositories: Vec<PathBuf>,
    read: Vec<PathBuf>,
    write: Vec<PathBuf>,
    /// A ledger caller's views, root's, bound as `write` and `read` are.
    views: Vec<FilesystemGrant>,
    /// Whether the spec's directories are bound from views alone: a
    /// workspace account's, whose lines name the human's directories.
    views_only: bool,
    /// Its worktrees, its checkouts last, and its repositories, each the
    /// view its line names, bound at the human's path from the view.
    viewed_trees: Vec<FilesystemGrant>,
    viewed_repositories: Vec<FilesystemGrant>,
}

impl Spec {
    /// Whether a `worktree`, `checkout`, `repository`, `read` or `write`
    /// line names `target`.
    fn names_directory(&self, target: &Path) -> bool {
        [
            &self.worktrees,
            &self.checkouts,
            &self.repositories,
            &self.read,
            &self.write,
        ]
        .iter()
        .any(|lines| lines.iter().any(|line| line == target))
    }
}

/// The exact, ordered keyfile: `format=1`, an optional `profile`, an
/// optional `runtime` naming a declared Flathub runtime, one `entry`, any
/// `program`s and `path` directories, one `home`, then any `worktree`,
/// `checkout`, `repository`, `read` and `write` lines, each group in that
/// order.
fn parse_spec(text: &str) -> io::Result<Spec> {
    let mut lines = text.lines().peekable();
    if lines.next() != Some(FORMAT_LINE) {
        return Err(invalid("workspace spec does not begin with format=1"));
    }
    let profile = lines.peek().copied();
    let development = matches!(
        profile,
        Some("profile=development" | "profile=development-offline")
    );
    let shared_network = profile == Some("profile=development");
    if development {
        lines.next();
    }
    const ORDER: &[&str] = &[
        "runtime",
        "entry",
        "program",
        "alias",
        "path",
        "home",
        "scratch",
        "worktree",
        "checkout",
        "repository",
        "read",
        "write",
    ];
    let mut spec = Spec {
        development,
        shared_network,
        ..Spec::default()
    };
    let mut last = 0usize;
    let mut home = None;
    let mut entry = false;
    for line in lines {
        let (key, value) = line
            .split_once('=')
            .ok_or_else(|| invalid(format!("workspace spec line {line:?} has no `=`")))?;
        let rank = ORDER
            .iter()
            .position(|known| *known == key)
            .ok_or_else(|| invalid(format!("workspace spec has an unknown key {key:?}")))?;
        let repeated = match key {
            "runtime" => spec.runtime.is_some(),
            "entry" => entry,
            "home" => home.is_some(),
            _ => false,
        };
        if rank < last || repeated {
            return Err(invalid(format!(
                "workspace spec key {key:?} is out of order or repeated"
            )));
        }
        last = rank;
        if key == "runtime" {
            spec.runtime = Some(flathub_app::flathub_runtime(value).ok_or_else(|| {
                invalid(format!(
                    "workspace spec names {value:?}, which is no declared runtime"
                ))
            })?);
            continue;
        }
        if key == "alias" {
            let (name, source) = value
                .split_once(' ')
                .ok_or_else(|| invalid("workspace alias needs a name and source"))?;
            if !crate::host_path::valid_name(name) {
                return Err(invalid("workspace alias has an invalid command name"));
            }
            spec.aliases.push(crate::host_path::Alias {
                name: name.into(),
                source: spec_path(key, source)?,
            });
            continue;
        }
        let path = spec_path(key, value)?;
        match key {
            "entry" => {
                entry = true;
                spec.programs.push(path);
            }
            "program" if entry => spec.programs.push(path),
            "program" => return Err(invalid("workspace spec names a program before its entry")),
            "path" => spec.path.push(path),
            "home" => home = Some(path),
            "scratch" => spec.scratch.push(path),
            "worktree" => spec.worktrees.push(path),
            "checkout" => spec.checkouts.push(path),
            "repository" => spec.repositories.push(path),
            "read" => spec.read.push(path),
            _ => spec.write.push(path),
        }
    }
    if !entry {
        return Err(invalid("workspace spec names no entry"));
    }
    spec.home = home.ok_or_else(|| invalid("workspace spec names no home"))?;
    if !spec.scratch.is_empty()
        && (!spec.development || spec.scratch.len() != SCRATCH_TARGETS.len())
    {
        return Err(invalid(
            "persistent scratch needs development and four directories",
        ));
    }
    if spec.aliases.len() > crate::host_path::MAX_ALIASES {
        return Err(invalid("workspace spec names more than 64 PATH aliases"));
    }
    if spec.path.len() > MAX_PATH_DIRECTORIES {
        return Err(invalid(format!(
            "workspace spec names more than {MAX_PATH_DIRECTORIES} path directories"
        )));
    }
    if spec.programs.len() > MAX_PROGRAMS {
        return Err(invalid(format!(
            "workspace spec names more than {MAX_PROGRAMS} programs"
        )));
    }
    if spec.worktrees.len()
        + spec.checkouts.len()
        + spec.repositories.len()
        + spec.read.len()
        + spec.write.len()
        > MAX_TREES
    {
        return Err(invalid(format!(
            "workspace spec names more than {MAX_TREES} directories"
        )));
    }
    Ok(spec)
}

fn spec_path(key: &str, value: &str) -> io::Result<PathBuf> {
    let path = PathBuf::from(value);
    if value.len() > MAX_PATH_BYTES || value.contains('\0') {
        return Err(invalid(format!(
            "workspace spec {key} is not a bounded path"
        )));
    }
    authority::validate_filesystem_target(&path)
        .map_err(|_| invalid(format!("workspace spec {key} {value:?} is not canonical")))?;
    Ok(path)
}

/// Reads and admits `spec_path` for the calling identity (§C's source
/// checks, with the workspace kind's departures, APPLICATIONS.md §C).
pub(crate) fn resolve<I>(spec_path: &OsStr, arguments: I) -> io::Result<WorkspacePlan>
where
    I: Iterator<Item = OsString>,
{
    let (uid, gid) = authority::caller_identity()?;
    if uid == 0 || gid == 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "the workspace kind requires a nonzero identity",
        ));
    }
    let caller = require_account(
        (uid, gid),
        Path::new(UID_MAP),
        Path::new(authority::CONFIG_PATH),
        crate::workload_accounts::load,
    )?;
    let spec_path = Path::new(spec_path);
    let text = read_spec(spec_path, uid, caller)?;
    let mut spec = parse_spec(&text)?;
    if caller == Caller::Human {
        require_human_spec(&spec)?;
    }
    if let Caller::Account(kind) = caller {
        require_account_spec(&spec, kind, uid)?;
        spec.account_private = true;
        let mountinfo = fs::read_to_string("/proc/self/mountinfo")?;
        // A workspace binds, and examines, only the views its spec's lines
        // name: another running instance's may be released or published
        // under it. A sandbox binds every view its manifest lists.
        let views = authority::workspace_views(
            &mountinfo,
            uid,
            kind,
            require_view_target,
            &reserved_identities(&mountinfo, RESERVED)?,
            &|target| kind != Kind::Workspace || spec.names_directory(target),
        )?;
        match kind {
            Kind::Workspace => select_views(
                &mut spec,
                views,
                &crate::primary_account::load()?.persistent_home(),
            )?,
            Kind::Sandbox | Kind::Application => spec.views = views,
        }
    }
    let runtime = spec.runtime;
    // A ledger account's home is the spec's, its own; the human's is the
    // one no directory may be or contain.
    let real_home = authority::account_home(match caller {
        Caller::Human => uid,
        Caller::Account(_) => TD_ACCOUNT_UID,
    })?;
    // The home as the account names it and as it resolves, which differ
    // where `/home` is a link (`/var/home` on OSTree hosts).
    let real_home = match fs::canonicalize(&real_home) {
        Ok(resolved) if resolved != real_home => vec![real_home, resolved],
        _ => vec![real_home],
    };
    let arguments = authority::collect_arguments(arguments)?;
    let mut plan = admit(spec, spec_path, uid, gid, &real_home, arguments, RESERVED)?;
    plan.intake = matches!(caller, Caller::Account(_));
    if let Some(runtime) = runtime {
        plan.runtime = Some(spec_runtime(runtime, &td_runtime_source())?);
    }
    Ok(plan)
}

/// Where a spec's runtime may come from: td, with its product
/// configuration at `config`, and the runtime store below `store` that
/// `owner` keeps.
pub(crate) struct RuntimeSource<'a> {
    pub(crate) config: &'a Path,
    pub(crate) store: &'a Path,
    pub(crate) owner: u32,
}

/// td's, compiled: no caller names another.
pub(crate) fn td_runtime_source() -> RuntimeSource<'static> {
    let (store, owner) = crate::flathub::WORKSPACE_RUNTIME_STORE;
    RuntimeSource {
        config: Path::new(authority::CONFIG_PATH),
        store: Path::new(store),
        owner,
    }
}

/// The spec's `runtime`, on td only, its store's `current` held.
fn spec_runtime(runtime: FlathubRuntime, source: &RuntimeSource) -> io::Result<WorkspaceRuntime> {
    require_td_for_runtime(source.config)?;
    let held = crate::flathub::hold_workspace_runtime(runtime, source.store, source.owner)?;
    Ok(WorkspaceRuntime {
        files: held.files,
        hold: held.generation.hold,
    })
}

/// A runtime replaces `/usr`, where a development host keeps its programs,
/// and its store is td's alone: where no product configuration is
/// installed at `config`, the spec may name none.
fn require_td_for_runtime(config: &Path) -> io::Result<()> {
    match fs::symlink_metadata(config) {
        Ok(_) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "a workspace runtime is td's alone: on a development host it would replace the \
             host's /usr, and the runtime store exists only on td",
        )),
        Err(error) => Err(io::Error::new(
            error.kind(),
            format!("inspect product application configuration: {error}"),
        )),
    }
}

/// Where the product configuration at `config` is installed, td's, the
/// kind serves td's account and the sandbox and workspace accounts of the
/// ledger `ledger` reads, as the initial user namespace knows them
/// (`uid_map`, the caller's map): a uid is the caller's namespace's, and
/// an application's own is the account's. A ledger account's gid is its
/// uid. On a development host, any nonzero identity, as td's account. It
/// grants nothing the caller could not do itself: td-jail is no more
/// privileged than its caller, and the kind only narrows what the
/// caller's tools reach.
fn require_account(
    (uid, gid): (u32, u32),
    uid_map: &Path,
    config: &Path,
    ledger: impl FnOnce() -> Result<crate::workload_accounts::Ledger, String>,
) -> io::Result<Caller> {
    let initial = initial_user_namespace(uid_map)?;
    if uid == TD_ACCOUNT_UID && initial {
        return Ok(Caller::Human);
    }
    if WORKLOAD_UIDS.contains(&uid) && initial && configuration_installed(config)? {
        let ledger = ledger().map_err(|error| {
            crate::error(
                io::ErrorKind::PermissionDenied,
                format_args!("the workspace kind reads the account ledger: {error}"),
            )
        })?;
        let kind = [Kind::Sandbox, Kind::Workspace]
            .into_iter()
            .find(|kind| ledger.name(*kind, uid).is_some())
            .filter(|_| gid == uid)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "the workspace kind's caller holds no sandbox or workspace ledger account",
                )
            })?;
        return Ok(Caller::Account(kind));
    }
    authority::require_no_configuration_at(
        config,
        "the workspace kind for any identity but td's account or a sandbox's or \
         workspace's ledger account in the initial user namespace",
    )?;
    Ok(Caller::Human)
}

/// Whether td's product configuration is installed at `config`.
fn configuration_installed(config: &Path) -> io::Result<bool> {
    match fs::symlink_metadata(config) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(crate::error(
            error.kind(),
            format_args!("inspect product application configuration: {error}"),
        )),
    }
}

/// A ledger caller's spec, which root wrote: the account's own home. A
/// sandbox's names no directory, since the account has none of its own and
/// reaches the human's through every view its manifest lists. A
/// workspace's may name the four persistent scratch directories of its own
/// tree, in their order (`account_scratch`), and directories of the human's
/// by role, each of which must be a view its manifest lists
/// (`select_views`).
fn require_account_spec(spec: &Spec, kind: Kind, uid: u32) -> io::Result<()> {
    let names_directories = !(spec.worktrees.is_empty()
        && spec.checkouts.is_empty()
        && spec.repositories.is_empty()
        && spec.read.is_empty()
        && spec.write.is_empty());
    match kind {
        Kind::Workspace => {
            if !spec.scratch.is_empty() && !account_scratch(&spec.scratch, uid) {
                return Err(invalid(
                    "a workspace account's persistent scratch is its own scratch tree's",
                ));
            }
        }
        Kind::Sandbox | Kind::Application => {
            if !spec.scratch.is_empty() || names_directories {
                return Err(invalid(
                    "a ledger account's workspace spec names no directory; its grants are its views",
                ));
            }
        }
    }
    if spec.home != kind.home(uid) {
        return Err(crate::error(
            io::ErrorKind::InvalidInput,
            format_args!(
                "a ledger account's workspace home is {}, not {}",
                kind.home(uid).display(),
                spec.home.display()
            ),
        ));
    }
    Ok(())
}

/// Whether `scratch` is exactly the workspace account `uid`'s persistent
/// scratch directories, root's tree of them below
/// `/var/lib/td/workload-scratch/UID` (td-authd/DESIGN.md, "Workload
/// account ledger"), in the order a spec's scratch lines bind them. Spec
/// paths are canonical, so their names compare exactly.
fn account_scratch(scratch: &[PathBuf], uid: u32) -> bool {
    use crate::workload_accounts::{DIRECTORY, SCRATCH, SCRATCH_NAMES};
    let ours = |path: &Path, name: &str| {
        let Ok(below) = path.strip_prefix(DIRECTORY) else {
            return false;
        };
        let mut names = below.iter().map(OsStr::to_str);
        let account = |digits: &str| {
            canonical_decimal(digits) && digits.parse::<u32>().is_ok_and(|n| n == uid)
        };
        names.next() == Some(Some(SCRATCH))
            && names.next().flatten().is_some_and(account)
            && names.next() == Some(Some(name))
            && names.next().is_none()
    };
    scratch.len() == SCRATCH_NAMES.len()
        && scratch
            .iter()
            .zip(SCRATCH_NAMES)
            .all(|(path, name)| ours(path, name))
}

/// A workspace account's directories, each its line's view: for every
/// `worktree`, `checkout`, `repository`, `read` and `write` line, the view
/// its manifest lists whose target, the human's directory below `human`,
/// the line names, at the access its role takes, of a path the role admits
/// (`workload_grants::validate_role`, td-agent's own trees in their one
/// role each). A line no view answers refuses the launch; a view no line
/// names, another running instance's, is not bound. Read and write views
/// become shared directories; the rest are bound from their views at the
/// paths their lines name.
fn select_views(spec: &mut Spec, mut views: Vec<FilesystemGrant>, human: &Path) -> io::Result<()> {
    use crate::workload_grants::{validate_role, Access, Role};
    let oom = |_| io::Error::from(io::ErrorKind::OutOfMemory);
    spec.views_only = true;
    for role in Role::ALL {
        let targets = match role {
            Role::Worktree => &spec.worktrees,
            Role::Checkout => &spec.checkouts,
            Role::Repository => &spec.repositories,
            Role::Read => &spec.read,
            Role::Write => &spec.write,
        };
        for target in targets {
            let relative = target
                .strip_prefix(human)
                .ok()
                .and_then(Path::to_str)
                .ok_or_else(|| {
                    crate::error(
                        io::ErrorKind::InvalidInput,
                        format_args!(
                            "workspace {} {} is not below the human's home",
                            role.as_str(),
                            target.display()
                        ),
                    )
                })?;
            validate_role(*role, relative.as_bytes()).map_err(invalid)?;
            let read_only = role.access() == Access::ReadOnly;
            let at = views
                .iter()
                .position(|view| view.target == *target && view.read_only == read_only)
                .ok_or_else(|| {
                    crate::error(
                        io::ErrorKind::InvalidInput,
                        format_args!(
                            "workspace {} {} is no view the account's manifest lists at its access",
                            role.as_str(),
                            target.display()
                        ),
                    )
                })?;
            let view = views.swap_remove(at);
            let into = match role {
                Role::Read | Role::Write => &mut spec.views,
                Role::Worktree | Role::Checkout => &mut spec.viewed_trees,
                Role::Repository => &mut spec.viewed_repositories,
            };
            crate::allocation::push(into, view).map_err(oom)?;
        }
    }
    spec.read.clear();
    spec.write.clear();
    Ok(())
}

/// A spec of td's account may not name `development-offline`: that is a
/// sandbox's profile, and a sandbox runs as its own account through
/// root's workspace intake (td-sandbox/DESIGN.md), never as the human.
fn require_human_spec(spec: &Spec) -> io::Result<()> {
    if spec.development && !spec.shared_network {
        return Err(invalid(
            "profile=development-offline is a sandbox's, which runs as its own account \
             through root's workspace intake, not as td's account",
        ));
    }
    Ok(())
}

/// Where a ledger caller's view of the human's directory may be bound: at
/// a canonical path that is no tree the jail keeps for itself.
pub(crate) fn require_view_target(target: &Path) -> io::Result<()> {
    refuse_reserved(target, &[], RESERVED)
}

/// Whether the map at `uid_map` is the initial user namespace's: the
/// whole range of ids, each itself.
fn initial_user_namespace(uid_map: &Path) -> io::Result<bool> {
    let mut text = String::new();
    fs::File::open(uid_map)?
        .take(MAX_UID_MAP_BYTES + 1)
        .read_to_string(&mut text)?;
    let fields: Vec<&str> = text.split_ascii_whitespace().collect();
    Ok(fields == ["0", "0", "4294967295"])
}

/// The spec: a direct regular file no one but its owner can write,
/// bounded. The human's own is the caller's; a ledger caller's is root's,
/// of one link in a directory of root's no one else can write, so neither
/// the account nor a link it made can stand in for it.
fn read_spec(path: &Path, uid: u32, caller: Caller) -> io::Result<String> {
    match caller {
        Caller::Human => read_spec_of(path, uid, false),
        Caller::Account(_) if account_spec_path(path, uid) => read_spec_of(path, 0, true),
        Caller::Account(_) => Err(crate::error(
            io::ErrorKind::PermissionDenied,
            format_args!(
                "a ledger account's workspace spec lies in root's {WORKSPACE_SPECS}/{uid}, not at {}",
                path.display()
            ),
        )),
    }
}

/// Whether `path` is a launch spec root writes for `uid`:
/// `WORKSPACE_SPECS/UID/N`, N canonical decimal, as td-authd's
/// `workspace-exec` admits it.
fn account_spec_path(path: &Path, uid: u32) -> bool {
    path.to_str()
        .and_then(|path| path.strip_prefix(WORKSPACE_SPECS))
        .and_then(|rest| rest.strip_prefix('/'))
        .and_then(|rest| rest.split_once('/'))
        .is_some_and(|(owner, id)| {
            canonical_decimal(owner)
                && owner.parse::<u32>() == Ok(uid)
                && canonical_decimal(id)
                && id.parse::<u64>().is_ok()
        })
}

/// `read_spec` for a spec `owner` owns, and with `sealed` of one link in a
/// directory `owner` owns that no one else can write.
fn read_spec_of(path: &Path, owner: u32, sealed: bool) -> io::Result<String> {
    if !path.is_absolute() {
        return Err(invalid("workspace spec path is not absolute"));
    }
    // The file opened, not followed, is the one checked and read.
    let mut file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(O_NOFOLLOW | O_NONBLOCK)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.file_type().is_file()
        || metadata.uid() != owner
        || metadata.permissions().mode() & 0o022 != 0
        || fs::canonicalize(path)? != path
    {
        return Err(invalid(format!(
            "workspace spec {} is not a direct regular file only its owner, {}, can write",
            path.display(),
            if sealed { "root" } else { "the caller" }
        )));
    }
    if sealed {
        let directory = path
            .parent()
            .map(fs::symlink_metadata)
            .transpose()?
            .ok_or_else(|| invalid("workspace spec has no directory"))?;
        if metadata.nlink() != 1
            || !directory.is_dir()
            || directory.uid() != owner
            || directory.permissions().mode() & 0o022 != 0
        {
            return Err(crate::error(
                io::ErrorKind::InvalidInput,
                format_args!(
                    "workspace spec {} is not root's one link in a directory of root's",
                    path.display()
                ),
            ));
        }
    }
    let mut text = String::new();
    (&mut file)
        .take(MAX_SPEC_BYTES + 1)
        .read_to_string(&mut text)?;
    if text.len() as u64 > MAX_SPEC_BYTES {
        return Err(invalid("workspace spec is too large"));
    }
    Ok(text)
}

fn admit(
    mut spec: Spec,
    spec_path: &Path,
    uid: u32,
    gid: u32,
    real_home: &[PathBuf],
    arguments: Vec<OsString>,
    reserved: &[&str],
) -> io::Result<WorkspacePlan> {
    let runtime = spec.runtime.is_some();
    let mut alias_names = std::collections::BTreeSet::new();
    for alias in &spec.aliases {
        if !alias_names.insert(alias.name.clone()) {
            return Err(invalid("workspace PATH alias is named twice"));
        }
        refuse_runtime_tree("PATH alias source", &alias.source, runtime)?;
        admit_alias(alias)?;
    }
    let mut programs: Vec<Program> = Vec::new();
    for source in spec.programs {
        let program = admit_program(&source)?;
        if programs.iter().any(|known| known.name == program.name) {
            return Err(invalid(format!(
                "workspace programs share the file name {:?}",
                program.name
            )));
        }
        programs.push(program);
    }
    let mut path: Vec<PathBuf> = Vec::new();
    for directory in spec.path {
        refuse_runtime_tree("path directory", &directory, runtime)?;
        admit_path_directory(&directory)?;
        if path.contains(&directory) {
            return Err(invalid(format!(
                "workspace path directory {} is named twice",
                directory.display()
            )));
        }
        path.push(directory);
    }
    let home = admit_directory(&spec.home, false)?;
    let home_metadata = fs::symlink_metadata(&spec.home)?;
    if home_metadata.uid() != uid || home_metadata.permissions().mode() & 0o777 != 0o700 {
        return Err(invalid(format!(
            "workspace home {} is not the caller's private 0700 directory",
            spec.home.display()
        )));
    }
    // A workspace account's are its views, bound at the human's paths; any
    // other caller's are its own directories.
    let worktrees = if spec.views_only {
        std::mem::take(&mut spec.viewed_trees)
    } else {
        spec.worktrees
            .iter()
            .chain(spec.checkouts.iter())
            .map(|tree| admit_directory(tree, false))
            .collect::<io::Result<Vec<_>>>()?
    };
    let repositories = if spec.views_only {
        std::mem::take(&mut spec.viewed_repositories)
    } else {
        spec.repositories
            .iter()
            .map(|tree| admit_directory(tree, false))
            .collect::<io::Result<Vec<_>>>()?
    };
    // Beside repositories, a git worktree is a checkout, whose `.git` is
    // protected: a plain worktree's could be pointed at a repository the
    // jail made.
    if !repositories.is_empty() {
        for tree in worktrees.iter().take(spec.worktrees.len()) {
            match fs::symlink_metadata(joined(&tree.source, ".git".as_ref())?) {
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                _ => {
                    return Err(invalid(format!(
                    "workspace worktree {} holds a `.git` beside repositories; name it a checkout",
                    tree.target.display()
                )))
                }
            }
        }
    }
    // Each chain bound from its tree's source, a view's for a workspace
    // account, at its tree's target.
    let mut chain = Vec::new();
    for checkout in worktrees.iter().skip(spec.worktrees.len()) {
        chain.push(admit_git_entry(
            joined(&checkout.source, ".git".as_ref())?,
            joined(&checkout.target, ".git".as_ref())?,
            FilesystemSourceKind::File,
            true,
        )?);
    }
    for repository in &repositories {
        chain.extend(repository_chain(&repository.source, &repository.target)?);
    }
    if chain.len() > MAX_CHAIN {
        return Err(invalid(format!(
            "workspace git chains need more than {MAX_CHAIN} mounts"
        )));
    }
    check_plan_bytes(
        std::iter::once(&spec.home)
            .chain(&path)
            .chain(&spec.worktrees)
            .chain(&spec.checkouts)
            .chain(&spec.repositories)
            .chain(&spec.read)
            .chain(&spec.write)
            .chain(spec.views.iter().map(|grant| &grant.target))
            .chain(chain.iter().map(|grant| &grant.target)),
        MAX_PLAN_BYTES,
    )?;
    let mut shared = spec
        .read
        .iter()
        .map(|tree| admit_directory(tree, true))
        .collect::<io::Result<Vec<_>>>()?;
    for tree in &spec.write {
        shared.push(admit_directory(tree, false)?);
    }
    // A ledger caller's views: each source root's published mount, its
    // target the human's path, held to every check a shared directory is.
    // Moved, not copied: `first_view` finds them again below.
    let first_view = shared.len();
    for view in std::mem::take(&mut spec.views) {
        crate::allocation::push(&mut shared, view)
            .map_err(|_| io::Error::from(io::ErrorKind::OutOfMemory))?;
    }
    let scratch = spec
        .scratch
        .iter()
        .zip(SCRATCH_TARGETS)
        .map(|(source, target)| {
            let mut grant = admit_directory(source, false)?;
            let metadata = fs::symlink_metadata(source)?;
            if metadata.uid() != uid || metadata.permissions().mode() & 0o7777 != 0o1777 {
                return Err(invalid("persistent scratch must be caller-owned mode 1777"));
            }
            grant.target = PathBuf::from(target);
            Ok(grant)
        })
        .collect::<io::Result<Vec<_>>>()?;
    let all: Vec<&FilesystemGrant> = std::iter::once(&home)
        .chain(scratch.iter())
        .chain(worktrees.iter())
        .chain(repositories.iter())
        .chain(shared.iter())
        .collect();
    let mountinfo = fs::read_to_string("/proc/self/mountinfo")?;
    let reserved_mounts = reserved_identities(&mountinfo, reserved)?;
    // Each spelling of the home that exists, by identity; one that cannot
    // be placed refuses the launch rather than going unchecked.
    let mut homes = Vec::new();
    for home in real_home {
        if fs::symlink_metadata(home).is_ok() {
            homes.push(authority::mount_identity_for_path(&mountinfo, home)?);
        }
    }
    // The spec lies outside every tree it grants, by path and through any
    // mount: an instance runs as the caller and could rewrite the next
    // instance's policy.
    let spec_dir = spec_path
        .parent()
        .map(|dir| authority::mount_identity_for_path(&mountinfo, dir))
        .transpose()?;
    // A tree the instance can write holds no program the caller runs:
    // td-jail itself, which runs outside every jail, nor the spec's, which
    // the next instance runs and its launcher may run unconfined.
    let mut held = vec![std::env::current_exe()?];
    held.extend(programs.iter().map(|program| program.source.clone()));
    let held = held
        .into_iter()
        .map(|path| {
            let identity = authority::mount_identity_for_path(&mountinfo, &path)?;
            Ok((path, identity))
        })
        .collect::<io::Result<Vec<_>>>()?;
    let mut identities = Vec::new();
    for grant in &all {
        refuse_reserved(&grant.source, real_home, reserved)?;
        refuse_pseudo_filesystems(&mountinfo, &grant.source)?;
        let mounts = authority::mount_tree_identities(&mountinfo, &grant.source)?;
        // The tree itself and every mount below it, any of which may be a
        // bind of the home or of a directory above it.
        if mounts
            .iter()
            .any(|mount| homes.iter().any(|home| contains_by_identity(mount, home)))
        {
            return Err(invalid(format!(
                "workspace directory {} is or contains the caller's home through a mount",
                grant.source.display()
            )));
        }
        if path_is_same_or_child(spec_path, &grant.source)
            || spec_dir
                .as_ref()
                .is_some_and(|dir| mounts.iter().any(|mount| contains_by_identity(mount, dir)))
        {
            return Err(invalid(format!(
                "workspace spec {} lies in the directory {} it grants",
                spec_path.display(),
                grant.source.display()
            )));
        }
        let writable_program = held.iter().find(|(path, identity)| {
            !grant.read_only
                && (path_is_same_or_child(path, &grant.source)
                    || mounts
                        .iter()
                        .any(|mount| contains_by_identity(mount, identity)))
        });
        if let Some((program, _)) = writable_program {
            return Err(invalid(format!(
                "workspace directory {} is writable and holds the program {}",
                grant.source.display(),
                program.display()
            )));
        }
        // The home as it resolves, last of its spellings (`resolve`), as an
        // application grant's check takes it.
        let account_private = spec.account_private
            && (grant.source == home.source
                || scratch.iter().any(|ours| ours.source == grant.source));
        let forbidden_alias = if account_private {
            let private = authority::mount_identity_for_path(&mountinfo, &grant.source)?;
            aliases_private(&mounts, &reserved_mounts, &private)
        } else {
            aliases_reserved(&mounts, &reserved_mounts, homes.last())
        };
        if forbidden_alias {
            return Err(invalid(format!(
                "workspace directory {} aliases a reserved tree",
                grant.source.display()
            )));
        }
        identities.push((grant.source.clone(), mounts));
    }
    // A view binds at the human's path, not at its source's, so its target
    // must meet no other bind's either: a workspace account's worktrees
    // and repositories are views too.
    let viewed = |trees: usize| if spec.views_only { trees } else { 0 };
    for view in shared
        .iter()
        .skip(first_view)
        .chain(worktrees.iter().take(viewed(worktrees.len())))
        .chain(repositories.iter().take(viewed(repositories.len())))
    {
        let met = all
            .iter()
            .filter(|grant| paths_overlap(&grant.target, &view.target))
            .count();
        if met > 1 {
            return Err(crate::error(
                io::ErrorKind::InvalidInput,
                format_args!(
                    "grant view at {} overlaps another workspace directory",
                    view.target.display()
                ),
            ));
        }
    }
    for (index, (source, mounts)) in identities.iter().enumerate() {
        for (other, other_mounts) in identities.iter().skip(index + 1) {
            if paths_overlap(source, other)
                || authority::mount_identity_sets_overlap(mounts, other_mounts)
            {
                return Err(invalid(format!(
                    "workspace directories {} and {} overlap",
                    source.display(),
                    other.display()
                )));
            }
        }
    }
    Ok(WorkspacePlan {
        development: spec.development,
        shared_network: spec.shared_network,
        scratch,
        uid,
        gid,
        programs,
        path,
        aliases: spec.aliases,
        home,
        worktrees,
        repositories,
        chain,
        shared,
        arguments,
        runtime: None,
        intake: false,
    })
}

/// With a `runtime`, a host path in a tree it replaces would resolve into
/// the runtime inside the jail, so a spec may not name one there.
pub(crate) fn refuse_runtime_tree(what: &str, path: &Path, runtime: bool) -> io::Result<()> {
    if runtime && in_runtime_tree(path) {
        return Err(invalid(format!(
            "workspace {what} {} lies in /usr, /lib or /lib64, which the spec's runtime replaces",
            path.display()
        )));
    }
    Ok(())
}

pub(crate) fn admit_alias(alias: &crate::host_path::Alias) -> io::Result<()> {
    if !crate::host_path::valid_name(&alias.name)
        || !alias.source.is_absolute()
        || !SYSTEM_TREES
            .iter()
            .any(|tree| path_is_same_or_child(&alias.source, &Path::new("/").join(tree)))
        || fs::canonicalize(&alias.source)? != alias.source
    {
        return Err(invalid(
            "workspace PATH alias source is outside the bound tool closure",
        ));
    }
    let meta = fs::symlink_metadata(&alias.source)?;
    if !meta.is_file() || meta.permissions().mode() & 0o111 == 0 {
        return Err(invalid("workspace PATH alias source is not executable"));
    }
    Ok(())
}

/// A `path` directory: named as one may be, and a directory there as it
/// resolves, so what the entry searches is a bound tree's own, not a link
/// out of it.
fn admit_path_directory(directory: &Path) -> io::Result<()> {
    if !path_directory_named(directory) {
        return Err(invalid(format!(
            "workspace path directory {} is not in a bound system tree",
            directory.display()
        )));
    }
    let resolved = fs::canonicalize(directory).map_err(|error| {
        invalid(format!(
            "workspace path directory {}: {error}",
            directory.display()
        ))
    })?;
    if resolved != directory || !fs::metadata(directory)?.is_dir() {
        return Err(invalid(format!(
            "workspace path directory {} is not a directory as it resolves",
            directory.display()
        )));
    }
    Ok(())
}

/// Refuses paths that would take more than `most` bytes of argv.
fn check_plan_bytes<'a>(paths: impl Iterator<Item = &'a PathBuf>, most: usize) -> io::Result<()> {
    let bytes = paths.fold(0usize, |sum, path| {
        sum.saturating_add(path.as_os_str().len())
            .saturating_add(ARGV_WORD)
    });
    if bytes > most {
        return Err(invalid(format!(
            "workspace plan's paths take more than {most} bytes"
        )));
    }
    Ok(())
}

/// A workspace repository's chain, in mount order: its `objects/`
/// read-write with `objects/info/` (which holds `alternates`) read-only
/// on it; its protected files and directories read-only; `worktrees/`
/// read-only; and each `worktrees/<id>/` read-write on that, with its
/// protected files read-only. Every protected entry must exist: its
/// launcher creates them before any instance binds the repository, and an
/// absent one cannot be protected. Each entry is reached below `source`,
/// a workspace account's view of the repository or the repository itself,
/// and bound at the same name below `target`, where the repository is.
fn repository_chain(source: &Path, target: &Path) -> io::Result<Vec<FilesystemGrant>> {
    use FilesystemSourceKind::{Directory, File};
    let entry = |name: &OsStr, kind, read_only| {
        admit_git_entry(
            joined(source, name)?,
            joined(target, name)?,
            kind,
            read_only,
        )
    };
    let mut chain = vec![
        entry("objects".as_ref(), Directory, false)?,
        entry("objects/info".as_ref(), Directory, true)?,
    ];
    for name in REPOSITORY_FILES {
        chain.push(entry(name.as_ref(), File, true)?);
    }
    for name in REPOSITORY_DIRECTORIES {
        chain.push(entry(name.as_ref(), Directory, true)?);
    }
    let linked = joined(source, "worktrees".as_ref())?;
    chain.push(entry("worktrees".as_ref(), Directory, true)?);
    let mut ids = fs::read_dir(&linked)?
        .take(MAX_TREES + 1)
        .map(|entry| entry.map(|entry| entry.file_name()))
        .collect::<io::Result<Vec<_>>>()?;
    if ids.len() > MAX_TREES {
        return Err(invalid(format!(
            "workspace repository {} has more than {MAX_TREES} linked worktrees",
            target.display()
        )));
    }
    ids.sort();
    for id in ids {
        let linked = joined("worktrees".as_ref(), &id)?;
        chain.push(entry(linked.as_os_str(), Directory, false)?);
        for name in LINKED_FILES {
            chain.push(entry(
                joined(&linked, name.as_ref())?.as_os_str(),
                File,
                true,
            )?);
        }
    }
    Ok(chain)
}

/// `base` joined with the relative `name`, as `Path::join` joins them, in
/// fallibly grown storage.
fn joined(base: &Path, name: &OsStr) -> io::Result<PathBuf> {
    let oom = |_| io::Error::from(io::ErrorKind::OutOfMemory);
    let (base, name) = (base.as_os_str().as_bytes(), name.as_bytes());
    let slash = !base.is_empty() && !base.ends_with(b"/");
    let mut bytes = crate::allocation::with_capacity(base.len() + usize::from(slash) + name.len())
        .map_err(oom)?;
    crate::allocation::extend_from_slice(&mut bytes, base).map_err(oom)?;
    if slash {
        crate::allocation::push(&mut bytes, b'/').map_err(oom)?;
    }
    crate::allocation::extend_from_slice(&mut bytes, name).map_err(oom)?;
    Ok(PathBuf::from(OsString::from_vec(bytes)))
}

/// One entry of a git chain: present at `source`, of its kind, not a
/// link, and a file with no other name; bound at `target`, the same name
/// where the instance sees its repository or checkout.
fn admit_git_entry(
    source: PathBuf,
    target: PathBuf,
    kind: FilesystemSourceKind,
    read_only: bool,
) -> io::Result<FilesystemGrant> {
    let path = source.as_path();
    authority::validate_filesystem_target(path)
        .and_then(|()| authority::validate_filesystem_target(&target))
        .map_err(|_| invalid(format!("workspace git entry {path:?} is not canonical")))?;
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| invalid(format!("workspace git entry {}: {error}", path.display())))?;
    // A protected file is its owner's to write, so that stage 2's write
    // to it fails for the mount alone (EROFS) and not for its mode.
    let fits = match kind {
        FilesystemSourceKind::Directory => metadata.file_type().is_dir(),
        FilesystemSourceKind::File => {
            metadata.file_type().is_file()
                && metadata.nlink() == 1
                && metadata.permissions().mode() & 0o200 != 0
        }
    };
    if !fits {
        return Err(invalid(format!(
            "workspace git entry {} is not a direct {}",
            path.display(),
            if kind == FilesystemSourceKind::File {
                "file with one name, its owner's to write"
            } else {
                "directory"
            }
        )));
    }
    Ok(FilesystemGrant {
        source,
        target,
        read_only,
        source_kind: kind,
        source_device: metadata.dev(),
        source_inode: metadata.ino(),
    })
}

/// A program: a canonical, executable regular file, named for its file
/// name in `PROGRAM_DIR`.
fn admit_program(source: &Path) -> io::Result<Program> {
    let metadata = fs::symlink_metadata(source)?;
    if !metadata.file_type().is_file()
        || metadata.permissions().mode() & 0o100 == 0
        || fs::canonicalize(source)? != source
    {
        return Err(invalid(format!(
            "workspace program {} is not a direct executable regular file",
            source.display()
        )));
    }
    let name = source
        .file_name()
        .and_then(OsStr::to_str)
        .filter(|name| valid_program_name(name))
        .ok_or_else(|| {
            invalid(format!(
                "workspace program {} has no plain file name",
                source.display()
            ))
        })?
        .to_string();
    Ok(Program {
        name,
        source: source.to_path_buf(),
        device: metadata.dev(),
        inode: metadata.ino(),
    })
}

pub(crate) fn valid_program_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_PROGRAM_NAME
        && !name.starts_with('.')
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

/// A directory, canonical with no link anywhere in it, bound at its own
/// path.
fn admit_directory(source: &Path, read_only: bool) -> io::Result<FilesystemGrant> {
    let metadata = fs::symlink_metadata(source)?;
    if !metadata.file_type().is_dir() || fs::canonicalize(source)? != source {
        return Err(invalid(format!(
            "workspace directory {} is not a direct directory",
            source.display()
        )));
    }
    Ok(FilesystemGrant {
        source: source.to_path_buf(),
        target: source.to_path_buf(),
        read_only,
        source_kind: FilesystemSourceKind::Directory,
        source_device: metadata.dev(),
        source_inode: metadata.ino(),
    })
}

/// No directory may overlap the jail's own trees, nor be the caller's
/// home or contain it: the home is absent but for what lies inside it
/// and is admitted.
fn refuse_reserved(source: &Path, real_home: &[PathBuf], reserved: &[&str]) -> io::Result<()> {
    if source == Path::new("/") {
        return Err(invalid("workspace directory / is the whole host"));
    }
    if let Some(reserved) = reserved
        .iter()
        .find(|reserved| paths_overlap(source, Path::new(reserved)))
    {
        return Err(invalid(format!(
            "workspace directory {} overlaps the reserved tree {reserved}",
            source.display()
        )));
    }
    if let Some(home) = real_home
        .iter()
        .find(|home| path_is_same_or_child(home, source))
    {
        return Err(invalid(format!(
            "workspace directory {} is or contains the caller's home {}",
            source.display(),
            home.display()
        )));
    }
    Ok(())
}

/// No mount at or below `source` is a pseudo-filesystem.
pub(crate) fn refuse_pseudo_filesystems(mountinfo: &str, source: &Path) -> io::Result<()> {
    for line in mountinfo.lines() {
        let (left, right) = line
            .split_once(" - ")
            .ok_or_else(|| invalid("mountinfo row has no separator"))?;
        let mountpoint = left
            .split_whitespace()
            .nth(4)
            .ok_or_else(|| invalid("mountinfo row has no mount point"))?;
        let mountpoint = authority::decode_mountinfo_path(mountpoint)?;
        let fstype = right.split_whitespace().next().unwrap_or_default();
        if path_is_same_or_child(&mountpoint, source) && PSEUDO_FILESYSTEMS.contains(&fstype) {
            return Err(invalid(format!(
                "{} carries a {fstype} mount at {}, which would reach past the jail",
                source.display(),
                mountpoint.display()
            )));
        }
    }
    Ok(())
}

/// `outer` is `inner` or one of its ancestors in the same filesystem,
/// however either is mounted: a bind of `/home` elsewhere contains the
/// home as surely as `/home` does. A root too irregular to compare, such
/// as a removed one's `PATH//deleted`, may contain it.
fn contains_by_identity(
    outer: &authority::MountIdentity,
    inner: &authority::MountIdentity,
) -> bool {
    outer.device == inner.device && authority::root_may_be_within(&inner.root, &outer.root)
}

/// Whether a directory's `mounts` alias a reserved tree: a reservation
/// strictly containing the caller's resolved `home`, as td's maintenance
/// mount of its volume at `/run/td-volume` does, admits a directory
/// strictly below that home, as an application grant's check does. With
/// no home placed, every alias refuses.
fn aliases_reserved(
    mounts: &std::collections::BTreeSet<authority::MountIdentity>,
    reserved: &std::collections::BTreeSet<authority::MountIdentity>,
    home: Option<&authority::MountIdentity>,
) -> bool {
    match home {
        Some(home) => authority::reserved_alias(mounts, reserved, home).is_some(),
        None => authority::mount_identity_sets_overlap(mounts, reserved),
    }
}

/// Root's validated account home or scratch can appear through td's
/// maintenance mount too. Exempt only an ancestor reservation of this
/// exact private root, and only identities within that root. An equal or
/// nested reservation, or a bind of a sibling, remains forbidden.
fn aliases_private(
    mounts: &std::collections::BTreeSet<authority::MountIdentity>,
    reserved: &std::collections::BTreeSet<authority::MountIdentity>,
    private: &authority::MountIdentity,
) -> bool {
    mounts.iter().any(|mount| {
        reserved.iter().any(|reserved| {
            let overlaps =
                contains_by_identity(mount, reserved) || contains_by_identity(reserved, mount);
            let maintenance_ancestor = reserved.device == private.device
                && reserved.root != private.root
                && authority::root_within(&private.root, &reserved.root);
            let inside_private = mount.device == private.device
                && authority::root_within(&mount.root, &private.root);
            overlaps && !(maintenance_ancestor && inside_private)
        })
    })
}

fn reserved_identities(
    mountinfo: &str,
    reserved: &[&str],
) -> io::Result<std::collections::BTreeSet<authority::MountIdentity>> {
    let mut identities = std::collections::BTreeSet::new();
    for reserved in reserved {
        // A reserved tree that is a link (`/bin` to `usr/bin`) reserves
        // what it resolves to.
        let Ok(path) = fs::canonicalize(reserved) else {
            continue;
        };
        if fs::metadata(&path).is_ok_and(|metadata| metadata.is_dir()) {
            identities.extend(authority::mount_tree_identities(mountinfo, &path)?);
        }
    }
    Ok(identities)
}

/// The channel, by its inode in the launcher's `/proc/net/unix`: a
/// connected Unix stream socket, unnamed for the human's launcher's
/// socket pair, or, for a ledger caller (`intake`), an accepted connection
/// that carries the intake's name. A datagram socket could address any
/// pathname socket on the host with `sendto`, and the jail's filter leaves
/// `sendto` alone; a connected stream socket's `sendto` reaches its peer
/// alone. The name proves the shape, not the origin, since any namespace
/// can bind a socket of that name: what makes the connection root's is
/// the caller's class, a ledger account only root's launch starts.
pub(crate) fn require_stream_pair(table: &str, inode: u64, intake: bool) -> io::Result<()> {
    require_stream_row(table, inode, intake.then_some(WORKSPACE_INTAKE))
}

/// `require_stream_pair`, a named row admitted only at `named`.
fn require_stream_row(table: &str, inode: u64, named: Option<&str>) -> io::Result<()> {
    let inode = inode.to_string();
    let row = table
        .lines()
        .skip(1)
        .map(|line| line.split_whitespace().collect::<Vec<_>>())
        .find(|fields| fields.get(6) == Some(&inode.as_str()))
        .ok_or_else(|| invalid("the workspace channel is not a Unix socket of this namespace"))?;
    let shape = match named {
        Some(name) => row.len() == 8 && row.get(7) == Some(&name),
        None => row.len() == 7,
    };
    if row.get(4) != Some(&UNIX_STREAM) || row.get(5) != Some(&UNIX_CONNECTED) || !shape {
        return Err(invalid(if named.is_some() {
            "the workspace channel is not a connection root's workspace intake accepted"
        } else {
            "the workspace channel is not an unnamed, connected Unix stream socket"
        }));
    }
    Ok(())
}

/// The passwd row the plan writes: the caller, at the workspace home.
pub(crate) fn passwd(uid: u32, gid: u32, home: &Path) -> String {
    format!("td:x:{uid}:{gid}:td:{}:/bin/sh\n", home.display())
}

pub(crate) fn group(gid: u32) -> String {
    format!("td:x:{gid}:\n")
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]
    use super::*;

    /// td's mounts: a read-only root, the persistent `@var` subvolume at
    /// `/var` (where `/home` resolves), and the whole volume, root and all,
    /// at `/run/td-volume` for updates.
    const TD_MOUNTINFO: &str = "\
20 1 7:0 / / ro - erofs /dev/loop0 ro
30 20 0:35 /@var /var rw,nodev,nosuid - btrfs /dev/vda rw
31 20 0:20 / /run rw,nosuid,nodev - tmpfs tmpfs rw
32 31 0:35 / /run/td-volume rw,nodev,nosuid,noexec - btrfs /dev/vda rw
33 20 0:21 / /tmp rw - tmpfs tmpfs rw
";

    #[test]
    fn development_offline_keeps_executable_policy_without_host_network() {
        let offline =
            parse_spec("format=1\nprofile=development-offline\nentry=/e\nhome=/h\n").unwrap();
        assert!(offline.development);
        assert!(!offline.shared_network);
        let online = parse_spec("format=1\nprofile=development\nentry=/e\nhome=/h\n").unwrap();
        assert!(online.development && online.shared_network);
        assert!(parse_spec(
            "format=1\nprofile=development-offline\nprofile=development\nentry=/e\nhome=/h\n"
        )
        .is_err());
    }

    #[test]
    fn only_the_validated_private_root_can_ignore_its_maintenance_ancestor() {
        let identity =
            |path: &str| authority::mount_identity_for_path(TD_MOUNTINFO, Path::new(path)).unwrap();
        let tree =
            |path: &str| authority::mount_tree_identities(TD_MOUNTINFO, Path::new(path)).unwrap();
        let mut reserved = tree("/run");
        reserved.extend(tree("/var/tmp"));
        let human = identity("/var/home/tester");
        for path in [
            "/var/lib/td/workloads/98304",
            "/var/lib/td/workload-scratch/98304/tmp",
        ] {
            let private = identity(path);
            let own = tree(path);
            // The pre-account home-relative rule rejects these roots on
            // the real td layout; the private-root rule admits them.
            assert!(aliases_reserved(&own, &reserved, Some(&human)));
            assert!(!aliases_private(&own, &reserved, &private));
            let child = tree(&format!("{path}/cache"));
            assert!(!aliases_private(&child, &reserved, &private));
            for elsewhere in [
                "/var/lib/td/workloads/98305",
                "/var/home/tester/secret",
                "/var/tmp/x",
                "/run/td-volume/@var",
            ] {
                let mut nested = own.clone();
                nested.extend(tree(elsewhere));
                assert!(aliases_private(&nested, &reserved, &private), "{elsewhere}");
            }
            let mut exact = reserved.clone();
            exact.insert(private.clone());
            assert!(aliases_private(&own, &exact, &private));
            let mut below = reserved.clone();
            below.extend(child);
            assert!(aliases_private(&own, &below, &private));
        }
        let human_spec = parse_spec("format=1\nentry=/e\nhome=/h\n").unwrap();
        assert!(!human_spec.account_private);
        assert!(parse_spec("format=1\naccount-private=true\nentry=/e\nhome=/h\n").is_err());
    }

    #[test]
    fn on_td_a_directory_below_the_home_is_not_the_volumes_maintenance_mount() {
        let identity =
            |path: &str| authority::mount_identity_for_path(TD_MOUNTINFO, Path::new(path)).unwrap();
        let tree =
            |path: &str| authority::mount_tree_identities(TD_MOUNTINFO, Path::new(path)).unwrap();
        let mut reserved = std::collections::BTreeSet::new();
        for root in ["/run", "/var/tmp", "/tmp"] {
            reserved.extend(tree(root));
        }
        // The home as it resolves; as passwd names it, through the root's
        // link, it is placed on the root's own filesystem and exempts nothing.
        let home = identity("/var/home/tester");
        let passwd = identity("/home/tester");
        for admitted in [
            "/var/home/tester/.local/state/td-agent-check-0/tree",
            "/var/home/tester/src/td",
        ] {
            assert!(
                !aliases_reserved(&tree(admitted), &reserved, Some(&home)),
                "{admitted}"
            );
            assert!(
                aliases_reserved(&tree(admitted), &reserved, Some(&passwd)),
                "{admitted}"
            );
        }
        // The home itself, a reserved tree's own data, and anything on the
        // volume outside the home stay refused.
        for refused in [
            "/var/home/tester",
            "/var/tmp/x",
            "/var/lib/x",
            "/run/td-volume/@var",
        ] {
            assert!(
                aliases_reserved(&tree(refused), &reserved, Some(&home)),
                "{refused}"
            );
        }
        // Without the home placed, every alias refuses, as before.
        assert!(aliases_reserved(
            &tree("/var/home/tester/src/td"),
            &reserved,
            None
        ));
    }

    #[test]
    fn path_alias_specs_are_bounded_and_cannot_name_private_executables() {
        let spec = parse_spec("format=1\nentry=/a\nalias=cc /usr/bin/gcc\nhome=/h\n").unwrap();
        assert_eq!(
            spec.aliases,
            vec![crate::host_path::Alias {
                name: "cc".into(),
                source: "/usr/bin/gcc".into()
            }]
        );
        for line in [
            "alias=../cc /usr/bin/gcc",
            "alias=cc gcc",
            "alias=cc",
            "alias=.. /usr/bin/gcc",
        ] {
            assert!(parse_spec(&format!("format=1\nentry=/a\n{line}\nhome=/h\n")).is_err());
        }
        let many: String = (0..=crate::host_path::MAX_ALIASES)
            .map(|n| format!("alias=a{n} /usr/bin/gcc\n"))
            .collect();
        assert!(parse_spec(&format!("format=1\nentry=/a\n{many}home=/h\n")).is_err());
        assert!(admit_alias(&crate::host_path::Alias {
            name: "cc".into(),
            source: "/home/private/compiler".into()
        })
        .is_err());
    }

    #[test]
    fn persistent_scratch_targets_follow_the_ledgers_scratch_names() {
        // Root makes a workspace account's scratch tree in the ledger's
        // order (engine/src/workload_accounts.rs `SCRATCH_NAMES`), which a
        // spec's scratch lines will follow onto these targets.
        let named: Vec<String> = crate::workload_accounts::SCRATCH_NAMES
            .iter()
            .map(|name| match *name {
                "var-tmp" => "/var/tmp".to_string(),
                "shm" => "/dev/shm".to_string(),
                other => format!("/{other}"),
            })
            .collect();
        assert_eq!(named, SCRATCH_TARGETS);
    }

    #[test]
    fn persistent_scratch_requires_development_and_all_four_sources() {
        let scratch = "scratch=/a\nscratch=/b\nscratch=/c\nscratch=/d\n";
        assert!(parse_spec(&format!("format=1\nentry=/e\nhome=/h\n{scratch}")).is_err());
        assert!(
            parse_spec("format=1\nprofile=development\nentry=/e\nhome=/h\nscratch=/a\n").is_err()
        );
        let spec = parse_spec(&format!(
            "format=1\nprofile=development\nentry=/e\nhome=/h\n{scratch}"
        ))
        .unwrap();
        assert_eq!(spec.scratch.len(), 4);
    }

    #[test]
    fn development_profile_is_explicit_and_cannot_be_repeated_or_reordered() {
        assert!(
            !parse_spec("format=1\nentry=/a\nhome=/h\n")
                .unwrap()
                .development
        );
        assert!(
            parse_spec("format=1\nprofile=development\nentry=/a\nhome=/h\n")
                .unwrap()
                .development
        );
        for text in [
            "format=1\nprofile=host\nentry=/a\nhome=/h\n",
            "format=1\nprofile=development\nprofile=development\nentry=/a\nhome=/h\n",
            "format=1\nentry=/a\nprofile=development\nhome=/h\n",
        ] {
            assert!(parse_spec(text).is_err(), "{text}");
        }
    }

    #[test]
    fn the_spec_is_an_exact_ordered_keyfile() {
        let spec = parse_spec(
            "format=1\nentry=/w/bin/td-agent\nprogram=/w/bin/td-txt\nhome=/s/home\n\
             worktree=/w/a\nworktree=/w/b\nread=/d\nwrite=/e\n",
        )
        .unwrap();
        assert_eq!(spec.programs.len(), 2);
        assert_eq!(spec.home, PathBuf::from("/s/home"));
        assert_eq!(spec.worktrees.len(), 2);
        assert_eq!((spec.read.len(), spec.write.len()), (1, 1));
        let spec = parse_spec(
            "format=1\nentry=/w/bin/td-agent\nhome=/s/home\nworktree=/w/a\n\
             checkout=/w/b\nrepository=/g/b.git\nread=/g/store/objects\n",
        )
        .unwrap();
        assert_eq!(spec.checkouts, [PathBuf::from("/w/b")]);
        assert_eq!(spec.repositories, [PathBuf::from("/g/b.git")]);
        let spec = parse_spec(
            "format=1\nentry=/a\nprogram=/b\npath=/gnu/store/x/bin\npath=/usr/bin\nhome=/h\n",
        )
        .unwrap();
        assert_eq!(
            spec.path,
            [PathBuf::from("/gnu/store/x/bin"), PathBuf::from("/usr/bin")]
        );
        let many: String = (0..=MAX_PATH_DIRECTORIES)
            .map(|n| format!("path=/usr/{n}\n"))
            .collect();
        assert!(parse_spec(&format!("format=1\nentry=/a\n{many}home=/h\n")).is_err());
        for bad in [
            "format=1\nentry=/a\nhome=/h\npath=/usr/bin\n",
            "format=1\npath=/usr/bin\nentry=/a\nhome=/h\n",
            "format=1\nentry=/a\npath=usr/bin\nhome=/h\n",
            "format=1\nentry=/a\nhome=/h\nrepository=/r\ncheckout=/c\n",
            "format=1\nentry=/a\nhome=/h\nread=/d\nrepository=/r\n",
            "format=1\nentry=/a\nhome=/h\ncheckout=/c\nworktree=/w\n",
            "",
            "format=2\nentry=/a\nhome=/h\n",
            "format=1\nhome=/h\n",
            "format=1\nentry=/a\n",
            "format=1\nentry=/a\nentry=/b\nhome=/h\n",
            "format=1\nentry=/a\nhome=/h\nhome=/i\n",
            "format=1\nentry=/a\nhome=/h\nprogram=/b\n",
            "format=1\nentry=/a\nhome=/h\nread=/r\nworktree=/w\n",
            "format=1\nentry=/a\nhome=/h\nnetwork=on\n",
            "format=1\nentry=a\nhome=/h\n",
            "format=1\nentry=/a/../b\nhome=/h\n",
            "format=1\nentry=/a//b\nhome=/h\n",
            "format=1\nentry=/a\nhome=/h\nworktree\n",
        ] {
            assert!(parse_spec(bad).is_err(), "{bad:?}");
        }
    }

    /// Where td's product configuration is installed, td's account alone
    /// may launch the kind, in the initial user namespace, not an
    /// application whose own uid is the account's; without it, any
    /// identity may.
    #[test]
    fn on_td_the_kind_is_its_accounts_alone() {
        let base = std::env::temp_dir().join(format!("td-jail-account-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).unwrap();
        let config = base.join("td-app.conf");
        let initial = base.join("initial");
        fs::write(&initial, "         0          0 4294967295\n").unwrap();
        // An application's: its own uid 1000 is the host's 65536.
        let jailed = base.join("jailed");
        fs::write(&jailed, "      1000      65536          1\n").unwrap();
        type Reader = fn() -> Result<crate::workload_accounts::Ledger, String>;
        fn ledger() -> Result<crate::workload_accounts::Ledger, String> {
            crate::workload_accounts::Ledger::parse(concat!(
                "td-accounts-v1\n",
                "application\tfirefox\t65536\n",
                "sandbox\tdefault\t98304\n",
                "workspace\tagent\t98305\n",
            ))
        }
        fn unread() -> Result<crate::workload_accounts::Ledger, String> {
            Err("no ledger".to_string())
        }
        let account = |uid: u32, map: &Path, ledger: Reader| {
            require_account((uid, uid), map, &config, ledger)
        };
        assert_eq!(account(1001, &initial, ledger).unwrap(), Caller::Human);
        assert_eq!(
            account(TD_ACCOUNT_UID, &jailed, ledger).unwrap(),
            Caller::Human
        );
        // A development host knows no ledger: any identity is its own.
        assert_eq!(account(98304, &initial, unread).unwrap(), Caller::Human);
        fs::write(&config, "format=1\n").unwrap();
        // On td both caller classes exist: td's account, which td-agent and
        // td-sandbox launch as, and a sandbox's or workspace's ledger
        // account, which root's intake starts.
        assert_eq!(
            account(TD_ACCOUNT_UID, &initial, unread).unwrap(),
            Caller::Human
        );
        assert_eq!(
            account(98304, &initial, ledger).unwrap(),
            Caller::Account(Kind::Sandbox)
        );
        assert_eq!(
            account(98305, &initial, ledger).unwrap(),
            Caller::Account(Kind::Workspace)
        );
        let refused = account(1001, &initial, ledger).unwrap_err();
        assert_eq!(refused.kind(), io::ErrorKind::PermissionDenied);
        // An application's account, a ledger uid no row holds, one outside
        // the range, one whose gid is not its uid, one in another
        // namespace, and any when the ledger cannot be read.
        for uid in [65536, 98303, 98306, 131071, 131072] {
            let refused = account(uid, &initial, ledger).unwrap_err();
            assert_eq!(refused.kind(), io::ErrorKind::PermissionDenied, "{uid}");
        }
        assert!(require_account((98304, 98305), &initial, &config, ledger).is_err());
        assert!(require_account((98304, 1000), &initial, &config, ledger).is_err());
        assert!(account(98304, &jailed, ledger).is_err());
        assert!(account(98304, &initial, unread).is_err());
        let refused = account(TD_ACCOUNT_UID, &jailed, ledger).unwrap_err();
        assert_eq!(refused.kind(), io::ErrorKind::PermissionDenied);
        // This process's own map reads as one or the other.
        assert!(initial_user_namespace(Path::new(UID_MAP)).is_ok());
        let _ = fs::remove_dir_all(&base);
    }

    /// A ledger caller's spec is root's, of one link in root's directory,
    /// never its own; the human's is its own.
    #[test]
    fn a_ledger_callers_spec_is_roots_and_the_humans_its_own() {
        let temp = crate::test_temp::TempDir::new("td-jail-spec-owner").unwrap();
        let directory = temp.path().join("specs");
        fs::create_dir(&directory).unwrap();
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o710)).unwrap();
        let directory = fs::canonicalize(&directory).unwrap();
        let path = directory.join("1");
        fs::write(&path, "format=1\nentry=/bin/true\nhome=/h\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
        let me = fs::metadata(&path).unwrap().uid();
        // The caller's own spec serves the human and never a ledger account.
        assert!(read_spec(&path, me, Caller::Human).is_ok());
        for kind in [Kind::Sandbox, Kind::Workspace] {
            let refused = read_spec(&path, me, Caller::Account(kind)).unwrap_err();
            assert!(refused.to_string().contains("root"), "{refused}");
        }
        // A ledger caller's spec is one root wrote for it, by name.
        assert!(account_spec_path(
            Path::new("/run/td-authd/workspaces/98304/7"),
            98304
        ));
        for other in [
            "/run/td-authd/workspaces/98305/7",
            "/run/td-authd/workspaces/98304/07",
            "/run/td-authd/workspaces/98304/x",
            "/run/td-authd/workspaces/98304/",
            "/run/td-authd/workspaces/98304/../98305/7",
            "/tmp/run/td-authd/workspaces/98304/7",
        ] {
            assert!(!account_spec_path(Path::new(other), 98304), "{other}");
        }
        // Sealed, as root's must be: one link, in a directory of its owner's
        // that no one else may write (this test's uid stands for root).
        assert!(read_spec_of(&path, me, true).is_ok());
        let linked = directory.join("2");
        fs::hard_link(&path, &linked).unwrap();
        assert!(read_spec_of(&path, me, true).is_err());
        assert!(read_spec_of(&path, me, false).is_ok());
        fs::remove_file(&linked).unwrap();
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o730)).unwrap();
        assert!(read_spec_of(&path, me, true).is_err());
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o710)).unwrap();
        assert!(read_spec_of(&path, me.wrapping_add(1), true).is_err());
        temp.cleanup().unwrap();
    }

    /// td's account may not name the offline profile, which only a
    /// sandbox's spec names: td-jail refuses it for that caller, td-agent
    /// writes only the online one, and td-sandbox calls no td-jail of its
    /// own.
    #[test]
    fn td_accounts_specs_may_not_name_the_sandbox_profile() {
        let spec =
            |profile: &str| parse_spec(&format!("format=1\n{profile}entry=/e\nhome=/h\n")).unwrap();
        assert!(require_human_spec(&spec("profile=development\n")).is_ok());
        assert!(require_human_spec(&spec("")).is_ok());
        let refused = require_human_spec(&spec("profile=development-offline\n")).unwrap_err();
        assert!(refused.to_string().contains("its own account"), "{refused}");
        let production = include_str!("workspace.rs")
            .split("#[cfg(test)]")
            .next()
            .unwrap();
        assert!(production.contains(
            "    if caller == Caller::Human {\n        require_human_spec(&spec)?;\n    }\n"
        ));
        let agent = include_str!("../../td-agent/src/jail.rs")
            .split("#[cfg(test)]")
            .next()
            .unwrap();
        assert!(agent.contains("text.push_str(\"profile=development\\n\");"));
        assert!(!agent.contains("development-offline"));
        for (file, source) in [
            ("main.rs", include_str!("../../td-sandbox/src/main.rs")),
            ("relay.rs", include_str!("../../td-sandbox/src/relay.rs")),
            ("intake.rs", include_str!("../../td-sandbox/src/intake.rs")),
        ] {
            let production = source.split("#[cfg(test)]").next().unwrap();
            for absent in ["td-jail", "--workspace", "development-offline"] {
                assert!(!production.contains(absent), "td-sandbox {file}: {absent}");
            }
        }
    }

    /// Root writes a ledger caller's spec with the account's own home and
    /// no directory to bind: its grants are its manifest's views, bound at
    /// the human's own paths.
    #[test]
    fn a_ledger_callers_spec_names_its_home_and_no_directory() {
        let home = "/var/lib/td/workloads/98304";
        let spec = |extra: &str| {
            parse_spec(&format!(
                "format=1\nprofile=development-offline\nentry=/td/store/x/bin/td-sandbox\nhome={home}\n{extra}"
            ))
            .unwrap()
        };
        assert!(require_account_spec(&spec(""), Kind::Sandbox, 98304).is_ok());
        assert!(require_account_spec(&spec(""), Kind::Workspace, 98304).is_ok());
        assert!(require_account_spec(&spec(""), Kind::Sandbox, 98305).is_err());
        for extra in [
            "worktree=/var/home/alice/src\n",
            "checkout=/var/home/alice/src\n",
            "repository=/var/home/alice/r.git\n",
            "read=/var/home/alice/src\n",
            "write=/var/home/alice/src\n",
        ] {
            assert!(
                require_account_spec(&spec(extra), Kind::Sandbox, 98304).is_err(),
                "{extra}"
            );
        }
        let other = parse_spec(
            "format=1\nentry=/td/store/x/bin/td-sandbox\nhome=/var/home/alice/.local/share/td-sandbox/x/home\n",
        )
        .unwrap();
        assert!(require_account_spec(&other, Kind::Sandbox, 98304).is_err());
        // A workspace's spec may name directories by role, which only its
        // views can answer (`select_views`), and its own scratch tree,
        // whole and in order; a sandbox's names neither.
        let scratch = |uid: u32| {
            crate::workload_accounts::SCRATCH_NAMES
                .iter()
                .map(|name| format!("scratch=/var/lib/td/workload-scratch/{uid}/{name}\n"))
                .collect::<String>()
        };
        let workspace = |extra: &str| {
            parse_spec(&format!(
                "format=1\nprofile=development\nentry=/td/store/x/bin/td-agent\nhome=/var/lib/td/workloads/98305\n{extra}"
            ))
            .unwrap()
        };
        let roles = "worktree=/var/home/alice/src\nrepository=/var/home/alice/r.git\nread=/var/home/alice/doc\nwrite=/var/home/alice/out\n";
        let ours = scratch(98305) + roles;
        assert!(require_account_spec(&workspace(&ours), Kind::Workspace, 98305).is_ok());
        assert!(require_account_spec(&workspace(roles), Kind::Workspace, 98305).is_ok());
        let sandbox = format!(
            "format=1\nprofile=development\nentry=/td/store/x/bin/td-sandbox\nhome={home}\n{}",
            scratch(98304)
        );
        assert!(
            require_account_spec(&parse_spec(&sandbox).unwrap(), Kind::Sandbox, 98304).is_err()
        );
        let another = scratch(98306);
        let reordered: String = scratch(98305)
            .lines()
            .rev()
            .map(|line| format!("{line}\n"))
            .collect();
        for wrong in [
            another.as_str(),
            reordered.as_str(),
            "scratch=/var/lib/td/workload-scratch/098305/tmp\nscratch=/var/lib/td/workload-scratch/098305/var-tmp\nscratch=/var/lib/td/workload-scratch/098305/shm\nscratch=/var/lib/td/workload-scratch/098305/run\n",
            "scratch=/var/lib/td/workloads/98305/tmp\nscratch=/var/lib/td/workloads/98305/var-tmp\nscratch=/var/lib/td/workloads/98305/shm\nscratch=/var/lib/td/workloads/98305/run\n",
        ] {
            assert!(
                require_account_spec(&workspace(wrong), Kind::Workspace, 98305).is_err(),
                "{wrong}"
            );
        }
        assert!(require_view_target(Path::new("/var/home/alice/sandbox")).is_ok());
        for reserved in ["/run/x", "/tmp/x", "/etc", "/opt/workspace", "/"] {
            assert!(
                require_view_target(Path::new(reserved)).is_err(),
                "{reserved}"
            );
        }
    }

    #[test]
    fn no_jail_reaches_the_compositor_control_socket() {
        // The control socket admits the human's kernel credentials and
        // answers with launches and directory grants (td-authd/DESIGN.md,
        // "Grant requests"); a workload that still runs as the human is
        // kept from it only because no jail binds it. `/run`'s reservation
        // refuses the socket and its directory to every workspace, by
        // path and by alias, and no jail source names it, so neither an
        // application's fixed sockets nor a profile's can be it.
        let home = &[PathBuf::from("/home/u")];
        for refused in [
            "/run/td-compositor",
            "/run/td-compositor/1000",
            "/run/td-compositor/1000/td-control",
        ] {
            assert!(
                refuse_reserved(Path::new(refused), home, RESERVED).is_err(),
                "{refused}"
            );
        }
        for (name, source) in [
            ("authority.rs", include_str!("authority.rs")),
            ("bus.rs", include_str!("bus.rs")),
            ("cgroup.rs", include_str!("cgroup.rs")),
            ("firefox.rs", include_str!("firefox.rs")),
            ("flathub.rs", include_str!("flathub.rs")),
            ("main.rs", include_str!("main.rs")),
            ("seccomp.rs", include_str!("seccomp.rs")),
            ("sys.rs", include_str!("sys.rs")),
            ("transition.rs", include_str!("transition.rs")),
            ("workspace.rs", include_str!("workspace.rs")),
        ] {
            let production = source.split("#[cfg(test)]").next().unwrap_or(source);
            for named in ["td-control", "TD_CONTROL_SOCKET"] {
                assert!(!production.contains(named), "{name} names {named}");
            }
        }
        assert!(
            std::fs::read_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/src"))
                .unwrap()
                .all(|entry| {
                    let name = entry.unwrap().file_name();
                    [
                        "authority.rs",
                        "bus.rs",
                        "cgroup.rs",
                        "firefox.rs",
                        "flathub.rs",
                        "main.rs",
                        "seccomp.rs",
                        "sys.rs",
                        "transition.rs",
                        "workspace.rs",
                    ]
                    .contains(&name.to_str().unwrap_or_default())
                })
        );
    }

    /// Root's intakes admit the human's kernel credentials, which a
    /// workload still running as the human carries; it is kept from them
    /// only because no jail binds them. `/run`'s reservation refuses the
    /// intakes and their directories to every workspace, by path and by
    /// alias, and no jail source binds one: td-jail names the workspace
    /// intake only to recognise a channel's row, never as a mount source.
    #[test]
    fn no_jail_reaches_roots_intakes() {
        let home = &[PathBuf::from("/home/u")];
        for refused in [
            "/run/td-authd",
            "/run/td-authd/1000",
            WORKSPACE_INTAKE,
            "/run/td-authd/1000/set",
            WORKSPACE_SPECS,
        ] {
            assert!(
                refuse_reserved(Path::new(refused), home, RESERVED).is_err(),
                "{refused}"
            );
        }
        for (name, source) in [
            ("authority.rs", include_str!("authority.rs")),
            ("bus.rs", include_str!("bus.rs")),
            ("cgroup.rs", include_str!("cgroup.rs")),
            ("firefox.rs", include_str!("firefox.rs")),
            ("flathub.rs", include_str!("flathub.rs")),
            ("main.rs", include_str!("main.rs")),
            ("seccomp.rs", include_str!("seccomp.rs")),
            ("sys.rs", include_str!("sys.rs")),
            ("transition.rs", include_str!("transition.rs")),
            ("workspace.rs", include_str!("workspace.rs")),
        ] {
            let production = source.split("#[cfg(test)]").next().unwrap_or(source);
            assert!(!production.contains("/run/td-authd"), "{name}");
            let named = production.matches("WORKSPACE_INTAKE").count();
            let expected = if name == "workspace.rs" { 2 } else { 0 };
            assert_eq!(named, expected, "{name} names WORKSPACE_INTAKE");
        }
        let production = include_str!("workspace.rs")
            .split("#[cfg(test)]")
            .next()
            .unwrap_or_default();
        assert!(production.contains(
            "use crate::workload_accounts::{Kind, WORKLOAD_UIDS, WORKSPACE_INTAKE, WORKSPACE_SPECS};"
        ));
        assert!(production.contains(
            "    require_stream_row(table, inode, intake.then_some(WORKSPACE_INTAKE))\n"
        ));
    }

    #[test]
    fn reserved_trees_and_the_home_are_refused() {
        let home = &[PathBuf::from("/home/u"), PathBuf::from("/var/home/u")];
        for refused in [
            "/",
            "/usr",
            "/usr/lib",
            "/etc",
            "/home",
            "/home/u",
            "/opt/workspace",
            "/proc/1",
            "/run/user",
            "/tmp",
            "/var/tmp/x",
            "/sys",
            "/gnu/store",
            "/nix",
            "/td",
            "/td/store",
            "/opt/project",
        ] {
            assert!(
                refuse_reserved(Path::new(refused), home, RESERVED).is_err(),
                "{refused}"
            );
        }
        assert!(refuse_reserved(Path::new("/var/home"), home, RESERVED).is_err());
        for refused in ["/var/log", "/var/log/svc", "/var/log/svc/wayland.log"] {
            assert!(
                refuse_reserved(Path::new(refused), home, RESERVED).is_err(),
                "{refused}"
            );
        }
        for admitted in ["/home/u/src/td", "/srv/data", "/var/lib/x"] {
            assert!(
                refuse_reserved(Path::new(admitted), home, RESERVED).is_ok(),
                "{admitted}"
            );
        }
    }

    /// Admission over real directories, under a scratch directory the
    /// reserved list this test passes leaves out (production reserves
    /// `/tmp`, where the scratch directory usually is).
    #[test]
    fn admission_requires_direct_private_disjoint_trees() {
        use std::os::unix::fs::symlink;
        let base = std::env::temp_dir().join(format!("td-jail-workspace-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).unwrap();
        let base = fs::canonicalize(&base).unwrap();
        let uid = fs::metadata(&base).unwrap().uid();
        let dir = |name: &str, mode: u32| {
            let path = base.join(name);
            fs::create_dir_all(&path).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
            path
        };
        let home = dir("home", 0o700);
        let tree = dir("tree", 0o755);
        let shared = dir("shared", 0o755);
        let loose = dir("loose", 0o755);
        let entry = base.join("td-agent");
        fs::write(&entry, b"#!/bin/sh\n").unwrap();
        fs::set_permissions(&entry, fs::Permissions::from_mode(0o755)).unwrap();
        let plain = base.join("plain");
        fs::write(&plain, b"").unwrap();
        fs::set_permissions(&plain, fs::Permissions::from_mode(0o644)).unwrap();
        symlink(&tree, base.join("link")).unwrap();
        let real_home = &[PathBuf::from("/nonexistent-td-jail-home")];
        let spec = |programs: &[&Path], home: &Path, worktrees: &[&Path], read: &[&Path]| Spec {
            development: false,
            shared_network: false,
            runtime: None,
            scratch: Vec::new(),
            programs: programs.iter().map(|path| path.to_path_buf()).collect(),
            aliases: Vec::new(),
            path: Vec::new(),
            home: home.to_path_buf(),
            worktrees: worktrees.iter().map(|path| path.to_path_buf()).collect(),
            checkouts: Vec::new(),
            repositories: Vec::new(),
            read: read.iter().map(|path| path.to_path_buf()).collect(),
            write: Vec::new(),
            views: Vec::new(),
            ..Spec::default()
        };
        let spec_path = base.join("spec");
        let admit_spec = |spec: Spec| {
            admit(
                spec,
                &spec_path,
                uid,
                uid,
                real_home,
                Vec::new(),
                &["/proc"],
            )
        };

        let plan = admit_spec(spec(&[&entry], &home, &[&tree], &[&shared])).unwrap();
        assert_eq!(
            plan.programs[0].target(),
            Path::new("/opt/workspace/bin/td-agent")
        );
        assert_eq!(plan.working_directory(), tree);
        assert!(plan.shared[0].read_only && !plan.home.read_only);
        // A ledger caller's views join the shared directories: bound at
        // their targets as listed, and refused where one overlaps another.
        let shown = fs::metadata(&shared).unwrap();
        let view = |target: &Path| FilesystemGrant {
            source: shared.clone(),
            target: target.to_path_buf(),
            read_only: false,
            source_kind: FilesystemSourceKind::Directory,
            source_device: shown.dev(),
            source_inode: shown.ino(),
        };
        let mut viewed = spec(&[&entry], &home, &[], &[]);
        viewed.views = vec![view(&shared)];
        let plan = admit_spec(viewed).unwrap();
        assert!(plan
            .shared
            .iter()
            .any(|grant| grant.target == shared && !grant.read_only));
        assert!(!plan.intake);
        let mut overlapping = spec(&[&entry], &home, &[&tree], &[]);
        overlapping.views = vec![view(&tree)];
        assert!(admit_spec(overlapping).is_err());
        let mut twice = spec(&[&entry], &home, &[], &[]);
        twice.views = vec![view(&shared), view(&shared)];
        assert!(admit_spec(twice).is_err());

        // Path directories, a bound system tree's own, in order, each once.
        let mut searched = spec(&[&entry], &home, &[&tree], &[]);
        searched.path = vec!["/usr".into()];
        assert_eq!(admit_spec(searched).unwrap().path, [PathBuf::from("/usr")]);
        for path in [
            vec![PathBuf::from("/usr"), "/usr".into()],
            vec![tree.clone()],
            vec!["/usr/no-such-directory-here".into()],
        ] {
            let mut searched = spec(&[&entry], &home, &[&tree], &[]);
            searched.path = path;
            assert!(admit_spec(searched).is_err());
        }
        // A home that others can enter.
        assert!(admit_spec(spec(&[&entry], &loose, &[&tree], &[])).is_err());
        // A program that cannot run, or is a directory.
        assert!(admit_spec(spec(&[&plain], &home, &[&tree], &[])).is_err());
        assert!(admit_spec(spec(&[&tree], &home, &[], &[])).is_err());
        // Two programs with one file name.
        assert!(admit_spec(spec(&[&entry, &entry], &home, &[], &[])).is_err());
        // A spec inside a tree it grants.
        assert!(admit(
            spec(&[&entry], &home, &[&tree], &[]),
            &tree.join("spec"),
            uid,
            uid,
            real_home,
            Vec::new(),
            &["/proc"],
        )
        .is_err());
        // A program in a tree the instance could write, but not in one
        // it can only read.
        let built = tree.join("built");
        fs::write(&built, b"#!/bin/sh\n").unwrap();
        fs::set_permissions(&built, fs::Permissions::from_mode(0o755)).unwrap();
        let refused = admit_spec(spec(&[&built], &home, &[&tree], &[])).unwrap_err();
        assert!(
            refused.to_string().contains("holds the program"),
            "{refused}"
        );
        let given = shared.join("given");
        fs::write(&given, b"#!/bin/sh\n").unwrap();
        fs::set_permissions(&given, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(admit_spec(spec(&[&given], &home, &[&tree], &[&shared])).is_ok());
        let mut written = spec(&[&given], &home, &[&tree], &[]);
        written.write.push(shared.clone());
        assert!(admit_spec(written).is_err());
        fs::remove_file(&built).unwrap();
        fs::remove_file(&given).unwrap();
        // td-jail's own executable, here the test's, in a writable tree.
        let own = std::env::current_exe().unwrap();
        let own_tree = fs::canonicalize(own.parent().unwrap()).unwrap();
        let refused = admit_spec(spec(&[&entry], &home, &[&own_tree], &[])).unwrap_err();
        assert!(
            refused.to_string().contains("holds the program"),
            "{refused}"
        );
        // A link, rather than the directory it names.
        assert!(admit_spec(spec(&[&entry], &home, &[&base.join("link")], &[])).is_err());
        // Overlap, by path or as the same directory twice.
        assert!(admit_spec(spec(&[&entry], &home, &[&base], &[])).is_err());
        assert!(admit_spec(spec(&[&entry], &home, &[&tree], &[&tree])).is_err());
        // A tree containing the caller's real home.
        assert!(admit(
            spec(&[&entry], &home, &[&tree], &[]),
            &spec_path,
            uid,
            uid,
            &[PathBuf::from("/elsewhere"), tree.join("me")],
            Vec::new(),
            &["/proc"],
        )
        .is_err());
        // A reserved tree.
        assert!(admit(
            spec(&[&entry], &home, &[&tree], &[]),
            &spec_path,
            uid,
            uid,
            real_home,
            Vec::new(),
            &[base.to_str().unwrap()],
        )
        .is_err());
        fs::remove_dir_all(&base).unwrap();
    }

    /// A workspace account binds, of its manifest's views, those its
    /// spec's role lines name, each at its role's access and of a path its
    /// role admits; a line no view answers refuses, and a view no line
    /// names, another instance's, stays unbound.
    #[test]
    fn a_workspace_binds_the_views_its_lines_name_in_their_roles() {
        let human = Path::new("/var/home/alice");
        let view = |index: u32, path: &str, read_only: bool| FilesystemGrant {
            source: PathBuf::from(format!("/var/lib/td/grant-views/98305/{index}")),
            target: human.join(path),
            read_only,
            source_kind: FilesystemSourceKind::Directory,
            source_device: 1,
            source_inode: u64::from(index),
        };
        let views = || {
            vec![
                view(0, "src", false),
                view(1, ".local/share/td-agent/ws/w/r.git", false),
                view(2, ".local/share/td-agent/store/s.git/objects", true),
                view(3, "doc", true),
                view(4, "other", false),
                view(
                    5,
                    ".local/state/td-agent/jail/0123456789abcdef0123456789abcdef/scratch",
                    false,
                ),
            ]
        };
        let lines = |text: &str| {
            parse_spec(&format!(
                "format=1\nprofile=development\nentry=/td/store/x/bin/td-agent\nhome=/var/lib/td/workloads/98305\n{text}"
            ))
            .unwrap()
        };
        let mut spec = lines(
            "worktree=/var/home/alice/.local/state/td-agent/jail/0123456789abcdef0123456789abcdef/scratch\ncheckout=/var/home/alice/src\nrepository=/var/home/alice/.local/share/td-agent/ws/w/r.git\nread=/var/home/alice/.local/share/td-agent/store/s.git/objects\nread=/var/home/alice/doc\n",
        );
        select_views(&mut spec, views(), human).unwrap();
        assert!(spec.views_only);
        let sources = |grants: &[FilesystemGrant]| {
            grants
                .iter()
                .map(|grant| grant.source_inode)
                .collect::<Vec<_>>()
        };
        assert_eq!(sources(&spec.viewed_trees), [5, 0]);
        assert_eq!(sources(&spec.viewed_repositories), [1]);
        assert_eq!(sources(&spec.views), [2, 3]);
        assert!(spec.read.is_empty() && spec.write.is_empty());
        assert_eq!(spec.checkouts, [human.join("src")]);
        // Each refusal: no view at that path, a view at another access, a
        // td-agent tree in another role, a path outside the human's home
        // or not one a plan admits.
        for wrong in [
            "worktree=/var/home/alice/absent\n",
            "write=/var/home/alice/doc\n",
            "read=/var/home/alice/src\n",
            "write=/var/home/alice/.local/share/td-agent/ws/w/r.git\n",
            "worktree=/var/home/alice/.local/share/td-agent/ws/w/r.git\n",
            "read=/var/home/alice/.local/share/td-agent/ws/w/r.git\n",
            "write=/var/home/alice/.local/share/td-agent/store/s.git/objects\n",
            "worktree=/var/home/bob/src\n",
            "worktree=/var/home/alice/.ssh\n",
        ] {
            let mut spec = lines(wrong);
            assert!(select_views(&mut spec, views(), human).is_err(), "{wrong}");
        }
        // One view answers one line.
        let mut twice = lines("read=/var/home/alice/doc\nread=/var/home/alice/doc\n");
        assert!(select_views(&mut twice, views(), human).is_err());
    }

    /// A workspace account's repository and checkout are bound from their
    /// views at the human's paths, and so is every entry of their git
    /// chain: checked where the view shows it, bound where the instance
    /// sees the repository.
    #[test]
    fn a_workspace_accounts_git_chain_is_bound_from_its_views() {
        let temp = crate::test_temp::TempDir::new("td-jail-chain-views").unwrap();
        let base = fs::canonicalize(temp.path()).unwrap();
        let uid = fs::metadata(&base).unwrap().uid();
        let home = base.join("home");
        fs::create_dir_all(&home).unwrap();
        fs::set_permissions(&home, fs::Permissions::from_mode(0o700)).unwrap();
        let entry = base.join("td-agent");
        fs::write(&entry, b"#!/bin/sh\n").unwrap();
        fs::set_permissions(&entry, fs::Permissions::from_mode(0o755)).unwrap();
        let (repository, checkout) = (base.join("views/1"), base.join("views/2"));
        lay_out_repository(&repository, &checkout);
        let human = base.join("human");
        let viewed = |source: &Path, target: &str| {
            let metadata = fs::metadata(source).unwrap();
            FilesystemGrant {
                source: source.to_path_buf(),
                target: human.join(target),
                read_only: false,
                source_kind: FilesystemSourceKind::Directory,
                source_device: metadata.dev(),
                source_inode: metadata.ino(),
            }
        };
        let spec = Spec {
            programs: vec![entry.clone()],
            home: home.clone(),
            checkouts: vec![human.join("tree/r")],
            repositories: vec![human.join("ws/r.git")],
            views_only: true,
            viewed_trees: vec![viewed(&checkout, "tree/r")],
            viewed_repositories: vec![viewed(&repository, "ws/r.git")],
            ..Spec::default()
        };
        let plan = admit(
            spec,
            &base.join("spec"),
            uid,
            uid,
            &[PathBuf::from("/nonexistent-td-jail-home")],
            Vec::new(),
            &["/proc"],
        )
        .unwrap();
        assert_eq!(plan.working_directory(), human.join("tree/r"));
        let relative = |path: &Path| path.strip_prefix(&base).unwrap().display().to_string();
        let chain: Vec<(String, String)> = plan
            .chain
            .iter()
            .map(|grant| (relative(&grant.source), relative(&grant.target)))
            .collect();
        assert_eq!(chain.len(), 16);
        assert_eq!(
            chain.first().unwrap(),
            &("views/2/.git".to_string(), "human/tree/r/.git".to_string())
        );
        assert!(chain.contains(&(
            "views/1/worktrees/id/gitdir".to_string(),
            "human/ws/r.git/worktrees/id/gitdir".to_string()
        )));
        for (source, target) in &chain {
            assert!(source.starts_with("views/"), "{source}");
            assert!(target.starts_with("human/"), "{target}");
        }
    }

    /// A workspace repository as td-agent lays it out, with one linked
    /// worktree `id` checked out at `checkout`.
    fn lay_out_repository(repository: &Path, checkout: &Path) {
        for dir in ["objects/info", "refs", "worktrees/id"]
            .iter()
            .chain(REPOSITORY_DIRECTORIES)
        {
            fs::create_dir_all(repository.join(dir)).unwrap();
        }
        for file in REPOSITORY_FILES {
            fs::write(repository.join(file), b"").unwrap();
        }
        fs::write(repository.join("commondir"), b".\n").unwrap();
        for file in LINKED_FILES {
            fs::write(repository.join("worktrees/id").join(file), b"").unwrap();
        }
        fs::create_dir_all(checkout).unwrap();
        fs::write(checkout.join(".git"), b"gitdir: x\n").unwrap();
    }

    #[test]
    fn a_repository_is_admitted_with_its_whole_chain() {
        let base = std::env::temp_dir().join(format!("td-jail-chain-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).unwrap();
        let base = fs::canonicalize(&base).unwrap();
        let uid = fs::metadata(&base).unwrap().uid();
        let home = base.join("home");
        fs::create_dir_all(&home).unwrap();
        fs::set_permissions(&home, fs::Permissions::from_mode(0o700)).unwrap();
        let entry = base.join("td-agent");
        fs::write(&entry, b"#!/bin/sh\n").unwrap();
        fs::set_permissions(&entry, fs::Permissions::from_mode(0o755)).unwrap();
        let repository = base.join("ws/r.git");
        let checkout = base.join("tree/r");
        lay_out_repository(&repository, &checkout);
        let spec = || Spec {
            programs: vec![entry.clone()],
            home: home.clone(),
            checkouts: vec![checkout.clone()],
            repositories: vec![repository.clone()],
            ..Spec::default()
        };
        let admit_spec = || {
            admit(
                spec(),
                &base.join("spec"),
                uid,
                uid,
                &[PathBuf::from("/nonexistent-td-jail-home")],
                Vec::new(),
                &["/proc"],
            )
        };
        let plan = admit_spec().unwrap();
        assert_eq!(plan.working_directory(), checkout);
        let chain: Vec<(String, bool)> = plan
            .chain
            .iter()
            .map(|grant| {
                let name = grant
                    .target
                    .strip_prefix(&base)
                    .unwrap()
                    .display()
                    .to_string();
                (name, grant.read_only)
            })
            .collect();
        let expected: Vec<(String, bool)> = [
            ("tree/r/.git", true),
            ("ws/r.git/objects", false),
            ("ws/r.git/objects/info", true),
            ("ws/r.git/commondir", true),
            ("ws/r.git/config", true),
            ("ws/r.git/config.worktree", true),
            ("ws/r.git/shallow", true),
            ("ws/r.git/branches", true),
            ("ws/r.git/hooks", true),
            ("ws/r.git/info", true),
            ("ws/r.git/remotes", true),
            ("ws/r.git/worktrees", true),
            ("ws/r.git/worktrees/id", false),
            ("ws/r.git/worktrees/id/commondir", true),
            ("ws/r.git/worktrees/id/config.worktree", true),
            ("ws/r.git/worktrees/id/gitdir", true),
        ]
        .iter()
        .map(|(name, read_only)| (name.to_string(), *read_only))
        .collect();
        assert_eq!(chain, expected);
        // Each protected entry must be present, direct and, for a file,
        // of one name: one missing, a link, or a second name is refused.
        let config = repository.join("config");
        fs::remove_file(&config).unwrap();
        assert!(admit_spec().unwrap_err().to_string().contains("config"));
        std::os::unix::fs::symlink(base.join("elsewhere"), &config).unwrap();
        assert!(admit_spec().is_err());
        fs::remove_file(&config).unwrap();
        fs::write(&config, b"").unwrap();
        fs::hard_link(&config, base.join("second")).unwrap();
        assert!(admit_spec().unwrap_err().to_string().contains("one name"));
        fs::remove_file(base.join("second")).unwrap();
        fs::remove_dir(repository.join("hooks")).unwrap();
        fs::write(repository.join("hooks"), b"").unwrap();
        assert!(admit_spec().is_err());
        fs::remove_file(repository.join("hooks")).unwrap();
        fs::create_dir(repository.join("hooks")).unwrap();
        fs::write(repository.join("worktrees/id/gitdir.lock"), b"").unwrap();
        assert!(admit_spec().is_ok(), "an extra file is the repository's");
        fs::write(repository.join("worktrees/stray"), b"").unwrap();
        let refused = admit_spec().unwrap_err().to_string();
        assert!(refused.contains("not a direct directory"), "{refused}");
        fs::remove_file(repository.join("worktrees/stray")).unwrap();
        // A checkout's `.git` is a file, not a directory or a link.
        fs::remove_file(checkout.join(".git")).unwrap();
        fs::create_dir(checkout.join(".git")).unwrap();
        assert!(admit_spec().is_err());
        fs::remove_dir(checkout.join(".git")).unwrap();
        assert!(admit_spec().is_err());
        fs::write(checkout.join(".git"), b"gitdir: x\n").unwrap();
        // A repository is a writable tree like any: it may not overlap
        // another directory, nor hold the program.
        let admit_with = |spec: Spec| {
            admit(
                spec,
                &base.join("spec"),
                uid,
                uid,
                &[PathBuf::from("/nonexistent-td-jail-home")],
                Vec::new(),
                &["/proc"],
            )
        };
        let mut overlapping = spec();
        overlapping.worktrees.push(repository.join("objects"));
        let refused = admit_with(overlapping).unwrap_err().to_string();
        assert!(refused.contains("overlap"), "{refused}");
        // Beside a repository, a plain worktree holding `.git` is refused.
        let plain = base.join("plain");
        fs::create_dir_all(&plain).unwrap();
        fs::write(plain.join(".git"), b"gitdir: x\n").unwrap();
        let mut beside = spec();
        beside.worktrees.push(plain.clone());
        let refused = admit_with(beside).unwrap_err().to_string();
        assert!(refused.contains("name it a checkout"), "{refused}");
        // A plan too large for stage 2's argv is refused by name.
        let paths: Vec<PathBuf> = (0..4).map(|n| PathBuf::from(format!("/p/{n}"))).collect();
        assert!(check_plan_bytes(paths.iter(), 4 * (4 + ARGV_WORD)).is_ok());
        let refused = check_plan_bytes(paths.iter(), 4 * (4 + ARGV_WORD) - 1).unwrap_err();
        assert!(refused.to_string().contains("take more than"), "{refused}");
        // A protected file its owner cannot write is refused.
        fs::set_permissions(
            repository.join("shallow"),
            fs::Permissions::from_mode(0o444),
        )
        .unwrap();
        let refused = admit_spec().unwrap_err().to_string();
        assert!(refused.contains("its owner's to write"), "{refused}");
        fs::set_permissions(
            repository.join("shallow"),
            fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        assert!(admit_spec().is_ok());
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn pseudo_filesystems_below_a_tree_are_refused() {
        let mountinfo = "1 0 0:1 / / rw - btrfs /dev/a rw\n\
            2 1 0:2 / /w/a/chroot/proc rw,nosuid - proc proc rw\n\
            3 1 0:3 / /w/b/tmp rw - tmpfs tmpfs rw\n\
            4 1 0:4 / /w/c\\040d/sys rw - sysfs sysfs rw\n";
        assert!(refuse_pseudo_filesystems(mountinfo, Path::new("/w/a")).is_err());
        assert!(refuse_pseudo_filesystems(mountinfo, Path::new("/w/a/chroot/proc")).is_err());
        assert!(refuse_pseudo_filesystems(mountinfo, Path::new("/w/b")).is_ok());
        assert!(refuse_pseudo_filesystems(mountinfo, Path::new("/w/c d")).is_err());
        assert!(refuse_pseudo_filesystems(mountinfo, Path::new("/w/ab")).is_ok());
    }

    #[test]
    fn a_bind_of_the_home_contains_it() {
        let identity = |device: &str, root: &str| authority::MountIdentity {
            device: device.into(),
            root: root.into(),
        };
        let home = identity("0:5", "/@home/u");
        assert!(contains_by_identity(&identity("0:5", "/@home"), &home));
        assert!(contains_by_identity(&identity("0:5", "/@home/u"), &home));
        assert!(!contains_by_identity(
            &identity("0:5", "/@home/u/src"),
            &home
        ));
        assert!(!contains_by_identity(&identity("0:6", "/@home"), &home));
        assert!(!contains_by_identity(
            &identity("0:5", "/@home/user"),
            &home
        ));
        // A removed root, shown `PATH//deleted`, is not compared as a path:
        // it may contain the home, so it refuses.
        assert!(contains_by_identity(
            &identity("0:5", "/@home/u/src//deleted"),
            &home
        ));
    }

    #[test]
    fn the_channel_is_a_connected_stream_pair() {
        let table = "Num       RefCount Protocol Flags    Type St Inode Path\n\
            00000000aae3d76a: 00000003 00000000 00000000 0001 03 11\n\
            00000000aae3d76b: 00000002 00000000 00000000 0002 03 12\n\
            00000000aae3d76c: 00000002 00000000 00000000 0005 03 13\n\
            00000000aae3d76d: 00000002 00000000 00010000 0001 01 14 /run/s\n\
            00000000aae3d76e: 00000003 00000000 00000000 0001 03 15 /run/t\n";
        assert!(require_stream_pair(table, 11, false).is_ok());
        for refused in [12, 13, 14, 15, 16, 1] {
            assert!(
                require_stream_pair(table, refused, false).is_err(),
                "{refused}"
            );
        }
        // A ledger caller's channel is a connection root's intake accepted,
        // which carries the intake's name, and nothing else.
        let intake = format!(
            "{table}00000000aae3d76f: 00000003 00000000 00000000 0001 03 17 {WORKSPACE_INTAKE}\n\
             00000000aae3d770: 00000002 00000000 00010000 0001 01 18 {WORKSPACE_INTAKE}\n\
             00000000aae3d771: 00000002 00000000 00000000 0002 03 19 {WORKSPACE_INTAKE}\n"
        );
        assert!(require_stream_pair(&intake, 17, true).is_ok());
        assert!(require_stream_pair(&intake, 17, false).is_err());
        for refused in [11, 15, 18, 19, 12] {
            assert!(
                require_stream_pair(&intake, refused, true).is_err(),
                "{refused}"
            );
        }
    }

    /// The kernel names an accepted connection after its listener in
    /// `/proc/net/unix`, which is how td-jail knows the intake's.
    #[test]
    fn an_accepted_connection_carries_its_listeners_name() {
        use std::os::unix::net::{UnixListener, UnixStream};
        let temp = crate::test_temp::TempDir::new("td-jail-intake-name").unwrap();
        let path = fs::canonicalize(temp.path()).unwrap().join("intake");
        let listener = UnixListener::bind(&path).unwrap();
        let client = UnixStream::connect(&path).unwrap();
        let (accepted, _) = listener.accept().unwrap();
        let inode = |stream: &UnixStream| {
            fs::metadata(format!(
                "/proc/self/fd/{}",
                std::os::fd::AsRawFd::as_raw_fd(stream)
            ))
            .unwrap()
            .ino()
        };
        let table = fs::read_to_string("/proc/net/unix").unwrap();
        let name = path.to_str().unwrap();
        require_stream_row(&table, inode(&accepted), Some(name)).unwrap();
        assert!(require_stream_row(&table, inode(&accepted), None).is_err());
        assert!(require_stream_row(&table, inode(&accepted), Some("/run/other")).is_err());
        // The connecting end is unnamed, as a socket pair's are.
        require_stream_row(&table, inode(&client), None).unwrap();
        temp.cleanup().unwrap();
    }

    #[test]
    fn program_names_are_plain() {
        for good in ["td-agent", "td-txt", "a.b_c"] {
            assert!(valid_program_name(good));
        }
        for bad in ["", ".hidden", "a/b", "a b", &"x".repeat(65)] {
            assert!(!valid_program_name(bad), "{bad}");
        }
    }

    #[test]
    fn the_environment_is_fixed_but_for_home_and_path() {
        let fixed = environment(Path::new("/s/home"), &[], false);
        let keys: Vec<_> = fixed.iter().map(|(key, _)| key.clone()).collect();
        assert_eq!(keys, ["HOME", "LANG", "PATH", "TERM", "TMPDIR"]);
        assert_eq!(fixed[0].1, "/s/home");
        assert_eq!(
            fixed[2].1,
            OsString::from(format!("{ALIAS_DIR}:{PROGRAM_DIR}:{PATH}"))
        );
        let searched = environment(
            Path::new("/s/home"),
            &["/gnu/store/x-profile/bin".into(), "/usr/local/sbin".into()],
            false,
        );
        assert_eq!(
            searched[2].1,
            OsString::from(format!(
                "{ALIAS_DIR}:/gnu/store/x-profile/bin:/usr/local/sbin:{PROGRAM_DIR}:{PATH}"
            ))
        );
        // With a runtime, td's `/bin` precedes the runtime's, and nothing
        // names a library path.
        let runtime = environment(Path::new("/s/home"), &["/td/store/x-git/bin".into()], true);
        assert_eq!(
            runtime[2].1,
            OsString::from(format!(
                "{ALIAS_DIR}:/td/store/x-git/bin:{PROGRAM_DIR}:/bin:/usr/bin"
            ))
        );
        assert_eq!(
            runtime
                .iter()
                .map(|(key, _)| key.clone())
                .collect::<Vec<_>>(),
            keys
        );
    }

    /// `runtime=` is optional, once, before `entry`, and names a declared
    /// runtime; a development host refuses it.
    #[test]
    fn a_spec_names_at_most_one_declared_runtime_before_its_entry() {
        let spec = parse_spec(
            "format=1\nprofile=development\nruntime=freedesktop-sdk-25-08\nentry=/a\nhome=/h\n",
        )
        .unwrap();
        assert_eq!(
            spec.runtime,
            flathub_app::flathub_runtime("freedesktop-sdk-25-08")
        );
        let spec = parse_spec("format=1\nruntime=freedesktop-platform-25-08\nentry=/a\nhome=/h\n")
            .unwrap();
        assert_eq!(spec.runtime.unwrap().name, "freedesktop-platform-25-08");
        assert_eq!(
            parse_spec("format=1\nentry=/a\nhome=/h\n").unwrap().runtime,
            None
        );
        for bad in [
            "format=1\nruntime=org.freedesktop.Platform\nentry=/a\nhome=/h\n",
            "format=1\nruntime=\nentry=/a\nhome=/h\n",
            "format=1\nruntime=freedesktop-platform-25-08\nruntime=freedesktop-platform-25-08\nentry=/a\nhome=/h\n",
            "format=1\nentry=/a\nruntime=freedesktop-platform-25-08\nhome=/h\n",
            "format=1\nentry=/a\nhome=/h\nruntime=freedesktop-platform-25-08\n",
            "format=1\nruntime=freedesktop-platform-25-08\nprofile=development\nentry=/a\nhome=/h\n",
        ] {
            assert!(parse_spec(bad).is_err(), "{bad:?}");
        }
        let base = crate::test_temp::TempDir::new("td-jail-workspace-runtime-host").unwrap();
        let config = base.path().join("td-app.conf");
        let refused = require_td_for_runtime(&config).unwrap_err();
        assert_eq!(refused.kind(), io::ErrorKind::PermissionDenied);
        assert!(refused.to_string().contains("development host"));
        fs::write(&config, "format=1\n").unwrap();
        assert!(require_td_for_runtime(&config).is_ok());
    }

    /// A spec's runtime refuses a development host, names its installers
    /// when absent, and is held from the store and owner given while the
    /// plan lives; production gives td's compiled ones.
    #[test]
    fn a_spec_runtime_is_tds_compiled_store_held() {
        use std::os::unix::fs::symlink;
        // Production names td's configuration and store, and no other.
        let td = td_runtime_source();
        assert_eq!(td.config, Path::new("/etc/td-app.conf"));
        assert_eq!(td.store, Path::new(flathub_app::RUNTIME_STORE));
        assert_eq!(td.owner, flathub_app::RUNTIME_UID);

        let base = crate::test_temp::TempDir::new("td-jail-workspace-spec-runtime").unwrap();
        let base = fs::canonicalize(base.path()).unwrap();
        let runtime = flathub_app::flathub_runtime("freedesktop-sdk-25-08").unwrap();
        let config = base.join("td-app.conf");
        let store = base.join("runtimes");
        let source = RuntimeSource {
            config: &config,
            store: &store,
            owner: fs::metadata(&base).unwrap().uid(),
        };
        // A development host: refused before the store is read.
        let refused = spec_runtime(runtime, &source).unwrap_err();
        assert_eq!(refused.kind(), io::ErrorKind::PermissionDenied);
        fs::write(&config, "format=1\n").unwrap();
        assert!(spec_runtime(runtime, &source)
            .unwrap_err()
            .to_string()
            .contains("no application installs it yet"));
        // The store as td-app publishes it.
        let commit = "7".repeat(64);
        let root = store.join(runtime.name);
        let generation = root.join("generations/1");
        fs::create_dir_all(&generation).unwrap();
        let selection = flathub_app::RuntimeSelection {
            name: runtime.name.into(),
            runtime: flathub_app::Admitted {
                exact_ref: runtime.exact_ref.into(),
                commit: commit.clone(),
                timestamp: 1,
            },
        };
        fs::write(generation.join("selection"), selection.render()).unwrap();
        fs::set_permissions(
            generation.join("selection"),
            fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        symlink("generations/1", root.join("current")).unwrap();
        let files = root.join("deploy").join(&commit).join("files");
        fs::create_dir_all(&files).unwrap();
        fs::set_permissions(&files, fs::Permissions::from_mode(0o755)).unwrap();
        let held = spec_runtime(runtime, &source).unwrap();
        assert_eq!(held.files, files);
        let probe = fs::File::open(generation.join("selection")).unwrap();
        assert!(matches!(
            probe.try_lock(),
            Err(fs::TryLockError::WouldBlock)
        ));
        drop(held);
        probe.try_lock().unwrap();
    }

    /// With a runtime, nothing the spec names may lie where the runtime
    /// replaces the host's trees; without one, those trees are the host's.
    #[test]
    fn a_runtime_refuses_path_and_alias_sources_in_the_trees_it_replaces() {
        for replaced in [
            "/usr",
            "/usr/bin",
            "/lib",
            "/lib64/x",
            "/usr/lib/x86_64-linux-gnu",
        ] {
            assert!(in_runtime_tree(Path::new(replaced)), "{replaced}");
            assert!(refuse_runtime_tree("path directory", Path::new(replaced), true).is_err());
            assert!(refuse_runtime_tree("path directory", Path::new(replaced), false).is_ok());
        }
        for kept in [
            "/bin",
            "/td/store/x/bin",
            "/lib32",
            "/libx32",
            "/usrx",
            "/sbin",
        ] {
            assert!(!in_runtime_tree(Path::new(kept)), "{kept}");
            assert!(refuse_runtime_tree("path directory", Path::new(kept), true).is_ok());
        }
        // Each replaced tree is one every instance otherwise binds.
        for tree in RUNTIME_TREES {
            assert!(SYSTEM_TREES.contains(tree), "{tree}");
        }
        // Admission refuses them before it looks at anything on disk.
        let runtime = flathub_app::flathub_runtime("freedesktop-platform-25-08");
        let path = Spec {
            runtime,
            path: vec!["/usr/bin".into()],
            home: "/h".into(),
            ..Spec::default()
        };
        let alias = Spec {
            runtime,
            aliases: vec![crate::host_path::Alias {
                name: "cc".into(),
                source: "/usr/bin/gcc".into(),
            }],
            home: "/h".into(),
            ..Spec::default()
        };
        for spec in [path, alias] {
            let refused = admit(spec, Path::new("/s/spec"), 1, 1, &[], Vec::new(), RESERVED)
                .unwrap_err()
                .to_string();
            assert!(refused.contains("runtime replaces"), "{refused}");
        }
    }

    #[test]
    fn a_path_directory_is_a_bound_system_trees_own() {
        for good in [
            "/gnu/store/x-profile/bin",
            "/nix/store/y/bin",
            "/td/store/z-git/bin",
            "/usr/bin",
            "/usr",
        ] {
            assert!(path_directory_named(Path::new(good)), "{good}");
        }
        for bad in [
            "/home/u/.guix-home/profile/bin",
            "/run/current-system/profile/bin",
            "/opt/bin",
            "/usr/../home/bin",
            "/usr//bin",
            "usr/bin",
            "/usr/a:b",
            "/usrx/bin",
            "/tdx/bin",
        ] {
            assert!(!path_directory_named(Path::new(bad)), "{bad}");
        }
        // As it resolves: a directory there, not a link or a file.
        assert!(admit_path_directory(Path::new("/usr")).is_ok());
        assert!(admit_path_directory(Path::new("/usr/no-such-directory-here")).is_err());
        // A file or a link there, whichever the host has: `/usr` is bound
        // by every instance, and `/usr/bin` holds `env` on every host.
        let other = ["/usr/bin", "/bin"]
            .into_iter()
            .filter_map(|dir| fs::read_dir(dir).ok())
            .flatten()
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .find(|path| fs::symlink_metadata(path).is_ok_and(|meta| !meta.is_dir()))
            .unwrap();
        assert!(admit_path_directory(&other).is_err(), "{}", other.display());
    }
}

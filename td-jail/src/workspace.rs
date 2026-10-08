//! The `workspace` launch kind (APPLICATIONS.md §C, td-agent/DESIGN.md §8):
//! a launch on a development host, or on td by td's account, whose policy
//! is a spec file its launcher writes, for an agent's tools. It runs a program the launcher names, not
//! a package; binds the launcher's worktrees read-write and executable at
//! their real paths, its git repositories through the git mount chain,
//! shared directories at theirs, a private home, and the host's own system
//! trees read-only; and grants no Wayland, bus, audio, fetch, tty or
//! network. Its entry's standard input and output are one stream socket
//! the launcher holds the other end of.
//!
//! This module reads and admits the spec, as `authority` does a package's;
//! `transition` performs it.

use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, Read};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use crate::authority::{
    self, path_is_same_or_child, paths_overlap, FilesystemGrant, FilesystemSourceKind,
};

/// The argument after the exact `td-jail` argv[0] that selects this kind.
pub(crate) const WORKSPACE_ARG: &str = "--workspace";
/// Where the spec's programs are bound, each under its own file name.
pub(crate) const PROGRAM_DIR: &str = "/opt/workspace/bin";
/// The entry's `PATH`, after the spec's own directories, and its
/// `TMPDIR` and `LANG`; `HOME` is the spec's home. The system profiles
/// of store-based hosts (Guix, NixOS) follow the ordinary directories;
/// on another host they are absent and cost a lookup.
pub(crate) const PATH: &str =
    "/usr/local/bin:/usr/bin:/bin:/run/current-system/profile/bin:/run/current-system/sw/bin";
pub(crate) const TMPDIR: &str = "/tmp";
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
    "terminfo",
    "timezone",
];
/// The files the plan writes into its `/etc`.
pub(crate) const SYNTHESIZED_ETC: &[&str] =
    &["group", "hostname", "hosts", "nsswitch.conf", "passwd"];
/// What no admitted directory may be, contain or lie inside: the jail's
/// own mount points and the host trees bound for it.
const RESERVED: &[&str] = &[
    "/bin", "/boot", "/dev", "/etc", "/gnu", "/lib", "/lib32", "/lib64", "/libx32", "/nix", "/opt",
    "/proc", "/run", "/sbin", "/sys", "/td", "/tmp", "/usr", "/var/tmp",
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
    pub(crate) uid: u32,
    pub(crate) gid: u32,
    /// The first program, which is the entry.
    pub(crate) programs: Vec<Program>,
    /// Directories of the bound system trees put first on the entry's
    /// `PATH`, in order.
    pub(crate) path: Vec<PathBuf>,
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
}

impl WorkspacePlan {
    /// Where the entry starts: the first worktree, else the home.
    pub(crate) fn working_directory(&self) -> &Path {
        self.worktrees
            .first()
            .map_or(&self.home.target, |tree| &tree.target)
    }
}

/// The entry's whole environment: `path` first on its `PATH`.
pub(crate) fn environment(home: &Path, path: &[PathBuf]) -> Vec<(OsString, OsString)> {
    let mut search = OsString::new();
    for directory in path {
        search.push(directory.as_os_str());
        search.push(":");
    }
    search.push(PATH);
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

pub(crate) fn is_workspace_argument(argument: &OsStr) -> bool {
    argument == WORKSPACE_ARG
}

/// The spec as written, before admission.
#[derive(Debug, Default, Eq, PartialEq)]
struct Spec {
    programs: Vec<PathBuf>,
    path: Vec<PathBuf>,
    home: PathBuf,
    worktrees: Vec<PathBuf>,
    checkouts: Vec<PathBuf>,
    repositories: Vec<PathBuf>,
    read: Vec<PathBuf>,
    write: Vec<PathBuf>,
}

/// The exact, ordered keyfile: `format=1`, one `entry`, any `program`s
/// and `path` directories, one `home`, then any `worktree`, `checkout`,
/// `repository`, `read` and `write` lines, each group in that order.
fn parse_spec(text: &str) -> io::Result<Spec> {
    let mut lines = text.lines();
    if lines.next() != Some(FORMAT_LINE) {
        return Err(invalid("workspace spec does not begin with format=1"));
    }
    const ORDER: &[&str] = &[
        "entry",
        "program",
        "path",
        "home",
        "worktree",
        "checkout",
        "repository",
        "read",
        "write",
    ];
    let mut spec = Spec::default();
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
        let once = matches!(key, "entry" | "home");
        if rank < last || (once && rank == last && (entry || home.is_some())) {
            return Err(invalid(format!(
                "workspace spec key {key:?} is out of order or repeated"
            )));
        }
        last = rank;
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
    require_account(uid, Path::new(UID_MAP), Path::new(authority::CONFIG_PATH))?;
    let spec_path = Path::new(spec_path);
    let text = read_spec(spec_path, uid)?;
    let spec = parse_spec(&text)?;
    let real_home = authority::account_home(uid)?;
    // The home as the account names it and as it resolves, which differ
    // where `/home` is a link (`/var/home` on OSTree hosts).
    let real_home = match fs::canonicalize(&real_home) {
        Ok(resolved) if resolved != real_home => vec![real_home, resolved],
        _ => vec![real_home],
    };
    let arguments = authority::collect_arguments(arguments)?;
    admit(spec, spec_path, uid, gid, &real_home, arguments, RESERVED)
}

/// Where the product configuration at `config` is installed, td's, the
/// kind serves td's account alone, as the initial user namespace knows it
/// (`uid_map`, the caller's map): a uid is the caller's namespace's, and
/// an application's own is the account's. On a development host, any
/// nonzero identity. It grants nothing the caller could not do itself:
/// td-jail is no more privileged than its caller, and the kind only
/// narrows what the caller's tools reach.
fn require_account(uid: u32, uid_map: &Path, config: &Path) -> io::Result<()> {
    if uid == TD_ACCOUNT_UID && initial_user_namespace(uid_map)? {
        return Ok(());
    }
    authority::require_no_configuration_at(
        config,
        "the workspace kind for any identity but td's account in the initial user namespace",
    )
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

/// The spec: a direct regular file of the caller's that no one else can
/// write, bounded.
fn read_spec(path: &Path, uid: u32) -> io::Result<String> {
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
        || metadata.uid() != uid
        || metadata.permissions().mode() & 0o022 != 0
        || fs::canonicalize(path)? != path
    {
        return Err(invalid(format!(
            "workspace spec {} is not a direct regular file only its owner, the caller, can write",
            path.display()
        )));
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
    spec: Spec,
    spec_path: &Path,
    uid: u32,
    gid: u32,
    real_home: &[PathBuf],
    arguments: Vec<OsString>,
    reserved: &[&str],
) -> io::Result<WorkspacePlan> {
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
    let worktrees = spec
        .worktrees
        .iter()
        .chain(spec.checkouts.iter())
        .map(|tree| admit_directory(tree, false))
        .collect::<io::Result<Vec<_>>>()?;
    let repositories = spec
        .repositories
        .iter()
        .map(|tree| admit_directory(tree, false))
        .collect::<io::Result<Vec<_>>>()?;
    // Beside repositories, a git worktree is a checkout, whose `.git` is
    // protected: a plain worktree's could be pointed at a repository the
    // jail made.
    if !spec.repositories.is_empty() {
        for tree in &spec.worktrees {
            match fs::symlink_metadata(tree.join(".git")) {
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                _ => {
                    return Err(invalid(format!(
                    "workspace worktree {} holds a `.git` beside repositories; name it a checkout",
                    tree.display()
                )))
                }
            }
        }
    }
    let mut chain = Vec::new();
    for checkout in &spec.checkouts {
        chain.push(admit_git_entry(
            &checkout.join(".git"),
            FilesystemSourceKind::File,
            true,
        )?);
    }
    for repository in &spec.repositories {
        chain.extend(repository_chain(repository)?);
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
    let all: Vec<&FilesystemGrant> = std::iter::once(&home)
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
        if aliases_reserved(&mounts, &reserved_mounts, homes.last()) {
            return Err(invalid(format!(
                "workspace directory {} aliases a reserved tree",
                grant.source.display()
            )));
        }
        identities.push((grant.source.clone(), mounts));
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
        uid,
        gid,
        programs,
        path,
        home,
        worktrees,
        repositories,
        chain,
        shared,
        arguments,
    })
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
/// absent one cannot be protected.
fn repository_chain(repository: &Path) -> io::Result<Vec<FilesystemGrant>> {
    use FilesystemSourceKind::{Directory, File};
    let mut chain = vec![
        admit_git_entry(&repository.join("objects"), Directory, false)?,
        admit_git_entry(&repository.join("objects/info"), Directory, true)?,
    ];
    for name in REPOSITORY_FILES {
        chain.push(admit_git_entry(&repository.join(name), File, true)?);
    }
    for name in REPOSITORY_DIRECTORIES {
        chain.push(admit_git_entry(&repository.join(name), Directory, true)?);
    }
    let linked = repository.join("worktrees");
    chain.push(admit_git_entry(&linked, Directory, true)?);
    let mut ids = fs::read_dir(&linked)?
        .take(MAX_TREES + 1)
        .map(|entry| entry.map(|entry| entry.file_name()))
        .collect::<io::Result<Vec<_>>>()?;
    if ids.len() > MAX_TREES {
        return Err(invalid(format!(
            "workspace repository {} has more than {MAX_TREES} linked worktrees",
            repository.display()
        )));
    }
    ids.sort();
    for id in ids {
        let entry = linked.join(id);
        chain.push(admit_git_entry(&entry, Directory, false)?);
        for name in LINKED_FILES {
            chain.push(admit_git_entry(&entry.join(name), File, true)?);
        }
    }
    Ok(chain)
}

/// One entry of a git chain: present, of its kind, not a link, and a
/// file with no other name.
fn admit_git_entry(
    path: &Path,
    kind: FilesystemSourceKind,
    read_only: bool,
) -> io::Result<FilesystemGrant> {
    authority::validate_filesystem_target(path)
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
        source: path.to_path_buf(),
        target: path.to_path_buf(),
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
/// home as surely as `/home` does.
fn contains_by_identity(
    outer: &authority::MountIdentity,
    inner: &authority::MountIdentity,
) -> bool {
    outer.device == inner.device && path_is_same_or_child(&inner.root, &outer.root)
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

/// The channel, by its inode in the launcher's `/proc/net/unix`: an
/// unnamed, connected Unix stream socket. A datagram socket could address
/// any pathname socket on the host with `sendto`, and the jail's filter
/// leaves `sendto` alone.
pub(crate) fn require_stream_pair(table: &str, inode: u64) -> io::Result<()> {
    let inode = inode.to_string();
    let row = table
        .lines()
        .skip(1)
        .map(|line| line.split_whitespace().collect::<Vec<_>>())
        .find(|fields| fields.get(6) == Some(&inode.as_str()))
        .ok_or_else(|| invalid("the workspace channel is not a Unix socket of this namespace"))?;
    if row.get(4) != Some(&UNIX_STREAM) || row.get(5) != Some(&UNIX_CONNECTED) || row.len() != 7 {
        return Err(invalid(
            "the workspace channel is not an unnamed, connected Unix stream socket",
        ));
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
        assert!(require_account(1001, &initial, &config).is_ok());
        assert!(require_account(TD_ACCOUNT_UID, &jailed, &config).is_ok());
        fs::write(&config, "format=1\n").unwrap();
        assert!(require_account(TD_ACCOUNT_UID, &initial, &config).is_ok());
        let refused = require_account(1001, &initial, &config).unwrap_err();
        assert_eq!(refused.kind(), io::ErrorKind::PermissionDenied);
        assert!(require_account(65536, &initial, &config).is_err());
        let refused = require_account(TD_ACCOUNT_UID, &jailed, &config).unwrap_err();
        assert_eq!(refused.kind(), io::ErrorKind::PermissionDenied);
        // This process's own map reads as one or the other.
        assert!(initial_user_namespace(Path::new(UID_MAP)).is_ok());
        let _ = fs::remove_dir_all(&base);
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
            programs: programs.iter().map(|path| path.to_path_buf()).collect(),
            path: Vec::new(),
            home: home.to_path_buf(),
            worktrees: worktrees.iter().map(|path| path.to_path_buf()).collect(),
            checkouts: Vec::new(),
            repositories: Vec::new(),
            read: read.iter().map(|path| path.to_path_buf()).collect(),
            write: Vec::new(),
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
    }

    #[test]
    fn the_channel_is_a_connected_stream_pair() {
        let table = "Num       RefCount Protocol Flags    Type St Inode Path\n\
            00000000aae3d76a: 00000003 00000000 00000000 0001 03 11\n\
            00000000aae3d76b: 00000002 00000000 00000000 0002 03 12\n\
            00000000aae3d76c: 00000002 00000000 00000000 0005 03 13\n\
            00000000aae3d76d: 00000002 00000000 00010000 0001 01 14 /run/s\n\
            00000000aae3d76e: 00000003 00000000 00000000 0001 03 15 /run/t\n";
        assert!(require_stream_pair(table, 11).is_ok());
        for refused in [12, 13, 14, 15, 16, 1] {
            assert!(require_stream_pair(table, refused).is_err(), "{refused}");
        }
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
        let fixed = environment(Path::new("/s/home"), &[]);
        let keys: Vec<_> = fixed.iter().map(|(key, _)| key.clone()).collect();
        assert_eq!(keys, ["HOME", "LANG", "PATH", "TERM", "TMPDIR"]);
        assert_eq!(fixed[0].1, "/s/home");
        assert_eq!(fixed[2].1, PATH);
        let searched = environment(
            Path::new("/s/home"),
            &["/gnu/store/x-profile/bin".into(), "/usr/local/sbin".into()],
        );
        assert_eq!(
            searched[2].1,
            OsString::from(format!("/gnu/store/x-profile/bin:/usr/local/sbin:{PATH}"))
        );
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

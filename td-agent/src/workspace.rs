//! A conversation's workspace (DESIGN.md §7, §8): a directory the human
//! admits, or a scratch directory td-agent makes, which the conversation's
//! tools work in through td-jail's `workspace` kind (`crate::jail`).
//!
//! Admission here is td-agent's half: what §8 refuses on top of td-jail's
//! own checks, so a directory is refused when it is chosen, by name, not
//! at its first tool call. td-jail then refuses the rest (reserved trees,
//! links, overlap, the caller's home by mount identity) at every launch.

use std::ffi::OsString;
use std::fs::{self, DirBuilder, File};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use td_json::Json;

use crate::jail::Policy;
use crate::store::{Id, StateDir};

/// What a conversation works in.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Workspace {
    /// A directory td-agent makes for it, under its jail directory,
    /// removed with the conversation: the Empty template.
    Scratch,
    /// A directory of the human's, admitted when chosen, never removed.
    Directory(PathBuf),
    /// A scratch directory made from the configured template named, which
    /// names no repository: it binds the template's shared directories.
    Template(String),
}

/// How a template's workspace is passed a new conversation's process.
const TEMPLATE_ARGUMENT: &str = "template:";

impl Workspace {
    pub fn to_json(&self) -> Json {
        match self {
            Self::Scratch => Json::Obj(vec![("kind".into(), Json::Str("scratch".into()))]),
            Self::Directory(path) => Json::Obj(vec![
                ("kind".into(), Json::Str("directory".into())),
                ("path".into(), Json::Str(path.display().to_string())),
            ]),
            Self::Template(name) => Json::Obj(vec![
                ("kind".into(), Json::Str("template".into())),
                ("name".into(), Json::Str(name.clone())),
            ]),
        }
    }

    pub fn from_json(value: &Json) -> Result<Self, String> {
        match value.get("kind").and_then(Json::as_str) {
            Some("scratch") => Ok(Self::Scratch),
            Some("directory") => value
                .get("path")
                .and_then(Json::as_str)
                .map(PathBuf::from)
                .filter(|path| path.is_absolute())
                .map(Self::Directory)
                .ok_or_else(|| "a directory workspace has no absolute path".into()),
            Some("template") => value
                .get("name")
                .and_then(Json::as_str)
                .ok_or_else(|| "a template workspace has no name".to_string())
                .and_then(crate::config::template_name)
                .map(Self::Template),
            _ => Err("a workspace is a scratch, a directory or a template one".into()),
        }
    }

    /// The word the window passes a new conversation's process.
    pub fn argument(&self) -> OsString {
        match self {
            Self::Scratch => "scratch".into(),
            Self::Directory(path) => path.as_os_str().to_os_string(),
            Self::Template(name) => format!("{TEMPLATE_ARGUMENT}{name}").into(),
        }
    }

    pub fn parse_argument(word: &str) -> Result<Self, String> {
        if word == "scratch" {
            return Ok(Self::Scratch);
        }
        if let Some(name) = word.strip_prefix(TEMPLATE_ARGUMENT) {
            return crate::config::template_name(name).map(Self::Template);
        }
        let path = PathBuf::from(word);
        if !path.is_absolute() {
            return Err(format!(
                "workspace {word:?} is neither scratch nor an absolute path"
            ));
        }
        Ok(Self::Directory(path))
    }

    /// How the window names it.
    pub fn label(&self) -> String {
        match self {
            Self::Scratch => "scratch".into(),
            Self::Directory(path) => path.display().to_string(),
            Self::Template(name) => format!("template {name}"),
        }
    }

    /// Whether td-agent made its directory, a scratch one.
    pub fn scratch(&self) -> bool {
        matches!(self, Self::Scratch | Self::Template(_))
    }
}

/// A shared directory (DESIGN.md §8): bound into every workspace,
/// read-only unless the human made it writable.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Shared {
    pub path: PathBuf,
    pub write: bool,
}

/// Where td-agent's own state and the human's sensitive places are, which
/// no workspace or shared directory may be, contain or lie inside, by
/// path or through a mount; and where programs the human runs live, which
/// none the model can write may be.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Places {
    pub home: PathBuf,
    pub state: PathBuf,
    pub config: Option<PathBuf>,
    pub runtime: Option<PathBuf>,
    /// `workspace_root`, where repository workspaces will live.
    pub root: PathBuf,
    /// `~/.config` and `$XDG_CONFIG_HOME`, `~/.local/share` and
    /// `$XDG_DATA_HOME`, where `CONFIG_SENSITIVE` and `DATA_SENSITIVE`
    /// live.
    pub config_homes: Vec<PathBuf>,
    pub data_homes: Vec<PathBuf>,
    /// The programs td-agent runs, outside the jail or in it, and the
    /// directories on `PATH`: no tree the model can write may hold one,
    /// or the model could replace its own jail, or what builds it.
    pub programs: Vec<PathBuf>,
    pub path: Vec<PathBuf>,
}

/// Below the home: credentials, keyrings, browser and mail profiles,
/// sandboxed applications' data.
const SENSITIVE: &[&str] = &[
    ".aws",
    ".azure",
    ".cargo/credentials",
    ".cargo/credentials.toml",
    ".docker",
    ".git-credentials",
    ".gnupg",
    ".kube",
    ".librewolf",
    ".mozilla",
    ".netrc",
    ".npmrc",
    ".password-store",
    ".pypirc",
    ".ssh",
    ".thunderbird",
    ".var/app",
    "snap",
];
/// Below each configuration home: credentials, browser profiles, and what
/// the session starts or git runs on the human's behalf.
const CONFIG_SENSITIVE: &[&str] = &[
    "BraveSoftware",
    "autostart",
    "chromium",
    "environment.d",
    "gcloud",
    "gh",
    "git",
    "google-chrome",
    "microsoft-edge",
    "opera",
    "systemd",
    "td-agent",
    "vivaldi",
];
/// Below each data home: keyrings, td-pass's vault, desktop entries.
const DATA_SENSITIVE: &[&str] = &["applications", "keyrings", "td-pass"];
/// Below the home: where the human's own programs are put, refused to a
/// tree the model can write, as `PATH`'s directories are.
const EXECUTED: &[&str] = &[".cargo/bin", ".local/bin", "bin"];

impl Places {
    /// From the environment, with td-agent's state at `state`, the
    /// configured `root`, and the jail's `programs`.
    pub fn from_env(
        state: &Path,
        root: &Path,
        programs: &crate::jail::Programs,
    ) -> Result<Self, String> {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .filter(|home| home.is_absolute())
            .ok_or("HOME is not an absolute path, so no workspace can be admitted")?;
        let absolute = |var: &str| {
            std::env::var_os(var)
                .map(PathBuf::from)
                .filter(|path| path.is_absolute())
        };
        let config = crate::config::path(
            std::env::var_os("XDG_CONFIG_HOME"),
            std::env::var_os("HOME"),
        )
        .and_then(|file| file.parent().map(Path::to_path_buf));
        // Without XDG_RUNTIME_DIR, where a session's would be.
        let runtime = absolute("XDG_RUNTIME_DIR").or_else(|| {
            use std::os::unix::fs::MetadataExt;
            fs::metadata(state)
                .ok()
                .map(|meta| PathBuf::from(format!("/run/user/{}", meta.uid())))
        });
        let both = |default: &str, var: &str| {
            let mut homes = vec![resolved(&home.join(default))];
            if let Some(set) = absolute(var).as_deref().map(resolved) {
                if !homes.contains(&set) {
                    homes.push(set);
                }
            }
            homes
        };
        Ok(Self {
            home: resolved(&home),
            state: resolved(state),
            config: config.as_deref().map(resolved),
            runtime: runtime.as_deref().map(resolved),
            root: resolved(root),
            config_homes: both(".config", "XDG_CONFIG_HOME"),
            data_homes: both(".local/share", "XDG_DATA_HOME"),
            programs: [&programs.jail, &programs.agent, &programs.txt]
                .into_iter()
                .map(|program| resolved(program))
                .collect(),
            path: std::env::var_os("PATH")
                .map(|path| {
                    std::env::split_paths(&path)
                        .filter(|dir| dir.is_absolute())
                        .map(|dir| resolved(&dir))
                        .collect()
                })
                .unwrap_or_default(),
        })
    }

    /// What no grant may be, contain or lie inside, named; with what no
    /// grant the model can write may hold, when `writable`.
    fn protected(&self, writable: bool) -> Vec<Protected> {
        let mut places = vec![("td-agent's state".to_string(), self.state.clone(), false)];
        places.extend(
            self.config
                .clone()
                .map(|dir| ("td-agent's configuration".to_string(), dir, false)),
        );
        places.extend(
            self.runtime
                .clone()
                .map(|dir| ("the session's runtime directory".to_string(), dir, false)),
        );
        places.push(("the workspace root".into(), self.root.clone(), false));
        let credentials = "where credentials, a profile or what the session runs live";
        let below = |base: &Path, names: &[&str]| -> Vec<Protected> {
            names
                .iter()
                .map(|name| (credentials.to_string(), resolved(&base.join(name)), false))
                .collect()
        };
        places.extend(below(&self.home, SENSITIVE));
        for config in &self.config_homes {
            places.extend(below(config, CONFIG_SENSITIVE));
        }
        for data in &self.data_homes {
            places.extend(below(data, DATA_SENSITIVE));
        }
        if writable {
            let replace = "which td-agent runs, so the model could replace its own jail";
            places.extend(
                self.programs
                    .iter()
                    .map(|program| (replace.to_string(), program.clone(), true)),
            );
            let run = "where programs the human runs live";
            places.extend(
                EXECUTED
                    .iter()
                    .map(|name| (run.to_string(), resolved(&self.home.join(name)), true)),
            );
            places.extend(
                self.path
                    .iter()
                    .map(|dir| (run.to_string(), dir.clone(), true)),
            );
        }
        places
    }

    /// Why `path` may not be bound into a workspace, the model able to
    /// write it or not, if it may not.
    fn refusal(&self, path: &Path, writable: bool) -> Option<String> {
        if path == Path::new("/") || self.home.starts_with(path) {
            return Some(format!(
                "{} is or contains your home directory",
                path.display()
            ));
        }
        let protected = self.protected(writable);
        if let Some((what, place, _)) = protected
            .iter()
            .find(|(_, place, held)| reaches(path, place, *held))
        {
            return Some(format!(
                "{} is, contains or lies inside {} ({what})",
                path.display(),
                place.display()
            ));
        }
        // The same through a bind mount: by where each tree the grant
        // carries lies on its device.
        match fs::read_to_string("/proc/self/mountinfo") {
            Ok(table) => mount_refusal(&table, path, &protected),
            Err(e) => Some(format!(
                "the mount table, to check {} against: {e}",
                path.display()
            )),
        }
    }
}

/// A mount as `/proc/self/mountinfo` has it: its device, the directory
/// of that device it shows, and where.
struct Mount {
    device: String,
    root: PathBuf,
    point: PathBuf,
}

/// A mountinfo field with its octal escapes (`\040` a space) read.
fn unescape(field: &str) -> PathBuf {
    let mut bytes = Vec::with_capacity(field.len());
    let raw = field.as_bytes();
    let mut at = 0;
    while let Some(&byte) = raw.get(at) {
        let octal = raw
            .get(at + 1..at + 4)
            .filter(|digits| byte == b'\\' && digits.iter().all(|d| (b'0'..=b'7').contains(d)))
            .and_then(|digits| u8::from_str_radix(std::str::from_utf8(digits).ok()?, 8).ok());
        match octal {
            Some(decoded) => {
                bytes.push(decoded);
                at += 4;
            }
            None => {
                bytes.push(byte);
                at += 1;
            }
        }
    }
    PathBuf::from(std::ffi::OsString::from_vec(bytes))
}

fn mounts(table: &str) -> Vec<Mount> {
    table
        .lines()
        .filter_map(|line| {
            let mut fields = line.split(' ');
            let device = fields.nth(2)?.to_string();
            let root = unescape(fields.next()?);
            let point = unescape(fields.next()?);
            Some(Mount {
                device,
                root,
                point,
            })
        })
        .collect()
}

/// Where `path` lies on its device: the device, and the path within it,
/// by the mount it lies under, the last mounted of the deepest.
fn identity(mounts: &[Mount], path: &Path) -> Option<(String, PathBuf)> {
    let mount = mounts
        .iter()
        .filter(|mount| path.starts_with(&mount.point))
        .max_by_key(|mount| mount.point.components().count())?;
    let rest = path.strip_prefix(&mount.point).ok()?;
    Some((mount.device.clone(), mount.root.join(rest)))
}

/// Why `path`, or a mount at or below it, reaches one of `protected` on
/// its device, if one does.
fn mount_refusal(table: &str, path: &Path, protected: &[Protected]) -> Option<String> {
    let mounts = mounts(table);
    let mut trees: Vec<(String, PathBuf)> = identity(&mounts, path).into_iter().collect();
    trees.extend(
        mounts
            .iter()
            .filter(|mount| mount.point.starts_with(path))
            .map(|mount| (mount.device.clone(), mount.root.clone())),
    );
    protected.iter().find_map(|(what, place, held)| {
        let (device, inside) = identity(&mounts, place)?;
        trees
            .iter()
            .any(|(tree_device, tree)| *tree_device == device && reaches(tree, &inside, *held))
            .then(|| {
                format!(
                    "{} reaches {} ({what}) through a mount",
                    path.display(),
                    place.display()
                )
            })
    })
}

/// `path` resolved: where it exists, as it resolves; where it does not,
/// its deepest existing ancestor resolved with the rest appended, so a
/// place not made yet is still named where it will be.
fn resolved(path: &Path) -> PathBuf {
    let mut missing = Vec::new();
    let mut at = path;
    loop {
        if let Ok(real) = fs::canonicalize(at) {
            return missing
                .iter()
                .rev()
                .fold(real, |resolved, name| resolved.join(name));
        }
        match (at.parent(), at.file_name()) {
            (Some(parent), Some(name)) => {
                missing.push(name.to_os_string());
                at = parent;
            }
            _ => return path.to_path_buf(),
        }
    }
}

/// One is the other or lies inside it.
fn overlaps(a: &Path, b: &Path) -> bool {
    a.starts_with(b) || b.starts_with(a)
}

/// A refused place: what it is, where, and whether only a grant that
/// holds it is refused, as for where programs live, which a grant inside
/// puts nothing into.
type Protected = (String, PathBuf, bool);

/// Whether `grant` reaches `place`: holds it, or, unless `held` alone
/// counts, lies inside it.
fn reaches(grant: &Path, place: &Path, held: bool) -> bool {
    if held {
        place.starts_with(grant)
    } else {
        overlaps(grant, place)
    }
}

/// Whether `dir` is a git repository's top: a work tree, which holds
/// `.git`, or a git directory.
pub fn is_repository(dir: &Path) -> bool {
    fs::symlink_metadata(dir.join(".git")).is_ok() || is_git_directory(dir)
}

/// Whether `dir` is a git directory, a bare repository's included: it
/// holds `HEAD`, `objects` and `refs`.
fn is_git_directory(dir: &Path) -> bool {
    dir.join("HEAD").is_file() && dir.join("objects").is_dir() && dir.join("refs").is_dir()
}

/// Why `path` cannot be named to the jail and the conversation's process,
/// if it cannot: they take text, one line.
fn untextual(path: &Path) -> Option<String> {
    path.to_str()
        .is_none_or(|text| text.contains(['\n', '\r']))
        .then(|| format!("{} cannot be named as text", path.display()))
}

/// The directory `chosen` names, admitted as a directory workspace, or why
/// not: a directory, in no git repository, and none of `places`, nor
/// overlapping a shared directory, which every workspace is given anyway.
pub fn admit_directory(
    chosen: &Path,
    places: &Places,
    shared: &[Shared],
) -> Result<PathBuf, String> {
    let path = fs::canonicalize(chosen).map_err(|e| format!("{}: {e}", chosen.display()))?;
    if !path.is_dir() {
        return Err(format!("{} is not a directory", path.display()));
    }
    if let Some(why) = untextual(&path) {
        return Err(why);
    }
    // A repository's top, or anything inside a git directory: the model
    // could write hooks or configuration the human's git then runs. A
    // work tree's subdirectory is admitted: its `.git` is out of reach.
    let git_directory = path
        .ancestors()
        .find(|dir| dir.file_name() == Some(".git".as_ref()) || is_git_directory(dir));
    if let Some(repository) = git_directory.or(is_repository(&path).then_some(path.as_path())) {
        return Err(format!(
            "{} is in the git repository {}; repository workspaces come with the git \
             worker (increment 11)",
            path.display(),
            repository.display()
        ));
    }
    if let Some(why) = places.refusal(&path, true) {
        return Err(why);
    }
    if let Some(shared) = shared.iter().find(|shared| overlaps(&path, &shared.path)) {
        return Err(format!(
            "{} overlaps the shared directory {}, which every workspace has",
            path.display(),
            shared.path.display()
        ));
    }
    Ok(path)
}

/// The configured shared directories that may be bound, resolved, and a
/// note for each that may not: absent, not a directory, or refused.
pub fn admit_shared(configured: &[Shared], places: &Places) -> (Vec<Shared>, Vec<String>) {
    let mut admitted: Vec<Shared> = Vec::new();
    let mut notes = Vec::new();
    for shared in configured {
        let path = match fs::canonicalize(&shared.path) {
            Ok(path) if path.is_dir() => path,
            Ok(path) => {
                notes.push(format!("shared {} is not a directory", path.display()));
                continue;
            }
            Err(e) => {
                notes.push(format!("shared {}: {e}", shared.path.display()));
                continue;
            }
        };
        if let Some(why) = untextual(&path).or_else(|| places.refusal(&path, shared.write)) {
            notes.push(format!("shared directory refused: {why}"));
            continue;
        }
        if let Some(other) = admitted.iter().find(|other| overlaps(&path, &other.path)) {
            notes.push(format!(
                "shared {} overlaps shared {}",
                path.display(),
                other.path.display()
            ));
            continue;
        }
        admitted.push(Shared {
            path,
            write: shared.write,
        });
    }
    (admitted, notes)
}

/// The state directory's directory of conversations' jail directories.
pub const JAIL: &str = "jail";

/// A conversation's jail directory: its instances' home, its scratch
/// workspace and its specs, removed with the conversation.
pub fn jail_dir(state: &StateDir, id: &Id) -> PathBuf {
    state.root().join(JAIL).join(id.as_str())
}

/// Removes the specs a conversation's earlier process left, killed
/// before it could: only a running process's instances need theirs.
pub fn clear_specs(state: &StateDir, id: &Id) -> Result<(), String> {
    let specs = jail_dir(state, id).join("specs");
    match fs::remove_dir_all(&specs) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            Err(format!("{}: {e}", specs.display()))
        }
        _ => Ok(()),
    }
}

/// Removes `root` and everything beneath it with a walk that never
/// follows a link and gives each directory back to its owner before
/// listing it, since a jailed tool may have left one unreadable or
/// unwritable (§7, Closing). Every step after `root` goes through a
/// descriptor of the directory it is in, opened without following a
/// link, so a process still running in the tree cannot swap a directory
/// for a link out of it. Each directory is listed once; the walk keeps
/// its own stack, so a deep tree cannot exhaust the thread's.
pub fn remove_tree(root: &Path) -> std::io::Result<()> {
    match fs::symlink_metadata(root) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
        Ok(meta) if !meta.is_dir() => return fs::remove_file(root),
        Ok(_) => {}
    }
    // A directory open, and the names of its directories still to remove.
    struct Frame {
        dir: File,
        name: Option<OsString>,
        pending: Vec<OsString>,
    }
    let through = |dir: &File| PathBuf::from(format!("/proc/self/fd/{}", dir.as_raw_fd()));
    let enter = |dir: File, name: Option<OsString>| -> std::io::Result<Frame> {
        let at = through(&dir);
        let mut pending = Vec::new();
        for entry in fs::read_dir(&at)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                pending.push(entry.file_name());
            } else {
                fs::remove_file(at.join(entry.file_name()))?;
            }
        }
        Ok(Frame { dir, name, pending })
    };
    let mut stack = vec![enter(open_directory(root)?, None)?];
    let mut hoisted = 0u64;
    while let Some(frame) = stack.last_mut() {
        if let Some(name) = frame.pending.pop() {
            let path = through(&frame.dir).join(&name);
            // Deeper than the walk holds descriptors for: the directory is
            // moved up beside the top's, within the tree, and walked from
            // there.
            if stack.len() >= MAX_HELD {
                let top = stack
                    .first()
                    .map(|top| through(&top.dir))
                    .ok_or_else(|| std::io::Error::other("the walk lost its top"))?;
                // Moving a directory to another parent rewrites its `..`,
                // which needs it writable: reclaimed first.
                match reclaim(&path) {
                    Ok(_) => {}
                    Err(e) if matches!(e.raw_os_error(), Some(ELOOP | ENOTDIR)) => {
                        fs::remove_file(&path)?;
                        continue;
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(e) => return Err(e),
                }
                let moved = loop {
                    hoisted += 1;
                    let name = OsString::from(format!(".deep-{hoisted}"));
                    match fs::rename(&path, top.join(&name)) {
                        Ok(()) => break Some(name),
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => break None,
                        // A name already there: the next.
                        Err(e) if matches!(e.raw_os_error(), Some(EEXIST | ENOTEMPTY)) => {}
                        Err(e) => return Err(e),
                    }
                };
                if let (Some(name), Some(top)) = (moved, stack.first_mut()) {
                    top.pending.push(name);
                }
                continue;
            }
            match open_directory(&path) {
                Ok(dir) => {
                    let child = enter(dir, Some(name))?;
                    stack.push(child);
                }
                // Made a link or a file since it was listed.
                Err(e) if matches!(e.raw_os_error(), Some(ELOOP | ENOTDIR)) => {
                    fs::remove_file(&path)?;
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
            continue;
        }
        let Some(done) = stack.pop() else {
            break;
        };
        match (done.name, stack.last()) {
            (Some(name), Some(parent)) => fs::remove_dir(through(&parent.dir).join(name))?,
            _ => fs::remove_dir(root)?,
        }
    }
    Ok(())
}

/// open(2) flags std does not name.
const O_NOFOLLOW: i32 = 0o400000;
const O_DIRECTORY: i32 = 0o200000;
const O_PATH: i32 = 0o10000000;
const ELOOP: i32 = 40;
const ENOTDIR: i32 = 20;
const EEXIST: i32 = 17;
const ENOTEMPTY: i32 = 39;
/// The most directories the removal walk holds open at once.
const MAX_HELD: usize = 256;

/// The directory `path` names, a link refused, given back to its owner:
/// held without reading, which a directory the jail made unreadable
/// allows, and made 0700 through that hold, so both are the one
/// directory. The hold names it at `/proc/self/fd/N`.
fn reclaim(path: &Path) -> std::io::Result<(File, PathBuf)> {
    let held = fs::OpenOptions::new()
        .read(true)
        .custom_flags(O_PATH | O_NOFOLLOW | O_DIRECTORY)
        .open(path)?;
    let at = PathBuf::from(format!("/proc/self/fd/{}", held.as_raw_fd()));
    fs::set_permissions(&at, fs::Permissions::from_mode(0o700))?;
    Ok((held, at))
}

/// `path` reclaimed, then opened again through its hold to be listed.
fn open_directory(path: &Path) -> std::io::Result<File> {
    let (_held, at) = reclaim(path)?;
    fs::OpenOptions::new()
        .read(true)
        .custom_flags(O_DIRECTORY)
        .open(&at)
}

/// The instance policy for `workspace`, its directories made where they
/// are td-agent's, and the directory specs go in, outside every grant.
pub fn policy(
    workspace: &Workspace,
    state: &StateDir,
    id: &Id,
    shared: &[Shared],
) -> Result<(Policy, PathBuf), String> {
    let dir = jail_dir(state, id);
    let made = |name: &str| -> Result<PathBuf, String> {
        let path = dir.join(name);
        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&path)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        fs::canonicalize(&path).map_err(|e| format!("{}: {e}", path.display()))
    };
    let tree = match workspace {
        Workspace::Scratch | Workspace::Template(_) => made("scratch")?,
        Workspace::Directory(path) => path.clone(),
    };
    let policy = Policy {
        home: made("home")?,
        worktrees: vec![tree],
        checkouts: Vec::new(),
        repositories: Vec::new(),
        objects: Vec::new(),
        read: shared
            .iter()
            .filter(|shared| !shared.write)
            .map(|shared| shared.path.clone())
            .collect(),
        write: shared
            .iter()
            .filter(|shared| shared.write)
            .map(|shared| shared.path.clone())
            .collect(),
    };
    Ok((policy, made("specs")?))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn places(home: &Path) -> Places {
        Places {
            home: home.to_path_buf(),
            state: home.join(".local/state/td-agent"),
            config: Some(home.join(".config/td-agent")),
            runtime: Some("/run/user/1000".into()),
            root: home.join("td-agent"),
            config_homes: vec![home.join(".config")],
            data_homes: vec![home.join(".local/share"), "/data".into()],
            programs: vec![home.join("src/td/target/release/td-jail")],
            path: vec![home.join("tools/bin")],
        }
    }

    #[test]
    fn a_workspace_round_trips() {
        for workspace in [
            Workspace::Scratch,
            Workspace::Directory("/w/a b".into()),
            Workspace::Template("td notes".into()),
        ] {
            assert_eq!(
                Workspace::from_json(&workspace.to_json()).unwrap(),
                workspace
            );
            let word = workspace.argument().into_string().unwrap();
            assert_eq!(Workspace::parse_argument(&word).unwrap(), workspace);
        }
        assert!(Workspace::parse_argument("relative").is_err());
        for word in [
            "template:",
            "template:Empty",
            "template: x",
            "template:a\tb",
        ] {
            assert!(Workspace::parse_argument(word).is_err(), "{word}");
        }
        assert_eq!(
            Workspace::Template("/home/u/p".into()).label(),
            "template /home/u/p"
        );
        assert!(Workspace::from_json(&Json::Obj(vec![(
            "kind".into(),
            Json::Str("directory".into())
        )]))
        .is_err());
    }

    #[test]
    fn sensitive_places_are_refused() {
        let home = Path::new("/home/u");
        let places = places(home);
        for refused in [
            "/",
            "/home",
            "/home/u",
            "/home/u/.ssh",
            "/home/u/.ssh/keys",
            "/home/u/.config",
            "/home/u/.config/td-agent/x",
            "/home/u/.local/state",
            "/home/u/td-agent",
            "/home/u/td-agent/w",
            "/run/user",
            "/home/u/.mozilla/firefox",
            "/home/u/.config/systemd/user",
            "/home/u/.config/autostart",
            "/home/u/.local/share/keyrings",
            "/data/td-pass",
            "/home/u/.var/app/org.example",
        ] {
            assert!(
                places.refusal(Path::new(refused), false).is_some(),
                "{refused}"
            );
        }
        for admitted in [
            "/home/u/src/other",
            "/home/u/Downloads",
            "/srv/data",
            "/home/u/.configs",
        ] {
            assert_eq!(
                places.refusal(Path::new(admitted), true),
                None,
                "{admitted}"
            );
        }
        // A checkout holding the jail, or a directory programs are run
        // from: read, never written.
        for checkout in [
            "/home/u/src",
            "/home/u/src/td",
            "/home/u/src/td/target",
            "/home/u/tools",
            "/home/u/.local/bin",
            "/home/u/bin",
        ] {
            let why = places.refusal(Path::new(checkout), true).unwrap();
            assert!(
                why.contains("replace its own jail") || why.contains("programs the human runs"),
                "{why}"
            );
            assert_eq!(places.refusal(Path::new(checkout), false), None);
        }
        // Inside where programs live puts nothing there.
        assert_eq!(places.refusal(Path::new("/home/u/tools/bin/x"), true), None);
        assert_eq!(places.refusal(Path::new("/home/u/bin/x"), true), None);
    }

    #[test]
    fn directories_and_shared_ones_are_admitted_or_refused_by_name() {
        let base = std::env::temp_dir().join(format!(
            "td-agent-workspace-{}-{}",
            std::process::id(),
            crate::store::random_hex(4).unwrap()
        ));
        for dir in [
            "home/src/a",
            "home/src/repo/.git/hooks",
            "home/src/repo/sub",
            "home/src/bare.git/objects",
            "home/src/bare.git/refs",
            "home/Downloads",
            "home/plain",
            "home/.ssh",
        ] {
            fs::create_dir_all(base.join(dir)).unwrap();
        }
        fs::write(base.join("home/file"), "").unwrap();
        fs::write(
            base.join("home/src/bare.git/HEAD"),
            "ref: refs/heads/main\n",
        )
        .unwrap();
        let base = fs::canonicalize(&base).unwrap();
        let home = base.join("home");
        let places = places(&home);
        let shared = vec![Shared {
            path: home.join("Downloads"),
            write: false,
        }];
        assert_eq!(
            admit_directory(&home.join("src/a"), &places, &shared).unwrap(),
            home.join("src/a")
        );
        // A work tree's subdirectory: its `.git` is out of reach.
        assert_eq!(
            admit_directory(&home.join("src/repo/sub"), &places, &shared).unwrap(),
            home.join("src/repo/sub")
        );
        for (refused, why) in [
            ("src/repo", "git repository"),
            ("src/repo/.git/hooks", "git repository"),
            ("src/bare.git", "git repository"),
            ("src/bare.git/objects", "git repository"),
            ("file", "not a directory"),
            ("Downloads", "shared directory"),
            ("missing", "No such file"),
            (".ssh", "credentials"),
        ] {
            let e = admit_directory(&home.join(refused), &places, &shared).unwrap_err();
            assert!(e.contains(why), "{refused}: {e}");
        }
        assert!(admit_directory(&home, &places, &shared)
            .unwrap_err()
            .contains("home directory"));

        let (admitted, notes) = admit_shared(
            &[
                Shared {
                    path: home.join("Downloads"),
                    write: false,
                },
                Shared {
                    path: home.join("Downloads/../plain"),
                    write: true,
                },
                Shared {
                    path: home.join("gone"),
                    write: false,
                },
                Shared {
                    path: home.clone(),
                    write: false,
                },
                Shared {
                    path: home.join("plain"),
                    write: false,
                },
            ],
            &places,
        );
        assert_eq!(
            admitted,
            [
                Shared {
                    path: home.join("Downloads"),
                    write: false
                },
                Shared {
                    path: home.join("plain"),
                    write: true
                }
            ]
        );
        assert_eq!(notes.len(), 3, "{notes:?}");
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn a_policy_binds_the_workspace_and_the_shared_directories() {
        let root = std::env::temp_dir().join(format!(
            "td-agent-workspace-policy-{}-{}",
            std::process::id(),
            crate::store::random_hex(4).unwrap()
        ));
        fs::create_dir_all(&root).unwrap();
        let root = fs::canonicalize(&root).unwrap();
        let state = StateDir::at(root.clone());
        let id = Id::random().unwrap();
        let shared = [
            Shared {
                path: "/d".into(),
                write: false,
            },
            Shared {
                path: "/e".into(),
                write: true,
            },
        ];
        let (policy, specs) = policy(&Workspace::Scratch, &state, &id, &shared).unwrap();
        let jail = root.join("jail").join(id.as_str());
        assert_eq!(policy.worktrees, [jail.join("scratch")]);
        assert_eq!(policy.home, jail.join("home"));
        assert_eq!(specs, jail.join("specs"));
        assert_eq!(policy.read, [PathBuf::from("/d")]);
        assert_eq!(policy.write, [PathBuf::from("/e")]);
        assert!(jail.join("scratch").is_dir());
        let (policy, _) = policy_of_directory(&state, &id);
        assert_eq!(policy.worktrees, [PathBuf::from("/w/a")]);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_place_not_made_yet_is_named_where_it_will_be() {
        let base = std::env::temp_dir().join(format!(
            "td-agent-workspace-resolved-{}-{}",
            std::process::id(),
            crate::store::random_hex(4).unwrap()
        ));
        fs::create_dir_all(base.join("real")).unwrap();
        std::os::unix::fs::symlink(base.join("real"), base.join("config")).unwrap();
        let base = fs::canonicalize(&base).unwrap();
        // `config/td-agent` does not exist: it is where it will be made.
        assert_eq!(
            resolved(&base.join("config/td-agent/key")),
            base.join("real/td-agent/key")
        );
        assert_eq!(resolved(Path::new("relative")), Path::new("relative"));
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn a_bind_mount_of_a_protected_place_is_refused() {
        // A share at /srv/share holding, at /srv/share/keys, a bind of
        // /home/u/.ssh, which lives on device 0:1 under /home's mount.
        let table = "\
            20 1 0:1 / / rw - ext4 /dev/a rw\n\
            21 20 0:2 / /home rw - ext4 /dev/b rw\n\
            22 20 0:3 / /srv rw - ext4 /dev/c rw\n\
            23 22 0:2 /u/.ssh /srv/share/keys rw - ext4 /dev/b rw\n\
            24 22 0:2 /u/src/a\\040b /srv/other\\040dir rw - ext4 /dev/b rw\n";
        let protected = [(
            "credentials".to_string(),
            PathBuf::from("/home/u/.ssh"),
            false,
        )];
        let why = mount_refusal(table, Path::new("/srv/share"), &protected).unwrap();
        assert!(why.contains("through a mount"), "{why}");
        // A bind of a directory holding it, and one of a directory inside it.
        let inside = [(
            "credentials".to_string(),
            PathBuf::from("/home/u/.ssh/keys"),
            false,
        )];
        assert!(mount_refusal(table, Path::new("/srv/share"), &inside).is_some());
        let above = [("credentials".to_string(), PathBuf::from("/home/u"), false)];
        assert!(mount_refusal(table, Path::new("/srv/share"), &above).is_some());
        // Where programs live is refused only to a grant holding it.
        let held = [("programs".to_string(), PathBuf::from("/home/u"), true)];
        assert_eq!(mount_refusal(table, Path::new("/srv/share"), &held), None);
        let held = [(
            "programs".to_string(),
            PathBuf::from("/home/u/.ssh/bin"),
            true,
        )];
        assert!(mount_refusal(table, Path::new("/srv/share"), &held).is_some());
        // Elsewhere on the device, and a name with a space, admitted.
        assert_eq!(
            mount_refusal(table, Path::new("/srv/other dir"), &protected),
            None
        );
        assert_eq!(
            mount_refusal(table, Path::new("/home/u/src"), &protected),
            None
        );
        let mounts = mounts(table);
        assert_eq!(
            identity(&mounts, Path::new("/srv/other dir/x")),
            Some(("0:2".to_string(), PathBuf::from("/u/src/a b/x")))
        );
    }

    #[test]
    fn a_tree_is_removed_whatever_the_jail_left() {
        let root = std::env::temp_dir().join(format!(
            "td-agent-workspace-remove-{}-{}",
            std::process::id(),
            crate::store::random_hex(4).unwrap()
        ));
        // Deeper than the walk holds descriptors for, built a step at a
        // time, since the whole path is past what one call takes.
        let deep = root.join("tree");
        fs::create_dir_all(&deep).unwrap();
        let mut at = open_directory(&deep).unwrap();
        for depth in 0..MAX_HELD + 40 {
            let child = PathBuf::from(format!("/proc/self/fd/{}/d", at.as_raw_fd()));
            fs::create_dir(&child).unwrap();
            // Where the walk's hold ends, ones the jail made read-only.
            if (MAX_HELD - 2..=MAX_HELD + 2).contains(&depth) {
                let locked = format!("/proc/self/fd/{}", at.as_raw_fd());
                fs::set_permissions(&locked, fs::Permissions::from_mode(0o500)).unwrap();
            }
            at = open_directory(&child).unwrap();
        }
        fs::write(format!("/proc/self/fd/{}/f", at.as_raw_fd()), "x").unwrap();
        drop(at);
        fs::create_dir_all(&deep).unwrap();
        fs::write(deep.join("f"), "x").unwrap();
        fs::create_dir_all(root.join("tree/locked/inner")).unwrap();
        fs::write(root.join("tree/locked/inner/g"), "y").unwrap();
        fs::write(root.join("outside"), "kept").unwrap();
        std::os::unix::fs::symlink(root.join("outside"), root.join("tree/link")).unwrap();
        std::os::unix::fs::symlink(&root, root.join("tree/up")).unwrap();
        for locked in ["tree/locked/inner", "tree/locked"] {
            fs::set_permissions(root.join(locked), fs::Permissions::from_mode(0o000)).unwrap();
        }
        remove_tree(&root.join("tree")).unwrap();
        assert!(!root.join("tree").exists());
        // A link is removed, never followed.
        assert_eq!(fs::read_to_string(root.join("outside")).unwrap(), "kept");
        // A directory swapped for a link after it was listed: the walk's
        // open refuses the link, which is then removed as a file.
        fs::create_dir_all(root.join("dir")).unwrap();
        std::os::unix::fs::symlink(root.join("dir"), root.join("swapped")).unwrap();
        let refused = open_directory(&root.join("swapped")).unwrap_err();
        assert!(
            matches!(refused.raw_os_error(), Some(ELOOP | ENOTDIR)),
            "{refused}"
        );
        fs::set_permissions(root.join("dir"), fs::Permissions::from_mode(0o000)).unwrap();
        assert!(open_directory(&root.join("dir")).is_ok());
        assert_eq!(
            fs::metadata(root.join("dir")).unwrap().permissions().mode() & 0o777,
            0o700
        );
        remove_tree(&root.join("tree")).unwrap();
        fs::remove_dir_all(&root).unwrap();
    }

    fn policy_of_directory(state: &StateDir, id: &Id) -> (Policy, PathBuf) {
        policy(&Workspace::Directory("/w/a".into()), state, id, &[]).unwrap()
    }
}

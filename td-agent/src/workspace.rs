//! A conversation's workspace (DESIGN.md §7, §8): a directory the human
//! admits, or a scratch directory td-agent makes, which the conversation's
//! tools work in through td-jail's `workspace` kind (`crate::jail`).
//!
//! Admission here is td-agent's half: what §8 refuses on top of td-jail's
//! own checks, so a directory is refused when it is chosen, by name, not
//! at its first tool call. td-jail then refuses the rest (reserved trees,
//! links, overlap, the caller's home by mount identity) at every launch.

use std::collections::BTreeSet;
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
    /// A repository template's: its worktrees, each of a workspace
    /// repository over the store.
    Repositories(Repositories),
}

/// How a template's workspace is passed a new conversation's process.
const TEMPLATE_ARGUMENT: &str = "template:";
/// And a repository template's, its record as JSON.
const REPOSITORIES_ARGUMENT: &str = "repositories:";
/// The longest workspace name, made from its template's.
const MAX_WORKSPACE_NAME: usize = 48;

/// A repository template's workspace (DESIGN.md §7, Layout), every path
/// fixed when its conversation is made, so a later edit of the template
/// or the configuration moves nothing of it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Repositories {
    pub template: String,
    /// Its directories' name, under the workspace root and the data
    /// directory's `ws/`: the template's, and the conversation's id.
    pub name: String,
    pub entries: Vec<Entry>,
}

/// The publish repository of `entry`, of the repository workspace named
/// `name`, in the data directory `data` (DESIGN.md §7, Layout):
/// `publish/<name>/<repo>.git`, as its workspace repository is under
/// `ws/`.
pub fn publish_repository(data: &Path, name: &str, entry: &Entry) -> Result<PathBuf, String> {
    let repo = entry
        .repository
        .file_name()
        .ok_or_else(|| format!("{} names no repository", entry.repository.display()))?;
    Ok(data.join("publish").join(name).join(repo))
}

/// One of a repository workspace's worktrees.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Entry {
    /// The remote, as td-agent's record names it (`Remote::url`).
    pub remote: String,
    pub base: String,
    pub branch: String,
    pub sparse: Option<Vec<String>>,
    /// The store's git directory, the workspace repository and the
    /// worktree's id there and checkout in the workspace tree.
    pub store: PathBuf,
    pub repository: PathBuf,
    pub id: String,
    pub checkout: PathBuf,
}

impl Entry {
    fn to_json(&self) -> Json {
        let path = |path: &Path| Json::Str(path.display().to_string());
        Json::Obj(vec![
            ("remote".into(), Json::Str(self.remote.clone())),
            ("base".into(), Json::Str(self.base.clone())),
            ("branch".into(), Json::Str(self.branch.clone())),
            (
                "sparse".into(),
                self.sparse.as_ref().map_or(Json::Null, |paths| {
                    Json::Arr(paths.iter().cloned().map(Json::Str).collect())
                }),
            ),
            ("store".into(), path(&self.store)),
            ("repository".into(), path(&self.repository)),
            ("id".into(), Json::Str(self.id.clone())),
            ("checkout".into(), path(&self.checkout)),
        ])
    }

    fn from_json(value: &Json) -> Result<Self, String> {
        let text = |key: &str| {
            value
                .get(key)
                .and_then(Json::as_str)
                .map(str::to_string)
                .ok_or_else(|| format!("a repository entry has no `{key}`"))
        };
        let path = |key: &str| {
            text(key).map(PathBuf::from).and_then(|path| {
                path.is_absolute()
                    .then_some(path)
                    .ok_or_else(|| format!("a repository entry's `{key}` is not absolute"))
            })
        };
        let sparse = match value.get("sparse") {
            None | Some(Json::Null) => None,
            Some(paths) => Some(
                paths
                    .as_arr()
                    .ok_or("a repository entry's `sparse` is not a list")?
                    .iter()
                    .map(|path| {
                        path.as_str()
                            .map(str::to_string)
                            .ok_or("a sparse path is not text")
                    })
                    .collect::<Result<Vec<_>, _>>()?,
            ),
        };
        let remote = text("remote")?;
        crate::git::Remote::parse(&remote)?;
        let base = text("base")?;
        crate::git::branch_name(&base)?;
        let branch = text("branch")?;
        crate::git::branch_name(&branch)?;
        crate::repo::cone(sparse.as_deref())?;
        let id = text("id")?;
        crate::repo::worktree_id(&id)?;
        Ok(Self {
            remote,
            base,
            branch,
            sparse,
            store: path("store")?,
            repository: path("repository")?,
            id,
            checkout: path("checkout")?,
        })
    }
}

impl Repositories {
    fn to_json(&self) -> Json {
        Json::Obj(vec![
            ("kind".into(), Json::Str("repositories".into())),
            ("template".into(), Json::Str(self.template.clone())),
            ("name".into(), Json::Str(self.name.clone())),
            (
                "entries".into(),
                Json::Arr(self.entries.iter().map(Entry::to_json).collect()),
            ),
        ])
    }

    fn from_json(value: &Json) -> Result<Self, String> {
        let template = value
            .get("template")
            .and_then(Json::as_str)
            .ok_or("a repository workspace has no template")
            .map_err(String::from)
            .and_then(crate::config::template_name)?;
        let name = value
            .get("name")
            .and_then(Json::as_str)
            .filter(|name| workspace_name(name))
            .ok_or("a repository workspace has no name td-agent makes")?
            .to_string();
        let entries = value
            .get("entries")
            .and_then(Json::as_arr)
            .ok_or("a repository workspace has no entries")?
            .iter()
            .map(Entry::from_json)
            .collect::<Result<Vec<_>, _>>()?;
        if entries.is_empty() || entries.len() > MAX_ENTRIES {
            return Err(format!(
                "a repository workspace has 1 to {MAX_ENTRIES} entries"
            ));
        }
        Ok(Self {
            template,
            name,
            entries,
        })
    }

    /// Its repositories, each once, in the order their entries come.
    pub fn repositories(&self) -> Vec<&Path> {
        let mut seen: Vec<&Path> = Vec::new();
        for entry in &self.entries {
            if !seen.contains(&entry.repository.as_path()) {
                seen.push(&entry.repository);
            }
        }
        seen
    }

    /// The workspace tree, the directory its checkouts are made in.
    pub fn tree(&self) -> Option<&Path> {
        self.entries
            .first()
            .and_then(|entry| entry.checkout.parent())
    }
}

/// td-agent's data directory (DESIGN.md §7, Layout), which holds the
/// stores and workspace repositories: `$XDG_DATA_HOME/td-agent`, else
/// `~/.local/share/td-agent`, a directory and no link, since the places
/// refused to grants name it so, made the caller's alone and named as it
/// resolves, since td-jail binds real paths.
pub fn data_dir() -> Result<PathBuf, String> {
    let data = td_ui::xdg::Base::Data;
    let base = td_ui::xdg::from_env(data).ok_or_else(|| data.missing())?;
    data_in(&base)
}

/// `data_dir` in the data home `base`.
fn data_in(base: &Path) -> Result<PathBuf, String> {
    let dir = base.join("td-agent");
    let failed = |e: std::io::Error| format!("{}: {e}", dir.display());
    DirBuilder::new()
        .recursive(true)
        .create(base)
        .map_err(|e| format!("{}: {e}", base.display()))?;
    match DirBuilder::new().mode(0o700).create(&dir) {
        Err(e) if e.kind() != std::io::ErrorKind::AlreadyExists => return Err(failed(e)),
        _ => {}
    }
    if !fs::symlink_metadata(&dir).map_err(failed)?.is_dir() {
        return Err(format!("{} is not a directory", dir.display()));
    }
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).map_err(failed)?;
    fs::canonicalize(&dir).map_err(failed)
}

/// Reserves `repositories`' name (DESIGN.md §7), its directory in the
/// data directory `data` made by this call alone, so no two workspaces,
/// whose names are short, share a repository or a checkout: false when
/// another has it, or its tree under the workspace root is there.
pub fn reserve(repositories: &Repositories, data: &Path) -> Result<bool, String> {
    let tree = repositories.tree().ok_or("a workspace with no worktrees")?;
    if fs::symlink_metadata(tree).is_ok() {
        return Ok(false);
    }
    let all = data.join("ws");
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&all)
        .map_err(|e| format!("{}: {e}", all.display()))?;
    let own = all.join(&repositories.name);
    match DirBuilder::new().mode(0o700).create(&own) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
        Err(e) => Err(format!("{}: {e}", own.display())),
    }
}

/// The most worktrees a repository workspace has: td-jail binds at most
/// 32 linked worktrees of one repository, and a workspace's entries are
/// held to that in all.
pub(crate) const MAX_ENTRIES: usize = 32;
/// The most directories td-jail binds for one instance: worktrees,
/// checkouts, repositories, the stores' objects and shared directories.
const MAX_TREES: usize = 32;
/// The most bytes a repository workspace's record takes as JSON: it is
/// held whole in `meta`, with the repositories prepared, and in the
/// argument a conversation's process starts with.
const MAX_RECORD: usize = 16 * 1024;

/// A workspace name td-agent makes: lower-case letters, digits and `-`,
/// not starting or ending with `-`.
/// `path`'s bytes as a key holds them: printable ASCII but `%`, `[` and
/// `]` as they are, every other byte as `%XX`, so two paths never share
/// a key.
fn escaped(path: &Path) -> String {
    use std::os::unix::ffi::OsStrExt;
    let mut out = String::new();
    for b in path.as_os_str().as_bytes() {
        if kept(*b) {
            out.push(char::from(*b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Whether `escaped` writes byte `b` as it is.
fn kept(b: u8) -> bool {
    b.is_ascii_graphic() && !matches!(b, b'%' | b'[' | b']')
}

/// Whether `text` is a path as `escaped` writes one, and so a directory's
/// key could hold it: absolute, and no byte written another way.
pub(crate) fn escaped_path(text: &str) -> bool {
    let bytes = text.as_bytes();
    if bytes.first() != Some(&b'/') {
        return false;
    }
    let mut at = 0;
    while let Some(&b) = bytes.get(at) {
        if b == b'%' {
            let hex = |at: usize| {
                bytes
                    .get(at)
                    .filter(|h| h.is_ascii_digit() || (b'A'..=b'F').contains(h))
                    .and_then(|h| char::from(*h).to_digit(16))
            };
            match (hex(at + 1), hex(at + 2)) {
                (Some(high), Some(low)) if !kept((high * 16 + low) as u8) => at += 3,
                _ => return false,
            }
        } else if kept(b) {
            at += 1;
        } else {
            return false;
        }
    }
    true
}

pub(crate) fn workspace_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_WORKSPACE_NAME
        && !name.starts_with('-')
        && !name.ends_with('-')
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// `text` as a name segment: ASCII letters and digits kept, lower case,
/// every run of anything else one `-`, at most `max` bytes.
fn slug(text: &str, max: usize) -> String {
    let mut slug = String::new();
    for c in text.chars() {
        if c.is_ascii_alphanumeric() {
            slug.push(c.to_ascii_lowercase());
        } else if !slug.ends_with('-') && !slug.is_empty() {
            slug.push('-');
        }
    }
    let mut slug: String = slug.chars().take(max).collect();
    while slug.ends_with('-') {
        slug.pop();
    }
    slug
}

/// The workspace `template` makes for conversation `id`: every remote
/// admitted by `admitted`, every branch and base a name td-agent passes
/// to git, and every path named, under `data` (td-agent's data
/// directory) and `root` (the workspace root). Entries naming one remote
/// share its repository; each has a worktree of its own. With its
/// `shared` shared directories it binds no more than td-jail does.
pub fn repositories(
    template: &crate::config::Template,
    id: &Id,
    data: &Path,
    root: &Path,
    admitted: &[crate::git::Admission],
    shared: usize,
) -> Result<Repositories, String> {
    let made = plan(template, id, data, root, shared)?;
    match unadmitted(&made, admitted).first() {
        Some(url) => Err(format!(
            "the remote {url} is not admitted: admit it on the template's card, or list it in `remotes` in the configuration"
        )),
        None => Ok(made),
    }
}

/// The remotes `repositories` names that no admission in `admitted`
/// covers, each once, as td-agent records them: what the human is asked
/// to admit (DESIGN.md §7).
pub fn unadmitted(repositories: &Repositories, admitted: &[crate::git::Admission]) -> Vec<String> {
    let mut unadmitted: Vec<String> = Vec::new();
    for entry in &repositories.entries {
        let covered = crate::git::Remote::parse(&entry.remote)
            .is_ok_and(|remote| admitted.iter().any(|admission| admission.admits(&remote)));
        if !covered && !unadmitted.contains(&entry.remote) {
            unadmitted.push(entry.remote.clone());
        }
    }
    unadmitted
}

/// `repositories` but for admission: every other check, so a template a
/// card's admission would not make is refused before the card asks.
pub fn plan(
    template: &crate::config::Template,
    id: &Id,
    data: &Path,
    root: &Path,
    shared: usize,
) -> Result<Repositories, String> {
    if template.repos.is_empty() || template.repos.len() > MAX_ENTRIES {
        return Err(format!(
            "template {:?} names 1 to {MAX_ENTRIES} repositories",
            template.name
        ));
    }
    let prefix = match slug(&template.name, MAX_WORKSPACE_NAME - 9) {
        empty if empty.is_empty() => "workspace".to_string(),
        named => named,
    };
    let short: String = id.to_string().chars().take(8).collect();
    let name = format!("{prefix}-{}", slug(&short, 8));
    let mut entries: Vec<Entry> = Vec::new();
    for repo in &template.repos {
        let remote = crate::git::Remote::parse(&repo.remote)?;
        let url = remote.url();
        crate::git::branch_name(&repo.base)?;
        crate::git::branch_name(&repo.branch)?;
        crate::repo::cone(repo.sparse.as_deref())?;
        let stem = remote
            .segments
            .last()
            .map(|last| last.strip_suffix(".git").unwrap_or(last))
            .map(|last| slug(last, 32))
            .filter(|stem| !stem.is_empty())
            .unwrap_or_else(|| "repo".to_string());
        let repository = match entries.iter().find(|entry| entry.remote == url) {
            Some(entry) => entry.repository.clone(),
            None => {
                let mut dir = stem.clone();
                let mut n = 2;
                while entries
                    .iter()
                    .any(|entry| entry.repository.file_stem() == Some(dir.as_ref()))
                {
                    dir = format!("{stem}-{n}");
                    n += 1;
                }
                data.join("ws").join(&name).join(format!("{dir}.git"))
            }
        };
        // Two worktrees on one branch would move each other's.
        if entries
            .iter()
            .any(|entry| entry.remote == url && entry.branch == repo.branch)
        {
            return Err(format!(
                "template {:?} names branch {} of {url} twice",
                template.name, repo.branch
            ));
        }
        let first = entries.iter().all(|entry| entry.remote != url);
        let wanted = if first {
            stem.clone()
        } else {
            format!("{stem}-{}", slug(&repo.branch, 32))
        };
        let mut worktree = wanted.clone();
        let mut n = 2;
        while entries.iter().any(|entry| entry.id == worktree) {
            worktree = format!("{wanted}-{n}");
            n += 1;
        }
        crate::repo::worktree_id(&worktree)?;
        entries.push(Entry {
            store: data
                .join("store")
                .join(format!("{}.git", remote.store_name())),
            checkout: root.join(&name).join(&worktree),
            remote: url,
            base: repo.base.clone(),
            branch: repo.branch.clone(),
            sparse: repo.sparse.clone(),
            repository,
            id: worktree,
        });
    }
    let made = Repositories {
        template: template.name.clone(),
        name,
        entries,
    };
    let stores: BTreeSet<&Path> = made.entries.iter().map(|e| e.store.as_path()).collect();
    let trees = made.entries.len() + made.repositories().len() + stores.len() + shared;
    if trees > MAX_TREES {
        return Err(format!(
            "template {:?} binds {trees} directories, its worktrees, repositories, stores and shared directories, and td-jail binds at most {MAX_TREES}",
            template.name
        ));
    }
    if Workspace::Repositories(made.clone())
        .to_json()
        .to_string()
        .len()
        > MAX_RECORD
    {
        return Err(format!(
            "template {:?} is past {MAX_RECORD} bytes as td-agent records it: name fewer sparse paths",
            template.name
        ));
    }
    Ok(made)
}

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
            Self::Repositories(repositories) => repositories.to_json(),
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
            Some("repositories") => Repositories::from_json(value).map(Self::Repositories),
            _ => {
                Err("a workspace is a scratch, a directory, a template or a repository one".into())
            }
        }
    }

    /// The word the window passes a new conversation's process.
    pub fn argument(&self) -> OsString {
        match self {
            Self::Scratch => "scratch".into(),
            Self::Directory(path) => path.as_os_str().to_os_string(),
            Self::Template(name) => format!("{TEMPLATE_ARGUMENT}{name}").into(),
            Self::Repositories(repositories) => {
                format!("{REPOSITORIES_ARGUMENT}{}", repositories.to_json()).into()
            }
        }
    }

    pub fn parse_argument(word: &str) -> Result<Self, String> {
        if word == "scratch" {
            return Ok(Self::Scratch);
        }
        if let Some(name) = word.strip_prefix(TEMPLATE_ARGUMENT) {
            return crate::config::template_name(name).map(Self::Template);
        }
        if let Some(record) = word.strip_prefix(REPOSITORIES_ARGUMENT) {
            let value =
                td_json::parse(record).map_err(|e| format!("a repository workspace: {e}"))?;
            return match Self::from_json(&value)? {
                repositories @ Self::Repositories(_) => Ok(repositories),
                _ => Err("a repository workspace's record names another kind".into()),
            };
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
            Self::Repositories(repositories) => format!("template {}", repositories.template),
        }
    }

    /// The key the human's rules for this workspace are kept under
    /// (DESIGN.md §11), conversation `id`'s: a repository workspace's
    /// name, which its forks share, a directory's path, or a scratch
    /// directory's conversation.
    pub fn key(&self, id: &Id) -> String {
        match self {
            Self::Scratch | Self::Template(_) => format!("conversation {}", id.as_str()),
            Self::Directory(path) => format!("directory {}", escaped(path)),
            Self::Repositories(repositories) => format!("workspace {}", repositories.name),
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
    /// The local repositories admitted as remotes (DESIGN.md §7, Local
    /// repositories), whose hooks run as the human: no workspace or
    /// shared directory may reach one.
    pub locals: Vec<PathBuf>,
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
/// Below each data home: keyrings, td-pass's vault, desktop entries,
/// and td-agent's own stores and workspace repositories.
const DATA_SENSITIVE: &[&str] = &["applications", "keyrings", "td-agent", "td-pass"];
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
            // The window adds them once it has read the admitted remotes.
            locals: Vec::new(),
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
        places.extend(self.locals.iter().map(|local| {
            (
                "a local repository admitted as a remote, whose hooks run as you".to_string(),
                local.clone(),
                false,
            )
        }));
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

/// The most bytes of a local repository's configuration read.
const LOCAL_CONFIG: u64 = 1024 * 1024;
/// Configuration a local repository may not set, as git names it: what
/// would have git read another file, run a program, or write a tree,
/// from elsewhere. `include.` and `includeif.` names are refused too.
const LOCAL_REFUSED: &[&str] = &[
    "core.alternaterefscommand",
    "core.attributesfile",
    "core.fsmonitor",
    "core.hookspath",
    "core.worktree",
    "extensions.worktreeconfig",
];

/// Whether the local repository at `path` may be fetched or pushed
/// (DESIGN.md §7, Local repositories), or why not: git runs its hooks
/// and reads its configuration as the human's, outside any jail, so no
/// jail may reach anything of it git reads. It is named by its own path,
/// through no link. Its git directory is `.git`, a directory and no
/// link, or the path itself when bare; it is no linked worktree's
/// (`commondir`), borrows no objects (`objects/info/alternates`), and
/// its configuration, a file and no link, includes no other and moves
/// no hooks (`LOCAL_REFUSED`). The path, its git directory, its
/// configuration, its hooks directory and every hook are none of
/// td-agent's or the human's protected places, the workspace root among
/// them, and none holds or lies inside any of `reached`, the shared
/// directories and directory workspaces, by path or through a mount.
pub fn local_repository(path: &Path, places: &Places, reached: &[PathBuf]) -> Result<(), String> {
    let real = fs::canonicalize(path).map_err(|e| format!("{}: {e}", path.display()))?;
    if real != path {
        return Err(format!(
            "{} is reached through a link, to {}: name the repository by its own path",
            path.display(),
            real.display()
        ));
    }
    let dot = path.join(".git");
    let git_dir = match fs::symlink_metadata(&dot) {
        Ok(meta) if meta.is_dir() => dot,
        Ok(_) => {
            return Err(format!(
                "{} is a link or a file, as a linked worktree's is: name the repository whose .git it points to",
                dot.display()
            ))
        }
        Err(_) if is_git_directory(path) => path.to_path_buf(),
        Err(_) => return Err(format!("{} is not a git repository", path.display())),
    };
    for (name, why) in [
        ("commondir", "is a linked worktree's"),
        (
            "objects/info/alternates",
            "borrows another repository's objects",
        ),
    ] {
        if fs::symlink_metadata(git_dir.join(name)).is_ok() {
            return Err(format!("{} {why}", git_dir.display()));
        }
    }
    // Nothing of it git reads is a link out of it.
    for dir in [git_dir.clone(), git_dir.join("objects")] {
        for entry in fs::read_dir(&dir).map_err(|e| format!("{}: {e}", dir.display()))? {
            let entry = entry.map_err(|e| format!("{}: {e}", dir.display()))?;
            if entry.file_type().is_ok_and(|kind| kind.is_symlink()) {
                return Err(format!("{} is a link", entry.path().display()));
            }
        }
    }
    if fs::symlink_metadata(git_dir.join("config.worktree")).is_ok() {
        return Err(format!(
            "{} has a config.worktree, which td-agent does not follow",
            git_dir.display()
        ));
    }
    let config = git_dir.join("config");
    match fs::symlink_metadata(&config) {
        Ok(meta) if meta.is_file() && meta.len() <= LOCAL_CONFIG => {
            if let Some(why) = local_config_refusal(&local_config_names(&config)?) {
                return Err(format!("{} {why}", config.display()));
            }
        }
        Ok(meta) if meta.is_file() => {
            return Err(format!("{} is past {LOCAL_CONFIG} bytes", config.display()))
        }
        Ok(_) => return Err(format!("{} is not a file", config.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("{}: {e}", config.display())),
    }
    // What git reads or runs, each where it really lies.
    let mut read = vec![path.to_path_buf(), git_dir.clone()];
    let hooks = git_dir.join("hooks");
    match fs::symlink_metadata(&hooks) {
        Ok(meta) if meta.is_dir() => {
            for entry in fs::read_dir(&hooks).map_err(|e| format!("{}: {e}", hooks.display()))? {
                let entry = entry.map_err(|e| format!("{}: {e}", hooks.display()))?;
                let hook = entry.path();
                read.push(fs::canonicalize(&hook).map_err(|e| format!("{}: {e}", hook.display()))?);
            }
        }
        Ok(_) => return Err(format!("{} is not a directory", hooks.display())),
        Err(_) => {}
    }
    // The repository itself is among `places`' admitted ones.
    let places = Places {
        locals: Vec::new(),
        ..places.clone()
    };
    for one in &read {
        if let Some(why) = places
            .refusal(one, false)
            .or_else(|| reach_refusal(one, reached))
        {
            return Err(why);
        }
    }
    Ok(())
}

/// The names `config` sets, as git's own parser reads them, its
/// includes not followed: git with nothing of the human's
/// configuration, which reads the file and runs nothing.
fn local_config_names(config: &Path) -> Result<Vec<String>, String> {
    let failed = |e: String| format!("{}, as git reads it: {e}", config.display());
    let out = std::process::Command::new("git")
        .env_clear()
        .envs(std::env::var_os("PATH").map(|path| ("PATH", path)))
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .current_dir("/")
        .stdin(std::process::Stdio::null())
        .args(["config", "--file"])
        .arg(config)
        .args(["--no-includes", "--null", "--name-only", "--list"])
        .output()
        .map_err(|e| failed(e.to_string()))?;
    if !out.status.success() {
        return Err(failed(
            String::from_utf8_lossy(&out.stderr).trim().to_string(),
        ));
    }
    Ok(out
        .stdout
        .split(|byte| *byte == 0)
        .filter(|name| !name.is_empty())
        .map(|name| String::from_utf8_lossy(name).to_ascii_lowercase())
        .collect())
}

/// Why a local repository's configuration, setting `names`, is refused,
/// if it is: an include, or a name of `LOCAL_REFUSED`.
fn local_config_refusal(names: &[String]) -> Option<String> {
    names.iter().find_map(|name| {
        if name.starts_with("include.") || name.starts_with("includeif.") {
            Some("includes another file".to_string())
        } else {
            LOCAL_REFUSED
                .contains(&name.as_str())
                .then(|| format!("sets {name}, which td-agent does not follow"))
        }
    })
}

/// Why `path` is refused for overlapping any of `reached`, where a
/// workspace writes, by path or through a mount, if it is.
fn reach_refusal(path: &Path, reached: &[PathBuf]) -> Option<String> {
    if let Some(reach) = reached.iter().find(|reach| overlaps(path, reach)) {
        return Some(format!(
            "{} overlaps {}, which a workspace can write, so what git runs there could be the model's",
            path.display(),
            reach.display()
        ));
    }
    let protected: Vec<Protected> = reached
        .iter()
        .map(|reach| ("where a workspace writes".to_string(), reach.clone(), false))
        .collect();
    match fs::read_to_string("/proc/self/mountinfo") {
        Ok(table) => mount_refusal(&table, path, &protected),
        Err(e) => Some(format!(
            "the mount table, to check {} against: {e}",
            path.display()
        )),
    }
}

/// Why `path` cannot be named to the jail and the conversation's process,
/// if it cannot: they take text, one line.
fn untextual(path: &Path) -> Option<String> {
    path.to_str()
        .is_none_or(|text| text.contains(['\n', '\r']))
        .then(|| format!("{} cannot be named as text", path.display()))
}

/// The git directory `path` is in, or the repository whose top it is.
fn repository_holding(path: &Path) -> Option<&Path> {
    let git_directory = path
        .ancestors()
        .find(|dir| dir.file_name() == Some(".git".as_ref()) || is_git_directory(dir));
    git_directory.or(is_repository(path).then_some(path))
}

/// The repository a template's remote would name for `chosen`, which
/// `admit_directory` refuses as being in it: its work tree's top, or a
/// bare repository itself.
pub fn repository_remote(chosen: &Path) -> Option<PathBuf> {
    let path = fs::canonicalize(chosen).ok()?;
    let repository = repository_holding(&path)?;
    match repository.file_name() {
        Some(name) if name == ".git" => repository.parent().map(Path::to_path_buf),
        _ => Some(repository.to_path_buf()),
    }
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
    if let Some(repository) = repository_holding(&path) {
        return Err(format!(
            "{} is in the git repository {}; a repository template makes workspaces of it",
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

/// The policy of a maintenance instance over one repository's worktrees
/// `entries` (DESIGN.md §9): its home under the conversation's jail
/// directory `dir`, the checkouts, the repository and its store's
/// objects; none without an entry.
pub fn maintenance(dir: &Path, entries: &[&Entry]) -> Option<crate::jail::Policy> {
    let first = entries.first()?;
    Some(crate::jail::Policy {
        home: dir.join("maintenance"),
        checkouts: entries.iter().map(|entry| entry.checkout.clone()).collect(),
        repositories: vec![first.repository.clone()],
        objects: vec![first.store.join("objects")],
        ..crate::jail::Policy::default()
    })
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
/// are td-agent's, and the directory specs go in, outside every grant. A
/// repository workspace binds the repositories in `prepared` alone, each
/// with its checkouts and its stores' objects, and none of the rest
/// until it is (DESIGN.md §7).
pub fn policy(
    workspace: &Workspace,
    state: &StateDir,
    id: &Id,
    shared: &[Shared],
    prepared: &[PathBuf],
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
    let (worktrees, checkouts, repositories, objects) = match workspace {
        Workspace::Scratch | Workspace::Template(_) => {
            (vec![made("scratch")?], Vec::new(), Vec::new(), Vec::new())
        }
        Workspace::Directory(path) => (vec![path.clone()], Vec::new(), Vec::new(), Vec::new()),
        Workspace::Repositories(repositories) => {
            let ready: Vec<&Entry> = repositories
                .entries
                .iter()
                .filter(|entry| prepared.contains(&entry.repository))
                .collect();
            let mut repositories: Vec<PathBuf> = Vec::new();
            let mut objects: Vec<PathBuf> = Vec::new();
            for entry in &ready {
                if !repositories.contains(&entry.repository) {
                    repositories.push(entry.repository.clone());
                }
                let store = entry.store.join("objects");
                if !objects.contains(&store) {
                    objects.push(store);
                }
            }
            let checkouts = ready.iter().map(|entry| entry.checkout.clone()).collect();
            (Vec::new(), checkouts, repositories, objects)
        }
    };
    // A repository workspace's working directory is its first worktree,
    // not bound until it is prepared.
    let directory = match workspace {
        Workspace::Repositories(repositories) => {
            repositories.entries.first().map(|e| e.checkout.clone())
        }
        _ => None,
    };
    let policy = Policy {
        home: made("home")?,
        worktrees,
        checkouts,
        repositories,
        objects,
        directory,
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
    let trees = policy.roots().len() + policy.repositories.len() + policy.objects.len();
    if trees > MAX_TREES {
        return Err(format!(
            "this workspace binds {trees} directories, and td-jail binds at most {MAX_TREES}"
        ));
    }
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
            locals: Vec::new(),
        }
    }

    fn template(repos: &[(&str, &str, &str)]) -> crate::config::Template {
        crate::config::Template {
            system: false,
            network: None,
            name: "td agent!".into(),
            repos: repos
                .iter()
                .map(|(remote, base, branch)| crate::config::Repo {
                    remote: remote.to_string(),
                    base: base.to_string(),
                    branch: branch.to_string(),
                    sparse: None,
                })
                .collect(),
            shared: None,
        }
    }

    /// A workspace's key for the human's rules: a scratch one's
    /// conversation, a directory's path made visible, a repository
    /// workspace's name, which its forks share; each one a header can
    /// hold.
    #[test]
    fn each_workspace_has_a_key_for_the_humans_rules() {
        let id = Id::random().unwrap();
        let other = Id::random().unwrap();
        assert_eq!(
            Workspace::Scratch.key(&id),
            format!("conversation {}", id.as_str())
        );
        assert_ne!(Workspace::Scratch.key(&id), Workspace::Scratch.key(&other));
        assert_eq!(
            Workspace::Template("notes".into()).key(&id),
            Workspace::Scratch.key(&id)
        );
        // Escaped so no two paths share one: a tab, the text naming it,
        // a space and a byte that is not UTF-8.
        assert_eq!(
            Workspace::Directory("/home/u/my\tnotes".into()).key(&id),
            "directory /home/u/my%09notes"
        );
        assert_eq!(
            Workspace::Directory("/home/u/my%09notes".into()).key(&id),
            "directory /home/u/my%2509notes"
        );
        assert_eq!(
            Workspace::Directory("/a b]".into()).key(&id),
            "directory /a%20b%5D"
        );
        use std::os::unix::ffi::OsStrExt;
        assert_eq!(
            Workspace::Directory(std::ffi::OsStr::from_bytes(b"/x\xff").into()).key(&id),
            "directory /x%FF"
        );
        // A directory header is only a path as a key writes it.
        for (path, ok) in [
            ("/a%20b%5D", true),
            ("/x%FF%25", true),
            ("/a b", false),
            ("/a%20", true),
            ("/a%2f", false),
            ("/a%41", false),
            ("/a%2", false),
            ("/a]", false),
            ("/é", false),
            ("a", false),
        ] {
            assert_eq!(escaped_path(path), ok, "{path}");
        }
        let repositories = Workspace::Repositories(Repositories {
            template: "td".into(),
            name: "td-1-ab".into(),
            entries: Vec::new(),
        });
        assert_eq!(repositories.key(&id), "workspace td-1-ab");
        assert_eq!(repositories.key(&id), repositories.key(&other));
        for workspace in [
            Workspace::Scratch,
            Workspace::Directory("/a b".into()),
            repositories,
        ] {
            let key = workspace.key(&id);
            assert_eq!(crate::rules::workspace_key(&key).unwrap(), key);
        }
    }

    #[test]
    fn a_repository_template_makes_its_workspace_record() {
        let id = Id::parse("0123456789abcdef0123456789abcdef").unwrap();
        let admitted = [crate::git::Admission::parse("github.com/timmydo").unwrap()];
        let (data, root) = (Path::new("/d/td-agent"), Path::new("/h/td-agent"));
        let made = repositories(
            &template(&[
                ("https://github.com/timmydo/td", "main", "agent"),
                ("HTTPS://GitHub.com/timmydo/td/", "main", "next"),
                ("git@github.com:timmydo/td", "main", "agent"),
            ]),
            &id,
            data,
            root,
            &admitted,
            0,
        )
        .unwrap();
        assert_eq!(made.name, "td-agent-01234567");
        assert_eq!(made.template, "td agent!");
        let names: Vec<(&str, &Path, &Path)> = made
            .entries
            .iter()
            .map(|e| (e.id.as_str(), e.repository.as_path(), e.checkout.as_path()))
            .collect();
        assert_eq!(
            names,
            [
                (
                    "td",
                    Path::new("/d/td-agent/ws/td-agent-01234567/td.git"),
                    Path::new("/h/td-agent/td-agent-01234567/td")
                ),
                // The same remote: its repository, a worktree of its own.
                (
                    "td-next",
                    Path::new("/d/td-agent/ws/td-agent-01234567/td.git"),
                    Path::new("/h/td-agent/td-agent-01234567/td-next")
                ),
                // Another remote of the same name: a repository of its own.
                (
                    "td-2",
                    Path::new("/d/td-agent/ws/td-agent-01234567/td-2.git"),
                    Path::new("/h/td-agent/td-agent-01234567/td-2")
                ),
            ]
        );
        let first = made.entries.first().unwrap();
        assert_eq!(first.remote, "https://github.com/timmydo/td");
        assert!(first.store.starts_with("/d/td-agent/store"), "{first:?}");
        assert_ne!(first.store, made.entries.get(2).unwrap().store);
        assert_eq!(made.repositories().len(), 2);
        assert_eq!(
            made.tree(),
            Some(Path::new("/h/td-agent/td-agent-01234567"))
        );
        // Its record round-trips, as meta and as the process's argument.
        let workspace = Workspace::Repositories(made.clone());
        assert_eq!(
            Workspace::from_json(&workspace.to_json()).unwrap(),
            workspace
        );
        let word = workspace.argument().into_string().unwrap();
        assert_eq!(Workspace::parse_argument(&word).unwrap(), workspace);
        assert_eq!(workspace.label(), "template td agent!");
        assert!(!workspace.scratch());
        // Refused: an unadmitted remote, a branch named twice for one
        // remote, a branch git would misread.
        for (repos, why) in [
            (
                vec![("https://gitlab.com/a/b", "main", "x")],
                "not admitted",
            ),
            (
                vec![
                    ("https://github.com/timmydo/td", "main", "x"),
                    ("https://github.com/timmydo/td", "next", "x"),
                ],
                "twice",
            ),
            (
                vec![("https://github.com/timmydo/td", "main", "-x")],
                "branch",
            ),
            (
                vec![("http://github.com/timmydo/td", "main", "x")],
                "no other transport",
            ),
        ] {
            let e = repositories(&template(&repos), &id, data, root, &admitted, 0).unwrap_err();
            assert!(e.contains(why), "{repos:?}: {e}");
        }
        assert!(repositories(&template(&[]), &id, data, root, &admitted, 0).is_err());
        // With its shared directories it binds no more than td-jail: one
        // worktree, its repository and its store leave 29.
        let one = template(&[("https://github.com/timmydo/td", "main", "x")]);
        assert!(repositories(&one, &id, data, root, &admitted, 29).is_ok());
        let e = repositories(&one, &id, data, root, &admitted, 30).unwrap_err();
        assert!(e.contains("td-jail binds at most 32"), "{e}");
        // Its record fits `meta` and an argument.
        let mut long = one.clone();
        if let Some(repo) = long.repos.first_mut() {
            repo.sparse = Some((0..800).map(|n| format!("some/long/path/{n:08}")).collect());
        }
        let e = repositories(&long, &id, data, root, &admitted, 0).unwrap_err();
        assert!(e.contains("16384 bytes"), "{e}");
    }

    /// What the human is asked to admit: each remote once, as recorded,
    /// none an admission covers; and only of a template that is otherwise
    /// whole, so an admission never outlasts a template refused.
    #[test]
    fn a_templates_remotes_not_admitted_are_asked_about_only_when_it_is_whole() {
        let id = Id::random().unwrap();
        let (data, root) = (Path::new("/d"), Path::new("/h"));
        let made = plan(
            &template(&[
                ("https://github.com/timmydo/td", "main", "a"),
                ("https://example.org/a/td", "main", "a"),
                ("HTTPS://Example.org/a/td/", "main", "b"),
                ("git@example.org:a/b", "main", "a"),
            ]),
            &id,
            data,
            root,
            0,
        )
        .unwrap();
        let some = [crate::git::Admission::parse("github.com/timmydo").unwrap()];
        assert_eq!(
            unadmitted(&made, &some),
            ["https://example.org/a/td", "git@example.org:a/b"]
        );
        let all = [
            crate::git::Admission::parse("github.com").unwrap(),
            crate::git::Admission::parse("example.org").unwrap(),
        ];
        assert!(unadmitted(&made, &all).is_empty());
        // A template refused for anything else is refused before any
        // card: a plain http remote, a branch named twice, a bad base.
        for repos in [
            vec![
                ("https://example.org/a/td", "main", "a"),
                ("http://example.org/plain", "main", "a"),
            ],
            vec![
                ("https://example.org/a/td", "main", "a"),
                ("https://example.org/a/td", "next", "a"),
            ],
            vec![("https://example.org/a/td", "-main", "a")],
        ] {
            assert!(
                plan(&template(&repos), &id, data, root, 0).is_err(),
                "{repos:?}"
            );
        }
    }

    #[test]
    fn a_repository_workspace_binds_only_what_is_prepared() {
        let scratch = crate::store::tests::Scratch::new("ws-repositories");
        let state = scratch.state();
        let id = Id::random().unwrap();
        let admitted = [crate::git::Admission::parse("github.com").unwrap()];
        let made = repositories(
            &template(&[
                ("https://github.com/a/one", "main", "x"),
                ("https://github.com/a/two", "main", "x"),
            ]),
            &id,
            Path::new("/d"),
            Path::new("/h"),
            &admitted,
            0,
        )
        .unwrap();
        let workspace = Workspace::Repositories(made.clone());
        let (none, _) = policy(&workspace, &state, &id, &[], &[]).unwrap();
        assert!(
            none.checkouts.is_empty() && none.repositories.is_empty() && none.objects.is_empty()
        );
        assert!(none.worktrees.is_empty());
        // Its working directory is its first worktree, bound or not.
        let first = made.entries.first().map(|e| e.checkout.clone());
        assert_eq!(none.directory, first);
        let one = made.entries.first().unwrap();
        let ready = std::slice::from_ref(&one.repository);
        let (bound, _) = policy(&workspace, &state, &id, &[], ready).unwrap();
        assert_eq!(bound.checkouts, std::slice::from_ref(&one.checkout));
        assert_eq!(bound.repositories, ready);
        assert_eq!(bound.objects, [one.store.join("objects")]);
        assert_eq!(bound.roots(), std::slice::from_ref(&one.checkout));
        assert_eq!(bound.directory, first);
        // No policy binds more than td-jail does.
        let many: Vec<Shared> = (0..32)
            .map(|n| Shared {
                path: PathBuf::from(format!("/s/{n}")),
                write: false,
            })
            .collect();
        let e = policy(&workspace, &state, &id, &many, ready).unwrap_err();
        assert!(e.contains("td-jail binds at most 32"), "{e}");
        let (scratch_policy, _) = policy(&Workspace::Scratch, &state, &id, &[], &[]).unwrap();
        assert_eq!(scratch_policy.directory, None);
    }

    #[test]
    fn a_workspace_name_is_reserved_once() {
        let scratch = crate::store::tests::Scratch::new("ws-reserve");
        let data = scratch.0.join("data");
        let id = Id::parse("0123456789abcdef0123456789abcdef").unwrap();
        let admitted = [crate::git::Admission::parse("github.com").unwrap()];
        let one = template(&[("https://github.com/a/one", "main", "x")]);
        let made = repositories(&one, &id, &data, &scratch.0.join("trees"), &admitted, 0).unwrap();
        assert!(reserve(&made, &data).unwrap());
        assert!(data.join("ws").join(&made.name).is_dir());
        // Another id with the same first eight: the name is held.
        let again = Id::parse("01234567ffffffffffffffffffffffff").unwrap();
        let other =
            repositories(&one, &again, &data, &scratch.0.join("trees"), &admitted, 0).unwrap();
        assert_eq!(other.name, made.name);
        assert!(!reserve(&other, &data).unwrap());
        // A tree under the workspace root holds it too.
        let elsewhere = scratch.0.join("data-2");
        fs::create_dir_all(made.tree().unwrap()).unwrap();
        assert!(!reserve(&made, &elsewhere).unwrap());
    }

    #[test]
    fn the_data_directory_is_made_the_callers_and_no_link() {
        let scratch = crate::store::tests::Scratch::new("ws-data");
        let base = scratch.0.join("share");
        let dir = data_in(&base).unwrap();
        assert_eq!(dir, fs::canonicalize(base.join("td-agent")).unwrap());
        let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&dir), 0o700);
        // One made wider before is narrowed.
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(data_in(&base).unwrap(), dir);
        assert_eq!(mode(&dir), 0o700);
        // A link there is refused: the places refused to grants name it.
        let linked = scratch.0.join("linked");
        fs::create_dir_all(&linked).unwrap();
        std::os::unix::fs::symlink(&dir, linked.join("td-agent")).unwrap();
        let e = data_in(&linked).unwrap_err();
        assert!(e.contains("is not a directory"), "{e}");
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
            "/home/u/.local/share/td-agent/ws",
            "/data/td-agent",
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

    /// A local repository is fetched and pushed only from its own path,
    /// a repository, where no jail reaches: none of the protected places
    /// nor anywhere a workspace writes.
    #[test]
    fn a_local_repository_lies_where_no_jail_reaches() {
        let base = std::env::temp_dir().join(format!(
            "td-agent-local-{}-{}",
            std::process::id(),
            crate::store::random_hex(4).unwrap()
        ));
        for dir in [
            "srv/git/td.git/objects",
            "srv/git/td.git/refs",
            "srv/plain",
            "home/src/work/.git/objects",
            "home/src/work/.git/refs",
            "home/td-agent/ws/x.git/objects",
            "home/td-agent/ws/x.git/refs",
            "shared/inner.git/objects",
            "shared/inner.git/refs",
        ] {
            fs::create_dir_all(base.join(dir)).unwrap();
        }
        for head in [
            "srv/git/td.git",
            "home/td-agent/ws/x.git",
            "shared/inner.git",
        ] {
            fs::write(base.join(head).join("HEAD"), "ref: refs/heads/main\n").unwrap();
        }
        let base = fs::canonicalize(&base).unwrap();
        std::os::unix::fs::symlink(base.join("srv/git"), base.join("srv/link")).unwrap();
        let home = base.join("home");
        let places = places(&home);
        let bare = base.join("srv/git/td.git");
        local_repository(&bare, &places, &[]).unwrap();
        local_repository(&home.join("src/work"), &places, &[]).unwrap();
        for (path, reached, why) in [
            (base.join("srv/link/td.git"), vec![], "through a link"),
            (base.join("srv/plain"), vec![], "not a git repository"),
            (base.join("srv/missing"), vec![], "No such file"),
            (home.join("td-agent/ws/x.git"), vec![], "workspace root"),
            (
                base.join("shared/inner.git"),
                vec![base.join("shared")],
                "overlaps",
            ),
            (bare.clone(), vec![base.join("srv")], "overlaps"),
            (bare.clone(), vec![bare.join("objects")], "overlaps"),
        ] {
            let e = local_repository(&path, &places, &reached).unwrap_err();
            assert!(e.contains(why), "{}: {e}", path.display());
        }
        // What git reads or runs elsewhere: a linked worktree's `.git`
        // file, a `commondir`, borrowed objects, a configuration that
        // includes another or moves the hooks, a linked configuration or
        // hooks directory, and a hook linked to where a workspace writes.
        let shared = base.join("shared");
        let refused = |setup: &dyn Fn(&Path), why: &str| {
            let repo = base.join("srv/case.git");
            let _ = fs::remove_dir_all(&repo);
            for dir in ["objects/info", "refs", "hooks"] {
                fs::create_dir_all(repo.join(dir)).unwrap();
            }
            fs::write(repo.join("HEAD"), "ref: refs/heads/main\n").unwrap();
            local_repository(&repo, &places, std::slice::from_ref(&shared)).unwrap();
            setup(&repo);
            let e = local_repository(&repo, &places, std::slice::from_ref(&shared)).unwrap_err();
            assert!(e.contains(why), "{why}: {e}");
        };
        let worktree = base.join("srv/linked");
        fs::create_dir_all(&worktree).unwrap();
        fs::write(worktree.join(".git"), "gitdir: /elsewhere\n").unwrap();
        let e = local_repository(&worktree, &places, &[]).unwrap_err();
        assert!(e.contains("linked worktree"), "{e}");
        refused(
            &|repo| fs::write(repo.join("commondir"), "..").unwrap(),
            "linked worktree",
        );
        refused(
            &|repo| fs::write(repo.join("objects/info/alternates"), "/x\n").unwrap(),
            "borrows",
        );
        for (config, why) in [
            ("[include]\n\tpath = /x\n", "includes"),
            ("[includeIf \"gitdir:/\"]\npath = /x\n", "includes"),
            ("[a][include]path = /f\n", "includes"),
            ("[core]\n\thooksPath = /x\n", "core.hookspath"),
            ("[core] HooksPath=/x\n", "core.hookspath"),
            ("[b]\r[core]\rfsmonitor = x\n", "core.fsmonitor"),
            (
                "[core]\nalternateRefsCommand = x\n",
                "core.alternaterefscommand",
            ),
            ("[core]\nworktree = /x\n", "core.worktree"),
            ("[core]\nattributesFile = /x\n", "core.attributesfile"),
            (
                "[extensions]\nworktreeConfig = true\n",
                "extensions.worktreeconfig",
            ),
            ("[core\n", "as git reads it"),
        ] {
            refused(&|repo| fs::write(repo.join("config"), config).unwrap(), why);
        }
        refused(
            &|repo| {
                std::os::unix::fs::symlink(base.join("srv/plain"), repo.join("config")).unwrap()
            },
            "is a link",
        );
        refused(
            &|repo| {
                fs::remove_dir_all(repo.join("hooks")).unwrap();
                std::os::unix::fs::symlink(&shared, repo.join("hooks")).unwrap();
            },
            "is a link",
        );
        refused(
            &|repo| std::os::unix::fs::symlink(&shared, repo.join("packed-refs")).unwrap(),
            "is a link",
        );
        refused(
            &|repo| std::os::unix::fs::symlink(&shared, repo.join("objects/pack")).unwrap(),
            "is a link",
        );
        refused(
            &|repo| fs::write(repo.join("config.worktree"), "").unwrap(),
            "config.worktree",
        );
        refused(
            &|repo| fs::write(repo.join("config"), "#".repeat(1024 * 1024 + 1)).unwrap(),
            "past",
        );
        refused(
            &|repo| {
                fs::write(shared.join("post-receive"), "#!/bin/sh\n").unwrap();
                std::os::unix::fs::symlink(
                    shared.join("post-receive"),
                    repo.join("hooks/post-receive"),
                )
                .unwrap();
            },
            "overlaps",
        );
        // A plain configuration, the repository's own, is no matter.
        let plain = base.join("srv/case.git");
        fs::remove_file(plain.join("hooks/post-receive")).unwrap();
        fs::write(
            plain.join("config"),
            "[core]\n\tbare = true\n[remote \"o\"]\n\turl = /x/include\n",
        )
        .unwrap();
        local_repository(&plain, &places, &[]).unwrap();
        // An admitted local repository is among the places no workspace
        // or shared directory may reach, and not refused for being one.
        let guarded = Places {
            locals: vec![bare.clone()],
            ..places.clone()
        };
        local_repository(&bare, &guarded, &[]).unwrap();
        let e = admit_directory(&base.join("srv/git"), &guarded, &[]).unwrap_err();
        assert!(e.contains("local repository"), "{e}");
        let (admitted, notes) = admit_shared(
            &[Shared {
                path: base.join("srv"),
                write: false,
            }],
            &guarded,
        );
        assert!(admitted.is_empty(), "{notes:?}");
        fs::remove_dir_all(&base).unwrap();
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
        // A repository refused is a template's remote: the work tree's
        // top, or the bare repository; a work tree's subdirectory, which
        // is admitted, and a plain directory are none.
        for (chosen, remote) in [
            ("src/repo", Some("src/repo")),
            ("src/repo/.git/hooks", Some("src/repo")),
            ("src/bare.git", Some("src/bare.git")),
            ("src/bare.git/objects", Some("src/bare.git")),
            ("src/repo/sub", None),
            ("src/a", None),
            ("missing", None),
        ] {
            assert_eq!(
                repository_remote(&home.join(chosen)),
                remote.map(|remote| home.join(remote)),
                "{chosen}"
            );
        }

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
        let (policy, specs) = policy(&Workspace::Scratch, &state, &id, &shared, &[]).unwrap();
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
        policy(&Workspace::Directory("/w/a".into()), state, id, &[], &[]).unwrap()
    }
}

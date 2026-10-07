//! A workspace repository (DESIGN.md §7, Layout; §8, The git mount
//! chain): laid out by td-agent as plain files, outside any jail and
//! before any instance binds it, and from then on acted on only by git
//! inside a maintenance instance (`Task`), never by one outside (§9).
//!
//! Every protected entry is made with a fresh `mkdir` or an exclusive
//! create, so nothing a jail planted is ever reused, and a repository
//! and a worktree's `worktrees/<id>/` are built aside and renamed into
//! place whole, so no instance finds one half made: each is made once,
//! and a second call on either is refused.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fs::{self, DirBuilder};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use td_json::Json;

use crate::git::{self, Failure};

/// The longest worktree id.
const MAX_ID: usize = 128;
/// The most linked worktrees a repository has: td-jail binds no more.
const MAX_WORKTREES: usize = 32;
/// The most a maintenance git may say, how long a checkout's git may
/// take in all, and how long its cleanup after a failure.
const MAX_SAID: u64 = 64 * 1024;
const CHECKOUT_TIME: Duration = Duration::from_secs(600);
const CLEANUP_TIME: Duration = Duration::from_secs(30);
/// How long the instance running one checkout may take: its git, its
/// cleanup and the jail's start and end, so the instance is never killed
/// before the task has cleaned up.
pub(crate) const TASK_TIME: Duration =
    CHECKOUT_TIME.saturating_add(CLEANUP_TIME.saturating_add(Duration::from_secs(60)));
/// How long a survey's git may take in all, and its instance.
const SURVEY_GIT_TIME: Duration = Duration::from_secs(240);
pub(crate) const SURVEY_TIME: Duration = SURVEY_GIT_TIME.saturating_add(Duration::from_secs(60));
/// How long setting the remote-tracking refs may take, and its instance.
const TRACK_GIT_TIME: Duration = Duration::from_secs(60);
pub(crate) const TRACK_TIME: Duration = TRACK_GIT_TIME.saturating_add(Duration::from_secs(60));
/// The word a maintenance instance's entry is started with.
pub const MAINTAIN: &str = "maintain";
/// How long resolving an export's branch may take.
const EXPORT_GIT_TIME: Duration = Duration::from_secs(60);
/// How long packing an export may take.
pub const EXPORT_TIME: Duration = Duration::from_secs(600);

/// The human's identity, which commits made in the jail carry.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Identity {
    pub name: Option<String>,
    pub email: Option<String>,
}

/// One linked worktree of a workspace repository.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Worktree {
    /// Its directory's name under the repository's `worktrees/`.
    pub id: String,
    /// Where it is checked out, in the workspace tree.
    pub checkout: PathBuf,
    pub branch: String,
    /// The cone-mode paths it checks out; none is the whole tree.
    pub sparse: Option<Vec<String>>,
}

/// `path` as one line of a file git reads: absolute, text, and with no
/// control character.
fn line_path(path: &Path) -> Result<&str, String> {
    path.to_str()
        .filter(|text| path.is_absolute() && !text.contains(char::is_control))
        .ok_or_else(|| format!("{} cannot be named to git", path.display()))
}

/// A worktree id: letters, digits, `.`, `_` and `-`, not starting with
/// `.` or `-`.
pub fn worktree_id(id: &str) -> Result<&str, String> {
    let ok = !id.is_empty()
        && id.len() <= MAX_ID
        && !id.starts_with(['.', '-'])
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
    ok.then_some(id)
        .ok_or_else(|| format!("{id:?} is not a worktree id td-agent makes"))
}

/// A value in git's configuration syntax: quoted, its `\` and `"`
/// escaped; a value with a control character is refused.
fn quoted(what: &str, value: &str) -> Result<String, String> {
    if value.contains(char::is_control) {
        return Err(format!("the {what} {value:?} has a control character"));
    }
    Ok(format!(
        "\"{}\"",
        value.replace('\\', "\\\\").replace('"', "\\\"")
    ))
}

/// The repository's configuration (DESIGN.md §8): what a jailed git
/// needs and nothing it could misuse, read-only in every instance.
pub fn config_text(repository: &Path, identity: &Identity) -> Result<String, String> {
    let hooks = quoted("hooks path", line_path(&repository.join("hooks"))?)?;
    let mut text = format!(
        "[core]\n\
         \trepositoryformatversion = 0\n\
         \tbare = true\n\
         \tsparseCheckout = true\n\
         \tsparseCheckoutCone = true\n\
         \tfsmonitor = false\n\
         \thooksPath = {hooks}\n"
    );
    if identity.name.is_some() || identity.email.is_some() {
        text.push_str("[user]\n");
        if let Some(name) = &identity.name {
            text.push_str(&format!("\tname = {}\n", quoted("name", name)?));
        }
        if let Some(email) = &identity.email {
            text.push_str(&format!("\temail = {}\n", quoted("email", email)?));
        }
    }
    text.push_str(
        "[branch]\n\
         \tautoSetupMerge = false\n\
         [submodule]\n\
         \trecurse = false\n\
         [diff]\n\
         \tignoreSubmodules = all\n\
         [status]\n\
         \tsubmoduleSummary = false\n\
         [rerere]\n\
         \tenabled = false\n\
         [gc]\n\
         \tauto = 0\n\
         \twriteCommitGraph = false\n\
         \tworktreePruneExpire = never\n\
         [repack]\n\
         \tupdateServerInfo = false\n\
         [maintenance]\n\
         \tauto = false\n",
    );
    Ok(text)
}

/// A fresh directory: refused if anything is at `path`.
fn fresh_dir(path: &Path) -> Result<(), String> {
    DirBuilder::new()
        .mode(0o755)
        .create(path)
        .map_err(|e| format!("{}: {e}", path.display()))
}

/// A fresh file holding `text`, written through to the disk: refused if
/// anything, a link included, is at `path`.
fn fresh_file(path: &Path, text: &str) -> Result<(), String> {
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o644)
        .open(path)
        .and_then(|mut file| {
            file.write_all(text.as_bytes())?;
            file.sync_all()
        })
        .map_err(|e| format!("{}: {e}", path.display()))
}

/// `path` with its parent made if need be (td-agent's own) and named as
/// it resolves, since td-jail binds real paths and git's files must name
/// what an instance finds.
fn resolved(path: &Path) -> Result<PathBuf, String> {
    let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
        return Err(format!("{} has no parent", path.display()));
    };
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(parent)
        .map_err(|e| format!("{}: {e}", parent.display()))?;
    let parent = fs::canonicalize(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    Ok(parent.join(name))
}

/// Refuses anything at `path`, a link included.
fn absent(path: &Path) -> Result<(), String> {
    match fs::symlink_metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Ok(_) => Err(format!("{} exists already", path.display())),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

/// Builds a directory with `build` in a fresh staging directory in `dir`,
/// which only td-agent writes, and renames it whole to `place`: no
/// instance ever finds it half made, and a crash leaves its debris in
/// `dir`, never at `place`. No jail can make anything where these are
/// placed (a workspace's data directory, the read-only `worktrees/`), so
/// checking `place` and renaming do not race one.
fn staged(
    dir: &Path,
    place: &Path,
    build: impl FnOnce(&Path) -> Result<(), String>,
) -> Result<(), String> {
    absent(place)?;
    let name = crate::store::random_hex(8).map_err(|e| format!("a staging name: {e}"))?;
    let staging = dir.join(format!(".td-agent-new-{name}"));
    fresh_dir(&staging)?;
    let into = place
        .parent()
        .ok_or_else(|| format!("{} has no parent", place.display()))?;
    let built = build(&staging)
        .and_then(|()| sync_directories(&staging))
        .and_then(|()| absent(place))
        .and_then(|()| fs::rename(&staging, place).map_err(|e| format!("{}: {e}", place.display())))
        .and_then(|()| sync_directory(into));
    if built.is_err() {
        let _ = fs::remove_dir_all(&staging);
    }
    built
}

/// Writes `dir`'s entries through to the disk.
fn sync_directory(dir: &Path) -> Result<(), String> {
    fs::File::open(dir)
        .and_then(|held| held.sync_all())
        .map_err(|e| format!("{}: {e}", dir.display()))
}

/// `sync_directory` for `dir` and every directory below it, each a
/// fresh one td-agent made, a few levels deep.
fn sync_directories(dir: &Path) -> Result<(), String> {
    let entries = fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("{}: {e}", dir.display()))?;
        if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            sync_directories(&entry.path())?;
        }
    }
    sync_directory(dir)
}

/// Makes the workspace repository at `repository`, which must not exist,
/// over the store `store`'s objects, and returns its path as it resolves:
/// every protected entry of the chain, `commondir` naming the repository
/// itself, `config` td-agent's, and `objects/info/alternates` naming the
/// store's objects, so creating it copies nothing. It is built beside
/// its place and renamed there whole.
pub fn create(repository: &Path, store: &Path, identity: &Identity) -> Result<PathBuf, String> {
    let store = fs::canonicalize(store).map_err(|e| format!("{}: {e}", store.display()))?;
    let alternates = format!("{}\n", line_path(&store.join("objects"))?);
    let repository = resolved(repository)?;
    let config = config_text(&repository, identity)?;
    let dir = repository
        .parent()
        .ok_or_else(|| format!("{} has no parent", repository.display()))?;
    staged(dir, &repository, |staging| {
        for name in REPOSITORY_DIRECTORIES {
            fresh_dir(&staging.join(name))?;
        }
        for (name, text) in [
            ("HEAD", "ref: refs/heads/main\n"),
            ("config", config.as_str()),
            // git refuses an empty `commondir`.
            ("commondir", ".\n"),
            ("config.worktree", ""),
            ("shallow", ""),
            ("objects/info/alternates", alternates.as_str()),
        ] {
            fresh_file(&staging.join(name), text)?;
        }
        Ok(())
    })?;
    Ok(repository)
}

/// A workspace repository's directories, parents first.
const REPOSITORY_DIRECTORIES: &[&str] = &[
    "objects",
    "objects/info",
    "objects/pack",
    "refs",
    "refs/heads",
    "refs/tags",
    "branches",
    "hooks",
    "info",
    "remotes",
    "worktrees",
];

/// A sparse path's segments: relative, inside the tree, none empty, `.`,
/// `..`, or holding what git's cone patterns would read as a glob or a
/// comment.
fn cone_segments(path: &str) -> Result<Vec<&str>, String> {
    let refuse = || format!("{path:?} is not a sparse path td-agent checks out");
    let segments: Vec<&str> = path.trim_end_matches('/').split('/').collect();
    for segment in &segments {
        let bad = segment.is_empty()
            || *segment == "."
            || *segment == ".."
            || segment.trim() != *segment
            || segment.starts_with(['!', '#'])
            || segment.contains(['*', '?', '[', ']', '\\'])
            || segment.contains(char::is_control);
        if bad {
            return Err(refuse());
        }
    }
    Ok(segments)
}

/// The cone-mode patterns of `sparse`, as `git sparse-checkout set
/// --cone` writes them: the top's files, each path's directory whole,
/// and the files of the directories above it. None is the whole tree;
/// no paths, the top's files alone.
pub fn cone(sparse: Option<&[String]>) -> Result<String, String> {
    let Some(paths) = sparse else {
        return Ok("/*\n".into());
    };
    let mut whole = BTreeSet::new();
    for path in paths {
        whole.insert(cone_segments(path)?.join("/"));
    }
    // A directory inside another taken whole adds nothing.
    let inside = |dir: &str| {
        whole
            .iter()
            .any(|other| dir.len() > other.len() && dir.starts_with(&format!("{other}/")))
    };
    let whole: BTreeSet<&String> = whole.iter().filter(|dir| !inside(dir)).collect();
    let mut above = BTreeSet::new();
    for dir in &whole {
        let mut at = dir.as_str();
        while let Some((parent, _)) = at.rsplit_once('/') {
            above.insert(parent.to_string());
            at = parent;
        }
    }
    let mut text = String::from("/*\n!/*/\n");
    let mut lines: Vec<(&str, bool)> = above.iter().map(|dir| (dir.as_str(), false)).collect();
    lines.extend(whole.iter().map(|dir| (dir.as_str(), true)));
    lines.sort();
    for (dir, all) in lines {
        text.push_str(&format!("/{dir}/\n"));
        if !all {
            text.push_str(&format!("!/{dir}/*/\n"));
        }
    }
    Ok(text)
}

/// Adds `worktree` to the repository made by `create` and returns its
/// checkout's path as it resolves: the checkout directory and its `.git`
/// file, then `worktrees/<id>/` with its `gitdir`, `commondir` and empty
/// `config.worktree` (protected), `HEAD` naming its branch and its sparse
/// patterns (its own), built beside the repository and renamed into
/// `worktrees/` whole. Neither the checkout nor the id may exist, nor
/// `MAX_WORKTREES` ids already; a failure removes what it made.
pub fn add_worktree(repository: &Path, worktree: &Worktree) -> Result<PathBuf, String> {
    let id = worktree_id(&worktree.id)?;
    let branch = git::branch_name(&worktree.branch)?;
    let patterns = cone(worktree.sparse.as_deref())?;
    let repository =
        fs::canonicalize(repository).map_err(|e| format!("{}: {e}", repository.display()))?;
    let trees = repository.join("worktrees");
    let listed = fs::read_dir(&trees)
        .map_err(|e| {
            format!(
                "{} is not a workspace repository: {e}",
                repository.display()
            )
        })?
        .count();
    if listed >= MAX_WORKTREES {
        return Err(format!(
            "{} has {MAX_WORKTREES} worktrees, the most td-jail binds",
            repository.display()
        ));
    }
    let dir = repository
        .parent()
        .ok_or_else(|| format!("{} has no parent", repository.display()))?;
    let linked = trees.join(id);
    let checkout = resolved(&worktree.checkout)?;
    let dot_git = checkout.join(".git");
    let gitdir = format!("{}\n", line_path(&dot_git)?);
    let pointer = format!("gitdir: {}\n", line_path(&linked)?);
    absent(&linked)?;
    fresh_dir(&checkout)?;
    let made = fresh_file(&dot_git, &pointer).and_then(|()| {
        staged(dir, &linked, |staging| {
            fresh_file(&staging.join("gitdir"), &gitdir)?;
            fresh_file(&staging.join("commondir"), "../..\n")?;
            fresh_file(&staging.join("config.worktree"), "")?;
            fresh_file(
                &staging.join("HEAD"),
                &format!("ref: refs/heads/{branch}\n"),
            )?;
            fresh_dir(&staging.join("info"))?;
            fresh_file(&staging.join("info/sparse-checkout"), &patterns)
        })
    });
    if let Err(e) = made {
        let _ = fs::remove_dir_all(&checkout);
        return Err(e);
    }
    Ok(checkout)
}

/// What a maintenance instance is asked (DESIGN.md §9): fixed git
/// commands over one worktree of one workspace repository, run as the
/// instance's entry, `td-agent maintain`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Task {
    /// Makes the worktree's branch at `base`, keeping one an earlier run
    /// made, and checks the worktree out as its sparse patterns select.
    /// Run only before its repository is recorded prepared, when nothing
    /// else writes it.
    Checkout {
        /// The git the instance runs, by the path it has there.
        git: PathBuf,
        repository: PathBuf,
        id: String,
        checkout: PathBuf,
        branch: String,
        base: String,
    },
    /// Says what removing the worktree would lose (DESIGN.md §7,
    /// Archiving and deleting): how many of its files are changed or
    /// untracked, and how many commits reachable from any ref of the
    /// repository (every worktree's `HEAD`, branches, tags, the stash)
    /// are in none of `bases`, the commits its worktrees started at. It
    /// writes nothing.
    Survey {
        git: PathBuf,
        repository: PathBuf,
        id: String,
        checkout: PathBuf,
        bases: Vec<String>,
    },
    /// Sets each base's remote-tracking ref, `refs/remotes/origin/<base>`,
    /// to the commit the trusted store resolved it to (DESIGN.md §7,
    /// Keeping current), whatever the jail left there, all or none; the
    /// commits are the store's, which the repository's alternates reach.
    /// `preparing` runs it before the repository is recorded prepared,
    /// when a ref's lock can only be one a killed run left.
    Track {
        git: PathBuf,
        repository: PathBuf,
        id: String,
        checkout: PathBuf,
        heads: Vec<(String, String)>,
        preparing: bool,
    },
    /// Resolves `branch`, td-agent's record of the worktree's branch, to
    /// its commit and sends the objects reachable from it and not from
    /// `base`, a commit of the store, as a pack (DESIGN.md §9, Pushing):
    /// frames of pack (`p`) and then the answer (`a`), the commit, so
    /// nothing but objects leaves for the import. It writes nothing.
    Export {
        git: PathBuf,
        repository: PathBuf,
        id: String,
        checkout: PathBuf,
        branch: String,
        base: String,
    },
}

/// A survey's answer, read back outside the instance: what the
/// workspace's own git reported, which the jail controls.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Survey {
    /// Files changed or untracked; none when more than git's answer
    /// could list.
    pub changes: Option<u64>,
    /// Commits not in the base.
    pub ahead: u64,
}

impl Survey {
    /// What the task says: `changes <n|many> ahead <n>`.
    fn say(&self) -> String {
        let changes = self
            .changes
            .map_or_else(|| "many".to_string(), |n| n.to_string());
        format!("changes {changes} ahead {}", self.ahead)
    }

    pub fn parse(said: &str) -> Result<Self, String> {
        let words: Vec<&str> = said.split(' ').collect();
        let number = |word: &str| word.parse::<u64>().ok();
        match words.as_slice() {
            ["changes", changes, "ahead", ahead] => Ok(Self {
                changes: match *changes {
                    "many" => None,
                    n => Some(number(n).ok_or_else(|| format!("a survey said {said:?}"))?),
                },
                ahead: number(ahead).ok_or_else(|| format!("a survey said {said:?}"))?,
            }),
            _ => Err(format!("a survey said {said:?}")),
        }
    }

    /// Whether removing the worktree loses nothing it reported.
    pub fn clean(&self) -> bool {
        self.changes == Some(0) && self.ahead == 0
    }
}

impl Task {
    /// Its words after `maintain`.
    pub fn args(&self) -> Vec<OsString> {
        match self {
            Self::Checkout {
                git,
                repository,
                id,
                checkout,
                branch,
                base,
            } => vec![
                "checkout".into(),
                git.into(),
                repository.into(),
                id.into(),
                checkout.into(),
                branch.into(),
                base.into(),
            ],
            Self::Survey {
                git,
                repository,
                id,
                checkout,
                bases,
            } => [
                "survey".into(),
                git.into(),
                repository.into(),
                id.into(),
                checkout.into(),
            ]
            .into_iter()
            .chain(bases.iter().map(OsString::from))
            .collect(),
            Self::Track {
                git,
                repository,
                id,
                checkout,
                heads,
                preparing,
            } => [
                if *preparing { "track-new" } else { "track" }.into(),
                git.into(),
                repository.into(),
                id.into(),
                checkout.into(),
            ]
            .into_iter()
            .chain(
                heads
                    .iter()
                    .flat_map(|(base, commit)| [OsString::from(base), OsString::from(commit)]),
            )
            .collect(),
            Self::Export {
                git,
                repository,
                id,
                checkout,
                branch,
                base,
            } => vec![
                "export".into(),
                git.into(),
                repository.into(),
                id.into(),
                checkout.into(),
                branch.into(),
                base.into(),
            ],
        }
    }

    pub fn parse(args: &[String]) -> Result<Self, String> {
        let absolute = |path: &str| {
            let path = PathBuf::from(path);
            line_path(&path)?;
            Ok::<_, String>(path)
        };
        match args {
            [word, git, repository, id, checkout, branch, base]
                if word == "checkout" || word == "export" =>
            {
                if !git::object_id(base) {
                    return Err(format!("{base:?} is not a full commit id"));
                }
                let (git, repository, id, checkout, branch, base) = (
                    absolute(git)?,
                    absolute(repository)?,
                    worktree_id(id)?.to_string(),
                    absolute(checkout)?,
                    git::branch_name(branch)?.to_string(),
                    base.clone(),
                );
                Ok(if word == "export" {
                    Self::Export {
                        git,
                        repository,
                        id,
                        checkout,
                        branch,
                        base,
                    }
                } else {
                    Self::Checkout {
                        git,
                        repository,
                        id,
                        checkout,
                        branch,
                        base,
                    }
                })
            }
            [word, git, repository, id, checkout, bases @ ..]
                if word == "survey" && !bases.is_empty() && bases.len() <= MAX_WORKTREES =>
            {
                if let Some(base) = bases.iter().find(|base| !git::object_id(base)) {
                    return Err(format!("{base:?} is not a full commit id"));
                }
                Ok(Self::Survey {
                    git: absolute(git)?,
                    repository: absolute(repository)?,
                    id: worktree_id(id)?.into(),
                    checkout: absolute(checkout)?,
                    bases: bases.to_vec(),
                })
            }
            [word, git, repository, id, checkout, heads @ ..]
                if (word == "track" || word == "track-new")
                    && !heads.is_empty()
                    && heads.len() % 2 == 0
                    && heads.len() / 2 <= MAX_WORKTREES =>
            {
                let heads = heads
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|[base, commit]| {
                        if !git::object_id(commit) {
                            return Err(format!("{commit:?} is not a full commit id"));
                        }
                        Ok((git::branch_name(base)?.to_string(), commit.clone()))
                    })
                    .collect::<Result<_, String>>()?;
                Ok(Self::Track {
                    git: absolute(git)?,
                    repository: absolute(repository)?,
                    id: worktree_id(id)?.into(),
                    checkout: absolute(checkout)?,
                    heads,
                    preparing: word == "track-new",
                })
            }
            _ => Err(
                "usage: td-agent maintain checkout (or export) GIT REPOSITORY ID CHECKOUT BRANCH BASE, survey GIT REPOSITORY ID CHECKOUT BASE..., or track (or track-new) GIT REPOSITORY ID CHECKOUT BASE COMMIT...".into(),
            ),
        }
    }

    /// Runs the task's git, in the instance, and says what it did.
    pub fn run(&self) -> Result<String, String> {
        match self {
            Self::Checkout {
                git,
                repository,
                id,
                checkout,
                branch,
                base,
            } => {
                let jailed = || jailed(git, repository, id, checkout);
                let now = Instant::now();
                let deadline = now.checked_add(CHECKOUT_TIME).unwrap_or(now);
                let run = |args: &[&str]| {
                    let left = deadline.saturating_duration_since(Instant::now());
                    git::run(jailed().args(args), MAX_SAID, left)
                };
                // `worktrees/<id>/` is the jail's to write once renamed into
                // place: an index there was not made by this task.
                let index = repository.join("worktrees").join(id).join("index");
                if fs::symlink_metadata(&index).is_ok() {
                    return Err(format!(
                        "{} exists, so the worktree is not checked out again",
                        index.display()
                    ));
                }
                let reference = format!("refs/heads/{branch}");
                // Until the repository is recorded prepared nothing but
                // these tasks writes it (DESIGN.md §7), so a lock is one a
                // run killed mid-step left, and would refuse every retry.
                let linked = repository.join("worktrees").join(id);
                for lock in [
                    linked.join("index.lock"),
                    linked.join("HEAD.lock"),
                    repository.join(format!("{reference}.lock")),
                    repository.join("packed-refs.lock"),
                ] {
                    match fs::remove_file(&lock) {
                        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                            return Err(format!("{}: {e}", lock.display()));
                        }
                        _ => {}
                    }
                }
                // An empty old value: made here, at the base. A branch
                // already there was made by a run of this task a crash cut
                // short, at the base resolved then, which is kept: a
                // retry never moves a branch.
                let at = match run(&["update-ref", "--no-deref", &reference, base, ""]) {
                    Ok(_) => base.clone(),
                    Err(made) => run(&[
                        "rev-parse",
                        "--verify",
                        "--quiet",
                        "--end-of-options",
                        &format!("{reference}^{{commit}}"),
                    ])
                    .ok()
                    .and_then(|said| String::from_utf8(said).ok())
                    .map(|said| said.trim().to_string())
                    .filter(|said| git::object_id(said))
                    .ok_or_else(|| said("making the branch", &made))?,
                };
                // `HEAD` set here and the tree read by its id, not through
                // a `HEAD` the jail could have written.
                let checked = run(&["symbolic-ref", "HEAD", &reference])
                    // `--reset`: what an earlier, broken run wrote is
                    // overwritten, and the index check above leaves no
                    // tracked change to lose.
                    .and_then(|_| run(&["read-tree", "--reset", "-u", &at]));
                if let Err(e) = checked {
                    // Removed while it is still where it was made, so a
                    // retry makes it again at the base it then resolves.
                    let _ = git::run(
                        jailed().args(["update-ref", "-d", &reference, &at]),
                        MAX_SAID,
                        CLEANUP_TIME,
                    );
                    return Err(said("checking out", &e));
                }
                Ok(format!("{branch} checked out at {at}"))
            }
            Self::Survey {
                git,
                repository,
                id,
                checkout,
                bases,
            } => {
                let now = Instant::now();
                let deadline = now.checked_add(SURVEY_GIT_TIME).unwrap_or(now);
                let run = |args: &[&str]| {
                    let left = deadline.saturating_duration_since(Instant::now());
                    git::run(
                        jailed(git, repository, id, checkout).args(args),
                        MAX_SAID,
                        left,
                    )
                };
                // `--no-optional-locks`: it only reads, so writes no
                // index, as a refresh would.
                let changes = match run(&[
                    "--no-optional-locks",
                    "status",
                    "--porcelain=v1",
                    "-z",
                    "--untracked-files=all",
                    "--no-renames",
                    "--ignore-submodules=none",
                ]) {
                    Ok(listed) => Some(
                        listed
                            .split(|byte| *byte == 0)
                            .filter(|entry| !entry.is_empty())
                            .count() as u64,
                    ),
                    Err(Failure::TooLong) => None,
                    Err(e) => return Err(said("listing its changes", &e)),
                };
                // Remote-tracking refs are upstream's, and step snapshots
                // td-agent's undo, not the work's.
                let counted = run(&[
                    "rev-list",
                    "--count",
                    "--exclude=refs/remotes/*",
                    "--exclude=refs/td-agent/*",
                    "--all",
                    "--not",
                ]
                .into_iter()
                .chain(bases.iter().map(String::as_str))
                .chain(["--"])
                .collect::<Vec<&str>>())
                .map_err(|e| said("counting its commits", &e))?;
                let ahead = String::from_utf8(counted)
                    .ok()
                    .and_then(|said| said.trim().parse::<u64>().ok())
                    .ok_or("counting its commits: git said no number")?;
                Ok(Survey { changes, ahead }.say())
            }
            Self::Track {
                git,
                repository,
                id,
                checkout,
                heads,
                preparing,
            } => {
                if *preparing {
                    let mut locks = vec![repository.join("packed-refs.lock")];
                    locks.extend(heads.iter().map(|(base, _)| {
                        repository.join(format!("refs/remotes/origin/{base}.lock"))
                    }));
                    for lock in locks {
                        match fs::remove_file(&lock) {
                            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                                return Err(format!("{}: {e}", lock.display()));
                            }
                            _ => {}
                        }
                    }
                }
                // One transaction: every ref is set, or none is, so the
                // record of them is never half right.
                let script: String = heads
                    .iter()
                    .map(|(base, commit)| format!("update refs/remotes/origin/{base} {commit}\n"))
                    .collect();
                git::run_fed(
                    jailed(git, repository, id, checkout).args([
                        "update-ref",
                        "--no-deref",
                        "--stdin",
                    ]),
                    Some(script.into_bytes()),
                    MAX_SAID,
                    TRACK_GIT_TIME,
                )
                .map_err(|e| said("setting the remote-tracking refs", &e))?;
                Ok(format!("{} remote-tracking refs set", heads.len()))
            }
            Self::Export { .. } => Err("an export is run with its frames' writer".into()),
        }
    }

    /// Runs an export, its pack in frames to `out`, and says the commit
    /// it exported; any other task as `run`.
    pub fn export(&self, out: &mut dyn Write) -> Result<String, String> {
        let Self::Export {
            git,
            repository,
            id,
            checkout,
            branch,
            base,
        } = self
        else {
            return self.run();
        };
        let jailed = || jailed(git, repository, id, checkout);
        let resolved = git::run(
            jailed().args([
                "rev-parse",
                "--verify",
                "--quiet",
                "--end-of-options",
                &format!("refs/heads/{branch}^{{commit}}"),
            ]),
            MAX_SAID,
            EXPORT_GIT_TIME,
        )
        .map_err(|e| said(&format!("resolving the branch {branch}"), &e))?;
        let commit = String::from_utf8_lossy(&resolved).trim().to_string();
        if !git::object_id(&commit) {
            return Err(format!("the branch {branch} resolved to {commit:?}"));
        }
        // Every object from the branch's commit back to the base, the
        // store's, which the publish repository borrows too.
        git::run_into(
            jailed().args(["pack-objects", "--stdout", "--revs", "--quiet"]),
            Some(format!("{commit}\n^{base}\n").into_bytes()),
            git::MAX_PACK,
            EXPORT_TIME,
            &mut |chunk| {
                let mut payload = Vec::with_capacity(chunk.len().saturating_add(1));
                payload.push(b'p');
                payload.extend_from_slice(chunk);
                crate::frame::write(out, &payload).map_err(|e| e.to_string())
            },
        )
        .map_err(|e| said("packing the commits", &e))?;
        Ok(commit)
    }
}

/// A maintenance instance's entry, `td-agent maintain ARGS`: the task
/// run and its answer written to `out`, one line, or for an export its
/// pack's frames and then the line as a frame of its own.
pub fn maintain(args: &[String], out: &mut dyn Write) -> Result<(), String> {
    let framed = args.first().is_some_and(|word| word == "export");
    let result = Task::parse(args).and_then(|task| task.export(out));
    let line = answer(&result);
    let _ = if framed {
        let mut payload = vec![b'a'];
        payload.extend_from_slice(line.as_bytes());
        crate::frame::write(out, &payload)
    } else {
        out.write_all(line.as_bytes()).and_then(|()| out.flush())
    };
    result.map(drop)
}

fn said(what: &str, failure: &Failure) -> String {
    format!("{what}: {failure}")
}

/// git over one linked worktree in the maintenance shape (DESIGN.md §9):
/// nothing of the instance's environment, no configuration but the
/// repository's own, which the jail cannot write, hooks and fsmonitor
/// off, and the directories named so a nested `.git` steers nothing.
fn jailed(git: &Path, repository: &Path, id: &str, checkout: &Path) -> Command {
    let mut command = Command::new(git);
    command
        .env_clear()
        .env("GIT_DIR", repository.join("worktrees").join(id))
        .env("GIT_WORK_TREE", checkout)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("HOME", "/nonexistent")
        .env("LANG", "C")
        .current_dir(checkout)
        .stdin(Stdio::null())
        .arg("-c")
        .arg(format!(
            "core.hooksPath={}",
            repository.join("hooks").display()
        ));
    for setting in [
        "core.fsmonitor=false",
        "core.attributesFile=/dev/null",
        "core.excludesFile=/dev/null",
        "submodule.recurse=false",
        "protocol.allow=never",
        "gc.auto=0",
        "maintenance.auto=false",
    ] {
        command.arg("-c").arg(setting);
    }
    command
}

/// The host's git, as a maintenance instance runs it: the first `git` on
/// `PATH`, by the path it resolves to, which the jail's system trees must
/// bind.
pub fn host_git() -> Result<PathBuf, String> {
    let path = std::env::var_os("PATH").ok_or("there is no PATH to find git on")?;
    let found = std::env::split_paths(&path)
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join("git"))
        .find(|candidate| candidate.is_file())
        .ok_or("there is no git on PATH")?;
    fs::canonicalize(&found).map_err(|e| format!("{}: {e}", found.display()))
}

/// The project instructions' files, in the order looked for (DESIGN.md
/// §13).
pub const INSTRUCTION_FILES: &[&str] = &["AGENTS.md", "CLAUDE.md"];

/// A worktree's project instructions as the git worker read them at its
/// base in the store (DESIGN.md §13): upstream's text, never a jail's.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Instructions {
    /// Neither `AGENTS.md` nor `CLAUDE.md` is at the base's top.
    Absent,
    /// The file `name`, whole.
    Found { name: String, text: String },
    /// Not read, and why: past a bound, not UTF-8, git failed.
    Unread { why: String },
}

impl Instructions {
    pub fn to_json(&self) -> Json {
        let kind = |kind: &str| ("kind".to_string(), Json::Str(kind.into()));
        match self {
            Self::Absent => Json::Obj(vec![kind("absent")]),
            Self::Found { name, text } => Json::Obj(vec![
                kind("found"),
                ("name".into(), Json::Str(name.clone())),
                ("text".into(), Json::Str(text.clone())),
            ]),
            Self::Unread { why } => {
                Json::Obj(vec![kind("unread"), ("why".into(), Json::Str(why.clone()))])
            }
        }
    }

    pub fn from_json(value: &Json) -> Result<Self, String> {
        let text = |key: &str| {
            value
                .get(key)
                .and_then(Json::as_str)
                .map(str::to_string)
                .ok_or_else(|| format!("project instructions without `{key}`"))
        };
        match value.get("kind").and_then(Json::as_str) {
            Some("absent") => Ok(Self::Absent),
            Some("found") => {
                let name = text("name")?;
                if !INSTRUCTION_FILES.contains(&name.as_str()) {
                    return Err(format!("project instructions from {name:?}"));
                }
                Ok(Self::Found {
                    name,
                    text: text("text")?,
                })
            }
            Some("unread") => Ok(Self::Unread { why: text("why")? }),
            _ => Err("project instructions of no known kind".into()),
        }
    }

    /// The bytes of text it carries.
    pub fn carried(&self) -> usize {
        match self {
            Self::Found { text, .. } => text.len(),
            Self::Absent | Self::Unread { .. } => 0,
        }
    }
}

/// A maintenance entry's answer, its standard output's one line.
pub fn answer(result: &Result<String, String>) -> String {
    let (word, text) = match result {
        Ok(text) => ("ok", text),
        Err(text) => ("failed", text),
    };
    format!("{word} {}\n", text.replace(char::is_control, " "))
}

/// Reads an answer back; anything else is a failure saying what came.
pub fn read_answer(bytes: &[u8]) -> Result<String, String> {
    let text = String::from_utf8_lossy(bytes);
    let line = text.strip_suffix('\n').unwrap_or(&text);
    if line.contains('\n') {
        return Err(format!("the maintenance instance said {text:?}"));
    }
    match line.split_once(' ') {
        Some(("ok", said)) => Ok(said.to_string()),
        Some(("failed", said)) => Err(said.to_string()),
        _ if line.is_empty() => Err("the maintenance instance said nothing".into()),
        _ => Err(format!("the maintenance instance said {line:?}")),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    #![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
    use super::*;
    use crate::store::tests::Scratch;

    fn worktree(checkout: &Path, sparse: Option<&[&str]>) -> Worktree {
        Worktree {
            id: "r".into(),
            checkout: checkout.to_path_buf(),
            branch: "agent/one".into(),
            sparse: sparse.map(|paths| paths.iter().map(|p| p.to_string()).collect()),
        }
    }

    #[test]
    fn cone_patterns_are_gits() {
        assert_eq!(cone(None).unwrap(), "/*\n");
        assert_eq!(cone(Some(&[])).unwrap(), "/*\n!/*/\n");
        let paths: Vec<String> = ["c/d", "a/", "c/d/e", "b/x/y"]
            .iter()
            .map(|p| p.to_string())
            .collect();
        assert_eq!(
            cone(Some(&paths)).unwrap(),
            "/*\n!/*/\n/a/\n/b/\n!/b/*/\n/b/x/\n!/b/x/*/\n/b/x/y/\n/c/\n!/c/*/\n/c/d/\n"
        );
        for bad in [
            "", "/", "a//b", "./a", "a/../b", "*", "a/b?", "!a", "#a", " a", "a\\b",
        ] {
            assert!(cone(Some(&[bad.to_string()])).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn the_config_quotes_the_identity_and_names_the_hooks() {
        let identity = Identity {
            name: Some("A \"B\" \\ C".into()),
            email: Some("a@b".into()),
        };
        let text = config_text(Path::new("/w/r.git"), &identity).unwrap();
        assert!(text.contains("\tname = \"A \\\"B\\\" \\\\ C\"\n"), "{text}");
        assert!(
            text.contains("\thooksPath = \"/w/r.git/hooks\"\n"),
            "{text}"
        );
        for line in [
            "sparseCheckoutCone = true",
            "autoSetupMerge = false",
            "ignoreSubmodules = all",
            "submoduleSummary = false",
            "enabled = false",
            "worktreePruneExpire = never",
            "updateServerInfo = false",
        ] {
            assert!(text.contains(line), "{line}: {text}");
        }
        assert!(!text.contains("worktreeConfig"));
        assert!(!config_text(Path::new("/w/r.git"), &Identity::default())
            .unwrap()
            .contains("[user]"));
        let broken = Identity {
            name: Some("a\nb".into()),
            email: None,
        };
        assert!(config_text(Path::new("/w/r.git"), &broken).is_err());
        assert!(config_text(Path::new("relative"), &Identity::default()).is_err());
    }

    /// Nothing of a staging directory is left beside a repository.
    fn staged_debris(dir: &Path) -> Vec<String> {
        fs::read_dir(dir)
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with(".td-agent-new-"))
            .collect()
    }

    #[test]
    fn a_repository_and_its_worktree_are_made_whole_and_once() {
        let scratch = Scratch::new("repo-layout");
        let base = fs::canonicalize(&scratch.0).unwrap();
        let store = base.join("store/s.git");
        fs::create_dir_all(&store).unwrap();
        // Named as they resolve, whatever the caller's path went through.
        std::os::unix::fs::symlink(base.join("store"), base.join("linked-store")).unwrap();
        let repository = base.join("ws/w/r.git");
        let made = create(
            &repository,
            &base.join("linked-store/s.git"),
            &Identity::default(),
        )
        .unwrap();
        assert_eq!(made, repository);
        for file in ["commondir", "config", "config.worktree", "shallow"] {
            let meta = fs::symlink_metadata(repository.join(file)).unwrap();
            assert!(meta.is_file(), "{file}");
        }
        assert_eq!(
            fs::read_to_string(repository.join("commondir")).unwrap(),
            ".\n"
        );
        assert_eq!(
            fs::read_to_string(repository.join("objects/info/alternates")).unwrap(),
            format!("{}\n", store.join("objects").display())
        );
        for dir in td_jail_directories() {
            assert!(repository.join(dir).is_dir(), "{dir}");
        }
        assert!(staged_debris(&base.join("ws/w")).is_empty());
        // Made once: a second call is refused and leaves the first alone.
        assert!(create(&repository, &store, &Identity::default()).is_err());
        assert!(repository.join("config").is_file());
        assert!(create(
            &base.join("ws/w/x.git"),
            &base.join("nowhere"),
            &Identity::default()
        )
        .is_err());
        let checkout = base.join("tree/w/r");
        assert_eq!(
            add_worktree(&repository, &worktree(&checkout, Some(&["src"]))).unwrap(),
            checkout
        );
        let linked = repository.join("worktrees/r");
        for (name, text) in [
            ("gitdir", format!("{}\n", checkout.join(".git").display())),
            ("commondir", "../..\n".to_string()),
            ("config.worktree", String::new()),
            ("HEAD", "ref: refs/heads/agent/one\n".to_string()),
            ("info/sparse-checkout", "/*\n!/*/\n/src/\n".to_string()),
        ] {
            assert_eq!(
                fs::read_to_string(linked.join(name)).unwrap(),
                text,
                "{name}"
            );
        }
        assert_eq!(
            fs::read_to_string(checkout.join(".git")).unwrap(),
            format!("gitdir: {}\n", linked.display())
        );
        assert!(staged_debris(&base.join("ws/w")).is_empty());
        // Neither an existing checkout nor an existing id is reused, and
        // a refused call removes only what it made.
        assert!(add_worktree(&repository, &worktree(&checkout, None)).is_err());
        assert!(checkout.join(".git").is_file() && linked.join("gitdir").is_file());
        let other = base.join("tree/w/r2");
        assert!(add_worktree(&repository, &worktree(&other, None)).is_err());
        assert!(!other.exists(), "the refused checkout is removed");
        assert!(linked.join("gitdir").is_file(), "the existing id is kept");
        // A planted link is never written through.
        let planted = base.join("tree/w/r3");
        std::os::unix::fs::symlink(base.join("elsewhere"), repository.join("worktrees/r3"))
            .unwrap();
        let mut third = worktree(&planted, None);
        third.id = "r3".into();
        assert!(add_worktree(&repository, &third).is_err());
        assert!(!base.join("elsewhere").exists() && !planted.exists());
        fs::remove_file(repository.join("worktrees/r3")).unwrap();
        for bad in ["", ".r", "-r", "a/b", "a b"] {
            let mut bad_id = worktree(&base.join("tree/w/x"), None);
            bad_id.id = bad.into();
            assert!(add_worktree(&repository, &bad_id).is_err(), "{bad:?}");
        }
        let mut bad_branch = worktree(&base.join("tree/w/y"), None);
        bad_branch.branch = "-x".into();
        assert!(add_worktree(&repository, &bad_branch).is_err());
        assert!(add_worktree(&base, &worktree(&base.join("z"), None)).is_err());
        // td-jail binds at most `MAX_WORKTREES`: one more is refused.
        for n in 1..MAX_WORKTREES {
            let mut more = worktree(&base.join(format!("tree/w/m{n}")), None);
            more.id = format!("m{n}");
            more.branch = format!("b{n}");
            add_worktree(&repository, &more).unwrap();
        }
        let mut past = worktree(&base.join("tree/w/past"), None);
        past.id = "past".into();
        let refused = add_worktree(&repository, &past).unwrap_err();
        assert!(refused.contains("the most td-jail binds"), "{refused}");
        assert!(!base.join("tree/w/past").exists());
        assert!(staged_debris(&base.join("ws/w")).is_empty());
    }

    /// The directories td-jail binds read-only, with `objects/`.
    fn td_jail_directories() -> &'static [&'static str] {
        &[
            "objects",
            "objects/info",
            "branches",
            "hooks",
            "info",
            "remotes",
            "worktrees",
        ]
    }

    #[test]
    fn a_task_round_trips_and_refuses_what_it_cannot_name() {
        let task = Task::Checkout {
            git: "/usr/bin/git".into(),
            repository: "/w/r.git".into(),
            id: "r".into(),
            checkout: "/t/r".into(),
            branch: "agent/one".into(),
            base: "a".repeat(40),
        };
        let words: Vec<String> = task
            .args()
            .into_iter()
            .map(|word| word.into_string().unwrap())
            .collect();
        assert_eq!(Task::parse(&words).unwrap(), task);
        for (index, bad) in [
            (1, "git"),
            (2, "rel/r.git"),
            (3, "../r"),
            (4, "/t/r\n"),
            (5, "-b"),
            (6, "main"),
        ] {
            let mut broken = words.clone();
            broken[index] = bad.into();
            assert!(Task::parse(&broken).is_err(), "{index} {bad:?}");
        }
        assert!(Task::parse(&words[..6]).is_err());
    }

    #[test]
    fn an_answer_is_one_line() {
        assert_eq!(answer(&Ok("done".into())), "ok done\n");
        assert_eq!(read_answer(b"ok done\n"), Ok("done".into()));
        assert_eq!(
            read_answer(answer(&Err("a\nb".into())).as_bytes()),
            Err("a b".into())
        );
        for bad in [&b""[..], b"ok", b"ok a\nok b\n", b"maybe x\n"] {
            assert!(read_answer(bad).is_err(), "{bad:?}");
        }
    }

    /// Plain git, the test's own, for the fixture's upstream and store.
    pub(crate) fn plain(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .current_dir(dir)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "-c",
                "init.defaultBranch=main",
                "-c",
                "protocol.file.allow=always",
            ])
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    }

    /// An upstream with a tree of a few directories, fetched into a bare
    /// store as the git worker's would be; the store and its `main`.
    pub(crate) fn store_fixture(root: &Path) -> (PathBuf, String) {
        let up = root.join("up");
        for path in ["a/x", "c/d/z", "c/w", "top"] {
            let file = up.join(path);
            fs::create_dir_all(file.parent().unwrap()).unwrap();
            fs::write(&file, format!("{path}\n")).unwrap();
        }
        plain(&up, &["init", "--quiet"]);
        plain(&up, &["add", "."]);
        plain(&up, &["commit", "--quiet", "-m", "one"]);
        let store = root.join("store/up.git");
        fs::create_dir_all(&store).unwrap();
        plain(&store, &["init", "--quiet", "--bare"]);
        plain(
            &store,
            &[
                "fetch",
                "--quiet",
                up.to_str().unwrap(),
                "+refs/heads/*:refs/heads/*",
            ],
        );
        let base = plain(&store, &["rev-parse", "main"]).trim().to_string();
        (store, base)
    }

    /// The task run as a maintenance instance runs it, here outside one:
    /// the layout is git's to read, the branch is made at the base, the
    /// checkout is the sparse cone's, and a run a crash cut short is
    /// taken up again, its branch kept where it was made.
    #[test]
    fn a_checkout_task_checks_the_cone_out_on_a_new_branch() {
        if !git::tests::have_git() {
            return;
        }
        let scratch = Scratch::new("repo-checkout");
        let (store, base) = store_fixture(&scratch.0);
        let repository = scratch.0.join("ws/w/r.git");
        let checkout = scratch.0.join("tree/w/r");
        let identity = Identity {
            name: Some("Human".into()),
            email: Some("h@example.org".into()),
        };
        create(&repository, &store, &identity).unwrap();
        add_worktree(&repository, &worktree(&checkout, Some(&["c/d"]))).unwrap();
        let found = String::from_utf8(
            Command::new("sh")
                .args(["-c", "command -v git"])
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap();
        let task = Task::Checkout {
            git: fs::canonicalize(found.trim()).unwrap(),
            repository: repository.clone(),
            id: "r".into(),
            checkout: checkout.clone(),
            branch: "agent/one".into(),
            base: base.clone(),
        };
        // A checkout that fails removes the branch it made, so it can be
        // asked again, over what it wrote: here a file whose blob is
        // missing from the store, which stops root as anyone, and a file
        // it left.
        let blob = plain(&store, &["rev-parse", &format!("{base}:top")]);
        let (fan, rest) = blob.trim().split_at(2);
        let object = store.join("objects").join(fan).join(rest);
        let aside = store.join("objects/aside");
        fs::rename(&object, &aside).unwrap();
        fs::write(checkout.join("top"), "left by a broken run\n").unwrap();
        let e = task.run().unwrap_err();
        assert!(e.starts_with("checking out"), "{e}");
        assert!(!repository.join("refs/heads/agent/one").exists());
        fs::rename(&aside, &object).unwrap();
        // `HEAD` is set by the task, whatever the jail wrote there.
        let head = repository.join("worktrees/r/HEAD");
        fs::write(&head, "ref: refs/heads/elsewhere\n").unwrap();
        assert_eq!(
            task.run().unwrap(),
            format!("agent/one checked out at {base}")
        );
        assert_eq!(
            fs::read_to_string(&head).unwrap(),
            "ref: refs/heads/agent/one\n"
        );
        let mut files: Vec<String> = Vec::new();
        for path in ["top", "c/w", "c/d/z", "a/x"] {
            if checkout.join(path).exists() {
                files.push(path.into());
            }
        }
        assert_eq!(files, ["top", "c/w", "c/d/z"], "the cone, not a/");
        assert_eq!(
            fs::read_to_string(checkout.join("top")).unwrap(),
            "top\n",
            "the broken run's file is overwritten"
        );
        assert_eq!(
            fs::read_to_string(repository.join("refs/heads/agent/one"))
                .unwrap()
                .trim(),
            base
        );
        // Checked out once: the index it made refuses a second run.
        let e = task.run().unwrap_err();
        assert!(e.contains("is not checked out again"), "{e}");
        // A commit there carries the repository's identity; no object is
        // copied from the store but the new ones.
        let mut commit = jailed(&task_git(&task), &repository, "r", &checkout);
        fs::write(checkout.join("top"), "changed\n").unwrap();
        git::run(
            commit.args(["commit", "--quiet", "-a", "-m", "two"]),
            MAX_SAID,
            CHECKOUT_TIME,
        )
        .unwrap();
        let mut log = jailed(&task_git(&task), &repository, "r", &checkout);
        let said = git::run(
            log.args(["log", "--format=%an %s", "-2"]),
            MAX_SAID,
            CHECKOUT_TIME,
        )
        .unwrap();
        assert_eq!(String::from_utf8(said).unwrap(), "Human two\nt one\n");
        // A run a crash cut short after making its branch, the base
        // resolved again since and moved on: the branch is kept where it
        // was made, and the locks the killed git left are cleared.
        let mut third = worktree(&scratch.0.join("tree/w/r3"), None);
        third.id = "r3".into();
        third.branch = "agent/three".into();
        let checkout_three = add_worktree(&repository, &third).unwrap();
        let earlier = fs::read_to_string(repository.join("refs/heads/agent/one")).unwrap();
        assert_ne!(earlier.trim(), base);
        fs::write(repository.join("refs/heads/agent/three"), &earlier).unwrap();
        let locks = [
            "refs/heads/agent/three.lock",
            "packed-refs.lock",
            "worktrees/r3/index.lock",
            "worktrees/r3/HEAD.lock",
        ];
        for lock in locks {
            fs::write(repository.join(lock), "").unwrap();
        }
        let task_three = Task::Checkout {
            git: task_git(&task),
            repository: repository.clone(),
            id: "r3".into(),
            checkout: checkout_three.clone(),
            branch: "agent/three".into(),
            base: base.clone(),
        };
        assert_eq!(
            task_three.run().unwrap(),
            format!("agent/three checked out at {}", earlier.trim())
        );
        assert_eq!(
            fs::read_to_string(checkout_three.join("top")).unwrap(),
            "changed\n"
        );
        assert_eq!(
            fs::read_to_string(repository.join("refs/heads/agent/three")).unwrap(),
            earlier
        );
        for lock in locks {
            assert!(!repository.join(lock).exists(), "{lock}");
        }
        assert!(!repository
            .join("objects/pack")
            .read_dir()
            .unwrap()
            .any(|_| true));
    }

    /// An export's frames read back: its pack, and its answer line.
    fn unframe(mut bytes: &[u8]) -> (Vec<u8>, String) {
        let (mut pack, mut answer) = (Vec::new(), None);
        while let Some(payload) = crate::frame::read(&mut bytes).unwrap() {
            assert!(answer.is_none(), "a frame after the answer");
            match payload.split_first().unwrap() {
                (b'p', chunk) => pack.extend_from_slice(chunk),
                (b'a', line) => answer = Some(String::from_utf8(line.to_vec()).unwrap()),
                (tag, _) => panic!("a frame tagged {tag}"),
            }
        }
        (pack, answer.unwrap())
    }

    /// An export sends the commits of td-agent's branch from the base as
    /// a pack, framed, which the publish repository imports strictly; an
    /// import refuses a pack missing what its objects name, one cut
    /// short, and one without the commit (DESIGN.md §9, Pushing).
    #[test]
    fn an_export_is_imported_strictly_into_the_publish_repository() {
        if !git::tests::have_git() {
            return;
        }
        let scratch = Scratch::new("repo-export");
        let (store, base) = store_fixture(&scratch.0);
        let repository = scratch.0.join("ws/w/r.git");
        let checkout = scratch.0.join("tree/w/r");
        create(&repository, &store, &Identity::default()).unwrap();
        add_worktree(&repository, &worktree(&checkout, None)).unwrap();
        let found = String::from_utf8(
            Command::new("sh")
                .args(["-c", "command -v git"])
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap();
        let git = fs::canonicalize(found.trim()).unwrap();
        Task::Checkout {
            git: git.clone(),
            repository: repository.clone(),
            id: "r".into(),
            checkout: checkout.clone(),
            branch: "agent/one".into(),
            base: base.clone(),
        }
        .run()
        .unwrap();
        // Two commits, as the model's own git in the jail makes them.
        let mut commits = Vec::new();
        for (file, text) in [("one", "first\n"), ("two", "second\n")] {
            fs::write(checkout.join(file), text).unwrap();
            plain(&checkout, &["add", file]);
            plain(&checkout, &["commit", "--quiet", "-m", file]);
            commits.push(plain(&checkout, &["rev-parse", "HEAD"]).trim().to_string());
        }
        let export = |from: &str| Task::Export {
            git: git.clone(),
            repository: repository.clone(),
            id: "r".into(),
            checkout: checkout.clone(),
            branch: "agent/one".into(),
            base: from.to_string(),
        };
        // Its words cross, and the entry frames its answer after the pack.
        let task = export(&base);
        let words: Vec<String> = task
            .args()
            .iter()
            .map(|w| w.to_str().unwrap().to_string())
            .collect();
        assert_eq!(Task::parse(&words).unwrap(), task);
        let mut out = Vec::new();
        maintain(&words, &mut out).unwrap();
        let (pack, line) = unframe(&out);
        assert_eq!(line, format!("ok {}\n", commits[1]));
        assert!(pack.starts_with(b"PACK"), "{:?}", pack.get(..8));
        // The publish repository takes it whole.
        let worker = git::Worker::new(&scratch.0.join("git"), &git::kept_env()).unwrap();
        let publish = scratch.0.join("publish/w/r.git");
        worker.publish(&publish, &store).unwrap();
        worker.publish(&publish, &store).unwrap();
        let file = scratch.0.join("export.pack");
        fs::write(&file, &pack).unwrap();
        worker.import(&publish, &file, &commits[1]).unwrap();
        let head = |c: &str| plain(&publish, &["rev-parse", &format!("{c}^{{tree}}")]);
        assert_eq!(
            head(&commits[1]),
            plain(&checkout, &["rev-parse", "HEAD^{tree}"])
        );
        // It holds no ref the jail wrote, only objects.
        assert_eq!(plain(&publish, &["for-each-ref"]), "");
        // A pack from the first commit on names a parent the publish
        // repository lacks, and is refused.
        let missing = scratch.0.join("publish/w/s.git");
        worker.publish(&missing, &store).unwrap();
        let mut out = Vec::new();
        assert_eq!(export(&commits[0]).export(&mut out).unwrap(), commits[1]);
        fs::write(&file, unframe_pack(&out)).unwrap();
        let e = worker.import(&missing, &file, &commits[1]).unwrap_err();
        assert!(e.starts_with("importing the export"), "{e}");
        // Cut short, refused.
        let other = scratch.0.join("publish/w/t.git");
        worker.publish(&other, &store).unwrap();
        fs::write(&file, pack.get(..pack.len() / 2).unwrap()).unwrap();
        let e = worker.import(&other, &file, &commits[1]).unwrap_err();
        assert!(e.starts_with("importing the export"), "{e}");
        // Whole, but without the commit named, refused.
        fs::write(&file, &pack).unwrap();
        let e = worker.import(&other, &file, &"f".repeat(40)).unwrap_err();
        assert!(e.contains("no commit"), "{e}");
        // Nor is an object that only peels to it, such as a tag.
        plain(&publish, &["tag", "-a", "-m", "t", "t", &commits[1]]);
        let tag = plain(&publish, &["rev-parse", "t"]).trim().to_string();
        assert_ne!(tag, commits[1]);
        let e = worker.import(&publish, &file, &tag).unwrap_err();
        assert!(e.contains("no commit"), "{e}");
        // A branch that is not there is said.
        let mut out = Vec::new();
        let gone = Task::Export {
            git: git.clone(),
            repository: repository.clone(),
            id: "r".into(),
            checkout: checkout.clone(),
            branch: "nowhere".into(),
            base: base.clone(),
        };
        let e = gone.export(&mut out).unwrap_err();
        assert!(e.starts_with("resolving the branch nowhere"), "{e}");
        assert!(out.is_empty());
    }

    /// A push's evidence (DESIGN.md §9, Pushing, step 3): the commits it
    /// adds and the paths they change, every binary file it carries, and
    /// the scan, which finds a secret in a binary file, behind a `-diff`
    /// attribute, added in one commit and removed in the next, in a
    /// message whatever its encoding says, in an author's name, on an
    /// added line that looks like a diff's header, in a path git quotes,
    /// in UTF-16, in a merge's own change, and a private key's armour
    /// that only the file's whole text shows.
    #[test]
    fn a_pushs_evidence_names_its_changes_and_finds_its_secrets() {
        if !git::tests::have_git() {
            return;
        }
        let scratch = Scratch::new("repo-evidence");
        let (store, base) = store_fixture(&scratch.0);
        let work = scratch.0.join("work");
        plain(
            &scratch.0,
            &["clone", "--quiet", store.to_str().unwrap(), "work"],
        );
        let commit = |files: &[(&str, &[u8])], message: &str| {
            for (path, bytes) in files {
                fs::write(work.join(path), bytes).unwrap();
                plain(&work, &["add", path]);
            }
            plain(
                &work,
                &["commit", "--quiet", "--allow-empty", "-m", message],
            );
            plain(&work, &["rev-parse", "HEAD"]).trim().to_string()
        };
        let removing = |path: &str, message: &str| {
            plain(&work, &["rm", "--quiet", path]);
            commit(&[], message)
        };
        let token = |prefix: &str, n: usize| format!("{prefix}{}", "a".repeat(n));
        let clean = commit(&[("notes.txt", b"ok\n")], "notes");
        let hidden = commit(
            &[
                (".gitattributes", b"secret.txt -diff\n"),
                ("secret.txt", token("ghp_", 36).as_bytes()),
            ],
            "hidden",
        );
        removing("secret.txt", "removed");
        let mut binary = vec![0u8, 1, 2, 0xff];
        binary.extend_from_slice(format!("AKIA{}", "B".repeat(16)).as_bytes());
        let binary_commit = commit(&[("blob.bin", &binary)], "binary");
        let told = commit(&[], &format!("told\n\n{}", token("xoxb-1-", 24)));
        // A line that looks like a header names no path for the next.
        let plus = format!(
            "++ {}\n{}\n",
            token("glpat-", 20),
            token("sk-ant-api03-", 40)
        );
        let header = commit(&[("plus.txt", plus.as_bytes())], "plus");
        // A path is published too, and a match in a later file's header
        // names no earlier file's path.
        let named = commit(
            &[("a.txt", b"a\n"), (&token("sk_live_", 24), b"x\n")],
            "named",
        );
        // A binary file the push carries, though gone at its end.
        commit(&[("archive.bin", &[0, 1, 2, 3])], "archive");
        removing("archive.bin", "unarchived");
        // A path git quotes, `"b/\tghp_..."`.
        let tabbed = format!("\t{}", token("ghp_", 36).replace('a', "b"));
        let quoted = commit(&[(&tabbed, b"x\n")], "quoted");
        // A message whose header says it is not ASCII, and a token in an
        // author's name: in the object as it is stored.
        plain(
            &work,
            &[
                "-c",
                &format!("user.name={}", token("sk-proj-", 32)),
                "-c",
                "i18n.commitEncoding=IBM037",
                "commit",
                "--quiet",
                "--allow-empty",
                "-m",
                &format!("encoded {}", token("glrt-", 20)),
            ],
        );
        let encoded = plain(&work, &["rev-parse", "HEAD"]).trim().to_string();
        // UTF-16, added and removed.
        let wide: Vec<u8> = [0xff, 0xfe]
            .into_iter()
            .chain(
                format!("x {}", token("ghp_", 36).replace('a', "c"))
                    .bytes()
                    .flat_map(|b| [b, 0]),
            )
            .collect();
        let utf16 = commit(&[("u16.txt", &wide)], "utf16");
        removing("u16.txt", "narrowed");
        // A text file overwritten as binary.
        commit(&[("plus.txt", &[0, 0, 0])], "overwritten");
        // A merge whose own change, against its first parent, adds one.
        plain(&work, &["checkout", "--quiet", "-b", "side", &base]);
        commit(&[("side.txt", b"side\n")], "side");
        plain(&work, &["checkout", "--quiet", "main"]);
        plain(
            &work,
            &["merge", "--quiet", "--no-ff", "--no-commit", "side"],
        );
        let merge = commit(&[("evil.txt", token("npm_", 36).as_bytes())], "merge");
        // A key's file whose change is far from its armour.
        plain(&work, &["checkout", "--quiet", "-b", "keys", &base]);
        let body = |changed: &str| {
            let mut text = "-----BEGIN RSA PRIVATE KEY-----\n".to_string();
            for line in 0..30 {
                text.push_str(if line == 20 { changed } else { "QUJDREVGR0g=" });
                text.push('\n');
            }
            text + "-----END RSA PRIVATE KEY-----\n"
        };
        let first_key = commit(&[("key.pem", body("QUJD").as_bytes())], "key");
        let second_key = commit(&[("key.pem", body("REVG").as_bytes())], "rekey");
        let publish = scratch.0.join("publish/w/r.git");
        let worker = git::Worker::new(&scratch.0.join("git"), &git::kept_env()).unwrap();
        worker.publish(&publish, &store).unwrap();
        plain(
            &publish,
            &[
                "fetch",
                "--quiet",
                work.to_str().unwrap(),
                "+main:refs/heads/pushed",
                "+keys:refs/heads/keys",
            ],
        );
        let evidence = worker.evidence(&publish, &merge, &base, None).unwrap();
        assert_eq!(evidence.merge_base, None);
        assert_eq!(
            evidence.commits.first().unwrap(),
            &(merge.clone(), "merge".to_string())
        );
        assert!(evidence
            .commits
            .contains(&(encoded.clone(), format!("encoded {}", token("glrt-", 20)))));
        assert_eq!(evidence.commits.len(), 16);
        assert_eq!(evidence.more_commits, 0);
        let path = |name: &str| {
            evidence
                .paths
                .iter()
                .find(|(p, _)| p == name)
                .map(|(_, l)| *l)
        };
        assert_eq!(path("notes.txt"), Some(Some((1, 0))));
        assert_eq!(path("blob.bin"), Some(None));
        assert_eq!(path("plus.txt"), Some(None));
        assert_eq!(path("secret.txt"), None, "added and removed");
        let mut binaries = evidence.binaries.clone();
        binaries.sort();
        assert_eq!(binaries, ["archive.bin", "blob.bin", "plus.txt", "u16.txt"]);
        let found: Vec<(&str, Option<&str>, Option<&str>)> = evidence
            .found
            .iter()
            .map(|f| (f.kind, f.commit.as_deref(), f.path.as_deref()))
            .collect();
        let at = |commit: &str| Some(commit.to_string());
        let wanted = [
            ("a GitHub token", at(&hidden), Some("secret.txt")),
            ("an AWS access key", at(&binary_commit), Some("blob.bin")),
            ("a Slack token", at(&told), None),
            ("a GitLab token", at(&header), Some("plus.txt")),
            ("an Anthropic key", at(&header), Some("plus.txt")),
            ("a Stripe key", at(&named), None),
            // In the top directory's names.
            ("a Stripe key", None, None),
            ("a GitHub token", at(&quoted), None),
            ("a GitHub token", None, None),
            ("an OpenAI key", at(&encoded), None),
            ("a GitLab token", at(&encoded), None),
            ("a GitHub token", at(&utf16), Some("u16.txt")),
            ("an npm token", at(&merge), Some("evil.txt")),
        ];
        for (kind, commit, path) in &wanted {
            let want = (*kind, commit.as_deref(), *path);
            assert!(found.contains(&want), "{want:?} not in {found:?}");
        }
        assert_eq!(found.len(), wanted.len(), "{found:?}");
        assert!(!evidence.clean());
        // Onto a branch whose tip upstream holds the first two commits:
        // only what it lacks, and the paths from where they meet.
        let evidence = worker
            .evidence(&publish, &merge, &base, Some(&hidden))
            .unwrap();
        assert_eq!(evidence.merge_base.as_deref(), Some(hidden.as_str()));
        assert_eq!(evidence.commits.len(), 14);
        assert_eq!(evidence.paths.iter().find(|(p, _)| p == "notes.txt"), None);
        assert!(!evidence
            .found
            .iter()
            .any(|f| f.commit.as_deref() == Some(hidden.as_str())));
        // A key's armour, which no added line shows.
        let evidence = worker
            .evidence(&publish, &second_key, &base, Some(&first_key))
            .unwrap();
        assert_eq!(
            evidence.found,
            [git::Found {
                kind: "a private key",
                commit: None,
                path: Some("key.pem".into()),
            }]
        );
        assert!(evidence.binaries.is_empty());
        // Nothing found, it is clean.
        let evidence = worker.evidence(&publish, &clean, &base, None).unwrap();
        assert!(evidence.clean(), "{evidence:?}");
        assert_eq!(evidence.commits, [(clean.clone(), "notes".to_string())]);
        // An id that is not one is refused before git runs.
        assert!(worker.evidence(&publish, "HEAD", &base, None).is_err());
    }

    /// An export's frames, the pack alone.
    fn unframe_pack(bytes: &[u8]) -> Vec<u8> {
        let mut pack = Vec::new();
        let mut rest = bytes;
        while let Some(payload) = crate::frame::read(&mut rest).unwrap() {
            if let (b'p', chunk) = payload.split_first().unwrap() {
                pack.extend_from_slice(chunk);
            }
        }
        pack
    }

    fn task_git(task: &Task) -> PathBuf {
        match task {
            Task::Checkout { git, .. }
            | Task::Survey { git, .. }
            | Task::Track { git, .. }
            | Task::Export { git, .. } => git.clone(),
        }
    }

    /// A survey counts changed and untracked files, and commits from
    /// `HEAD` or any branch not in the base, writing nothing; its words
    /// cross and are read back.
    #[test]
    fn a_survey_counts_what_removing_the_worktree_would_lose() {
        if !git::tests::have_git() {
            return;
        }
        let scratch = Scratch::new("repo-survey");
        let (store, base) = store_fixture(&scratch.0);
        let repository = scratch.0.join("ws/w/r.git");
        let checkout = scratch.0.join("tree/w/r");
        let identity = Identity {
            name: Some("Human".into()),
            email: Some("h@example.org".into()),
        };
        create(&repository, &store, &identity).unwrap();
        add_worktree(&repository, &worktree(&checkout, None)).unwrap();
        let found = String::from_utf8(
            Command::new("sh")
                .args(["-c", "command -v git"])
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap();
        let git = fs::canonicalize(found.trim()).unwrap();
        Task::Checkout {
            git: git.clone(),
            repository: repository.clone(),
            id: "r".into(),
            checkout: checkout.clone(),
            branch: "agent/one".into(),
            base: base.clone(),
        }
        .run()
        .unwrap();
        let survey = Task::Survey {
            git: git.clone(),
            repository: repository.clone(),
            id: "r".into(),
            checkout: checkout.clone(),
            bases: vec![base.clone()],
        };
        let words: Vec<String> = survey
            .args()
            .iter()
            .map(|w| w.to_string_lossy().into_owned())
            .collect();
        assert_eq!(Task::parse(&words).unwrap(), survey);
        let read = || Survey::parse(&survey.run().unwrap()).unwrap();
        assert!(read().clean(), "{:?}", read());
        // A changed file and an untracked one; a lock a killed tool left
        // stops nothing.
        fs::write(checkout.join("top"), "changed\n").unwrap();
        fs::write(checkout.join("new"), "new\n").unwrap();
        let lock = repository.join("worktrees/r/index.lock");
        fs::write(&lock, "").unwrap();
        assert_eq!(
            read(),
            Survey {
                changes: Some(2),
                ahead: 0
            }
        );
        fs::remove_file(&lock).unwrap();
        // Committed, then a second branch past it: three commits.
        let mut commit = jailed(&git, &repository, "r", &checkout);
        git::run(
            commit.args(["commit", "--quiet", "-a", "-m", "two"]),
            MAX_SAID,
            CHECKOUT_TIME,
        )
        .unwrap();
        let mut branch = jailed(&git, &repository, "r", &checkout);
        git::run(
            branch.args(["checkout", "--quiet", "-b", "agent/side"]),
            MAX_SAID,
            CHECKOUT_TIME,
        )
        .unwrap();
        fs::write(checkout.join("new"), "newer\n").unwrap();
        let mut more = jailed(&git, &repository, "r", &checkout);
        git::run(more.args(["add", "new"]), MAX_SAID, CHECKOUT_TIME).unwrap();
        let mut side = jailed(&git, &repository, "r", &checkout);
        git::run(
            side.args(["commit", "--quiet", "-m", "three"]),
            MAX_SAID,
            CHECKOUT_TIME,
        )
        .unwrap();
        // Back on the first branch: the side's commit is counted only as
        // a branch's.
        let mut back = jailed(&git, &repository, "r", &checkout);
        git::run(
            back.args(["checkout", "--quiet", "agent/one"]),
            MAX_SAID,
            CHECKOUT_TIME,
        )
        .unwrap();
        assert_eq!(
            read(),
            Survey {
                changes: Some(0),
                ahead: 2
            }
        );
        let git_in = |args: &[&str]| {
            let mut command = jailed(&git, &repository, "r", &checkout);
            git::run(command.args(args), MAX_SAID, CHECKOUT_TIME).unwrap()
        };
        // A stash is work: its two commits count, the tree clean.
        fs::write(checkout.join("top"), "stashed\n").unwrap();
        git_in(&["stash", "--quiet"]);
        assert_eq!(
            read(),
            Survey {
                changes: Some(0),
                ahead: 4
            }
        );
        git_in(&["stash", "drop", "--quiet"]);
        // A tag keeps a branch's commit when the branch goes.
        git_in(&["tag", "keep", "agent/side"]);
        git_in(&["branch", "--quiet", "-D", "agent/side"]);
        assert_eq!(read().ahead, 2);
        git_in(&["tag", "-d", "keep"]);
        assert_eq!(read().ahead, 1);
        // A step snapshot is td-agent's undo, not the work.
        let tree = String::from_utf8(git_in(&["write-tree"])).unwrap();
        let kept =
            String::from_utf8(git_in(&["commit-tree", tree.trim(), "-m", "snapshot"])).unwrap();
        git_in(&["update-ref", "refs/td-agent/snapshots/r", kept.trim()]);
        assert_eq!(read().ahead, 1);
        // Every base its repository's worktrees started at is upstream's:
        // a commit in any is not counted.
        let two = String::from_utf8(git_in(&["rev-parse", "agent/one"]))
            .unwrap()
            .trim()
            .to_string();
        let both = Task::Survey {
            git: git.clone(),
            repository: repository.clone(),
            id: "r".into(),
            checkout: checkout.clone(),
            bases: vec![base.clone(), two],
        };
        let words_both: Vec<String> = both
            .args()
            .iter()
            .map(|w| w.to_string_lossy().into_owned())
            .collect();
        assert_eq!(Task::parse(&words_both).unwrap(), both);
        assert_eq!(Survey::parse(&both.run().unwrap()).unwrap().ahead, 0);
        // A remote-tracking ref is set to the commit given, whatever was
        // there, and is not the work's.
        let origin = || {
            String::from_utf8(git_in(&["rev-parse", "refs/remotes/origin/main"]))
                .unwrap()
                .trim()
                .to_string()
        };
        let tracking = |heads: Vec<(String, String)>, preparing: bool| Task::Track {
            git: git.clone(),
            repository: repository.clone(),
            id: "r".into(),
            checkout: checkout.clone(),
            heads,
            preparing,
        };
        let track = |commit: &str| tracking(vec![("main".into(), commit.to_string())], false);
        // A lock a killed run left stops a later run, but not one made
        // while preparing.
        let lock = repository.join("refs/remotes/origin/main.lock");
        fs::create_dir_all(lock.parent().unwrap()).unwrap();
        fs::write(&lock, "").unwrap();
        assert!(track(&base).run().is_err());
        let preparing = tracking(vec![("main".into(), base.clone())], true);
        let words_new: Vec<String> = preparing
            .args()
            .iter()
            .map(|w| w.to_string_lossy().into_owned())
            .collect();
        assert_eq!(Task::parse(&words_new).unwrap(), preparing);
        preparing.run().unwrap();
        assert!(!lock.exists());
        assert_eq!(origin(), base);
        let tip = String::from_utf8(git_in(&["rev-parse", "agent/one"]))
            .unwrap()
            .trim()
            .to_string();
        track(&tip).run().unwrap();
        assert_eq!(origin(), tip);
        // All or none: a second base's missing commit leaves the first.
        let both = tracking(
            vec![
                ("main".into(), base.clone()),
                ("next".into(), "f".repeat(40)),
            ],
            false,
        );
        assert!(both.run().is_err());
        assert_eq!(origin(), tip);
        assert_eq!(Survey::parse(&survey.run().unwrap()).unwrap().ahead, 1);
        let words_track: Vec<String> = track(&base)
            .args()
            .iter()
            .map(|w| w.to_string_lossy().into_owned())
            .collect();
        assert_eq!(Task::parse(&words_track).unwrap(), track(&base));
        assert!(track(&"f".repeat(40)).run().is_err(), "no such commit");
        for bad in [
            vec!["main"],
            vec!["main", "abc"],
            vec!["-x", base.as_str()],
            vec![base.as_str(), "main"],
        ] {
            let mut words: Vec<String> = words_track.iter().take(5).cloned().collect();
            words.extend(bad.iter().map(|w| w.to_string()));
            assert!(Task::parse(&words).is_err(), "{bad:?}");
        }
        assert!(Survey::parse("changes many ahead 0")
            .unwrap()
            .changes
            .is_none());
        for bad in ["", "changes 1", "changes x ahead 0", "changes 1 ahead -1"] {
            assert!(Survey::parse(bad).is_err(), "{bad:?}");
        }
        let mut words = words;
        if let Some(last) = words.last_mut() {
            *last = "main".into();
        }
        assert!(Task::parse(&words).is_err());
    }
}

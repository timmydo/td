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
/// The word a maintenance instance's entry is started with.
pub const MAINTAIN: &str = "maintain";

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
        }
    }

    pub fn parse(args: &[String]) -> Result<Self, String> {
        let absolute = |path: &str| {
            let path = PathBuf::from(path);
            line_path(&path)?;
            Ok::<_, String>(path)
        };
        match args {
            [word, git, repository, id, checkout, branch, base] if word == "checkout" => {
                if !git::object_id(base) {
                    return Err(format!("{base:?} is not a full commit id"));
                }
                Ok(Self::Checkout {
                    git: absolute(git)?,
                    repository: absolute(repository)?,
                    id: worktree_id(id)?.into(),
                    checkout: absolute(checkout)?,
                    branch: git::branch_name(branch)?.into(),
                    base: base.clone(),
                })
            }
            _ => Err(
                "usage: td-agent maintain checkout GIT REPOSITORY ID CHECKOUT BRANCH BASE".into(),
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
        }
    }
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

    fn task_git(task: &Task) -> PathBuf {
        match task {
            Task::Checkout { git, .. } => git.clone(),
        }
    }
}

//! Step snapshots (DESIGN.md §12): each worktree of a repository
//! workspace recorded as a git tree, before and after a model step that
//! may change files, by the tool host in a jail instance. A worktree's
//! index is copied to a private one, `git add -A` brings it to the
//! worktree, `git write-tree` writes the tree into the workspace
//! repository's objects, and a commit of it onto the worktree's
//! `refs/td-agent/snapshots/<worktree>` keeps it from gc. What comes back
//! is jail-controlled: tree ids the conversation checks, and the names of
//! the files changed, which it only shows.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;

use td_json::Json;

/// Where a worktree's snapshots are kept, by its id in the repository.
pub const REF: &str = "refs/td-agent/snapshots/";

/// The most changed files a snapshot names for a worktree; the rest are
/// counted.
pub const MAX_CHANGED: usize = 40;

/// The most of a changed file's name kept, in bytes.
const MAX_NAME: usize = 200;

/// The most a worktree's named files take as a log line escapes them;
/// past it the rest are counted.
pub const MAX_NAMED: usize = 16 * 1024;

/// The most every worktree's named files take together, escaped: escaped
/// again in the tool host's answer, at most twice as much, a snapshot
/// fits a frame and a log line (`frame::MAX_FRAME`, `store::MAX_LINE`).
pub const MAX_NAMED_ALL: usize = 256 * 1024;

/// One worktree's snapshot: its checkout, its tree, and, against the
/// tree before, the files that changed, as many as `MAX_CHANGED` and
/// `MAX_NAMED` allow, and how many more did.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Taken {
    pub checkout: String,
    pub tree: String,
    pub changed: Vec<String>,
    pub more: u64,
}

/// The host's git by an absolute path, run in the environment a command
/// in the instance has.
pub struct Git {
    pub path: PathBuf,
    pub env: Vec<(OsString, OsString)>,
}

impl Git {
    /// Git in `checkout`, finding its repository as a command there
    /// would, reading no configuration, ignore rules or attributes from
    /// the home or the system and running no hook or fsmonitor, so what a
    /// jail can plant in its home neither runs nor shapes the record.
    fn at(&self, checkout: &Path) -> Command {
        let mut command = Command::new(&self.path);
        command
            .env_clear()
            .envs(self.env.iter().cloned())
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_ATTR_NOSYSTEM", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .current_dir(checkout)
            .args([
                "-c",
                "core.hooksPath=/dev/null",
                "-c",
                "core.fsmonitor=false",
                "-c",
                "core.excludesFile=/dev/null",
                "-c",
                "core.attributesFile=/dev/null",
                "-c",
                "core.autocrlf=false",
            ]);
        command
    }
}

/// Each of `checkouts`, which must each be one of `roots`, snapshotted
/// with `git`; `before`, empty or a tree for each in its order, gives
/// what changed.
pub fn take(
    git: &Git,
    checkouts: &[String],
    before: &[String],
    roots: &[PathBuf],
) -> Result<Vec<Taken>, String> {
    if !git.path.is_absolute() {
        return Err(format!("{} is not an absolute path", git.path.display()));
    }
    if !before.is_empty() && before.len() != checkouts.len() {
        return Err("a snapshot's trees before do not match its worktrees".into());
    }
    if !before.iter().all(|tree| crate::git::object_id(tree)) {
        return Err("a snapshot's tree before is no object id".into());
    }
    let mut budget = MAX_NAMED_ALL;
    checkouts
        .iter()
        .enumerate()
        .map(|(at, checkout)| {
            let path = Path::new(checkout);
            if !roots.iter().any(|root| root == path) {
                return Err(format!("{checkout} is not a worktree here"));
            }
            one(git, path, before.get(at).map(String::as_str), &mut budget)
        })
        .collect()
}

fn one(
    git: &Git,
    checkout: &Path,
    before: Option<&str>,
    budget: &mut usize,
) -> Result<Taken, String> {
    let (id, gitdir) = linked(git, checkout)?;
    let tree = current(git, checkout, &gitdir)?;
    keep(git, checkout, &id, &tree)?;
    let (changed, more) = match before {
        Some(before) if before != tree => changed(git, checkout, before, &tree, budget)?,
        _ => (Vec::new(), 0),
    };
    Ok(Taken {
        checkout: checkout.display().to_string(),
        tree,
        changed,
        more,
    })
}

/// Each of `checkouts` brought from tree `from` to tree `to`, in their
/// order (DESIGN.md §12, undo and redo). Before any is written, each
/// must be at its `from`, so nothing changed since is overwritten, or at
/// its `to` already, done by an earlier try; no submodule may differ;
/// and nothing a tree cannot hold, an ignored file, may be in the way of
/// what is written. Then in each the files `to` lacks are removed and the
/// rest that differ written from it, the result checked to be `to` and
/// kept on its ref. What changed is answered as a snapshot's; a failure
/// names its worktree and those restored before it.
pub fn restore(
    git: &Git,
    checkouts: &[String],
    from: &[String],
    to: &[String],
    roots: &[PathBuf],
) -> Result<Vec<Taken>, String> {
    if !git.path.is_absolute() {
        return Err(format!("{} is not an absolute path", git.path.display()));
    }
    if from.len() != checkouts.len() || to.len() != checkouts.len() {
        return Err("a restore's trees do not match its worktrees".into());
    }
    if !from
        .iter()
        .chain(to)
        .all(|tree| crate::git::object_id(tree))
    {
        return Err("a restore's tree is no object id".into());
    }
    let mut found = Vec::new();
    for ((checkout, from), to) in checkouts.iter().zip(from).zip(to) {
        let path = Path::new(checkout);
        if !roots.iter().any(|root| root == path) {
            return Err(format!("{checkout} is not a worktree here"));
        }
        let checked = |why: String| format!("{checkout}: {why}");
        let (id, gitdir) = linked(git, path).map_err(checked)?;
        let now = current(git, path, &gitdir).map_err(checked)?;
        let plan = if now == *to {
            None
        } else if now == *from {
            let plan = plan(git, path, from, to).map_err(checked)?;
            clear(git, path, &plan).map_err(checked)?;
            Some(plan)
        } else {
            return Err(format!("{checkout} has changed since that step"));
        };
        found.push((path, id, gitdir, plan));
    }
    let mut budget = MAX_NAMED_ALL;
    let mut restored: Vec<String> = Vec::new();
    let mut taken = Vec::new();
    for ((checkout, id, gitdir, plan), (from, to)) in found.into_iter().zip(from.iter().zip(to)) {
        let mut one = || -> Result<Taken, String> {
            if let Some(plan) = &plan {
                apply(git, checkout, to, plan)?;
                if current(git, checkout, &gitdir)? != *to {
                    return Err("it did not come back to that step's tree".into());
                }
            }
            keep(git, checkout, &id, to)?;
            let (changed, more) = if from == to {
                (Vec::new(), 0)
            } else {
                changed(git, checkout, from, to, &mut budget)?
            };
            Ok(Taken {
                checkout: checkout.display().to_string(),
                tree: to.clone(),
                changed,
                more,
            })
        };
        match one() {
            Ok(one) => {
                restored.push(one.checkout.clone());
                taken.push(one);
            }
            Err(why) => {
                let before = if restored.is_empty() {
                    String::new()
                } else {
                    format!("; restored already: {}", restored.join(", "))
                };
                return Err(format!("{}: {why}{before}", checkout.display()));
            }
        }
    }
    Ok(taken)
}

/// What bringing a worktree from one tree to another does: the paths
/// removed, and those written, each ended by a NUL, which include the
/// ones added, each with the object it is to be.
struct Plan {
    gone: Vec<PathBuf>,
    added: Vec<(PathBuf, String)>,
    wanted: Vec<u8>,
}

/// The files that differ between trees `from` and `to`, as a plan; a
/// submodule among them is refused, since git writes none.
fn plan(git: &Git, checkout: &Path, from: &str, to: &str) -> Result<Plan, String> {
    let listed =
        ran(git
            .at(checkout)
            .args(["diff-tree", "-r", "-z", "--no-renames", "--raw", from, to]))?;
    let mut fields = listed.split(|&b| b == 0).filter(|field| !field.is_empty());
    let mut plan = Plan {
        gone: Vec::new(),
        added: Vec::new(),
        wanted: Vec::new(),
    };
    while let Some(meta) = fields.next() {
        let path = fields.next().ok_or("git: a change without its path")?;
        let relative = relative(path)?;
        // `:<mode> <mode> <id> <id> <status>`
        let meta: Vec<&[u8]> = meta
            .strip_prefix(b":")
            .ok_or("git: a change it did not describe")?
            .split(|&b| b == b' ')
            .collect();
        if meta.iter().take(2).any(|mode| *mode == b"160000") {
            return Err(format!(
                "{} is a submodule, which td-agent does not restore",
                relative.display()
            ));
        }
        match meta.get(4).and_then(|status| status.first()) {
            Some(b'D') => plan.gone.push(relative),
            Some(b'A') => {
                let id = meta
                    .get(3)
                    .map(|id| String::from_utf8_lossy(id).into_owned())
                    .filter(|id| crate::git::object_id(id))
                    .ok_or("git: an addition of no object")?;
                plan.added.push((relative, id));
                plan.wanted.extend(path.iter().copied().chain([0]));
            }
            Some(b'M' | b'T') => plan.wanted.extend(path.iter().copied().chain([0])),
            _ => return Err("git: a change of no kind it should give".into()),
        }
    }
    Ok(plan)
}

/// Refused if anything no tree holds, an ignored file, is where `plan`
/// writes: at a path it adds, unless a file of the very bytes it writes
/// there or a directory holding only what it removes, or on the way to
/// one, unless a directory or what it removes.
fn clear(git: &Git, checkout: &Path, plan: &Plan) -> Result<(), String> {
    let gone: std::collections::HashSet<&Path> = plan.gone.iter().map(PathBuf::as_path).collect();
    let in_the_way = |path: &Path| format!("{} is in the way and in no snapshot", path.display());
    for (added, id) in &plan.added {
        let mut at = PathBuf::new();
        let mut reached = true;
        if let Some(parent) = added.parent() {
            for name in parent.components() {
                at.push(name);
                match std::fs::symlink_metadata(checkout.join(&at)) {
                    Ok(meta) if meta.is_dir() => {}
                    Ok(_) if gone.contains(at.as_path()) => {
                        reached = false;
                        break;
                    }
                    Ok(_) => return Err(in_the_way(&at)),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                        reached = false;
                        break;
                    }
                    Err(e) => return Err(format!("{}: {e}", at.display())),
                }
            }
        }
        if !reached {
            continue;
        }
        match std::fs::symlink_metadata(checkout.join(added)) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Ok(meta) if meta.is_dir() => only_gone(checkout, added, &gone)?,
            Ok(meta) if meta.is_file() => {
                let held = said(
                    git.at(checkout)
                        .args(["hash-object", "--no-filters", "--"])
                        .arg(added),
                )?;
                if held != *id {
                    return Err(in_the_way(added));
                }
            }
            Ok(_) => return Err(in_the_way(added)),
            Err(e) => return Err(format!("{}: {e}", added.display())),
        }
    }
    Ok(())
}

/// Refused unless directory `dir` of `checkout` holds, at any depth, no
/// file but those in `gone`; links are not followed.
fn only_gone(
    checkout: &Path,
    dir: &Path,
    gone: &std::collections::HashSet<&Path>,
) -> Result<(), String> {
    let mut left = vec![dir.to_path_buf()];
    while let Some(dir) = left.pop() {
        let entries = std::fs::read_dir(checkout.join(&dir))
            .map_err(|e| format!("{}: {e}", dir.display()))?;
        for entry in entries {
            let entry = entry.map_err(|e| format!("{}: {e}", dir.display()))?;
            let path = dir.join(entry.file_name());
            let kind = entry
                .file_type()
                .map_err(|e| format!("{}: {e}", path.display()))?;
            if kind.is_dir() {
                left.push(path);
            } else if !gone.contains(path.as_path()) {
                return Err(format!(
                    "{} is in the way and in no snapshot",
                    path.display()
                ));
            }
        }
    }
    Ok(())
}

/// `plan` carried out in `checkout`: the files it removes removed, through
/// no link, and the directories they leave empty; those it writes
/// written from tree `to` by git.
fn apply(git: &Git, checkout: &Path, to: &str, plan: &Plan) -> Result<(), String> {
    for path in &plan.gone {
        remove(checkout, path)?;
    }
    for path in &plan.gone {
        prune(checkout, path);
    }
    if plan.wanted.is_empty() {
        return Ok(());
    }
    let index = private_index()?;
    let list = index.with_extension("wanted");
    let written = write_out(git, checkout, to, &index, &list, &plan.wanted);
    let _ = std::fs::remove_file(&index);
    let _ = std::fs::remove_file(&list);
    written
}

/// `wanted`, paths each ended by a NUL, written into `checkout` from tree
/// `to` through `index`, a private one, as their blobs' bytes.
fn write_out(
    git: &Git,
    checkout: &Path,
    to: &str,
    index: &Path,
    list: &Path,
    wanted: &[u8],
) -> Result<(), String> {
    let empty = empty_tree(git, checkout)?;
    ran(git
        .at(checkout)
        .env("GIT_INDEX_FILE", index)
        .args(["read-tree", to]))?;
    std::fs::write(list, wanted).map_err(|e| format!("{}: {e}", list.display()))?;
    let input = std::fs::File::open(list).map_err(|e| format!("{}: {e}", list.display()))?;
    ran(git
        .at(checkout)
        .env("GIT_INDEX_FILE", index)
        .env("GIT_ATTR_SOURCE", &empty)
        .stdin(input)
        .args(["checkout-index", "-f", "-z", "--stdin"]))?;
    Ok(())
}

/// `path`, a tree's, as a relative path of plain names: none empty,
/// `.`, `..` or `.git`.
fn relative(path: &[u8]) -> Result<PathBuf, String> {
    use std::os::unix::ffi::OsStrExt;
    let plain = !path.is_empty()
        && path.split(|&b| b == b'/').all(|name| {
            !name.is_empty() && name != b"." && name != b".." && !name.eq_ignore_ascii_case(b".git")
        });
    if !plain {
        return Err(format!(
            "git named a path {:?}",
            crate::tools::visible(&String::from_utf8_lossy(path))
        ));
    }
    Ok(PathBuf::from(std::ffi::OsStr::from_bytes(path)))
}

/// `path` in `checkout` removed, a file or a link, never followed: each
/// directory on the way must be one, not a link to one.
fn remove(checkout: &Path, path: &Path) -> Result<(), String> {
    let mut at = checkout.to_path_buf();
    if let Some(parent) = path.parent() {
        for name in parent.components() {
            at.push(name);
            match std::fs::symlink_metadata(&at) {
                Ok(meta) if meta.is_dir() => {}
                Ok(_) => return Err(format!("{} is not a directory", at.display())),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                Err(e) => return Err(format!("{}: {e}", at.display())),
            }
        }
    }
    let at = checkout.join(path);
    match std::fs::symlink_metadata(&at) {
        Ok(meta) if meta.is_dir() => Err(format!("{} is a directory", at.display())),
        Ok(_) => std::fs::remove_file(&at).map_err(|e| format!("{}: {e}", at.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("{}: {e}", at.display())),
    }
}

/// The directories above `path` in `checkout` it left empty, removed.
fn prune(checkout: &Path, path: &Path) {
    let mut at = path.parent();
    while let Some(dir) = at.filter(|dir| !dir.as_os_str().is_empty()) {
        if std::fs::remove_dir(checkout.join(dir)).is_err() {
            return;
        }
        at = dir.parent();
    }
}

/// `checkout`'s id among its repository's worktrees, and its git
/// directory; refused for any but a linked worktree.
fn linked(git: &Git, checkout: &Path) -> Result<(String, PathBuf), String> {
    let gitdir = PathBuf::from(said(
        git.at(checkout).args(["rev-parse", "--absolute-git-dir"]),
    )?);
    let id = gitdir
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|_| {
            gitdir
                .parent()
                .is_some_and(|parent| parent.file_name() == Some("worktrees".as_ref()))
        })
        .ok_or_else(|| format!("{} is not a linked worktree", checkout.display()))?
        .to_string();
    Ok((id, gitdir))
}

/// `checkout`'s tree as it is now, written with a private index, so the
/// worktree's own, and what is staged in it, are left as they are.
fn current(git: &Git, checkout: &Path, gitdir: &Path) -> Result<String, String> {
    let index = private_index()?;
    let made = make_tree(git, checkout, gitdir, &index);
    let _ = std::fs::remove_file(&index);
    made
}

/// A fresh name for a private index in the instance's temporary
/// directory.
fn private_index() -> Result<PathBuf, String> {
    Ok(std::env::temp_dir().join(format!(
        "td-agent-snapshot-{}.index",
        crate::store::random_hex(8).map_err(|e| format!("an index name: {e}"))?
    )))
}

/// The empty tree's id in `checkout`'s repository, whatever its hash.
fn empty_tree(git: &Git, checkout: &Path) -> Result<String, String> {
    said(
        git.at(checkout)
            .args(["hash-object", "-t", "tree", "/dev/null"]),
    )
}

fn make_tree(git: &Git, checkout: &Path, gitdir: &Path, index: &Path) -> Result<String, String> {
    // A copy of the worktree's own, its time kept with it, keeps its stat
    // cache, so a tracked file unchanged since is not read again, and its
    // sparse checkout, so what lies outside it is not taken as deleted.
    let own = gitdir.join("index");
    if own.is_file() {
        std::fs::copy(&own, index).map_err(|e| format!("{}: {e}", own.display()))?;
        let modified = std::fs::metadata(&own)
            .and_then(|meta| meta.modified())
            .map_err(|e| format!("{}: {e}", own.display()))?;
        std::fs::File::options()
            .write(true)
            .open(index)
            .and_then(|file| file.set_modified(modified))
            .map_err(|e| format!("{}: {e}", index.display()))?;
    }
    // Attributes from no tree, an empty one, so each blob is its file's
    // bytes, whatever a `.gitattributes` there says.
    let empty = empty_tree(git, checkout)?;
    let staged = |command: &mut Command| {
        command
            .env("GIT_INDEX_FILE", index)
            .env("GIT_ATTR_SOURCE", &empty);
    };
    // A file marked assume-unchanged is checked like any other, so a
    // change under the mark is taken; a skip-worktree mark, the sparse
    // checkout's, stays.
    let mut listed = git.at(checkout);
    staged(&mut listed);
    let listed = ran(listed.args(["ls-files", "-z", "-v"]))?;
    let marked: Vec<u8> = listed
        .split(|&b| b == 0)
        .filter(|entry| entry.first().is_some_and(u8::is_ascii_lowercase))
        .filter_map(|entry| entry.get(2..))
        .flat_map(|path| path.iter().copied().chain([0]))
        .collect();
    if !marked.is_empty() {
        // Fed from a file, so no pipe waits on the other.
        let list = index.with_extension("marked");
        let unmarked = std::fs::write(&list, &marked)
            .and_then(|()| std::fs::File::open(&list))
            .map_err(|e| format!("{}: {e}", list.display()))
            .and_then(|input| {
                let mut unmark = git.at(checkout);
                staged(&mut unmark);
                ran(unmark.stdin(input).args([
                    "update-index",
                    "-z",
                    "--no-assume-unchanged",
                    "--stdin",
                ]))
            });
        let _ = std::fs::remove_file(&list);
        unmarked?;
    }
    // `--sparse`, so a file made outside the sparse checkout is taken too.
    let mut add = git.at(checkout);
    staged(&mut add);
    said(add.args(["add", "-A", "--sparse"]))?;
    let mut write = git.at(checkout);
    staged(&mut write);
    let tree = said(write.arg("write-tree"))?;
    crate::git::object_id(&tree)
        .then_some(tree)
        .ok_or_else(|| "git wrote no tree".to_string())
}

/// `tree` committed onto the worktree's snapshot ref, unless the ref's
/// commit already holds it.
fn keep(git: &Git, checkout: &Path, id: &str, tree: &str) -> Result<(), String> {
    let name = format!("{REF}{id}");
    // What the ref holds, which the update must find there, and the
    // commit it is, if it is one: anything else is replaced.
    let held = said(
        git.at(checkout)
            .args(["rev-parse", "--verify", "--quiet", &name]),
    )
    .ok();
    let parent = held.as_ref().and_then(|_| {
        said(git.at(checkout).args([
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("{name}^{{commit}}"),
        ]))
        .ok()
    });
    if let Some(parent) = &parent {
        let held = said(
            git.at(checkout)
                .args(["rev-parse", &format!("{parent}^{{tree}}")]),
        )?;
        if held == tree {
            return Ok(());
        }
    }
    let mut commit = git.at(checkout);
    commit.args(["commit-tree", tree, "-m", "td-agent step snapshot"]);
    if let Some(parent) = &parent {
        commit.args(["-p", parent]);
    }
    for (name, value) in [
        ("GIT_AUTHOR_NAME", "td-agent"),
        ("GIT_AUTHOR_EMAIL", "td-agent@localhost"),
        ("GIT_COMMITTER_NAME", "td-agent"),
        ("GIT_COMMITTER_EMAIL", "td-agent@localhost"),
    ] {
        commit.env(name, value);
    }
    let commit = said(&mut commit)?;
    // Only over what it held, or nothing: a ref moved meanwhile fails.
    said(
        git.at(checkout)
            .args(["update-ref", &name, &commit, held.as_deref().unwrap_or("")]),
    )?;
    Ok(())
}

/// The files that differ between trees `before` and `after`, as many as
/// the bounds and what is left of `budget` allow, and how many more.
fn changed(
    git: &Git,
    checkout: &Path,
    before: &str,
    after: &str,
    budget: &mut usize,
) -> Result<(Vec<String>, u64), String> {
    let output = ran(git.at(checkout).args([
        "diff-tree",
        "-r",
        "-z",
        "--name-only",
        "--no-renames",
        before,
        after,
    ]))?;
    Ok(named(
        output.split(|&b| b == 0).filter(|name| !name.is_empty()),
        budget,
    ))
}

/// `names`, each cut to `MAX_NAME`, as many as `MAX_CHANGED`, `MAX_NAMED`
/// and what is left of `budget` allow, and how many more.
fn named<'a>(names: impl Iterator<Item = &'a [u8]>, budget: &mut usize) -> (Vec<String>, u64) {
    let mut named = Vec::new();
    let mut used = 0;
    let mut more = 0u64;
    for name in names {
        let name: String = String::from_utf8_lossy(name)
            .char_indices()
            .take_while(|(at, c)| at + c.len_utf8() <= MAX_NAME)
            .map(|(_, c)| c)
            .collect();
        let cost = escaped(&name);
        if named.len() < MAX_CHANGED && used + cost <= MAX_NAMED && cost <= *budget {
            used += cost;
            *budget -= cost;
            named.push(name);
        } else {
            more += 1;
        }
    }
    (named, more)
}

/// The most `name` takes as JSON escapes it: six bytes for a control.
fn escaped(name: &str) -> usize {
    name.chars()
        .map(|c| match c {
            '"' | '\\' => 2,
            c if c.is_control() => 6,
            c => c.len_utf8(),
        })
        .sum::<usize>()
        + 3
}

/// What `command` printed, or why it failed.
fn ran(command: &mut Command) -> Result<Vec<u8>, String> {
    let output = command.output().map_err(|e| format!("git: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "git: {}",
            crate::tools::visible(String::from_utf8_lossy(&output.stderr).trim())
        ));
    }
    Ok(output.stdout)
}

/// What `command` printed, trimmed, or why it failed.
fn said(command: &mut Command) -> Result<String, String> {
    Ok(String::from_utf8_lossy(&ran(command)?).trim().to_string())
}

/// `taken` as the tool host answers it.
pub fn encode(taken: &[Taken]) -> String {
    Json::Arr(
        taken
            .iter()
            .map(|one| {
                Json::Obj(vec![
                    ("checkout".into(), Json::Str(one.checkout.clone())),
                    ("tree".into(), Json::Str(one.tree.clone())),
                    (
                        "changed".into(),
                        Json::Arr(one.changed.iter().cloned().map(Json::Str).collect()),
                    ),
                    ("more".into(), Json::from(one.more)),
                ])
            })
            .collect(),
    )
    .to_string()
}

/// The tool host's answer for `checkouts`, in their order, checked: each
/// tree an object id, each list within its bounds.
pub fn decode(text: &str, checkouts: &[String]) -> Result<Vec<Taken>, String> {
    let value = td_json::parse(text).map_err(|e| format!("a snapshot's answer: {e}"))?;
    let items = value.as_arr().ok_or("a snapshot's answer is not a list")?;
    let mut budget = MAX_NAMED_ALL;
    if items.len() != checkouts.len() {
        return Err("a snapshot's answer names other worktrees".into());
    }
    items
        .iter()
        .zip(checkouts)
        .map(move |(item, checkout)| {
            let text = |name: &str| {
                item.get(name)
                    .and_then(Json::as_str)
                    .map(String::from)
                    .ok_or_else(|| format!("a snapshot with no {name}"))
            };
            if text("checkout")? != *checkout {
                return Err("a snapshot's answer names other worktrees".into());
            }
            let tree = text("tree")?;
            if !crate::git::object_id(&tree) {
                return Err("a snapshot's tree is no object id".into());
            }
            let changed: Vec<String> = item
                .get("changed")
                .and_then(Json::as_arr)
                .ok_or("a snapshot with no changed files")?
                .iter()
                .map(|name| {
                    name.as_str()
                        .filter(|name| name.len() <= MAX_NAME)
                        .map(String::from)
                        .ok_or("a snapshot's changed file is not a name")
                })
                .collect::<Result<_, _>>()?;
            let cost: usize = changed.iter().map(|name| escaped(name)).sum();
            if changed.len() > MAX_CHANGED || cost > MAX_NAMED || cost > budget {
                return Err("a snapshot names too many changed files".into());
            }
            budget -= cost;
            Ok(Taken {
                checkout: checkout.clone(),
                tree,
                changed,
                more: item
                    .get("more")
                    .and_then(Json::as_u64)
                    .ok_or("a snapshot with no count")?,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
    use super::*;
    use crate::store::tests::Scratch;

    fn git() -> Git {
        Git {
            path: crate::repo::host_git().unwrap(),
            env: crate::shell::environment(),
        }
    }

    fn run(dir: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .args(["-c", "user.name=t", "-c", "user.email=t@example.org"])
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    #[test]
    fn a_worktree_is_kept_as_a_tree_and_its_changes_named() {
        if !crate::git::tests::have_git() {
            return;
        }
        let scratch = Scratch::new("snapshot");
        let main = scratch.0.join("main");
        std::fs::create_dir_all(&main).unwrap();
        run(&main, &["init", "-q", "-b", "main"]);
        std::fs::write(main.join("kept"), "a\n").unwrap();
        std::fs::write(main.join("gone"), "b\n").unwrap();
        std::fs::write(main.join(".gitignore"), "target/\n").unwrap();
        run(&main, &["add", "-A"]);
        run(&main, &["commit", "-q", "-m", "one"]);
        let checkout = scratch.0.join("tree");
        run(
            &main,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "agent",
                checkout.to_str().unwrap(),
            ],
        );
        let checkout = std::fs::canonicalize(&checkout).unwrap();
        let name = checkout.display().to_string();
        let roots = [checkout.clone()];
        // A file staged in the worktree's own index stays staged.
        std::fs::write(checkout.join("kept"), "staged\n").unwrap();
        run(&checkout, &["add", "kept"]);
        let staged = run(&checkout, &["diff", "--cached", "--name-only"]);
        let before = take(&git(), std::slice::from_ref(&name), &[], &roots).unwrap();
        let first = &before[0];
        assert!(crate::git::object_id(&first.tree));
        assert!(first.changed.is_empty());
        let kept = run(&checkout, &["rev-parse", "refs/td-agent/snapshots/tree"]);
        assert_eq!(
            run(&checkout, &["rev-parse", &format!("{kept}^{{tree}}")]),
            first.tree
        );
        // A step edits one file, deletes one, adds one, and builds.
        std::fs::write(checkout.join("kept"), "edited\n").unwrap();
        std::fs::remove_file(checkout.join("gone")).unwrap();
        std::fs::write(checkout.join("new"), "c\n").unwrap();
        std::fs::create_dir(checkout.join("target")).unwrap();
        std::fs::write(checkout.join("target/out"), "x\n").unwrap();
        let after = take(
            &git(),
            std::slice::from_ref(&name),
            std::slice::from_ref(&first.tree),
            &roots,
        )
        .unwrap();
        let mut changed = after[0].changed.clone();
        changed.sort();
        assert_eq!(changed, ["gone", "kept", "new"]);
        assert_eq!(after[0].more, 0);
        let next = run(&checkout, &["rev-parse", "refs/td-agent/snapshots/tree"]);
        assert_eq!(run(&checkout, &["rev-parse", &format!("{next}^")]), kept);
        // The worktree's own index is as it was.
        assert_eq!(run(&checkout, &["diff", "--cached", "--name-only"]), staged);
        // Nothing changed: the same tree, no new commit.
        let again = take(
            &git(),
            std::slice::from_ref(&name),
            &[after[0].tree.clone()],
            &roots,
        )
        .unwrap();
        assert_eq!(again[0].tree, after[0].tree);
        assert!(again[0].changed.is_empty());
        assert_eq!(
            run(&checkout, &["rev-parse", "refs/td-agent/snapshots/tree"]),
            next
        );
        // The answer crosses whole, and one for other worktrees is refused.
        let text = encode(&after);
        assert_eq!(decode(&text, std::slice::from_ref(&name)).unwrap(), after);
        assert!(decode(&text, &["/elsewhere".into()]).is_err());
        assert!(decode(&text, &[]).is_err());
        // Only a worktree here, a tree id before, and as many as worktrees.
        assert!(take(&git(), &["/elsewhere".into()], &[], &roots).is_err());
        assert!(take(
            &git(),
            std::slice::from_ref(&name),
            &["HEAD".into()],
            &roots
        )
        .is_err());
        assert!(take(
            &git(),
            std::slice::from_ref(&name),
            &[first.tree.clone(), first.tree.clone()],
            &roots
        )
        .is_err());
        // Git by an absolute path only.
        assert!(take(
            &Git {
                path: "git".into(),
                env: Vec::new()
            },
            std::slice::from_ref(&name),
            &[],
            &roots
        )
        .is_err());
        // A repository's own checkout is no linked worktree.
        assert!(take(
            &git(),
            &[main.display().to_string()],
            &[],
            std::slice::from_ref(&main)
        )
        .is_err());
    }

    #[test]
    fn many_changes_are_counted_past_the_bound() {
        if !crate::git::tests::have_git() {
            return;
        }
        let scratch = Scratch::new("snapshot-many");
        let main = scratch.0.join("main");
        std::fs::create_dir_all(&main).unwrap();
        run(&main, &["init", "-q", "-b", "main"]);
        std::fs::write(main.join("a"), "a\n").unwrap();
        run(&main, &["add", "-A"]);
        run(&main, &["commit", "-q", "-m", "one"]);
        let checkout = scratch.0.join("tree");
        run(
            &main,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "agent",
                checkout.to_str().unwrap(),
            ],
        );
        let checkout = std::fs::canonicalize(&checkout).unwrap();
        let name = checkout.display().to_string();
        let roots = [checkout.clone()];
        let before = take(&git(), std::slice::from_ref(&name), &[], &roots).unwrap();
        for n in 0..MAX_CHANGED + 5 {
            std::fs::write(checkout.join(format!("f{n}")), "x\n").unwrap();
        }
        let after = take(
            &git(),
            std::slice::from_ref(&name),
            &[before[0].tree.clone()],
            &roots,
        )
        .unwrap();
        assert_eq!(after[0].changed.len(), MAX_CHANGED);
        assert_eq!(after[0].more, 5);
    }

    /// A repository with `files` committed and a linked worktree of it,
    /// canonical: the worktree's path and its name.
    fn worktree(scratch: &Scratch, files: &[(&str, &str)]) -> (PathBuf, String) {
        let main = scratch.0.join("main");
        std::fs::create_dir_all(&main).unwrap();
        run(&main, &["init", "-q", "-b", "main"]);
        for (path, text) in files {
            let at = main.join(path);
            std::fs::create_dir_all(at.parent().unwrap()).unwrap();
            std::fs::write(at, text).unwrap();
        }
        run(&main, &["add", "-A"]);
        run(&main, &["commit", "-q", "-m", "one"]);
        let checkout = scratch.0.join("tree");
        run(
            &main,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "agent",
                checkout.to_str().unwrap(),
            ],
        );
        let checkout = std::fs::canonicalize(&checkout).unwrap();
        let name = checkout.display().to_string();
        (checkout, name)
    }

    /// The files changed between two snapshots, sorted.
    fn between(git: &Git, checkout: &Path, name: &str, step: impl FnOnce()) -> Vec<String> {
        let roots = [checkout.to_path_buf()];
        let before = take(git, &[name.to_string()], &[], &roots).unwrap();
        step();
        let after = take(git, &[name.to_string()], &[before[0].tree.clone()], &roots).unwrap();
        let mut changed = after[0].changed.clone();
        changed.sort();
        changed
    }

    #[test]
    fn a_file_made_outside_a_sparse_checkout_is_taken_and_the_rest_kept() {
        if !crate::git::tests::have_git() {
            return;
        }
        let scratch = Scratch::new("snapshot-sparse");
        let (checkout, name) = worktree(&scratch, &[("a/x", "x\n"), ("b/y", "y\n")]);
        run(&checkout, &["sparse-checkout", "set", "a"]);
        assert!(!checkout.join("b/y").exists());
        let changed = between(&git(), &checkout, &name, || {
            std::fs::write(checkout.join("a/z"), "z\n").unwrap();
            std::fs::create_dir(checkout.join("c")).unwrap();
            std::fs::write(checkout.join("c/new"), "n\n").unwrap();
        });
        assert_eq!(changed, ["a/z", "c/new"]);
    }

    #[test]
    fn what_else_the_ref_holds_is_replaced() {
        if !crate::git::tests::have_git() {
            return;
        }
        let scratch = Scratch::new("snapshot-planted");
        let (checkout, name) = worktree(&scratch, &[("a", "a\n")]);
        let main = scratch.0.join("main");
        let refname = format!("{REF}tree");
        let blob = run(&main, &["hash-object", "-w", "a"]);
        let dangling = "1".repeat(blob.trim().len());
        for (n, planted) in [blob.trim(), dangling.as_str()].into_iter().enumerate() {
            std::fs::create_dir_all(main.join(".git").join(REF)).unwrap();
            std::fs::write(main.join(".git").join(&refname), format!("{planted}\n")).unwrap();
            std::fs::write(checkout.join("a"), format!("{n}\n")).unwrap();
            let taken = take(
                &git(),
                std::slice::from_ref(&name),
                &[],
                std::slice::from_ref(&checkout),
            )
            .unwrap();
            assert_eq!(
                run(&main, &["rev-parse", &format!("{refname}^{{tree}}")]).trim(),
                taken[0].tree
            );
        }
    }

    #[test]
    fn nothing_in_the_home_or_the_tree_shapes_the_record() {
        if !crate::git::tests::have_git() {
            return;
        }
        let scratch = Scratch::new("snapshot-home");
        let (checkout, name) = worktree(
            &scratch,
            &[(".gitattributes", "* text=auto\n"), ("kept", "k\n")],
        );
        // A home that would ignore everything and convert every file.
        let home = scratch.0.join("home");
        std::fs::create_dir_all(home.join(".config/git")).unwrap();
        std::fs::write(home.join(".config/git/ignore"), "*\n").unwrap();
        std::fs::write(home.join(".config/git/attributes"), "* eol=crlf\n").unwrap();
        std::fs::write(
            home.join(".gitconfig"),
            "[core]\n\texcludesFile = ~/.config/git/ignore\n",
        )
        .unwrap();
        let mut env: Vec<(OsString, OsString)> = crate::shell::environment()
            .into_iter()
            .filter(|(name, _)| name != "HOME")
            .collect();
        env.push(("HOME".into(), home.into_os_string()));
        let git = Git {
            path: crate::repo::host_git().unwrap(),
            env,
        };
        // Nor does a repository's own line-ending conversion.
        run(&checkout, &["config", "core.autocrlf", "true"]);
        // An edit under an assume-unchanged mark is taken too.
        run(&checkout, &["update-index", "--assume-unchanged", "kept"]);
        let changed = between(&git, &checkout, &name, || {
            std::fs::write(checkout.join("new"), "a\r\nb\r\n").unwrap();
            std::fs::write(checkout.join("kept"), "edited\n").unwrap();
        });
        assert_eq!(changed, ["kept", "new"]);
        // The file's bytes, not the tree's `text=auto`.
        let tree = take(
            &git,
            std::slice::from_ref(&name),
            &[],
            std::slice::from_ref(&checkout),
        )
        .unwrap();
        assert_eq!(
            run(
                &checkout,
                &["cat-file", "-p", &format!("{}:new", tree[0].tree)]
            ),
            "a\r\nb"
        );
        // The worktree's own mark is left as it was.
        assert!(run(&checkout, &["ls-files", "-v", "kept"]).starts_with('h'));
    }

    #[test]
    fn every_worktree_together_is_named_within_one_budget() {
        let name = "\u{1}".repeat(MAX_NAME);
        let one = Taken {
            checkout: "/w".into(),
            tree: "a".repeat(40),
            changed: vec![name.clone(); MAX_CHANGED],
            more: 0,
        };
        // Past the bounds, an answer is refused.
        assert!(decode(&encode(std::slice::from_ref(&one)), &["/w".into()]).is_err());
        let fits = MAX_NAMED / escaped(&name);
        let one = Taken {
            changed: vec![name.clone(); fits],
            ..one
        };
        assert!(decode(&encode(std::slice::from_ref(&one)), &["/w".into()]).is_ok());
        let many = MAX_NAMED_ALL / MAX_NAMED + 1;
        let checkouts: Vec<String> = (0..many).map(|n| format!("/w{n}")).collect();
        let all: Vec<Taken> = checkouts
            .iter()
            .map(|checkout| Taken {
                checkout: checkout.clone(),
                ..one.clone()
            })
            .collect();
        assert!(decode(&encode(&all), &checkouts).is_err());
        // Escaped twice, as the tool host answers, the most fits a frame.
        let most = &all[..many - 1];
        let answer = Json::Str(encode(most)).to_string();
        assert!(answer.len() < crate::frame::MAX_FRAME, "{}", answer.len());
    }

    #[test]
    fn naming_stops_at_each_bound_and_counts_the_rest() {
        let control = vec![1u8; MAX_NAME + 50];
        let cost = escaped(&"\u{1}".repeat(MAX_NAME));
        // Cut to its bound, then within the worktree's.
        let mut budget = MAX_NAMED_ALL;
        let (kept, more) = named(std::iter::repeat_n(control.as_slice(), 100), &mut budget);
        assert_eq!(kept.len(), MAX_NAMED / cost);
        assert!(kept.iter().all(|name| name.len() == MAX_NAME));
        assert_eq!(more, 100 - kept.len() as u64);
        assert_eq!(budget, MAX_NAMED_ALL - kept.len() * cost);
        // Within what is left of every worktree's.
        let mut budget = cost * 3 + 1;
        let (kept, more) = named(std::iter::repeat_n(control.as_slice(), 10), &mut budget);
        assert_eq!((kept.len(), more, budget), (3, 7, 1));
        // And at most so many names, however short.
        let mut budget = MAX_NAMED_ALL;
        let (kept, more) = named(std::iter::repeat_n(b"a".as_slice(), 50), &mut budget);
        assert_eq!((kept.len(), more), (MAX_CHANGED, 10));
    }

    #[test]
    fn a_step_is_undone_and_redone_and_nothing_changed_since_is_overwritten() {
        use std::os::unix::fs::PermissionsExt;
        if !crate::git::tests::have_git() {
            return;
        }
        let scratch = Scratch::new("snapshot-restore");
        let (checkout, name) = worktree(
            &scratch,
            &[
                ("a", "one\n"),
                ("d/x", "x\n"),
                ("run", "#!/bin/sh\n"),
                (".gitignore", "target/\n"),
            ],
        );
        std::os::unix::fs::symlink("a", checkout.join("link")).unwrap();
        std::fs::create_dir(checkout.join("target")).unwrap();
        std::fs::write(checkout.join("target/kept"), "built\n").unwrap();
        // Attributes that would write a file other than its bytes.
        std::fs::write(checkout.join(".gitattributes"), "lf text eol=crlf\n").unwrap();
        std::fs::write(checkout.join("lf"), "x\n").unwrap();
        let git = git();
        let roots = [checkout.clone()];
        let names = [name.clone()];
        let before = take(&git, &names, &[], &roots).unwrap()[0].tree.clone();
        // The step: an edit, a deletion emptying a directory, an addition
        // in a new one, a mode, a link turned into a file.
        std::fs::write(checkout.join("a"), "two\n").unwrap();
        std::fs::remove_file(checkout.join("d/x")).unwrap();
        std::fs::remove_dir(checkout.join("d")).unwrap();
        std::fs::create_dir(checkout.join("new")).unwrap();
        std::fs::write(checkout.join("new/n"), "n\n").unwrap();
        std::fs::set_permissions(checkout.join("run"), std::fs::Permissions::from_mode(0o755))
            .unwrap();
        std::fs::remove_file(checkout.join("link")).unwrap();
        std::fs::write(checkout.join("link"), "plain\n").unwrap();
        std::fs::remove_file(checkout.join("lf")).unwrap();
        let after = take(&git, &names, std::slice::from_ref(&before), &roots).unwrap()[0]
            .tree
            .clone();
        let back = |from: &str, to: &str| {
            restore(&git, &names, &[from.to_string()], &[to.to_string()], &roots)
        };
        // Undone: each file as it was, the new directory gone.
        let undone = back(&after, &before).unwrap();
        assert_eq!(undone[0].tree, before);
        // Asked again, it is done already, and nothing changes.
        assert_eq!(back(&after, &before).unwrap()[0].tree, before);
        let mut changed = undone[0].changed.clone();
        changed.sort();
        assert_eq!(changed, ["a", "d/x", "lf", "link", "new/n", "run"]);
        assert_eq!(std::fs::read(checkout.join("lf")).unwrap(), b"x\n");
        assert_eq!(
            std::fs::read_to_string(checkout.join("a")).unwrap(),
            "one\n"
        );
        assert_eq!(
            std::fs::read_to_string(checkout.join("d/x")).unwrap(),
            "x\n"
        );
        assert!(!checkout.join("new").exists());
        assert_eq!(
            std::fs::read_link(checkout.join("link")).unwrap(),
            Path::new("a")
        );
        let mode = std::fs::metadata(checkout.join("run"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o111, 0);
        assert_eq!(
            std::fs::read_to_string(checkout.join("target/kept")).unwrap(),
            "built\n"
        );
        let main = scratch.0.join("main");
        assert_eq!(
            run(&main, &["rev-parse", &format!("{REF}tree^{{tree}}")]),
            before
        );
        // Redone: the step's again.
        assert_eq!(back(&before, &after).unwrap()[0].tree, after);
        assert_eq!(
            std::fs::read_to_string(checkout.join("a")).unwrap(),
            "two\n"
        );
        assert_eq!(
            std::fs::read_to_string(checkout.join("new/n")).unwrap(),
            "n\n"
        );
        assert!(!checkout.join("d").exists());
        // A worktree changed since is refused, and nothing written.
        std::fs::write(checkout.join("later"), "mine\n").unwrap();
        let refused = back(&after, &before).unwrap_err();
        assert!(refused.contains("has changed since"), "{refused}");
        assert_eq!(
            std::fs::read_to_string(checkout.join("a")).unwrap(),
            "two\n"
        );
        assert!(checkout.join("later").exists());
        // Bad requests.
        assert!(restore(&git, &names, std::slice::from_ref(&after), &[], &roots).is_err());
        assert!(restore(
            &git,
            &names,
            &["HEAD".into()],
            std::slice::from_ref(&before),
            &roots
        )
        .is_err());
        assert!(restore(
            &git,
            &names,
            std::slice::from_ref(&after),
            std::slice::from_ref(&before),
            &[]
        )
        .is_err());
    }

    #[test]
    fn a_restore_names_only_plain_paths_and_removes_through_no_link() {
        for bad in [
            &b""[..],
            b"/abs",
            b"../x",
            b"a/../b",
            b"a//b",
            b"./a",
            b".git/config",
            b"a/.GIT/x",
        ] {
            assert!(relative(bad).is_err(), "{bad:?}");
        }
        assert_eq!(relative(b"a/b.git/c").unwrap(), Path::new("a/b.git/c"));
        let scratch = Scratch::new("snapshot-remove");
        let checkout = scratch.0.join("tree");
        let outside = scratch.0.join("outside");
        std::fs::create_dir_all(&checkout).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("f"), "kept\n").unwrap();
        std::os::unix::fs::symlink(&outside, checkout.join("l")).unwrap();
        assert!(remove(&checkout, Path::new("l/f")).is_err());
        assert!(outside.join("f").exists());
        // A link itself is removed, not what it names.
        std::os::unix::fs::symlink(outside.join("f"), checkout.join("g")).unwrap();
        remove(&checkout, Path::new("g")).unwrap();
        assert!(outside.join("f").exists());
        // What is gone already is no failure; a directory is.
        remove(&checkout, Path::new("none/x")).unwrap();
        std::fs::create_dir(checkout.join("dir")).unwrap();
        assert!(remove(&checkout, Path::new("dir")).is_err());
        // Emptied directories go, up to the first that is not.
        std::fs::create_dir_all(checkout.join("p/q/r")).unwrap();
        std::fs::write(checkout.join("p/keep"), "k\n").unwrap();
        prune(&checkout, Path::new("p/q/r/file"));
        assert!(!checkout.join("p/q").exists());
        assert!(checkout.join("p/keep").exists());
    }

    #[test]
    fn what_no_snapshot_holds_is_never_written_over() {
        if !crate::git::tests::have_git() {
            return;
        }
        let scratch = Scratch::new("snapshot-in-the-way");
        let (checkout, name) = worktree(
            &scratch,
            &[("out", "one\n"), ("p/q", "q\n"), (".gitignore", "*.o\n")],
        );
        std::fs::write(checkout.join("notes"), "mine\n").unwrap();
        let git = git();
        let roots = [checkout.clone()];
        let names = [name.clone()];
        let before = take(&git, &names, &[], &roots).unwrap()[0].tree.clone();
        // The step: a file turned directory, a directory gone, and a file
        // newly ignored.
        std::fs::remove_file(checkout.join("out")).unwrap();
        std::fs::create_dir(checkout.join("out")).unwrap();
        std::fs::write(checkout.join("out/x"), "x\n").unwrap();
        std::fs::remove_file(checkout.join("p/q")).unwrap();
        std::fs::remove_dir(checkout.join("p")).unwrap();
        std::fs::write(checkout.join(".gitignore"), "*.o\nnotes\np\n").unwrap();
        let after = take(&git, &names, std::slice::from_ref(&before), &roots).unwrap()[0]
            .tree
            .clone();
        let undo = || {
            restore(
                &git,
                &names,
                std::slice::from_ref(&after),
                std::slice::from_ref(&before),
                &roots,
            )
        };
        // An ignored build output in the directory to become a file.
        std::fs::write(checkout.join("out/build.o"), "built\n").unwrap();
        let refused = undo().unwrap_err();
        assert!(refused.contains("out/build.o is in the way"), "{refused}");
        assert!(refused.starts_with(&name), "{refused}");
        assert!(checkout.join("out/x").exists(), "nothing written");
        std::fs::remove_file(checkout.join("out/build.o")).unwrap();
        // An ignored file where a directory must be.
        std::fs::write(checkout.join("p"), "ignored\n").unwrap();
        let refused = undo().unwrap_err();
        assert!(refused.contains("p is in the way"), "{refused}");
        std::fs::remove_file(checkout.join("p")).unwrap();
        // The newly ignored file, edited since, is the human's; as it was,
        // it is the very bytes written.
        std::fs::write(checkout.join("notes"), "edited\n").unwrap();
        let refused = undo().unwrap_err();
        assert!(refused.contains("notes is in the way"), "{refused}");
        assert_eq!(
            std::fs::read_to_string(checkout.join("notes")).unwrap(),
            "edited\n"
        );
        std::fs::write(checkout.join("notes"), "mine\n").unwrap();
        // Out of the way, the directory holding only what goes is undone.
        assert_eq!(undo().unwrap()[0].tree, before);
        assert_eq!(
            std::fs::read_to_string(checkout.join("out")).unwrap(),
            "one\n"
        );
        assert_eq!(
            std::fs::read_to_string(checkout.join("p/q")).unwrap(),
            "q\n"
        );
        assert_eq!(
            std::fs::read_to_string(checkout.join("notes")).unwrap(),
            "mine\n"
        );
    }

    #[test]
    fn a_submodule_is_not_restored() {
        if !crate::git::tests::have_git() {
            return;
        }
        let scratch = Scratch::new("snapshot-submodule");
        let (checkout, name) = worktree(&scratch, &[("a", "a\n")]);
        let git = git();
        let roots = [checkout.clone()];
        let names = [name.clone()];
        let before = take(&git, &names, &[], &roots).unwrap()[0].tree.clone();
        let sub = checkout.join("sub");
        std::fs::create_dir(&sub).unwrap();
        run(&sub, &["init", "-q"]);
        std::fs::write(sub.join("s"), "s\n").unwrap();
        run(&sub, &["add", "s"]);
        run(&sub, &["commit", "-q", "-m", "s"]);
        std::fs::write(checkout.join("a"), "b\n").unwrap();
        let after = take(&git, &names, std::slice::from_ref(&before), &roots).unwrap()[0]
            .tree
            .clone();
        let refused = restore(
            &git,
            &names,
            std::slice::from_ref(&after),
            std::slice::from_ref(&before),
            &roots,
        )
        .unwrap_err();
        assert!(refused.contains("sub is a submodule"), "{refused}");
        assert_eq!(std::fs::read_to_string(checkout.join("a")).unwrap(), "b\n");
    }
}

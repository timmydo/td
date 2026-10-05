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
    // A private index, so the worktree's own, and what is staged in it,
    // are left as they are.
    let index = std::env::temp_dir().join(format!(
        "td-agent-snapshot-{}.index",
        crate::store::random_hex(8).map_err(|e| format!("an index name: {e}"))?
    ));
    let made = make_tree(git, checkout, &gitdir, &index);
    let _ = std::fs::remove_file(&index);
    let tree = made?;
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
    let empty = said(
        git.at(checkout)
            .args(["hash-object", "-t", "tree", "/dev/null"]),
    )?;
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
}

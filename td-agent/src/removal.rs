//! Removing a repository workspace with its conversation (DESIGN.md §7,
//! Archiving and deleting). The window stops the conversation's
//! processes, then surveys each prepared worktree in a maintenance
//! instance (`repo::Task::Survey`): what removing it would lose, as the
//! workspace's own git reports it, which the jail controls. A workspace
//! that reports nothing to lose goes with its conversation; one that
//! reports anything, or whose answer cannot be read, goes only on the
//! human's confirmation listing it. Removal runs no git over the tree:
//! the workspace tree and its repositories' directory are renamed out of
//! the way and removed by `workspace::remove_tree`, which follows no
//! link, and what a crash leaves the next start sweeps. The store stays.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::repo::{Survey, Task};
use crate::store::{Id, Instructed, StateDir};
use crate::workspace::{Entry, Repositories};

/// What a removal renames a directory to, before it is removed.
pub const DOOMED: &str = ".deleting-";
/// How long a workspace's survey may take in all: past it, a worktree
/// not yet asked is said not asked.
pub const SURVEY_ALL: Duration = Duration::from_secs(600);
/// The longest line the loss card lists, within its dialog's bound on a
/// detail.
const MAX_LOST_LINE: usize = 1024;

/// What the survey found of one worktree.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Found {
    pub checkout: PathBuf,
    pub branch: String,
    /// Its workspace repository, whose refs every worktree of it counts.
    pub repository: PathBuf,
    pub survey: Result<Survey, String>,
}

/// Surveys conversation `id`'s workspace, its processes stopped, in
/// instances of `programs`, within `SURVEY_ALL`: each worktree of a
/// repository it recorded prepared. One never prepared was bound by no
/// instance but maintenance, so holds nothing the model made, and is
/// not asked.
pub fn survey(
    state: &StateDir,
    id: &Id,
    repositories: &Repositories,
    programs: Result<crate::jail::Programs, String>,
) -> Vec<Found> {
    let prepared = state.prepared(id);
    let bases = state.instructions(id);
    let tools = programs.and_then(|programs| Ok((programs, crate::repo::host_git()?)));
    let dir = crate::workspace::jail_dir(state, id);
    let now = Instant::now();
    let deadline = now.checked_add(SURVEY_ALL).unwrap_or(now);
    let mut found = Vec::new();
    for entry in &repositories.entries {
        let left = deadline.saturating_duration_since(Instant::now());
        let survey = match &prepared {
            Err(why) => Err(format!(
                "whether it was checked out could not be read: {why}"
            )),
            Ok(prepared) if !prepared.contains(&entry.repository) => continue,
            Ok(_) if left.is_zero() => Err(format!(
                "not asked: the survey took more than its {} minutes",
                SURVEY_ALL.as_secs() / 60
            )),
            Ok(_) => surveyed(entry, repositories, &bases, &tools, &dir, left),
        };
        found.push(Found {
            checkout: entry.checkout.clone(),
            branch: entry.branch.clone(),
            repository: entry.repository.clone(),
            survey,
        });
    }
    found
}

fn surveyed(
    entry: &Entry,
    repositories: &Repositories,
    bases: &Result<Vec<Instructed>, String>,
    tools: &Result<(crate::jail::Programs, PathBuf), String>,
    dir: &Path,
    left: Duration,
) -> Result<Survey, String> {
    let (programs, git) = tools.as_ref().map_err(Clone::clone)?;
    let recorded = bases
        .as_ref()
        .map_err(|why| format!("the commit it started at could not be read: {why}"))?;
    let together: Vec<&Entry> = repositories
        .entries
        .iter()
        .filter(|other| other.repository == entry.repository)
        .collect();
    let base_of = |of: &Entry| {
        recorded
            .iter()
            .find(|recorded| recorded.checkout == of.checkout)
            .map(|recorded| recorded.base.clone())
    };
    base_of(entry).ok_or("the commit it started at is not recorded")?;
    // Every commit its repository's worktrees started at is upstream's.
    let mut every: Vec<String> = Vec::new();
    for base in together.iter().filter_map(|other| base_of(other)) {
        if !every.contains(&base) {
            every.push(base);
        }
    }
    let task = Task::Survey {
        git: git.clone(),
        repository: entry.repository.clone(),
        id: entry.id.clone(),
        checkout: entry.checkout.clone(),
        bases: every,
    };
    let policy =
        crate::workspace::maintenance(dir, &together).ok_or("its repository has no worktree")?;
    let said = crate::jail::maintain(
        programs,
        &policy,
        &dir.join("specs"),
        &task,
        left.min(crate::repo::SURVEY_TIME),
    )?;
    Survey::parse(&said)
}

/// What removing the surveyed worktrees would lose, a line each, as the
/// confirmation lists it: none when every one reported nothing. Commits
/// are its repository's, so said with its first worktree alone.
pub fn lost(found: &[Found]) -> Vec<String> {
    let mut counted: Vec<&Path> = Vec::new();
    found
        .iter()
        .filter_map(|found| {
            let place = format!(
                "{} (branch {})",
                crate::tools::visible(&found.checkout.display().to_string()),
                crate::tools::visible(&found.branch)
            );
            let survey = match &found.survey {
                Ok(survey) if survey.clean() => return None,
                Ok(survey) => survey,
                Err(why) => {
                    return Some(cut(format!(
                        "{place}: could not be checked, so may hold work: {}",
                        crate::tools::visible(why)
                    )))
                }
            };
            let mut said = Vec::new();
            match survey.changes {
                Some(0) => {}
                Some(1) => said.push("1 changed or untracked file".to_string()),
                Some(n) => said.push(format!("{n} changed or untracked files")),
                None => {
                    said.push("more changed or untracked files than could be counted".into())
                }
            }
            if survey.ahead > 0 && !counted.contains(&found.repository.as_path()) {
                counted.push(&found.repository);
                said.push(match survey.ahead {
                    1 => "1 commit, in its repository's branches, tags or stash, not in a commit its worktrees started at".into(),
                    n => format!("{n} commits, in its repository's branches, tags or stash, not in a commit its worktrees started at"),
                });
            }
            (!said.is_empty()).then(|| cut(format!("{place}: {}", said.join(", "))))
        })
        .collect()
}

/// `line` within `MAX_LOST_LINE` bytes, cut on a character with an
/// ellipsis.
fn cut(line: String) -> String {
    if line.len() <= MAX_LOST_LINE {
        return line;
    }
    let mut end = MAX_LOST_LINE - '\u{2026}'.len_utf8();
    while !line.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\u{2026}", line.get(..end).unwrap_or_default())
}

/// A workspace's directories renamed out of the way, before its
/// conversation is deleted: removed once it is (`finish`), put back if
/// it is not (`restore`).
#[derive(Debug)]
pub struct Doomed {
    moved: Vec<(PathBuf, PathBuf)>,
    problems: Vec<String>,
}

/// Renames `repositories`' workspace tree and its repositories'
/// directory, each beside itself, so they are gone at once and, should
/// td-agent stop before `Doomed::finish`, swept at its next start. A
/// directory not named for the workspace is left, and said.
pub fn doom(repositories: &Repositories) -> Doomed {
    let name = OsString::from(&repositories.name);
    let dirs: Vec<&Path> = repositories
        .tree()
        .into_iter()
        .chain(
            repositories
                .entries
                .first()
                .and_then(|entry| entry.repository.parent()),
        )
        .collect();
    let mut doomed = Doomed {
        moved: Vec::new(),
        problems: Vec::new(),
    };
    for dir in dirs {
        if dir.file_name() != Some(name.as_os_str()) {
            doomed.problems.push(format!(
                "{} is not named for the workspace, so is left",
                dir.display()
            ));
            continue;
        }
        match rename_away(dir) {
            Ok(Some(to)) => doomed.moved.push((dir.to_path_buf(), to)),
            Ok(None) => {}
            Err(e) => doomed.problems.push(format!("{}: {e}", dir.display())),
        }
    }
    doomed
}

impl Doomed {
    /// Removes what was renamed, on a thread, since a large tree takes
    /// time; what was not is said.
    pub fn finish(self) -> Result<(), String> {
        let mut problems = self.problems;
        let moved: Vec<PathBuf> = self.moved.into_iter().map(|(_, to)| to).collect();
        if !moved.is_empty() {
            if let Err(e) = removing(moved) {
                problems.push(e);
            }
        }
        if problems.is_empty() {
            Ok(())
        } else {
            Err(problems.join("; "))
        }
    }

    /// Puts back what was renamed, its conversation kept.
    pub fn restore(self) -> Result<(), String> {
        let problems: Vec<String> = self
            .moved
            .into_iter()
            .filter_map(|(from, to)| {
                std::fs::rename(&to, &from)
                    .err()
                    .map(|e| format!("{} back to {}: {e}", to.display(), from.display()))
            })
            .collect();
        if problems.is_empty() {
            Ok(())
        } else {
            Err(problems.join("; "))
        }
    }
}

/// Renames `dir` beside itself to `.deleting-<name>`, or, where an
/// earlier removal left that, `.deleting-<name>.<random>`; none when it
/// is gone already.
fn rename_away(dir: &Path) -> std::io::Result<Option<PathBuf>> {
    let (Some(parent), Some(name)) = (dir.parent(), dir.file_name()) else {
        return Err(std::io::Error::other("no directory to rename"));
    };
    let mut to = parent.join(format!("{DOOMED}{}", name.to_string_lossy()));
    // EEXIST and ENOTEMPTY: the name is taken.
    match std::fs::rename(dir, &to) {
        Ok(()) => Ok(Some(to)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) if matches!(e.raw_os_error(), Some(17 | 39)) => {
            let suffix = crate::store::random_hex(4).map_err(std::io::Error::other)?;
            to = parent.join(format!("{DOOMED}{}.{suffix}", name.to_string_lossy()));
            std::fs::rename(dir, &to).map(|()| Some(to))
        }
        Err(e) => Err(e),
    }
}

/// Removes `doomed` on a thread of its own.
fn removing(doomed: Vec<PathBuf>) -> Result<(), String> {
    std::thread::Builder::new()
        .name("td-agent-workspace-removal".into())
        .spawn(move || {
            for dir in doomed {
                if let Err(e) = crate::workspace::remove_tree(&dir) {
                    eprintln!("td-agent: {}: {e}", dir.display());
                }
            }
        })
        .map(drop)
        .map_err(|e| format!("the removal's thread: {e}"))
}

/// Whether `name` is one `doom` gives: `.deleting-` and a workspace's
/// name, which ends in `-` and eight hex digits, then perhaps `.` and
/// eight more. The workspace root is the human's to name, so nothing
/// else there is taken for td-agent's.
fn doomed_name(name: &str) -> bool {
    let hex = |text: &str| text.len() == 8 && text.bytes().all(|b| b.is_ascii_hexdigit());
    let Some(rest) = name.strip_prefix(DOOMED) else {
        return false;
    };
    let workspace = match rest.rsplit_once('.') {
        Some((workspace, suffix)) if hex(suffix) => workspace,
        _ => rest,
    };
    workspace
        .rsplit_once('-')
        .is_some_and(|(template, id)| !template.is_empty() && hex(id))
}

/// Removes, on a thread, what removals a crash cut short left in `dirs`:
/// the workspace root and the data directory's `ws/`, each name one
/// `doom` gives.
pub fn sweep(dirs: &[PathBuf]) {
    let doomed: Vec<PathBuf> = dirs
        .iter()
        .filter_map(|dir| std::fs::read_dir(dir).ok())
        .flat_map(|entries| entries.flatten())
        .filter(|entry| {
            doomed_name(&entry.file_name().to_string_lossy())
                && entry.file_type().is_ok_and(|kind| kind.is_dir())
        })
        .map(|entry| entry.path())
        .collect();
    if !doomed.is_empty() {
        if let Err(e) = removing(doomed) {
            eprintln!("td-agent: sweeping workspaces: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic)]
    use super::*;
    use crate::store::tests::Scratch;

    const NAME: &str = "td-0123abcd";

    fn entry(root: &Path, data: &Path, id: &str) -> Entry {
        Entry {
            remote: "https://github.com/timmydo/td".into(),
            base: "main".into(),
            branch: format!("td-agent/{id}"),
            sparse: None,
            store: data.join("stores/td.git"),
            repository: data.join("ws").join(NAME).join("td.git"),
            id: id.into(),
            checkout: root.join(NAME).join(id),
        }
    }

    /// Clean worktrees lose nothing; changes and a survey that failed
    /// are listed a worktree a line, by checkout and branch, and commits,
    /// which are its repository's, once a repository; a line is held to
    /// its bound.
    #[test]
    fn what_would_be_lost_is_listed_a_worktree_a_line() {
        let found = |checkout: &str, repository: &str, survey| Found {
            checkout: checkout.into(),
            branch: "td-agent/td-1".into(),
            repository: repository.into(),
            survey,
        };
        let clean = Survey {
            changes: Some(0),
            ahead: 0,
        };
        assert!(lost(&[found("/w/a", "/r/a", Ok(clean))]).is_empty());
        let lines = lost(&[
            found("/w/a", "/r/a", Ok(clean)),
            found(
                "/w/b",
                "/r/b",
                Ok(Survey {
                    changes: Some(1),
                    ahead: 2,
                }),
            ),
            // The same repository's commits, said once.
            found(
                "/w/b2",
                "/r/b",
                Ok(Survey {
                    changes: Some(0),
                    ahead: 2,
                }),
            ),
            found(
                "/w/c",
                "/r/c",
                Ok(Survey {
                    changes: None,
                    ahead: 1,
                }),
            ),
            found("/w/d", "/r/d", Err("the jail\u{202e} failed".into())),
            found(
                "/w/e",
                "/r/e",
                Ok(Survey {
                    changes: Some(0),
                    ahead: 3,
                }),
            ),
        ]);
        let commits =
            "in its repository's branches, tags or stash, not in a commit its worktrees started at";
        assert_eq!(
            lines,
            [
                format!("/w/b (branch td-agent/td-1): 1 changed or untracked file, 2 commits, {commits}"),
                format!("/w/c (branch td-agent/td-1): more changed or untracked files than could be counted, 1 commit, {commits}"),
                "/w/d (branch td-agent/td-1): could not be checked, so may hold work: the jail<U+202E> failed".to_string(),
                format!("/w/e (branch td-agent/td-1): 3 commits, {commits}"),
            ]
        );
        let long = lost(&[found("/w/f", "/r/f", Err("\u{1}".repeat(2000)))]);
        let line = long.first().unwrap();
        assert!(
            line.len() <= MAX_LOST_LINE && line.ends_with('\u{2026}'),
            "{}",
            line.len()
        );
    }

    fn names(dir: &Path) -> Vec<OsString> {
        let mut names: Vec<OsString> = std::fs::read_dir(dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name())
            .collect();
        names.sort();
        names
    }

    fn until(done: impl Fn() -> bool) {
        for _ in 0..300 {
            if done() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(done(), "the removal thread did not finish");
    }

    /// The tree and the repositories' directory are renamed out of the
    /// way at once: put back when the conversation is kept, removed when
    /// it goes, links not followed; a name an earlier removal left is
    /// passed over; one not the workspace's is left and said; a sweep
    /// takes only names a removal gives.
    #[test]
    fn a_workspace_is_renamed_away_then_removed_or_put_back() {
        let scratch = Scratch::new("removal");
        let root = scratch.0.join("root");
        let data = scratch.0.join("data");
        let repositories = Repositories {
            template: "td".into(),
            name: NAME.into(),
            entries: vec![entry(&root, &data, "td"), entry(&root, &data, "td-x")],
        };
        let outside = scratch.0.join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("keep"), "kept").unwrap();
        let tree = root.join(NAME);
        let repos = data.join("ws").join(NAME);
        for dir in [tree.join("td"), repos.join("td.git")] {
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("f"), "x").unwrap();
            std::os::unix::fs::symlink(&outside, dir.join("link")).unwrap();
        }
        // Renamed away, then put back: the conversation was kept.
        let doomed = doom(&repositories);
        assert!(!tree.exists() && !repos.exists());
        doomed.restore().unwrap();
        assert!(tree.join("td/f").exists() && repos.join("td.git/f").exists());
        // An earlier removal's leftover holds the plain name, and the
        // human's own `.deleting-` files are no removal's, a directory's
        // name or not.
        let leftover = format!(".deleting-{NAME}");
        std::fs::create_dir_all(root.join(&leftover).join("old")).unwrap();
        std::fs::write(root.join(".deleting-notes"), "mine").unwrap();
        std::fs::write(root.join(".deleting-backup-20240101"), "mine").unwrap();
        doom(&repositories).finish().unwrap();
        assert!(!tree.exists() && !repos.exists());
        until(|| {
            names(&root)
                == [
                    OsString::from(".deleting-backup-20240101"),
                    OsString::from(".deleting-notes"),
                    OsString::from(&leftover),
                ]
                && names(&data.join("ws")).is_empty()
        });
        assert!(outside.join("keep").exists(), "a link was followed");
        sweep(&[root.clone(), data.join("ws")]);
        until(|| {
            names(&root)
                == [
                    OsString::from(".deleting-backup-20240101"),
                    OsString::from(".deleting-notes"),
                ]
        });
        // Gone already, removing again is no matter; a tree not named
        // for the workspace is left.
        doom(&repositories).finish().unwrap();
        let mut renamed = repositories.clone();
        renamed.name = "other-0123abcd".into();
        std::fs::create_dir_all(tree.join("td")).unwrap();
        let e = doom(&renamed).finish().unwrap_err();
        assert!(e.contains("not named for the workspace"), "{e}");
        assert!(tree.join("td").exists());
    }

    /// Only names a removal gives are a sweep's.
    #[test]
    fn a_sweep_knows_a_removals_names() {
        for name in [
            ".deleting-td-0123abcd",
            ".deleting-my-repo-89abcdef",
            ".deleting-td-0123abcd.deadbeef",
        ] {
            assert!(doomed_name(name), "{name}");
        }
        for name in [
            ".deleting-notes",
            ".deleting-td",
            ".deleting--0123abcd",
            ".deleting-td-0123abc",
            "td-0123abcd",
            ".deleting-td-0123abcd.x",
        ] {
            assert!(!doomed_name(name), "{name}");
        }
    }
}

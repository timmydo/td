//! The worktree sweep: agents work in `.claude/worktrees/<name>`, and a
//! workstream that has landed leaves one behind. Removing them is the
//! integrator's job because the integrator is the one who knows what landed.
//!
//! Everything here decides; only `sweep` removes, and it removes exactly what
//! was decided. A worktree is somebody's working directory, so the rules are
//! all conservative: unless the tree is clean, the branch's every commit is
//! already on the base, and no remote still carries it, this keeps it.

use std::io;

use crate::git::Git;

/// One entry of `git worktree list --porcelain`.
#[derive(Debug, PartialEq, Eq)]
pub struct Worktree {
    pub path: String,
    /// Short branch name; `None` for a detached HEAD.
    pub branch: Option<String>,
    pub locked: bool,
    /// git already considers it removable — its directory is gone.
    pub prunable: bool,
    /// The MAIN worktree — git lists it first. Never swept.
    pub main: bool,
    /// This process's own working directory. Also never swept: removing the
    /// directory you are running in is not a cleanup anyone asked for.
    pub cwd: bool,
}

/// What sweeping would do with one worktree, and why. The reason is shown
/// either way: a sweep nobody can audit is one nobody will run twice.
#[derive(Debug, PartialEq, Eq)]
pub enum Verdict {
    Remove,
    Keep(String),
}

/// What the sweep needs to know about a worktree's branch, gathered by `facts`.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Facts {
    /// Any uncommitted change at all, tracked or not.
    pub dirty: bool,
    /// Commits on the branch with no equivalent on the base. A rebase landing
    /// rewrites object ids, so this is patch equivalence, not ancestry.
    pub unlanded: usize,
    /// Remotes that still carry the branch — a pushed branch is a live ask.
    pub remotes: Vec<String>,
    /// Commits over the base, landed or not. Zero means the branch has not
    /// started, which `unlanded` alone reads as "everything landed".
    pub commits: usize,
}

/// The verdicts that need no git query. Answered FIRST, because a worktree whose
/// directory is gone cannot be asked anything — `git -C <gone> status` fails,
/// and gathering facts first would replace every message below with that error.
pub fn early_verdict(wt: &Worktree) -> Option<Verdict> {
    if wt.main {
        return Some(Verdict::Keep("the main checkout".to_string()));
    }
    if wt.cwd {
        return Some(Verdict::Keep("this process is running in it".to_string()));
    }
    if wt.prunable {
        // Directory already gone: `git worktree prune` is the right tool, and
        // `remove` would fail on it.
        return Some(Verdict::Keep(
            "directory is missing (git worktree prune)".to_string(),
        ));
    }
    if wt.locked {
        return Some(Verdict::Keep("locked".to_string()));
    }
    let Some(branch) = &wt.branch else {
        return Some(Verdict::Keep(
            "detached HEAD — nothing to compare".to_string(),
        ));
    };
    // Fully landed is the NORMAL state of a rolling branch between increments,
    // not evidence that anyone is done with it.
    if crate::git::is_rolling(branch) {
        return Some(Verdict::Keep(
            "rolling branch — it outlives its landings".to_string(),
        ));
    }
    None
}

pub fn decide(wt: &Worktree, facts: &Facts) -> Verdict {
    if let Some(v) = early_verdict(wt) {
        return v;
    }
    if facts.dirty {
        return Verdict::Keep("uncommitted changes".to_string());
    }
    if facts.commits == 0 {
        return Verdict::Keep("no commits over the base — not started".to_string());
    }
    if facts.unlanded > 0 {
        return Verdict::Keep(format!(
            "{} commit{} not on the base",
            facts.unlanded,
            if facts.unlanded == 1 { "" } else { "s" }
        ));
    }
    if let Some(r) = facts.remotes.first() {
        return Verdict::Keep(format!("still on {r}"));
    }
    Verdict::Remove
}

/// Parse `git worktree list --porcelain`. Records are blank-line separated; the
/// first is the main checkout.
/// `cwd` is this process's own toplevel, which is never swept — see
/// `Worktree::cwd`.
pub fn parse_list(out: &str, cwd: Option<&str>) -> Vec<Worktree> {
    let mut all = Vec::new();
    let mut cur: Option<Worktree> = None;
    for line in out.lines() {
        if let Some(path) = line.strip_prefix("worktree ") {
            if let Some(w) = cur.take() {
                all.push(w);
            }
            let path = path.trim().to_string();
            cur = Some(Worktree {
                cwd: cwd.is_some_and(|c| c == path),
                path,
                branch: None,
                locked: false,
                prunable: false,
                main: all.is_empty(),
            });
            continue;
        }
        let Some(w) = cur.as_mut() else { continue };
        if let Some(refname) = line.strip_prefix("branch ") {
            w.branch = Some(
                refname
                    .trim()
                    .strip_prefix("refs/heads/")
                    .unwrap_or(refname.trim())
                    .to_string(),
            );
        } else if line.starts_with("locked") {
            w.locked = true;
        } else if line.starts_with("prunable") {
            w.prunable = true;
        }
    }
    if let Some(w) = cur.take() {
        all.push(w);
    }
    all
}

/// Read what `decide` needs about one worktree's branch. `plan` asks
/// `early_verdict` first, so the detached case below is unreachable through it
/// — it is here so a direct caller cannot query a branch that does not exist.
pub fn facts(git: &Git, base: &str, wt: &Worktree) -> io::Result<Facts> {
    let Some(branch) = &wt.branch else {
        return Ok(Facts::default());
    };
    Ok(Facts {
        dirty: git.worktree_dirty(&wt.path)?,
        unlanded: git.unlanded_commits(base, branch)?,
        remotes: git.remotes_carrying(branch)?,
        commits: git.commits_over(base, branch)?,
    })
}

/// One worktree's row in the plan.
pub struct Planned {
    pub worktree: Worktree,
    pub verdict: Verdict,
}

/// Decide every worktree. A branch whose facts cannot be read is KEPT, with the
/// error as the reason: the sweep never removes on a failed query.
pub fn plan(git: &Git, base: &str) -> io::Result<Vec<Planned>> {
    let out = git.run_ok(&["worktree", "list", "--porcelain"])?;
    let here = git.run_ok(&["rev-parse", "--show-toplevel"]).unwrap_or_default();
    let mut planned = Vec::new();
    for worktree in parse_list(&out, Some(here.trim()).filter(|h| !h.is_empty())) {
        let verdict = match early_verdict(&worktree) {
            Some(v) => v,
            None => match facts(git, base, &worktree) {
                Ok(f) => decide(&worktree, &f),
                Err(e) => Verdict::Keep(format!("could not be read: {e}")),
            },
        };
        planned.push(Planned { worktree, verdict });
    }
    Ok(planned)
}

/// What a sweep did to one worktree.
pub struct Swept {
    pub path: String,
    pub error: Option<String>,
}

/// Remove exactly the worktrees `plan` decided on. `git worktree remove` makes
/// its own dirty check, so a tree that changed between the plan and here is
/// refused by git rather than lost.
pub fn sweep(git: &Git, planned: &[Planned]) -> Vec<Swept> {
    let mut done = Vec::new();
    for p in planned {
        if p.verdict != Verdict::Remove {
            continue;
        }
        let run = git.run(&["worktree", "remove", &p.worktree.path]);
        let error = match run {
            Ok(r) if r.ok => None,
            Ok(r) => Some(r.failure()),
            Err(e) => Some(e.to_string()),
        };
        done.push(Swept {
            path: p.worktree.path.clone(),
            error,
        });
    }
    done
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wt(branch: Option<&str>) -> Worktree {
        Worktree {
            path: "/w/x".to_string(),
            branch: branch.map(str::to_string),
            locked: false,
            prunable: false,
            main: false,
            cwd: false,
        }
    }

    fn landed() -> Facts {
        Facts {
            dirty: false,
            unlanded: 0,
            remotes: Vec::new(),
            commits: 3,
        }
    }

    #[test]
    fn a_landed_branchs_worktree_is_swept() {
        assert_eq!(decide(&wt(Some("td-sh-fix")), &landed()), Verdict::Remove);
    }

    #[test]
    fn the_checkout_itself_is_never_swept() {
        let mut w = wt(Some("td-sh-fix"));
        w.main = true;
        assert!(matches!(decide(&w, &landed()), Verdict::Keep(_)));
    }

    #[test]
    fn a_rolling_branch_is_kept_even_when_fully_landed() {
        let v = decide(&wt(Some("ui-rolling")), &landed());
        assert_eq!(
            v,
            Verdict::Keep("rolling branch — it outlives its landings".to_string())
        );
    }

    #[test]
    fn unlanded_work_dirt_and_remotes_each_keep_it() {
        let dirty = Facts {
            dirty: true,
            ..landed()
        };
        assert!(matches!(decide(&wt(Some("x")), &dirty), Verdict::Keep(_)));

        let ahead = Facts {
            unlanded: 2,
            ..landed()
        };
        assert_eq!(
            decide(&wt(Some("x")), &ahead),
            Verdict::Keep("2 commits not on the base".to_string())
        );

        let pushed = Facts {
            remotes: vec!["origin".to_string()],
            ..landed()
        };
        assert_eq!(
            decide(&wt(Some("x")), &pushed),
            Verdict::Keep("still on origin".to_string())
        );
    }

    /// `git cherry` reports zero unlanded commits for a branch that has NOT
    /// STARTED just as it does for one fully landed. An agent's worktree in the
    /// minutes before its first commit is the common case, and sweeping it
    /// deletes work in progress.
    #[test]
    fn a_branch_with_no_commits_yet_is_not_landed() {
        let fresh = Facts {
            commits: 0,
            ..landed()
        };
        assert_eq!(
            decide(&wt(Some("just-created")), &fresh),
            Verdict::Keep("no commits over the base — not started".to_string())
        );
    }

    /// Removing the directory the sweep is running in is not a cleanup anyone
    /// asked for — and `main` alone does not cover it, since git lists the MAIN
    /// worktree first whatever the process's CWD is.
    #[test]
    fn the_worktree_this_process_runs_in_is_kept() {
        let mut here = wt(Some("some-work"));
        here.cwd = true;
        assert_eq!(
            decide(&here, &landed()),
            Verdict::Keep("this process is running in it".to_string())
        );
    }

    #[test]
    fn the_cwd_is_marked_from_the_porcelain_list() {
        let out = "worktree /repo\nHEAD aaa\nbranch refs/heads/main\n\n\
                   worktree /repo/wt\nHEAD bbb\nbranch refs/heads/x\n";
        let list = parse_list(out, Some("/repo/wt"));
        assert!(list.get(1).is_some_and(|w| w.cwd && !w.main));
        assert!(list.first().is_some_and(|w| w.main && !w.cwd));
    }

    #[test]
    fn a_detached_or_locked_worktree_is_kept() {
        assert!(matches!(decide(&wt(None), &landed()), Verdict::Keep(_)));
        let mut locked = wt(Some("x"));
        locked.locked = true;
        assert!(matches!(decide(&locked, &landed()), Verdict::Keep(_)));
    }

    #[test]
    fn the_porcelain_list_parses() {
        let out = "worktree /repo\nHEAD aaa\nbranch refs/heads/main\n\n\
                   worktree /repo/.claude/worktrees/w1\nHEAD bbb\nbranch refs/heads/td-sh-fix\n\n\
                   worktree /repo/.claude/worktrees/w2\nHEAD ccc\ndetached\nlocked\n\n\
                   worktree /gone\nHEAD ddd\nbranch refs/heads/x\nprunable gitdir file points to non-existent location\n";
        let list = parse_list(out, None);
        assert_eq!(list.len(), 4);
        let main = list.first().expect("main");
        assert!(main.main && main.branch.as_deref() == Some("main"));
        let w1 = list.get(1).expect("w1");
        assert!(!w1.main && w1.branch.as_deref() == Some("td-sh-fix"));
        let w2 = list.get(2).expect("w2");
        assert!(w2.branch.is_none() && w2.locked);
        assert!(list.get(3).is_some_and(|w| w.prunable));
    }

    #[test]
    fn a_missing_directory_is_left_to_git_worktree_prune() {
        let mut gone = wt(Some("x"));
        gone.prunable = true;
        assert!(matches!(decide(&gone, &landed()), Verdict::Keep(_)));
    }
}

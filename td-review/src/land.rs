//! Preview a branch against the base, squash it in, commit it with the
//! message its own commits carry, and push the result to every remote.

use std::fs;
use std::io;

use crate::git::{Git, HeadClaim, MergeResult, NO_PUSH};
use crate::term::{Line, Style, CYAN, GREEN, RED, YELLOW};

/// What the landing did. Anything other than `Committed` left HEAD unmoved.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Committed { sha: String },
    /// The branch contributes no staged change over the base.
    Nothing,
    /// `merge --squash` left unmerged index entries.
    Conflict,
    /// A precondition failed before anything was touched.
    Blocked(String),
    /// The merge or commit failed. The work tree may hold a staged squash.
    Failed(String),
}

pub struct Landing {
    pub log: Vec<Line>,
    pub outcome: Outcome,
}

/// Everything the review screen shows for one branch, pinned to the exact
/// object ids it was rendered from.
pub struct Preview {
    pub branch_oid: String,
    /// The base tip the diff was computed against. Landing re-checks it: another
    /// worktree sharing this .git can advance the base while a review is open.
    pub base_oid: String,
    pub merge_base: String,
    pub commits: Vec<String>,
    pub message: String,
    pub stat: String,
    pub diff: String,
    /// Set when `diff` is NOT the tree the landing would stage — the pane must
    /// say so rather than let it read as the landing.
    pub note: Option<String>,
}

impl Preview {
    /// Nothing to land: no commits in range, or no textual change.
    pub fn is_empty(&self) -> bool {
        self.commits.is_empty() || self.diff.trim().is_empty()
    }
}

fn ok_line(text: impl Into<String>) -> Line {
    Line::new(text, Style::fg(GREEN))
}

fn err_line(text: impl Into<String>) -> Line {
    Line::new(text, Style::fg(RED))
}

fn step_line(text: impl Into<String>) -> Line {
    Line::new(text, Style::fg(CYAN).with_bold())
}

fn note_line(text: impl Into<String>) -> Line {
    Line::new(text, Style::dim())
}

/// Log every line of a git invocation's output, one row each.
fn log_output(log: &mut Vec<Line>, run: &crate::git::Run) {
    for l in run.stdout.lines().chain(run.stderr.lines()) {
        log.push(note_line(format!("  {l}")));
    }
}

/// Read everything needed to review `branch` against `base`. Every range below
/// names object ids: worktrees share refs, so a concurrent fetch could otherwise
/// give the preview a diff and a message from different commits.
pub fn preview(git: &Git, base: &str, branch: &str) -> io::Result<Preview> {
    let branch_oid = git.branch_oid(branch)?;
    let base_oid = git.rev_parse(&format!("refs/heads/{base}"))?;
    let merge_base = git.merge_base(&base_oid, &branch_oid)?;
    let range = format!("{merge_base}..{branch_oid}");
    let commits = git
        .run_ok(&["log", "--reverse", "--oneline", "--no-decorate", &range, "--"])?
        .lines()
        .map(str::to_string)
        .collect();
    // The same bytes the landing will commit, decoded only for display: a
    // separate query could show a message the commit does not carry.
    let message = String::from_utf8_lossy(&git.message_bytes(&range)?).into_owned();

    // The landing is a three-way squash onto the base, NOT the branch's own
    // diff: with a base-side rename the two name different paths.
    let (stat, diff, note) = match git.merge_tree(&base_oid, &branch_oid)? {
        MergeResult::Clean(tree) => (
            git.run_ok(&["diff", "--stat", &base_oid, &tree, "--"])?,
            git.run_ok(&["diff", &base_oid, &tree, "--"])?,
            None,
        ),
        // No mergeable tree to show: fall back to the branch's own changes and
        // say so, rather than passing them off as the landing.
        MergeResult::Conflicted => (
            git.run_ok(&["diff", "--stat", &range, "--"])?,
            git.run_ok(&["diff", &range, "--"])?,
            Some(format!(
                "{branch} does not merge cleanly onto {base} — landing will stop at the \
                 conflict. Shown below is the BRANCH's own diff, not the landing."
            )),
        ),
        MergeResult::Unavailable(why) => (
            git.run_ok(&["diff", "--stat", &range, "--"])?,
            git.run_ok(&["diff", &range, "--"])?,
            Some(format!(
                "the merge result could not be computed ({why}) — shown below is the \
                 BRANCH's own diff, which can differ from what the squash stages"
            )),
        ),
    };
    Ok(Preview { branch_oid, base_oid, merge_base, commits, message, stat, diff, note })
}

/// Refuse to land unless the work tree is clean and the base branch is checked
/// out — the two guards `git squash-in` opens with, plus the branch check the
/// integrator otherwise makes by eye.
pub fn preflight(git: &Git, base: &str) -> io::Result<Result<(), String>> {
    match git.head_branch()? {
        Some(head) if head == base => {}
        Some(head) => {
            return Ok(Err(format!(
                "HEAD is on '{head}', not '{base}' — check out {base} first"
            )))
        }
        None => return Ok(Err("HEAD is detached — check out the base branch first".to_string())),
    }
    let dirty = git.dirty_entries()?;
    if !dirty.is_empty() {
        let shown: Vec<&str> = dirty.iter().map(String::as_str).take(5).collect();
        let more = dirty.len().saturating_sub(shown.len());
        let suffix = if more > 0 { format!(" (+{more} more)") } else { String::new() };
        return Ok(Err(format!("working tree not clean: {}{suffix}", shown.join(", "))));
    }
    Ok(Ok(()))
}

/// Squash `branch` into the checked-out base and commit it with the branch's
/// own commit messages. Leaves HEAD untouched unless it reports `Committed`.
///
/// `expected_oid` is the tip the integrator actually reviewed; if the ref has
/// moved since, the landing is refused rather than committing unreviewed work.
pub fn squash_land(
    git: &Git,
    base: &str,
    branch: &str,
    expected_oid: Option<&str>,
    expected_base: Option<&str>,
) -> io::Result<Landing> {
    let mut log = Vec::new();

    if let Err(why) = preflight(git, base)? {
        log.push(err_line(format!("refusing: {why}")));
        return Ok(Landing { log, outcome: Outcome::Blocked(why) });
    }

    let branch_oid = match git.branch_oid(branch) {
        Ok(oid) => oid,
        Err(e) => return Ok(blocked(log, e.to_string())),
    };
    if let Some(expected) = expected_oid {
        if expected != branch_oid {
            return Ok(blocked(
                log,
                format!(
                    "{branch} moved since it was reviewed ({} -> {}) — review it again",
                    short(expected),
                    short(&branch_oid)
                ),
            ));
        }
    }

    if let Some(expected) = expected_base {
        let current = match git.rev_parse(&format!("refs/heads/{base}")) {
            Ok(oid) => oid,
            Err(e) => return Ok(blocked(log, e.to_string())),
        };
        if expected != current {
            return Ok(blocked(
                log,
                format!(
                    "{base} moved since the review ({} -> {}) — the squash would differ",
                    short(expected),
                    short(&current)
                ),
            ));
        }
    }

    // The parent the squash is built on, re-checked before the commit and
    // verified after it: a base that advances mid-landing would otherwise get a
    // commit whose tree predates it, reverting what arrived in between.
    let head_before = match git.rev_parse("HEAD") {
        Ok(oid) => oid,
        Err(e) => return Ok(blocked(log, e.to_string())),
    };

    let merge_base = match git.merge_base("HEAD", &branch_oid) {
        Ok(mb) => mb,
        Err(e) => return Ok(blocked(log, e.to_string())),
    };
    // Everything below names the pinned oid, so the content that gets merged
    // and the message that gets committed come from the same commit.
    let range = format!("{merge_base}..{branch_oid}");
    log.push(step_line(format!("merge-base {base}..{branch}")));
    log.push(note_line(format!("  {merge_base}")));

    let commits = git.run_ok(&["log", "--reverse", "--oneline", "--no-decorate", &range, "--"])?;
    let commit_lines: Vec<&str> = commits.lines().collect();
    if commit_lines.is_empty() {
        log.push(err_line(format!("refusing: {branch} has no commits beyond {base}")));
        return Ok(Landing { log, outcome: Outcome::Nothing });
    }
    log.push(step_line(format!("commits to squash ({})", commit_lines.len())));
    for c in &commit_lines {
        log.push(note_line(format!("  {c}")));
    }

    // Build the message before the merge, so a blank one refuses while the
    // work tree is still pristine. Bytes, not text: it is committed verbatim.
    let message = git.message_bytes(&range)?;
    if message.iter().all(u8::is_ascii_whitespace) {
        return Ok(blocked(log, format!("{branch}'s commits carry an empty message")));
    }

    log.push(step_line(format!("git merge --squash {branch}")));
    let merge = git.run(&["merge", "--squash", &branch_oid])?;
    log_output(&mut log, &merge);

    // Mirror the alias: SQUASH_MSG is written after the merge attempt whether
    // or not it succeeded, so a hand-finished conflict still commits with the
    // branch's message rather than git's auto-template.
    let squash_msg = match git.git_dir() {
        Ok(dir) => {
            let path = dir.join("SQUASH_MSG");
            match fs::write(&path, &message) {
                Ok(()) => Some(path),
                Err(e) => {
                    log.push(err_line(format!("could not write {}: {e}", path.display())));
                    None
                }
            }
        }
        Err(e) => {
            log.push(err_line(format!("could not locate the git dir: {e}")));
            None
        }
    };

    if !merge.ok {
        // Only unmerged index entries are a conflict. A rejected merge with a
        // clean index (merge.ff=only, a bad ref, a locked index) is a plain
        // failure, and must not be offered a destructive "discard" remedy.
        let unmerged = git.run(&["diff", "--name-only", "--diff-filter=U"])?;
        if unmerged.ok && unmerged.line().is_empty() {
            let why = merge.failure();
            log.push(err_line(format!("merge failed: {why}")));
            log.push(note_line("  the work tree was not modified"));
            return Ok(Landing { log, outcome: Outcome::Failed(why) });
        }
        log.push(err_line(
            "merge --squash hit conflicts — resolve them, or discard with reset --hard".to_string(),
        ));
        if squash_msg.is_some() {
            log.push(note_line(
                "  SQUASH_MSG holds the branch's message for the hand-finished commit",
            ));
        }
        return Ok(Landing { log, outcome: Outcome::Conflict });
    }

    // `git diff --cached --quiet` exits non-zero when something is staged.
    let staged = git.run(&["diff", "--cached", "--quiet"])?;
    if staged.ok {
        log.push(err_line(format!(
            "nothing staged: {branch} changes nothing on top of {base} (already landed?)"
        )));
        let reset = git.run(&["reset", "--hard", "HEAD"])?;
        if !reset.ok {
            log.push(err_line(format!("  reset --hard failed: {}", reset.failure())));
        }
        return Ok(Landing { log, outcome: Outcome::Nothing });
    }

    let Some(squash_msg) = squash_msg else {
        return Ok(staged_failure(log, "the squash message could not be written".to_string()));
    };

    // Last look before the commit: the squash is staged against `head_before`,
    // and committing onto a base that moved would bury the new commit.
    match git.rev_parse("HEAD") {
        Ok(now) if now == head_before => {}
        Ok(now) => {
            // NOT `reset --hard head_before`: that rewinds past whatever just
            // landed. Dropping the staged squash keeps it.
            return Ok(staged_failure_with(
                log,
                format!(
                    "{base} moved to {} while the squash was staged on {}",
                    short(&now),
                    short(&head_before)
                ),
                "do NOT commit this by hand — its tree predates the move. \
                 Discard with reset --hard HEAD and land again",
            ))
        }
        Err(e) => return Ok(staged_failure(log, e.to_string())),
    }

    // What the index holds now. The commit is checked against this, so content
    // a `pre-commit` hook (or a concurrent `git add`) slips in cannot ride
    // along unreviewed.
    let staged_tree = match git.run(&["write-tree"]) {
        Ok(run) if run.ok => run.line().to_string(),
        Ok(run) => return Ok(staged_failure(log, run.failure())),
        Err(e) => return Ok(staged_failure(log, e.to_string())),
    };
    // And that index is the merge result the review pane diffed. Only checked
    // when git can compute it: Conflicted cannot reach here, and Unavailable
    // means the pane already said so.
    if let MergeResult::Clean(previewed) = git.merge_tree(&head_before, &branch_oid)? {
        if previewed != staged_tree {
            return Ok(staged_failure_with(
                log,
                format!(
                    "the staged tree {} is not the merge result {} that was reviewed",
                    short(&staged_tree),
                    short(&previewed)
                ),
                "do NOT commit this by hand — discard with reset --hard HEAD",
            ));
        }
    }

    log.push(step_line("git commit (message prefilled from the branch)"));
    let path = squash_msg.to_string_lossy().into_owned();
    let commit = git.run(&["commit", "--cleanup=whitespace", "-F", &path])?;
    log_output(&mut log, &commit);
    if !commit.ok {
        return Ok(staged_failure(log, commit.failure()));
    }

    // git commit is not a compare-and-swap. Read the new commit and its parent
    // in ONE invocation, so the pair cannot come from two different states, and
    // accept it only if it is the one this run built: our parent and our tree.
    // All three are literals, so no --end-of-options: rev-parse echoes it back
    // as a result and would shift the fields.
    let head = git.run(&["rev-parse", "HEAD", "HEAD^", "HEAD^{tree}"])?;
    let mut fields = head.stdout.lines().map(str::trim);
    let (true, Some(sha), Some(parent), Some(tree)) =
        (head.ok, fields.next(), fields.next(), fields.next())
    else {
        return Ok(staged_failure(log, format!("committed, but unreadable: {}", head.failure())));
    };
    if parent != head_before || tree != staged_tree {
        // Undo only what this run created: HEAD^ keeps whatever arrived in
        // between, which `reset --hard head_before` would throw away. Left
        // standing, the commit is clean-tree — no prompt would offer to
        // remove it — and the next landing would publish it.
        let (sha, parent, tree) = (sha.to_string(), parent.to_string(), tree.to_string());
        let undo = git.run(&["reset", "--hard", "HEAD^"])?;
        log_output(&mut log, &undo);
        let recovery = if undo.ok {
            format!("rolled back to {}", short(&parent))
        } else {
            format!("ROLLBACK FAILED ({}) — reset --hard {} by hand", undo.failure(), short(&parent))
        };
        let why = if parent != head_before {
            format!(
                "raced: {}'s parent is {} but the squash was staged on {} — it would revert \
                 what landed in between",
                short(&sha),
                short(&parent),
                short(&head_before)
            )
        } else {
            format!(
                "{} carries tree {}, not the staged {} — something changed it during the commit",
                short(&sha),
                short(&tree),
                short(&staged_tree)
            )
        };
        let why = format!("{why}. NOT published; {recovery}");
        log.push(err_line(format!("failed: {why}")));
        return Ok(Landing { log, outcome: Outcome::Failed(why) });
    }
    let sha = sha.to_string();
    let subject = git
        .run(&["log", "-1", "--format=%s"])
        .map(|r| r.line().to_string())
        .unwrap_or_default();
    log.push(ok_line(format!("committed {}  {subject}", short(&sha))));
    Ok(Landing { log, outcome: Outcome::Committed { sha } })
}

fn blocked(mut log: Vec<Line>, why: String) -> Landing {
    log.push(err_line(format!("refusing: {why}")));
    Landing { log, outcome: Outcome::Blocked(why) }
}

fn staged_failure(log: Vec<Line>, why: String) -> Landing {
    staged_failure_with(log, why, "the squash is still staged — commit by hand, or discard with reset --hard")
}

/// Same, for the failures where finishing the commit by hand is the WRONG move:
/// the staged tree is not the one that was approved.
fn staged_failure_with(mut log: Vec<Line>, why: String, remedy: &str) -> Landing {
    log.push(err_line(format!("failed: {why}")));
    log.push(err_line(remedy.to_string()));
    Landing { log, outcome: Outcome::Failed(why) }
}

pub fn short(sha: &str) -> String {
    sha.chars().take(12).collect()
}

/// Throw away a conflicted or staged squash, as the alias's bail-out does.
pub fn discard(git: &Git) -> io::Result<Vec<Line>> {
    let mut log = vec![step_line("git reset --hard HEAD")];
    let reset = git.run(&["reset", "--hard", "HEAD"])?;
    log_output(&mut log, &reset);
    if !reset.ok {
        log.push(err_line(format!("reset failed: {}", reset.failure())));
        return Ok(log);
    }
    // reset --hard leaves untracked rename-conflict copies behind, and preflight
    // would then block the next landing with no explanation.
    let leftovers = match git.dirty_entries() {
        Ok(l) => l,
        Err(e) => {
            log.push(Line::new(
                format!("reset ran, but the tree could not be checked ({e})"),
                Style::fg(YELLOW),
            ));
            return Ok(log);
        }
    };
    if leftovers.is_empty() {
        log.push(ok_line("discarded — working tree back at HEAD"));
    } else {
        log.push(Line::new(
            format!("reset, but {} untracked leftover(s) remain:", leftovers.len()),
            Style::fg(YELLOW),
        ));
        for l in leftovers.iter().take(10) {
            log.push(note_line(format!("  {l}")));
        }
    }
    Ok(log)
}

/// Outcome of publishing the base branch.
pub struct Pushed {
    pub log: Vec<Line>,
    pub all_ok: bool,
    /// Remotes the operation actually reached. Empty means nothing was
    /// published or deleted, however green the individual steps looked — and
    /// on a partial push it names exactly the remotes that do have the work.
    pub reached: Vec<String>,
}

impl Pushed {
    pub fn count(&self) -> usize {
        self.reached.len()
    }
}

/// Never deleted whatever `--base` says: an integration branch is one typo away
/// from a mirror that has no `HEAD` ref to consult.
const PROTECTED: [&str; 2] = ["main", "master"];

/// A pushable remote carrying a branch, pinned to the tip we last fetched.
pub struct DeleteTarget {
    pub remote: String,
    pub oid: String,
}

/// Split `targets` into those at `landed_oid` and those that have diverged.
/// Remotes are matched by branch NAME, so a same-named branch on another remote
/// can hold commits the landing never took — those are not ours to delete.
fn partition_landed(
    targets: Vec<DeleteTarget>,
    landed_oid: &str,
) -> (Vec<DeleteTarget>, Vec<DeleteTarget>) {
    targets.into_iter().partition(|t| t.oid == landed_oid)
}

/// What a delete of `short` would touch: (deletable, diverged). `only` limits the
/// search to the remotes it names; `landed_oid` filters out remotes whose tip the
/// landing never took. The single place both the TUI and the CLI decide this.
pub fn delete_plan(
    git: &Git,
    short: &str,
    only: Option<&[String]>,
    landed_oid: Option<&str>,
) -> io::Result<(Vec<DeleteTarget>, Vec<DeleteTarget>)> {
    let targets = remotes_carrying(git, short, only)?;
    Ok(match landed_oid {
        Some(oid) => partition_landed(targets, oid),
        None => (targets, Vec::new()),
    })
}

/// Pushable remotes that currently carry `short`, each with the tip our
/// remote-tracking ref records. That oid becomes the delete's lease.
fn remotes_carrying(
    git: &Git,
    short: &str,
    only: Option<&[String]>,
) -> io::Result<Vec<DeleteTarget>> {
    let mut out = Vec::new();
    for (name, url) in git.remotes()? {
        if url == NO_PUSH || only.is_some_and(|rs| !rs.contains(&name)) {
            continue;
        }
        if let Some(oid) = git.remote_branch_oid(&name, short)? {
            out.push(DeleteTarget { remote: name, oid });
        }
    }
    Ok(out)
}

/// Delete branch `short` from every pushable remote that has it. Deleting a
/// published ref is irreversible for anyone who has not fetched it: a
/// hand-picked branch (`D`, `--delete`) is gated behind an explicit
/// confirmation, and the post-push sweep by the checks in `delete_plan` plus
/// the per-target lease below.
pub fn delete_branch(
    git: &Git,
    base: &str,
    short: &str,
    targets: &[DeleteTarget],
) -> io::Result<Pushed> {
    let mut log = Vec::new();
    // A bare remote also refuses to delete its checked-out branch, but that
    // refusal is a network round trip away; refuse locally and name the reason.
    let reason = if short.is_empty() {
        Some("it has no branch name".to_string())
    } else if short == base {
        Some("it is the base branch".to_string())
    } else if short == "HEAD" {
        Some("it is not a branch".to_string())
    } else if PROTECTED.contains(&short) {
        Some(format!("'{short}' is never deleted by this tool"))
    } else {
        // A remote's default branch need not be this run's --base, and
        // receive.denyDeleteCurrent is a server-side default we do not control.
        // An unanswerable remote refuses: "unknown" is the state in which the
        // branch could be exactly the one that must not go.
        targets.iter().find_map(|t| match git.head_claim(&t.remote, short) {
            Ok(HeadClaim::Default) => Some(format!("it is {}'s default branch", t.remote)),
            // Never the URL: a push URL routinely carries a credential.
            Ok(HeadClaim::Unnamed) => Some(format!(
                "{} reported HEAD without naming a branch, and it could be this one",
                t.remote
            )),
            Ok(HeadClaim::NotDefault) => None,
            Err(e) => Some(format!("{}'s default branch could not be established ({e})", t.remote)),
        })
    };
    if let Some(reason) = reason {
        log.push(err_line(format!("refusing to delete '{short}': {reason}")));
        return Ok(Pushed { log, all_ok: false, reached: Vec::new() });
    }
    if targets.is_empty() {
        log.push(Line::new(format!("no pushable remote carries {short}"), Style::fg(YELLOW)));
        return Ok(Pushed { log, all_ok: true, reached: Vec::new() });
    }
    // Fully qualified: an unqualified name is ambiguous when the remote also
    // carries a tag of the same name.
    let refname = format!("refs/heads/{short}");
    let delete = format!(":{refname}");
    let mut all_ok = true;
    let mut reached = Vec::new();
    for t in targets {
        // The lease is the whole safety story: if anyone pushed to the branch
        // since our last fetch, the remote tip no longer matches and git
        // refuses rather than dropping work nobody here has seen.
        let lease = format!("--force-with-lease={refname}:{}", t.oid);
        let name = &t.remote;
        log.push(step_line(format!("==> {name}  git push {lease} {name} {delete}")));
        let del = git.run(&["push", &lease, name, &delete])?;
        log_output(&mut log, &del);
        if del.ok {
            reached.push(name.clone());
            log.push(ok_line(format!("  deleted {short} from {name}")));
        } else {
            all_ok = false;
            log.push(err_line(format!("  delete on {name} failed: {}", del.failure())));
        }
    }
    Ok(Pushed { log, all_ok, reached })
}

/// Push the just-committed `sha` to every remote, skipping the `no_push` ones
/// — what `git pushall` does, with the refspec spelled out rather than left to
/// `push.default`, and pinned to the reviewed commit rather than to HEAD.
pub fn push_all(
    git: &Git,
    base: &str,
    sha: &str,
    remotes: &[(String, String)],
) -> io::Result<Pushed> {
    let mut log = Vec::new();

    // HEAD may have moved during the confirmation; publish only what was
    // approved, and only if the local base still names it.
    let current = git.rev_parse(&format!("refs/heads/{base}"))?;
    if current != sha {
        log.push(err_line(format!(
            "refusing to push: {base} is now {} but {} was approved",
            short(&current),
            short(sha)
        )));
        return Ok(Pushed { log, all_ok: false, reached: Vec::new() });
    }

    if remotes.is_empty() {
        log.push(Line::new("no remotes configured — nothing to publish", Style::fg(YELLOW)));
        return Ok(Pushed { log, all_ok: true, reached: Vec::new() });
    }
    let refspec = format!("{sha}:refs/heads/{base}");
    let mut all_ok = true;
    let mut reached = Vec::new();
    for (name, url) in remotes {
        if url.as_str() == NO_PUSH {
            log.push(note_line(format!("==> {name} (skipped: {NO_PUSH})")));
            continue;
        }
        log.push(step_line(format!("==> {name}  git push {name} {refspec}")));
        let push = git.run(&["push", name, &refspec])?;
        log_output(&mut log, &push);
        if push.ok {
            reached.push(name.clone());
            log.push(ok_line(format!("  pushed to {name}")));
        } else {
            all_ok = false;
            log.push(err_line(format!("  push to {name} failed: {}", push.failure())));
        }
    }
    if reached.is_empty() && all_ok {
        log.push(Line::new(
            "every remote is marked no_push — the commit is local only",
            Style::fg(YELLOW),
        ));
    }
    Ok(Pushed { log, all_ok, reached })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(remote: &str, oid: &str) -> DeleteTarget {
        DeleteTarget { remote: remote.to_string(), oid: oid.to_string() }
    }

    #[test]
    fn a_same_named_branch_at_another_oid_is_not_ours_to_delete() {
        let (landed, diverged) = partition_landed(
            vec![target("origin", "aaa"), target("backup", "bbb"), target("mirror", "aaa")],
            "aaa",
        );
        assert_eq!(
            landed.iter().map(|t| t.remote.as_str()).collect::<Vec<_>>(),
            vec!["origin", "mirror"]
        );
        assert_eq!(
            diverged.iter().map(|t| t.remote.as_str()).collect::<Vec<_>>(),
            vec!["backup"]
        );
    }
}

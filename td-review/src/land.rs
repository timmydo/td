//! Preview a branch against the base and take it onto the base — squashed into
//! one commit carrying the messages its own commits do, or those commits
//! replayed — then push the result to every remote.

use std::fs;
use std::io;

use crate::git::{Git, HeadClaim, MergeResult, NO_PUSH, SKIP_PUSH_ALL};
use crate::term::{Line, Style, CYAN, GREEN, RED, YELLOW};

/// How a branch is taken onto the base.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// One commit carrying the whole branch, its messages concatenated.
    Squash,
    /// The branch's own commits, replayed onto the base tip.
    Rebase,
}

impl Mode {
    pub fn verb(self) -> &'static str {
        match self {
            Mode::Squash => "squash",
            Mode::Rebase => "rebase",
        }
    }
}

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
    /// The same commits with their messages and paths, for the review-record
    /// scan the pane annotates them with. A replay lands each one, so the
    /// record is per commit rather than per branch.
    pub records: Vec<crate::record::Commit>,
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
    // Not fatal to the review: an unreadable batch leaves the record UNKNOWN
    // (the pane compares this against `commits`), rather than refusing the diff.
    let records = git.commit_records(&range).unwrap_or_default();

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
                "{branch} does not merge cleanly onto {base} — a squash (s) will stop at \
                 the conflict and a replay (r) refuses, having no reviewed tree to hold \
                 itself to. Shown below is the BRANCH's own diff, not the landing."
            )),
        ),
        MergeResult::Unavailable(why) => (
            git.run_ok(&["diff", "--stat", &range, "--"])?,
            git.run_ok(&["diff", &range, "--"])?,
            Some(format!(
                "the merge result could not be computed ({why}) — shown below is the \
                 BRANCH's own diff, which can differ from what a landing takes"
            )),
        ),
    };
    Ok(Preview { branch_oid, base_oid, merge_base, commits, records, message, stat, diff, note })
}

/// True while the sequencer holds a stopped cherry-pick or revert.
///
/// A replay stopped by a CONFLICT leaves a dirty tree, which `preflight` already
/// refuses on. One stopped by a commit that replayed to nothing does not: the
/// tree is clean and only this says so. Both matter, because whoever aborts an
/// inherited sequence rewinds the base to wherever that sequence began — past
/// commits this run never made.
fn replay_in_progress(git: &Git) -> bool {
    if git.run(&["rev-parse", "--verify", "--quiet", "CHERRY_PICK_HEAD"]).is_ok_and(|r| r.ok) {
        return true;
    }
    git.git_dir().is_ok_and(|dir| dir.join("sequencer").exists())
}

/// Refuse to land unless the work tree is clean, the base branch is checked out
/// and no sequencer operation is already under way — the two guards `git
/// squash-in` opens with, the branch check the integrator otherwise makes by
/// eye, and the one a replay adds.
pub fn preflight(git: &Git, base: &str) -> io::Result<Result<(), String>> {
    // Before the tree check, because this is the state a clean tree hides. A
    // landing that started here would abort a sequence it did not begin, and
    // `--abort` rewinds to where that one started, not to where we found it.
    if replay_in_progress(git) {
        return Ok(Err(
            "a cherry-pick or revert is already in progress — finish it, or \
             `git cherry-pick --abort`, before landing"
                .to_string(),
        ));
    }
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

/// Take `branch` onto the checked-out base, `mode` deciding whether that is one
/// squash commit or the branch's own commits replayed. Leaves HEAD untouched
/// unless it reports `Committed`.
///
/// `expected_oid` is the tip the integrator actually reviewed; if the ref has
/// moved since, the landing is refused rather than committing unreviewed work.
pub fn land(
    git: &Git,
    base: &str,
    branch: &str,
    mode: Mode,
    expected_oid: Option<&str>,
    expected_base: Option<&str>,
) -> io::Result<Landing> {
    let pinned = match pin(git, base, branch, mode, expected_oid, expected_base)? {
        Ok(p) => p,
        Err(landing) => return Ok(landing),
    };
    match mode {
        Mode::Squash => squash(git, base, branch, pinned),
        Mode::Rebase => rebase(git, base, branch, pinned),
    }
}

/// What a landing acts on, pinned to object ids and checked against the commits
/// the review pane was rendered from.
struct Pinned {
    log: Vec<Line>,
    branch_oid: String,
    /// The base tip the landing builds on, verified again once it is over.
    head_before: String,
    range: String,
    commits: Vec<String>,
}

/// The guards both modes share: a clean work tree with the base checked out, a
/// branch and a base still at the commits that were reviewed, and a range with
/// commits in it. `Err` carries the refusal, already logged.
fn pin(
    git: &Git,
    base: &str,
    branch: &str,
    mode: Mode,
    expected_oid: Option<&str>,
    expected_base: Option<&str>,
) -> io::Result<Result<Pinned, Landing>> {
    let mut log = Vec::new();

    if let Err(why) = preflight(git, base)? {
        log.push(err_line(format!("refusing: {why}")));
        return Ok(Err(Landing { log, outcome: Outcome::Blocked(why) }));
    }

    let branch_oid = match git.branch_oid(branch) {
        Ok(oid) => oid,
        Err(e) => return Ok(Err(blocked(log, e.to_string()))),
    };
    if let Some(expected) = expected_oid {
        if expected != branch_oid {
            return Ok(Err(blocked(
                log,
                format!(
                    "{branch} moved since it was reviewed ({} -> {}) — review it again",
                    short(expected),
                    short(&branch_oid)
                ),
            )));
        }
    }

    if let Some(expected) = expected_base {
        let current = match git.rev_parse(&format!("refs/heads/{base}")) {
            Ok(oid) => oid,
            Err(e) => return Ok(Err(blocked(log, e.to_string()))),
        };
        if expected != current {
            return Ok(Err(blocked(
                log,
                format!(
                    "{base} moved since the review ({} -> {}) — the {} would differ",
                    short(expected),
                    short(&current),
                    mode.verb()
                ),
            )));
        }
    }

    // The parent the landing is built on, re-checked before the commit and
    // verified after it: a base that advances mid-landing would otherwise get a
    // commit whose tree predates it, reverting what arrived in between.
    let head_before = match git.rev_parse("HEAD") {
        Ok(oid) => oid,
        Err(e) => return Ok(Err(blocked(log, e.to_string()))),
    };

    let merge_base = match git.merge_base("HEAD", &branch_oid) {
        Ok(mb) => mb,
        Err(e) => return Ok(Err(blocked(log, e.to_string()))),
    };
    // Everything below names the pinned oid, so the content that gets taken and
    // the messages that get committed come from the same commit.
    let range = format!("{merge_base}..{branch_oid}");
    log.push(step_line(format!("merge-base {base}..{branch}")));
    log.push(note_line(format!("  {merge_base}")));

    let out = git.run_ok(&["log", "--reverse", "--oneline", "--no-decorate", &range, "--"])?;
    let commits: Vec<String> = out.lines().map(str::to_string).collect();
    if commits.is_empty() {
        log.push(err_line(format!("refusing: {branch} has no commits beyond {base}")));
        return Ok(Err(Landing { log, outcome: Outcome::Nothing }));
    }
    let noun = match mode {
        Mode::Squash => "squash",
        Mode::Rebase => "replay",
    };
    log.push(step_line(format!("commits to {noun} ({})", commits.len())));
    for c in &commits {
        log.push(note_line(format!("  {c}")));
    }

    Ok(Ok(Pinned { log, branch_oid, head_before, range, commits }))
}

/// Stage the whole branch as one commit and commit it with the branch's own
/// commit messages.
fn squash(git: &Git, base: &str, branch: &str, pinned: Pinned) -> io::Result<Landing> {
    let Pinned { mut log, branch_oid, head_before, range, .. } = pinned;

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

/// Replay the branch's own commits onto the base with the sequencer, then check
/// that what they add up to is the tree the review pane diffed.
///
/// All of them or none. A replay that stops has already committed the commits
/// before the one that stopped it, which is a state the squash path cannot
/// reach — a staged squash never moved HEAD — so it is undone here rather than
/// left for the operator: half a branch on the base is work nobody approved as
/// a set, and it is one `p` from being published.
fn rebase(git: &Git, base: &str, branch: &str, pinned: Pinned) -> io::Result<Landing> {
    let Pinned { mut log, branch_oid, head_before, range, commits } = pinned;

    // cherry-pick replays one parent's changes; a merge commit has two and
    // nothing in the range says which. Squashing takes the same tree without
    // the ambiguity.
    let merges = git.run_ok(&["rev-list", "--merges", &range, "--"])?;
    let merges = merges.lines().count();
    if merges > 0 {
        return Ok(blocked(
            log,
            format!(
                "{branch} carries {merges} merge commit(s) — replaying them is ambiguous; \
                 squash it instead"
            ),
        ));
    }

    // The tree the review pane diffed, and what the replay has to add up to.
    let target = match git.merge_tree(&head_before, &branch_oid)? {
        MergeResult::Clean(tree) => tree,
        // The pane showed the BRANCH's own diff and said so, because there is
        // no landing tree to show. A squash cannot land that blind — the merge
        // conflicts too, and it stops — but a replay's successive merges can
        // resolve where the one-shot one does not, and would then commit work
        // whose diff nobody has seen. Refused, not risked.
        MergeResult::Conflicted => {
            return Ok(blocked(
                log,
                format!(
                    "{branch} does not merge cleanly onto {base}, so there is no reviewed \
                     tree to hold a replay to — rebase it onto {base} and review it again"
                ),
            ))
        }
        // Git could not compute one at all. The squash path lands blind here
        // and its confirmation is the human check on that; `r` has no
        // confirmation, so there is nobody in the loop to be blind for it.
        // Nothing to hold the result to means nothing lands.
        MergeResult::Unavailable(why) => {
            return Ok(blocked(
                log,
                format!(
                    "the merge result onto {base} could not be computed ({why}), so \
                     there is no reviewed tree to hold a replay to — land it \
                     squashed instead"
                ),
            ))
        }
    };
    let base_tree = match git.tree_of(&head_before) {
        Ok(tree) => tree,
        Err(e) => return Ok(blocked(log, e.to_string())),
    };
    // The squash path learns this from an empty index after the merge; here it
    // has to be known BEFORE the sequencer starts, because a commit that
    // replays to nothing stops it rather than being dropped.
    if target == base_tree {
        log.push(err_line(format!(
            "nothing to replay: {branch} changes nothing on top of {base} (already landed?)"
        )));
        return Ok(Landing { log, outcome: Outcome::Nothing });
    }

    // What `git rebase` replays: the branch's commits less the ones whose patch
    // the base already carries, by the same `--cherry-pick --right-only`
    // selection it makes. cherry-pick does no such filtering when handed a
    // range — it replays such a commit to nothing and stops the whole landing —
    // so the choice is made here, where it can be said out loud.
    let selection = git.run_ok(&[
        "rev-list",
        "--reverse",
        "--no-merges",
        "--cherry-pick",
        "--right-only",
        &format!("{head_before}...{branch_oid}"),
        "--",
    ])?;
    let picks: Vec<String> = selection.lines().map(str::to_string).collect();
    if picks.len() < commits.len() {
        log.push(note_line(format!(
            "  {} of {} commit(s) are already on {base} by patch and are skipped",
            commits.len().saturating_sub(picks.len()),
            commits.len()
        )));
    }
    if picks.is_empty() {
        log.push(err_line(format!(
            "nothing to replay: every commit on {branch} is already on {base} by patch"
        )));
        return Ok(Landing { log, outcome: Outcome::Nothing });
    }

    // --ff so a branch already sitting on the base tip lands the very commits
    // that were reviewed, oids and all, rather than rewritten copies of them.
    //
    // --allow-empty because `r` promises the branch's OWN commits and a
    // deliberately empty one is one of them; without it the sequencer stops on
    // it and the whole landing aborts. It only covers commits that were empty
    // to begin with: one that BECOMES empty against this base still stops,
    // which is the case that must not be quietly dropped.
    let mut argv: Vec<&str> = vec!["cherry-pick", "--ff", "--allow-empty"];
    argv.extend(picks.iter().map(String::as_str));
    log.push(step_line(format!(
        "git cherry-pick --ff --allow-empty ({} commit(s))",
        picks.len()
    )));
    let pick = git.run(&argv)?;
    log_output(&mut log, &pick);
    if !pick.ok {
        return abort_replay(git, log, base, branch, &head_before, pick.failure());
    }

    let head_after = match git.rev_parse("HEAD") {
        Ok(oid) => oid,
        Err(e) => return Ok(left_standing(log, e.to_string(), &head_before)),
    };
    if head_after == head_before {
        // The merge result differs from the base tree — checked above — so a
        // clean replay had to produce something. NOT `blocked`: that reads as
        // "nothing was touched", and what this actually says is that a step
        // which reported success did not do what success means.
        let why = format!("cherry-pick reported success but {base} did not move");
        log.push(err_line(format!("failed: {why}")));
        return Ok(Landing { log, outcome: Outcome::Failed(why) });
    }
    // Both structural checks come before any rollback: only once the base still
    // descends from where it started, by exactly the commits that were picked,
    // is `head_before..HEAD` known to be ours alone to remove. They report
    // rather than propagate, as does everything below — the commits are on the
    // base now, and an error out of here would tear the TUI down with them
    // landed and nothing on screen saying so.
    if !git.contains(&head_after, &head_before).unwrap_or(false) {
        return Ok(left_standing(
            log,
            format!(
                "{base} is now {}, which does not descend from the {} the replay started on",
                short(&head_after),
                short(&head_before)
            ),
            &head_before,
        ));
    }
    // A picked commit that replays to nothing stops the sequence rather than
    // being dropped from it, so every pick made exactly one commit. Anything
    // else means something committed here that we did not — and a count that
    // could not be read is a different state from one that came out wrong.
    let why = match git.run(&["rev-list", "--count", &format!("{head_before}..{head_after}"), "--"])
    {
        Ok(run) if !run.ok => Some(format!("{base}'s new commits could not be counted: {}", run.failure())),
        Err(e) => Some(format!("{base}'s new commits could not be counted: {e}")),
        Ok(run) if run.line() != picks.len().to_string() => Some(format!(
            "{} commit(s) were replayed but {base} gained {}",
            picks.len(),
            run.line()
        )),
        Ok(_) => None,
    };
    if let Some(why) = why {
        return Ok(left_standing(log, why, &head_before));
    }

    let tree = match git.rev_parse(&format!("{head_after}^{{tree}}")) {
        Ok(tree) => tree,
        Err(e) => return Ok(left_standing(log, e.to_string(), &head_before)),
    };
    // Every commit applied cleanly and the total is still not what the pane
    // diffed: successive three-way merges resolved something the one-shot merge
    // does not, or a hook rewrote content on the way through. Either way it is
    // not what was approved. Unconditional now — every path that reaches here
    // has a reviewed tree, because the ones that would not have refused above.
    if tree != target {
        return Ok(roll_back(
            git,
            log,
            format!(
                "the replayed tree {} is not the merge result {} that was reviewed",
                short(&tree),
                short(&target)
            ),
            &head_before,
        ));
    }

    log.push(ok_line(format!(
        "replayed {} commit{} onto {base} — now at {}",
        picks.len(),
        if picks.len() == 1 { "" } else { "s" },
        short(&head_after)
    )));
    Ok(Landing { log, outcome: Outcome::Committed { sha: head_after } })
}

/// Put a stopped replay back: the sequencer's own abort restores both the base
/// and the work tree, so the branch can be fixed and landed again rather than
/// half-landed.
fn abort_replay(
    git: &Git,
    mut log: Vec<Line>,
    base: &str,
    branch: &str,
    head_before: &str,
    why: String,
) -> io::Result<Landing> {
    log.push(err_line(format!("cherry-pick stopped: {why}")));
    // Only when there is one to abort: a pick that failed before the sequencer
    // started has nothing in progress, and `--abort` would answer that with a
    // `fatal:` of its own — reading as though the recovery were what failed.
    if replay_in_progress(git) {
        log.push(step_line("git cherry-pick --abort"));
        let abort = git.run(&["cherry-pick", "--abort"])?;
        log_output(&mut log, &abort);
    }
    let restored = git.rev_parse("HEAD").map(|head| head == head_before).unwrap_or(false);
    let clean = git.dirty_entries().map(|d| d.is_empty()).unwrap_or(false);
    if !restored || !clean {
        return Ok(left_standing(
            log,
            format!("the replay stopped ({why}) and the abort did not put {base} back"),
            head_before,
        ));
    }
    // Nothing was applied and nothing is left in the tree, so this reads like
    // the refusals rather than like a conflict to clean up: rebase the branch
    // onto the base, review it again, or land it squashed. The reason is
    // already on the log a line up; only the outcome repeats it, for the
    // headless caller that prints that alone.
    log.push(note_line(format!(
        "  {base} is back at {} — nothing was applied",
        short(head_before)
    )));
    Ok(Landing { log, outcome: Outcome::Blocked(format!("{branch} does not replay onto {base}: {why}")) })
}

/// Remove the commits a replay put on the base. Only reached once both
/// structural checks have passed, so `head_before..HEAD` is exactly what this
/// run added and nothing else goes with it.
fn roll_back(git: &Git, mut log: Vec<Line>, why: String, head_before: &str) -> Landing {
    log.push(step_line(format!("git reset --hard {}", short(head_before))));
    let reset = match git.run(&["reset", "--hard", head_before]) {
        Ok(run) => run,
        Err(e) => {
            let why = format!(
                "{why}. ROLLBACK FAILED ({e}) — reset --hard {} by hand",
                short(head_before)
            );
            log.push(err_line(why.clone()));
            return Landing { log, outcome: Outcome::Failed(why) };
        }
    };
    log_output(&mut log, &reset);
    let why = if reset.ok {
        format!("{why}. NOT published; rolled back to {}", short(head_before))
    } else {
        format!(
            "{why}. ROLLBACK FAILED ({}) — reset --hard {} by hand",
            reset.failure(),
            short(head_before)
        )
    };
    log.push(err_line(format!("failed: {why}")));
    Landing { log, outcome: Outcome::Failed(why) }
}

/// Report a replay whose commits are still on the base and must NOT be removed
/// from here: either the base no longer descends from where the replay started,
/// or what sits on it is not only ours.
fn left_standing(mut log: Vec<Line>, why: String, head_before: &str) -> Landing {
    log.push(err_line(format!("failed: {why}")));
    log.push(err_line(format!(
        "left standing, and NOT ours to remove — inspect `git log {}..HEAD` by hand",
        short(head_before)
    )));
    // The tree is clean, so no bail-out prompt will offer to remove them, and
    // `p` publishes every local commit on the base without asking.
    log.push(err_line(
        "and the next p WILL PUBLISH them — remove them by hand first".to_string(),
    ));
    Landing { log, outcome: Outcome::Failed(why) }
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

/// Throw away a conflicted or staged landing, as the alias's bail-out does.
/// `reset --hard` alone would leave a stopped replay's sequencer state behind,
/// and the commits it already applied with it, so the abort comes first.
pub fn discard(git: &Git) -> io::Result<Vec<Line>> {
    let mut log = Vec::new();
    if replay_in_progress(git) {
        log.push(step_line("git cherry-pick --abort"));
        let abort = git.run(&["cherry-pick", "--abort"])?;
        log_output(&mut log, &abort);
        // Not "discarded": the sequence and whatever it applied may both still
        // be there, and `reset --hard HEAD` below cannot see either.
        if !abort.ok {
            log.push(err_line(format!(
                "abort failed: {} — the stopped replay is still there, finish it by hand",
                abort.failure()
            )));
            return Ok(log);
        }
    }
    log.push(step_line("git reset --hard HEAD"));
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

/// Why a push-all left a remote alone.
#[derive(Debug)]
pub enum LeftOut {
    /// `remote.<name>.skipPushAll` asked to be.
    OptedOut,
    /// Its value is not a boolean at all, carrying git's own diagnostic — which
    /// names both the key and what it holds. Left out rather than pushed to:
    /// a stated intent nobody can read is not permission to push on a keystroke.
    Unreadable(String),
}

impl LeftOut {
    /// The whole line, remote and reason — the record that this remote was not
    /// published to.
    pub fn line(&self, remote: &str) -> String {
        match self {
            LeftOut::OptedOut => {
                format!("{remote} left out: remote.{remote}.{SKIP_PUSH_ALL} is set")
            }
            LeftOut::Unreadable(e) => format!("{remote} left out: {e}"),
        }
    }
}

/// Which remotes a push-all publishes to, and which of them it leaves alone.
pub struct PushAllTargets {
    /// Passed to [`push_all`] as they come from `git remote`, sentinel and all:
    /// the `no_push` skip is the push's own, and it reports it per target.
    pub targets: Vec<(String, String)>,
    /// Never a silent omission: the line naming the targets does not name what
    /// is missing from it, so nothing else would say the laptop was skipped.
    pub left_out: Vec<(String, LeftOut)>,
}

/// Split every configured remote into the ones a push-all takes and the ones
/// `remote.<name>.skipPushAll` excuses. The one place both the TUI's `P` and
/// the CLI's `--push` decide this; `p` and a hand-typed `git push <remote>`
/// name a remote and so go there whatever it has configured.
pub fn push_all_targets(git: &Git) -> io::Result<PushAllTargets> {
    let mut targets = Vec::new();
    let mut left_out = Vec::new();
    for (name, url) in git.remotes()? {
        match git.skips_push_all(&name) {
            Ok(false) => targets.push((name, url)),
            Ok(true) => left_out.push((name, LeftOut::OptedOut)),
            Err(e) => left_out.push((name, LeftOut::Unreadable(format!("{e}")))),
        }
    }
    Ok(PushAllTargets { targets, left_out })
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
        // Not "no remotes configured": since `skipPushAll` an empty target list
        // is as likely to be every remote opting out, and the caller has
        // already said which by name.
        log.push(Line::new("no remote to publish to", Style::fg(YELLOW)));
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

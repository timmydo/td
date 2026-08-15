//! ready — `td-builder ready`: the gate an agent runs before pushing a rolling
//! branch. Readiness lives in the branch rather than in a flag somebody sets
//! (AGENTS.md), so this reads it back out of the branch: the bounded checks over
//! the committed diff, plus the review record every commit not yet on the base
//! has to carry. Landing replays those commits one at a time, so the record is
//! per commit — a branch-wide claim would say nothing about the commit the
//! integrator actually stops at.

use std::path::Path;
use std::process::{Command, ExitCode};

use crate::affected;

const HELP: &str = "\
usage: td-builder ready [options]

  Gate a rolling branch before pushing it: run the bounded checks over the
  committed diff, and verify that every commit not yet on the base carries
  its review record (AGENTS.md, `Code review`).

options:
  --base <ref>    base to land on (default: origin/main, falling back to main)
  --record-only   scan the record only; do not run the checks
  -h, --help      this text
";

/// The reviewers a record may name. Closed on purpose: a misspelt reviewer is a
/// review nobody ran, and an open roster cannot tell those two apart.
pub const SUBAGENT: &str = "subagent";
pub const CLI_REVIEWERS: [&str; 3] = ["agy", "claude", "codex"];
/// The cross-model reviewer BOTH acting-agent rosters name. Whichever model is
/// acting, the other two reviews are Agy plus the model that is not the actor —
/// so "any two of three" would accept a Codex agent reviewed by Codex and
/// Claude, no Agy, which the roster does not allow.
const SHARED_CLI: &str = "agy";
/// The one waiver that stands in for all three, and only on a commit whose every
/// path is documentation — a claim this checks rather than believes.
pub const DOCS_ONLY: &str = "docs-only";

/// What one commit's message claims about its review.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Record {
    /// `(tool, model)` per `Reviewed-by:`, deduped by tool, first-seen order.
    /// The MODEL is kept because the rule is two distinct models, not two
    /// distinct commands: `subagent/opus-4.8` + `claude/opus-4.8` is one model
    /// reviewing itself through two front ends.
    pub reviewers: Vec<(String, String)>,
    /// `Review-waiver:` subjects paired with the reason given.
    pub waivers: Vec<(String, String)>,
    pub checks: Option<String>,
    /// Trailers of ours that did not parse — reported, never ignored.
    pub malformed: Vec<String>,
}

/// An unindented `Key: value` line, no spaces in the key. What keeps a message
/// that SHOWS the record from claiming it is that such a quote is indented (or
/// fenced, whose closing line is not a trailer) and so never reaches the
/// trailing block.
fn is_trailer_line(line: &str) -> bool {
    match line.split_once(':') {
        Some((key, _)) => {
            key.starts_with(|c: char| c.is_ascii_alphabetic())
                && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        }
        None => false,
    }
}

/// A line that looks like part of the record, wherever it sits. Used only to
/// tell "no review" apart from "a review that does not close the message".
fn is_record_line(line: &str) -> bool {
    is_trailer_line(line)
        && line.split_once(':').is_some_and(|(key, _)| {
            ["reviewed-by", "review-waiver", "checks"]
                .iter()
                .any(|k| key.eq_ignore_ascii_case(k))
        })
}

/// The trailing run of trailer lines. Only the LAST block counts, the way git's
/// own trailers do: a record quoted mid-body is prose, not a claim.
fn trailer_block(message: &str) -> Vec<&str> {
    let lines: Vec<&str> = message.lines().collect();
    let mut end = lines.len();
    while end > 0 && lines.get(end - 1).is_some_and(|l| l.trim().is_empty()) {
        end -= 1;
    }
    let mut start = end;
    while start > 0 && lines.get(start - 1).is_some_and(|l| is_trailer_line(l)) {
        start -= 1;
    }
    lines
        .get(start..end)
        .map(<[&str]>::to_vec)
        .unwrap_or_default()
}

/// `agy — CLI unavailable, approved by <who>` splits into the reviewer excused
/// and why. The reason is load-bearing for a reviewer waiver: one without it
/// records nothing a human could later disagree with.
fn split_waiver(value: &str) -> (String, String) {
    let mut parts = value.splitn(2, char::is_whitespace);
    // `agy: unavailable` and `agy — unavailable` name the same reviewer; the
    // punctuation belongs to the separator, not to the name, and keeping it
    // would report "unknown reviewer 'agy:'".
    let subject = parts
        .next()
        .unwrap_or_default()
        .trim()
        .trim_end_matches([':', '-', ','])
        .to_string();
    let reason = parts
        .next()
        .unwrap_or_default()
        .trim_start_matches(|c: char| c == '—' || c == '-' || c == ':' || c.is_whitespace())
        .trim()
        .to_string();
    (subject, reason)
}

pub fn parse_record(message: &str) -> Record {
    let mut rec = Record::default();
    for line in trailer_block(message) {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        // Case-insensitively, as git matches its own trailer tokens: a
        // `Reviewed-By:` that fell through would be reported as no review at
        // all, which sends an agent looking for the wrong thing.
        let key = key.to_ascii_lowercase();
        match key.as_str() {
            "reviewed-by" => match value.split_once('/') {
                Some((tool, model)) if !tool.trim().is_empty() && !model.trim().is_empty() => {
                    let tool = tool.trim().to_string();
                    let model = model.trim().to_string();
                    if !rec.reviewers.iter().any(|(t, _)| *t == tool) {
                        rec.reviewers.push((tool, model));
                    }
                }
                _ => rec
                    .malformed
                    .push(format!("`Reviewed-by: {value}` is not <tool>/<model>")),
            },
            "review-waiver" => {
                let (subject, reason) = split_waiver(value);
                if subject.is_empty() {
                    rec.malformed
                        .push("`Review-waiver:` names no reviewer".to_string());
                } else {
                    rec.waivers.push((subject, reason));
                }
            }
            "checks" if !value.is_empty() => rec.checks = Some(value.to_string()),
            _ => {}
        }
    }
    rec
}

/// Who approved this waiver, out of its reason. A waiver is not something an
/// agent may issue: it records the answer a HUMAN gave when asked, so the
/// answer has to name them. The gate cannot see that the question was asked —
/// naming the person who answered is the closest checkable thing to it.
fn approver(reason: &str) -> Option<&str> {
    let lower = reason.to_ascii_lowercase();
    let at = lower.find("approved by")?;
    let who = reason.get(at.saturating_add("approved by".len())..)?.trim();
    (!who.is_empty()).then_some(who)
}

fn known_reviewer(name: &str) -> bool {
    name == SUBAGENT || CLI_REVIEWERS.contains(&name)
}

/// The model families an identity may name — closed, like the tool roster, and
/// for the same reason: this has to answer WHICH MODEL looked, and a spelling it
/// does not recognise is one it cannot compare. Splitting on the first `-` was
/// not enough: `claude-opus-4.8` and `opus-4.8` are the same model, and that is
/// the spelling an agent naturally writes for its own subagent.
const MODELS: [&str; 6] = ["opus", "sonnet", "haiku", "fable", "gpt", "gemini"];

fn family(model: &str) -> Option<&'static str> {
    let lower = model.to_ascii_lowercase();
    MODELS.into_iter().find(|m| {
        lower
            .split(|c: char| !c.is_ascii_alphanumeric())
            .any(|token| token == *m)
    })
}

/// Everything wrong with one commit's record. Empty means the commit may land.
/// `paths` are the files the commit touches, which only the `docs-only` waiver
/// consults — it is the one claim here that the message alone cannot support.
pub fn problems(message: &str, paths: &[String]) -> Vec<String> {
    let rec = parse_record(message);
    let mut out = rec.malformed.clone();

    let docs_only = rec.waivers.iter().any(|(s, _)| s == DOCS_ONLY);
    for (subject, reason) in &rec.waivers {
        if subject == DOCS_ONLY {
            // An EMPTY path list is not agreement. `git show --name-only` prints
            // nothing for a merge, and a failed query prints nothing either —
            // this is the one place the gate checks reality instead of believing
            // the message, so "no evidence" must not read as "no code".
            if paths.is_empty() {
                out.push("waived as docs-only, but no paths could be read".to_string());
            } else if let Some(p) = paths
                .iter()
                .find(|p| !p.to_ascii_lowercase().ends_with(".md"))
            {
                out.push(format!("waived as docs-only, but it touches {p}"));
            }
        } else if subject == SUBAGENT {
            // The waiver exists for a reviewer CLI that is UNAVAILABLE. The
            // subagent is not a CLI and cannot be unavailable, so waiving it is
            // just declining the one review that always could have run.
            out.push("the subagent review cannot be waived".to_string());
        } else if !known_reviewer(subject) {
            out.push(format!("waives unknown reviewer '{subject}'"));
        } else if reason.is_empty() {
            out.push(format!("waiver for '{subject}' gives no reason"));
        } else if approver(reason).is_none() {
            out.push(format!(
                "waiver for '{subject}' names nobody who approved it — a waiver records \
                 a human's answer, so it ends `approved by <who>`"
            ));
        }
    }

    // One at most — a backstop, not an allowance. Nothing here can see whether
    // the human was actually asked, so what the gate CAN do is bound what a
    // waiver reaches: stacked ones get to "no reviews at all" in four extra
    // words, and a commit with two reviewers down is one to stop on anyway.
    let reviewer_waivers = rec
        .waivers
        .iter()
        .filter(|(s, _)| s != DOCS_ONLY)
        .count();
    if reviewer_waivers > 1 {
        out.push(format!(
            "{reviewer_waivers} reviewers waived at once — ask a human rather than record around it"
        ));
    }

    for (tool, model) in &rec.reviewers {
        if !known_reviewer(tool) {
            out.push(format!("unknown reviewer '{tool}'"));
        } else if family(model).is_none() {
            out.push(format!(
                "'{model}' is not a known model ({}) — the cross-model rule cannot compare it",
                MODELS.join("/")
            ));
        }
    }

    if !docs_only {
        // A waiver stands in for a review only with a reason; the roster check
        // above has already reported an unknown subject.
        // Only a waiver that names its approver stands in for a review.
        let waived =
            |who: &str| rec.waivers.iter().any(|(s, r)| s == who && approver(r).is_some());
        let model_of = |who: &str| {
            rec.reviewers
                .iter()
                .find(|(t, _)| t == who)
                .map(|(_, m)| m.as_str())
        };
        let have = |who: &str| model_of(who).is_some() || waived(who);
        if !have(SUBAGENT) {
            out.push("no subagent review".to_string());
        }
        if !have(SHARED_CLI) {
            out.push(format!(
                "no {SHARED_CLI} review — both acting-agent rosters name it"
            ));
        }
        // The remaining slot is the model that is NOT acting. Comparing tools
        // alone would accept `subagent/opus-4.8` + `claude/opus-4.8`: the same
        // model reviewing its own work through a second front end, which is the
        // exact blind spot the cross-model rule exists to cover. A waived slot
        // has no model to compare, and its reason is the record.
        let acting = model_of(SUBAGENT).and_then(family);
        let others: Vec<&str> = CLI_REVIEWERS
            .iter()
            .copied()
            .filter(|c| *c != SHARED_CLI)
            .collect();
        // An unrecognised model counts as NOT cross-model: it was reported just
        // above, and a spelling nothing can compare must not settle the check.
        let cross = others.iter().any(|c| match model_of(c) {
            Some(m) => family(m).is_some_and(|f| acting.is_none_or(|a| f != a)),
            None => waived(c),
        });
        if !cross {
            let named = others.join(" or ");
            match others.iter().find_map(|c| model_of(c)) {
                Some(m) => out.push(format!(
                    "the {named} review is {m}, the model that reviewed it as subagent — \
                     the second opinion must be the model that is not acting"
                )),
                None => out.push(format!("no cross-model review from {named}")),
            }
        }
    }

    if rec.checks.is_none() {
        out.push("no `Checks:` trailer".to_string());
    }

    // Only when something IS missing: a record that does not close the message
    // is invisible to the block scan (a `Next:` paragraph or a wrapped trailer
    // below it is enough), and reporting the reviews as absent would send an
    // agent looking for the wrong thing. A complete record plus a quoted example
    // elsewhere is not an error, which is why this is not checked unconditionally.
    if !out.is_empty() {
        let block = trailer_block(message);
        let stray = message
            .lines()
            .filter(|l| is_record_line(l) && !block.contains(l))
            .count();
        if stray > 0 {
            out.push(format!(
                "{stray} record line(s) do not close the message — the record must be the \
                 last block, one trailer per line (do not wrap them)"
            ));
        }
    }
    out
}

/// git's stdout, or `None` if it failed. A gate may not read a failed query as
/// an empty answer: that is how "rev-list broke" becomes "nothing to land".
fn git_try(root: &Path, args: &[&str]) -> Option<String> {
    match Command::new("git").args(args).current_dir(root).output() {
        Ok(o) if o.status.success() => Some(String::from_utf8_lossy(&o.stdout).to_string()),
        _ => None,
    }
}

fn git_out(root: &Path, args: &[&str]) -> String {
    git_try(root, args).unwrap_or_default()
}

/// The paths a commit touches, as the `docs-only` waiver must see them.
///
/// `--no-renames` because `--name-only` reports only the DESTINATION of a
/// detected rename, and this list is the one place the gate checks reality
/// rather than believing the message: `git mv builder/src/nar.rs notes.md`
/// reports `notes.md` alone, so every path ends in `.md` and a commit that
/// DELETED a Rust source waives all three reviews. Off, a rename is a delete
/// plus an add and both sides are seen.
fn commit_paths(root: &Path, oid: &str) -> Vec<String> {
    git_try(
        root,
        &["show", "--no-renames", "--name-only", "--format=", oid],
    )
    .map(|o| {
        o.lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(str::to_string)
            .collect::<Vec<String>>()
    })
    .unwrap_or_default()
}

fn current_branch(root: &Path) -> String {
    let b = git_out(root, &["rev-parse", "--abbrev-ref", "HEAD"])
        .trim()
        .to_string();
    if b.is_empty() {
        "HEAD".to_string()
    } else {
        b
    }
}

pub fn main(args: &[String]) -> ExitCode {
    let root = affected::resolve_root();
    let mut base = "origin/main".to_string();
    let mut record_only = false;

    let mut i = 0;
    while let Some(arg) = args.get(i) {
        match arg.as_str() {
            "--base" => {
                i += 1;
                match args.get(i) {
                    Some(v) => base = v.clone(),
                    None => {
                        eprintln!("ready: --base needs a ref");
                        return ExitCode::from(2);
                    }
                }
            }
            "--record-only" => record_only = true,
            "-h" | "--help" => {
                print!("{HELP}");
                return ExitCode::SUCCESS;
            }
            other => {
                eprintln!("ready: unknown arg '{other}'");
                eprint!("{HELP}");
                return ExitCode::from(2);
            }
        }
        i += 1;
    }

    // The fallback affected-checks makes, for the same reason: a clone without
    // an `origin` still has a main to measure against.
    if !affected::git_ok(&root, &["rev-parse", "--verify", &format!("{base}^{{commit}}")]) {
        if base == "origin/main"
            && affected::git_ok(&root, &["rev-parse", "--verify", "main^{commit}"])
        {
            base = "main".to_string();
        } else {
            eprintln!("ready: base ref '{base}' is not available");
            return ExitCode::from(2);
        }
    }

    // The tree is read BEFORE the commit count: an empty branch with edits in
    // it is the "forgot to commit" case, and "nothing to land" is the one
    // answer that helps nobody.
    // `--untracked-files=all` because `status.showUntrackedFiles=no` in anyone's
    // config would otherwise hide the very files this is here to catch, and it
    // expands directories the default collapses to one entry. `-z` because
    // porcelain v1 QUOTES a path with a space in it, and a quoted path is not
    // the path `selects_checks` would be asked about.
    let Some(status) = git_try(
        &root,
        &["status", "--porcelain", "-z", "--untracked-files=all"],
    ) else {
        eprintln!("ready: git status failed — refusing to guess at a clean tree");
        return ExitCode::from(2);
    };
    // `-z` entries are `XY <path>`. A rename adds a bare second path with no
    // status prefix; this drops it as the continuation it is, except for the
    // rare source path whose third byte is a space, which counts as one more
    // dirty entry — an over-count, which fails closed.
    let entries: Vec<&str> = status
        .split('\0')
        .filter(|e| e.len() > 3 && e.get(2..3) == Some(" "))
        .collect();
    let dirty = entries.iter().filter(|l| !l.starts_with("??")).count();
    // Untracked files are not in any commit and `--committed-only` cannot see
    // them, but a compile CAN: a module the branch references and nobody staged
    // makes the checks pass on a tree that will never exist anywhere else. Only
    // the ones that route to a check block; scratch files are a note.
    let untracked: Vec<&str> = entries
        .iter()
        .filter_map(|l| l.strip_prefix("?? "))
        .collect();
    let blocking: Vec<&&str> = untracked
        .iter()
        .filter(|p| affected::selects_checks(&root, p))
        .collect();

    let Some(rev_list) = git_try(&root, &["rev-list", "--reverse", &format!("{base}..HEAD")])
    else {
        eprintln!("ready: git rev-list {base}..HEAD failed");
        return ExitCode::from(2);
    };
    let commits: Vec<&str> = rev_list.lines().collect();

    // The checks below run on THIS tree; a replay runs on the base's. Nothing
    // here fetches — that is the agent's call — but a branch measured against a
    // base it is behind has been checked in a state that will not exist again.
    let behind = git_try(&root, &["rev-list", "--count", &format!("HEAD..{base}")])
        .and_then(|s| s.trim().parse::<usize>().ok())
        .unwrap_or(0);
    if commits.is_empty() && dirty == 0 && blocking.is_empty() {
        println!("ready: no commits over {base} — nothing to land");
        return ExitCode::SUCCESS;
    }

    let branch = current_branch(&root);
    println!("ready: {} commit(s) over {base} on {branch}", commits.len());

    let mut unready = 0usize;
    for oid in &commits {
        let short = oid.get(..8).unwrap_or(oid);
        // Parents and message in one query, NUL-separated: a body cannot contain
        // a NUL, so nothing a committer writes can move the boundary.
        // The encoding is pinned for the reason td-review pins it: the record
        // is read out of the bytes the commit carries, not out of whatever
        // `i18n.logOutputEncoding` asks for.
        let raw = git_try(
            &root,
            &["-c", "i18n.logOutputEncoding=UTF-8", "show", "-s", "--format=%P%x00%B", oid],
        );
        let Some((parents, message)) = raw.as_deref().and_then(|r| r.split_once('\0')) else {
            unready += 1;
            println!("  {short}  (could not be read)");
            continue;
        };
        let paths = commit_paths(&root, oid);
        let subject = message.lines().next().unwrap_or_default();
        let mut probs = problems(message, &paths);
        if parents.split_whitespace().count() > 1 {
            // Landing replays commits onto the base; td-review refuses a merge,
            // and `--name-only` shows a merge no paths, so a docs-only waiver on
            // one would go unchecked.
            probs.push("merge commit — a replay landing cannot take it".to_string());
        }
        println!("  {short}  {subject}");
        if !probs.is_empty() {
            unready += 1;
            for p in &probs {
                println!("            - {p}");
            }
        }
    }

    if dirty > 0 {
        println!("  {dirty} uncommitted change(s) to tracked files — in no commit's record");
    }
    for p in &blocking {
        println!("  untracked, and it routes to a check: {p}");
    }
    let scratch = untracked.len().saturating_sub(blocking.len());
    if scratch > 0 {
        println!("  note: {scratch} other untracked path(s), ignored here");
    }
    if behind > 0 {
        println!("  note: {base} is {behind} commit(s) ahead — rebase before pushing");
    }
    if !branch.ends_with("-rolling") && branch != "HEAD" {
        // A note, not a failure: the suffix governs what the integrator's sweep
        // keeps, not whether this work is landable.
        println!("  note: '{branch}' is not a -rolling branch (AGENTS.md, Parallel work)");
    }

    let checks = if record_only {
        0
    } else {
        println!();
        affected::run(&[
            "--committed-only".to_string(),
            "--run".to_string(),
            "--base".to_string(),
            base.clone(),
        ])
    };

    println!();
    let clean = dirty == 0 && blocking.is_empty();
    if unready == 0 && clean && checks == 0 {
        if record_only {
            // A different WORD, not a different exit status — `--record-only`
            // did what it was asked, so it exits 0 and a `&&` chain cannot tell
            // the two apart. The word is for the human reading the output.
            println!("RECORD OK: {} commit(s) over {base} — checks NOT run", commits.len());
        } else {
            println!("READY: {} commit(s) over {base}", commits.len());
        }
        return ExitCode::SUCCESS;
    }
    if unready > 0 {
        // Not "missing their record": a merge commit or one that could not be
        // read is counted here too, and neither is a missing record.
        println!("NOT READY: {unready} commit(s) not ready — see above");
    }
    if dirty > 0 {
        println!("NOT READY: {dirty} uncommitted change(s)");
    }
    if !blocking.is_empty() {
        println!(
            "NOT READY: {} untracked path(s) that route to a check",
            blocking.len()
        );
    }
    if checks != 0 {
        println!("NOT READY: checks exited {checks}");
    }
    ExitCode::FAILURE
}


#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    const COMPLETE: &str = "\
td-sh: something

A body.

Reviewed-by: subagent/opus-4.8
Reviewed-by: codex/gpt-5.6-sol
Reviewed-by: agy/gemini-3.1-pro
Checks: affected-checks --committed-only (green)
";

    fn code(paths: &[&str]) -> Vec<String> {
        paths.iter().map(|p| p.to_string()).collect()
    }

    fn probs(message: &str) -> Vec<String> {
        problems(message, &code(&["a.rs"]))
    }

    #[test]
    fn complete_record_is_ready() {
        assert!(problems(COMPLETE, &code(&["builder/src/ready.rs"])).is_empty());
    }

    #[test]
    fn subagent_alone_is_not() {
        let p = probs("s\n\nReviewed-by: subagent/opus-4.8\nChecks: green\n");
        assert!(p.iter().any(|s| s.starts_with("no agy review")));
        assert!(p.iter().any(|s| s.starts_with("no cross-model review")));
    }

    /// The roster is agy PLUS the model that is not acting, so two reviews that
    /// skip agy are not two cross-model reviews.
    #[test]
    fn agy_is_required_by_both_rosters() {
        let p = probs("s\n\nReviewed-by: subagent/opus-4.8\nReviewed-by: codex/gpt-5.6-sol\nReviewed-by: claude/opus-4.8\nChecks: g\n");
        assert_eq!(p.len(), 1);
        assert!(p.first().is_some_and(|s| s.starts_with("no agy review")));
    }

    /// The acting model reviewing its own work through a second front end is
    /// not a second opinion: `subagent/opus-4.8` + `claude/opus-4.8` is one
    /// model, and the blind spot the cross-model rule covers is exactly the one
    /// it would share with itself.
    #[test]
    fn the_acting_model_cannot_be_its_own_cross_model_review() {
        let m = "s\n\nReviewed-by: subagent/opus-4.8\nReviewed-by: claude/opus-4.8\n\
                 Reviewed-by: agy/gemini-3.1-pro\nChecks: g\n";
        let p = probs(m);
        assert_eq!(p.len(), 1);
        assert!(p.first().is_some_and(|s| s.contains("model that is not acting")), "{p:?}");

        // The same shape the other way round, which is the Codex actor's.
        let m = "s\n\nReviewed-by: subagent/gpt-5.6-sol\nReviewed-by: codex/gpt-5.6-sol\n\
                 Reviewed-by: agy/gemini-3.1-pro\nChecks: g\n";
        assert_eq!(probs(m).len(), 1);

        // A version apart is still the same model family.
        let m = "s\n\nReviewed-by: subagent/opus-4.8\nReviewed-by: claude/opus-4.7\n\
                 Reviewed-by: agy/gemini-3.1-pro\nChecks: g\n";
        assert_eq!(probs(m).len(), 1);

        // And the roster's own pairing passes.
        let m = "s\n\nReviewed-by: subagent/opus-4.8\nReviewed-by: codex/gpt-5.6-sol\n\
                 Reviewed-by: agy/gemini-3.1-pro\nChecks: g\n";
        assert!(probs(m).is_empty());
    }

    /// The same model spelled differently is still the same model. Each of
    /// these was accepted while `family()` was "everything before the first
    /// `-`", and the first is the spelling an agent naturally writes for its
    /// own subagent.
    #[test]
    fn spelling_does_not_get_round_the_same_model_check() {
        for cli in ["claude/opus-4.8", "claude/Opus-4.8", "claude/opus_4.8"] {
            let m = format!(
                "s\n\nReviewed-by: subagent/claude-opus-4.8\nReviewed-by: {cli}\n\
                 Reviewed-by: agy/gemini-3.1-pro\nChecks: g\n"
            );
            let p = probs(&m);
            assert!(
                p.iter().any(|s| s.contains("model that is not acting")),
                "{cli}: {p:?}"
            );
        }
    }

    /// A model nothing recognises cannot settle the cross-model check, and is
    /// reported rather than passed over.
    #[test]
    fn an_unknown_model_is_named() {
        let m = "s\n\nReviewed-by: subagent/opus-4.8\nReviewed-by: codex/some-new-thing\n\
                 Reviewed-by: agy/gemini-3.1-pro\nChecks: g\n";
        let p = probs(m);
        assert!(p.iter().any(|s| s.contains("is not a known model")), "{p:?}");
    }

    #[test]
    fn agy_alone_is_not_enough_either() {
        let p = probs("s\n\nReviewed-by: subagent/opus-4.8\nReviewed-by: agy/gemini-3.1-pro\nChecks: g\n");
        assert_eq!(p.len(), 1);
        assert!(p
            .first()
            .is_some_and(|s| s.starts_with("no cross-model review from claude or codex")));
    }

    #[test]
    fn the_same_cli_twice_counts_once() {
        let p = probs("s\n\nReviewed-by: subagent/opus-4.8\nReviewed-by: codex/gpt-5.6-sol\nReviewed-by: codex/gpt-5.6-sol\nChecks: g\n");
        assert!(p.iter().any(|s| s.starts_with("no agy review")));
    }

    #[test]
    fn checks_trailer_is_required() {
        let m = "s\n\nReviewed-by: subagent/opus-4.8\nReviewed-by: codex/gpt-5.6-sol\nReviewed-by: agy/gemini-3.1-pro\n";
        assert_eq!(probs(m), vec!["no `Checks:` trailer"]);
    }

    #[test]
    fn a_waiver_with_a_reason_substitutes() {
        let m = "s\n\nReviewed-by: subagent/opus-4.8\nReviewed-by: codex/gpt-5.6-sol\n\
                 Review-waiver: agy — CLI unavailable, approved by tester\nChecks: green\n";
        assert!(probs(m).is_empty());
    }

    #[test]
    fn a_waiver_without_one_does_not() {
        let m = "s\n\nReviewed-by: subagent/opus-4.8\nReviewed-by: codex/gpt-5.6-sol\nReview-waiver: agy\nChecks: g\n";
        let p = probs(m);
        assert!(p.iter().any(|s| s.contains("gives no reason")));
        assert!(p.iter().any(|s| s.starts_with("no agy review")));
    }

    /// `agy: unavailable` names agy — the punctuation is the separator, and
    /// reporting "unknown reviewer 'agy:'" would send an agent hunting a typo
    /// that is not the problem.
    #[test]
    fn waiver_punctuation_is_not_part_of_the_name() {
        let m = "s\n\nReviewed-by: subagent/opus-4.8\nReviewed-by: codex/gpt-5.6-sol\n\
                 Review-waiver: agy: not installed here, approved by tester\nChecks: g\n";
        assert!(probs(m).is_empty());
    }

    /// The waiver is for a reviewer CLI that is UNAVAILABLE. The subagent is
    /// not a CLI, so waiving it is declining the one review that always could
    /// have run — and three stacked waivers were a commit with no reviews at
    /// all passing the gate in four extra words.
    /// A waiver is the record of an answer a HUMAN gave. An agent that decides
    /// on its own that a reviewer was unavailable has recorded nothing: the
    /// reason has to name who approved it.
    #[test]
    fn a_waiver_must_name_who_approved_it() {
        let m = "s\n\nReviewed-by: subagent/opus-4.8\nReviewed-by: codex/gpt-5.6-sol\n\
                 Review-waiver: agy — the CLI is not installed on this host\nChecks: g\n";
        let p = probs(m);
        assert!(p.iter().any(|s| s.contains("names nobody who approved it")), "{p:?}");
        assert!(p.iter().any(|s| s.starts_with("no agy review")), "{p:?}");
    }

    #[test]
    fn the_subagent_review_cannot_be_waived() {
        let m = "s\n\nReviewed-by: codex/gpt-5.6-sol\nReviewed-by: agy/gemini-3.1-pro\n\
                 Review-waiver: subagent — skipped, approved by tester\nChecks: g\n";
        let p = probs(m);
        assert!(p.iter().any(|s| s == "the subagent review cannot be waived"), "{p:?}");
    }

    #[test]
    fn only_one_reviewer_may_be_waived_at_a_time() {
        let m = "s\n\nReviewed-by: subagent/opus-4.8\n\
                 Review-waiver: agy — unavailable, approved by tester\n\
                 Review-waiver: codex — unavailable, approved by tester\nChecks: g\n";
        let p = probs(m);
        assert!(p.iter().any(|s| s.contains("waived at once")), "{p:?}");

        let m = "s\n\nReview-waiver: subagent — skipped\nReview-waiver: agy — skipped\n\
                 Review-waiver: codex — skipped\nChecks: green\n";
        assert!(!probs(m).is_empty(), "a commit with no reviews at all must not pass");
    }

    #[test]
    fn docs_only_waives_the_reviews_but_is_checked() {
        let m = "s\n\nReview-waiver: docs-only\nChecks: none needed\n";
        assert!(problems(m, &code(&["AGENTS.md", "README.md"])).is_empty());
        let p = problems(m, &code(&["AGENTS.md", "builder/src/ready.rs"]));
        assert_eq!(p.len(), 1);
        assert!(p.first().is_some_and(|s| s.contains("builder/src/ready.rs")));
    }

    /// A rename must not launder a source file into a docs-only waiver.
    ///
    /// Driven against a REAL commit rather than a pinned argv, because what is
    /// being asserted is git's behaviour: with rename detection on, the source
    /// path is simply absent from the listing, and every path the gate then
    /// judges ends in `.md`.
    #[test]
    fn a_rename_cannot_hide_a_source_file_from_the_docs_only_waiver() {
        let root = std::env::temp_dir().join(format!("td-ready-rename-{}", std::process::id()));
        std::fs::remove_dir_all(&root).ok();
        std::fs::create_dir_all(&root).unwrap();
        // The fixture must not inherit the developer's git config, as
        // td-review's do not: a global `commit.gpgsign`, hook path or
        // `init.templateDir` would otherwise decide whether this passes, and
        // a global `diff.renames=false` would make it pass having exercised
        // nothing.
        let git = |args: &[&str]| {
            let o = Command::new("git")
                .args(args)
                .current_dir(&root)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_SYSTEM", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@example.invalid")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@example.invalid")
                .output()
                .unwrap();
            assert!(
                o.status.success(),
                "git {args:?} failed:\n{}",
                String::from_utf8_lossy(&o.stderr)
            );
            String::from_utf8_lossy(&o.stdout).to_string()
        };
        git(&["init", "-q", "-b", "main"]);
        // Pinned rather than inherited, and pinned ON: this test's whole
        // subject is what happens when git DOES pair the move.
        git(&["config", "diff.renames", "true"]);
        std::fs::write(root.join("code.rs"), "fn main() {}\n").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-qm", "seed"]);
        // An exact move: diffcore pairs it by blob OID, so no similarity
        // scoring is involved and the file's size is irrelevant.
        git(&["mv", "code.rs", "notes.md"]);
        git(&["commit", "-qm", "moved"]);

        // POSITIVE CONTROL: the hazard has to be real on this machine before
        // the fix can be shown to close it. `--no-renames` puts `code.rs` in
        // the listing unconditionally, so without this the assertion below
        // cannot tell "the flag works" from "rename detection was never on".
        let detected = git(&["show", "--name-only", "--format=", "HEAD"]);
        let detected: Vec<&str> = detected.lines().filter(|l| !l.trim().is_empty()).collect();
        assert_eq!(
            detected,
            vec!["notes.md"],
            "rename detection did not hide the source, so this proves nothing"
        );

        let paths = commit_paths(&root, "HEAD");
        let m = "s\n\nReview-waiver: docs-only\nChecks: none needed\n";
        let p = problems(m, &paths);
        // Cleaned BEFORE the assertions, so the run that catches a regression
        // is not the one that strands a repo in the temp dir.
        std::fs::remove_dir_all(&root).ok();
        assert!(
            paths.iter().any(|q| q == "code.rs"),
            "the renamed-away source must be in the listing: {paths:?}"
        );
        assert_eq!(p.len(), 1, "{p:?}");
        assert!(p.first().is_some_and(|s| s.contains("code.rs")), "{p:?}");
    }

    /// No paths is not "no code": a merge shows none, and so does a failed
    /// query. The one check that consults reality must not accept silence.
    #[test]
    fn docs_only_refuses_an_empty_path_list() {
        let m = "s\n\nReview-waiver: docs-only\nChecks: none needed\n";
        let p = problems(m, &[]);
        assert_eq!(p, vec!["waived as docs-only, but no paths could be read"]);
    }

    #[test]
    fn unknown_reviewers_are_named() {
        let m = "s\n\nReviewed-by: subagent/opus-4.8\nReviewed-by: codex/gpt-5.6-sol\nReviewed-by: gemini/x\nChecks: g\n";
        assert!(probs(m).iter().any(|s| s == "unknown reviewer 'gemini'"));
    }

    #[test]
    fn a_malformed_identity_is_reported_not_ignored() {
        let m = "s\n\nReviewed-by: subagent\nReviewed-by: codex/gpt-5.6-sol\nReviewed-by: agy/gemini-3.1-pro\nChecks: g\n";
        let p = probs(m);
        assert!(p.iter().any(|s| s.contains("is not <tool>/<model>")));
        assert!(p.iter().any(|s| s == "no subagent review"));
    }

    /// git matches its own trailer tokens case-insensitively, and an agent that
    /// wrote `Reviewed-By:` must not be told the review is missing.
    #[test]
    fn trailer_keys_are_case_insensitive() {
        let m = "s\n\nReviewed-By: subagent/opus-4.8\nREVIEWED-BY: codex/gpt-5.6-sol\nreviewed-by: agy/gemini-3.1-pro\nchecks: g\n";
        assert!(probs(m).is_empty());
    }

    /// The message this file's own workflow tells agents to write: a `Next:`
    /// paragraph below the record hides it from the block scan, and "no
    /// subagent review" with three of them on screen is the worst diagnostic
    /// an unattended agent could act on.
    #[test]
    fn a_record_that_does_not_close_the_message_says_so() {
        let m = "s\n\nReviewed-by: subagent/opus-4.8\nReviewed-by: codex/gpt-5.6-sol\nReviewed-by: agy/gemini-3.1-pro\n\
                 Checks: green\n\nNext: finish the parser\nthen wire the CLI\n";
        let p = probs(m);
        assert!(
            p.iter().any(|s| s.contains("do not close the message")),
            "{p:?}"
        );
    }

    /// The same scan must not fire on a COMPLETE record that happens to quote
    /// the shape as well — this commit's own message does exactly that.
    #[test]
    fn a_complete_record_plus_a_quoted_one_is_still_ready() {
        let m = "docs: explain the record\n\nIt reads:\n\n    Reviewed-by: subagent/opus-4.8\n\n\
                 Reviewed-by: subagent/opus-4.8\nReviewed-by: codex/gpt-5.6-sol\nReviewed-by: agy/gemini-3.1-pro\nChecks: g\n";
        assert!(probs(m).is_empty());
    }

    #[test]
    fn a_quoted_record_does_not_count() {
        let m = "docs: explain the record\n\nIt looks like this:\n\n    \
                 Reviewed-by: subagent/opus-4.8\n    Reviewed-by: codex/gpt-5.6-sol\n    \
                 Reviewed-by: agy/gemini-3.1-pro\n    Checks: green\n\nThat is all.\n";
        assert_eq!(parse_record(m), Record::default());
    }

    #[test]
    fn only_the_trailing_block_is_the_record() {
        let m = "s\n\nReviewed-by: subagent/opus-4.7\n\nprose after it\n\n\
                 Reviewed-by: subagent/opus-4.8\nReviewed-by: codex/gpt-5.6-sol\nReviewed-by: agy/gemini-3.1-pro\nChecks: g\n";
        let rec = parse_record(m);
        let tools: Vec<&str> = rec.reviewers.iter().map(|(t, _)| t.as_str()).collect();
        assert_eq!(tools, vec!["subagent", "codex", "agy"]);
        assert!(probs(m).is_empty());
    }

    fn repo_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .to_path_buf()
    }

    /// The fixture two crates share: `td-builder ready` gates on the record and
    /// td-review displays it, from separate implementations (neither crate may
    /// depend on the other). Nothing but this makes them agree.
    #[test]
    fn shared_fixture_cases() {
        let root = repo_root();
        // Only the crate ships to the td-builder package build, so the repo
        // fixture is absent there — as affected.rs's self-test also allows. The
        // skip is gated on a repo marker so a MISTYPED path is red instead.
        if !root.join("AGENTS.md").is_file() {
            return;
        }
        let text = std::fs::read_to_string(root.join("tests/review-record.cases"))
            .expect("tests/review-record.cases");
        let cases = parse_cases(&text);
        assert!(cases.len() >= 26, "fixture shrank: {} cases", cases.len());
        for (name, expect_ready, paths, message) in cases {
            let p = problems(&message, &paths);
            assert_eq!(p.is_empty(), expect_ready, "case {name}: problems {p:?}");
        }
    }

    /// `case <name> <ready|not-ready> [paths=a,b]`, then the message until the
    /// next case. Mirrored in td-review's `record.rs`. A malformed case line is
    /// a panic, not a silent "not-ready": this fixture is the only thing keeping
    /// the two implementations honest, so it may not quietly assert the opposite
    /// of what was written.
    fn parse_cases(text: &str) -> Vec<(String, bool, Vec<String>, String)> {
        let mut out: Vec<(String, bool, Vec<String>, String)> = Vec::new();
        let mut cur: Option<(String, bool, Vec<String>, Vec<String>)> = None;
        for line in text.lines() {
            if let Some(rest) = line.strip_prefix("case ") {
                if let Some((n, r, p, body)) = cur.take() {
                    out.push((n, r, p, body.join("\n")));
                }
                let mut fields = rest.split_whitespace();
                let name = fields.next().expect("case needs a name").to_string();
                let ready = match fields.next() {
                    Some("ready") => true,
                    Some("not-ready") => false,
                    other => panic!("case {name}: verdict must be ready|not-ready, got {other:?}"),
                };
                let mut paths = vec!["src/a.rs".to_string()];
                for f in fields {
                    let value = f
                        .strip_prefix("paths=")
                        .unwrap_or_else(|| panic!("case {name}: unknown field {f}"));
                    paths = if value.is_empty() {
                        Vec::new()
                    } else {
                        value.split(',').map(str::to_string).collect()
                    };
                }
                cur = Some((name, ready, paths, Vec::new()));
            } else if let Some((_, _, _, body)) = cur.as_mut() {
                body.push(line.to_string());
            }
        }
        if let Some((n, r, p, body)) = cur.take() {
            out.push((n, r, p, body.join("\n")));
        }
        out
    }
}

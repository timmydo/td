//! ready — `td-builder ready`: the gate an agent runs before pushing a rolling
//! branch. Readiness lives in the branch rather than in a flag somebody sets
//! (AGENTS.md), so this reads it back out of the branch: the bounded checks over
//! the committed diff, plus the review record every commit not yet on the base
//! has to carry. Landing replays those commits one at a time, so the record is
//! per commit — a branch-wide claim would say nothing about the commit the
//! integrator actually stops at.

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use crate::affected;

#[path = "../../td-review/src/reviewer.rs"]
mod reviewer;

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

pub use reviewer::problems;
#[cfg(test)]
use reviewer::{parse as parse_record, Record};

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
/// DELETED a Rust source waives both frontier reviews. Off, a rename is a delete
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

pub fn main(
    args: &[String],
    hosted: bool,
    forward: impl FnOnce(&[String]) -> ExitCode,
) -> ExitCode {
    run_at(affected::resolve_root(), args, hosted, forward)
}

fn run_at(
    root: PathBuf,
    args: &[String],
    hosted: bool,
    forward: impl FnOnce(&[String]) -> ExitCode,
) -> ExitCode {
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
    if !affected::git_ok(
        &root,
        &["rev-parse", "--verify", &format!("{base}^{{commit}}")],
    ) {
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
    // the path `blocking_untracked` would be asked about.
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
    let blocking: Vec<&str> = affected::blocking_untracked(&root, &untracked);

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
            &[
                "-c",
                "i18n.logOutputEncoding=UTF-8",
                "show",
                "-s",
                "--format=%P%x00%B",
                oid,
            ],
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
        let mut request = vec!["ready".to_string()];
        request.extend_from_slice(args);
        match affected::run_selected(
            &root,
            &[
                "--committed-only".to_string(),
                "--run".to_string(),
                "--base".to_string(),
                base.clone(),
            ],
            !hosted,
        ) {
            affected::RunOutcome::Complete(code) => code,
            affected::RunOutcome::NeedsHost => {
                println!(
                    "ready: waiting for the shared check host; validation repeats there before checks"
                );
                return forward(&request);
            }
        }
    };

    println!();
    let clean = dirty == 0 && blocking.is_empty();
    if unready == 0 && clean && checks == 0 {
        if record_only {
            // A different WORD, not a different exit status — `--record-only`
            // did what it was asked, so it exits 0 and a `&&` chain cannot tell
            // the two apart. The word is for the human reading the output.
            println!(
                "RECORD OK: {} commit(s) over {base} — checks NOT run",
                commits.len()
            );
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

    fn local_ready(root: PathBuf, args: &[String]) -> ExitCode {
        run_at(root, args, false, |request| {
            panic!("unexpected host handoff: {request:?}")
        })
    }

    #[test]
    fn empty_selection_still_checks_the_real_branch() {
        let _root_cleanup = crate::test_temp::TempDir::new(&format!(
            "td-ready-empty-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
        .unwrap();
        let root = _root_cleanup.join("fixture");
        std::fs::create_dir_all(&root).unwrap();
        let git = |args: &[&str]| {
            let output = Command::new("git")
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
            assert!(output.status.success(), "{args:?}: {output:?}");
        };
        git(&["init", "-q", "-b", "main"]);
        std::fs::write(root.join("code.rs"), "fn main() {}\n").unwrap();
        git(&["add", "code.rs"]);
        git(&["commit", "-qm", "seed"]);
        git(&["switch", "-qc", "docs"]);
        std::fs::write(root.join("notes.md"), "Notes\n").unwrap();
        git(&["add", "notes.md"]);
        git(&["commit", "-qm", "missing record"]);
        assert_eq!(local_ready(root.clone(), &[]), ExitCode::FAILURE);
        git(&[
            "commit",
            "--amend",
            "-qm",
            "docs: notes\n\nReview-waiver: docs-only\nChecks: prose checked\n",
        ]);
        // No origin: both ready and selection must use the main fallback.
        assert_eq!(local_ready(root.clone(), &[]), ExitCode::SUCCESS);
        let custom = vec!["--base".to_string(), "main".to_string()];
        assert_eq!(local_ready(root.clone(), &custom), ExitCode::SUCCESS);
        assert_eq!(
            local_ready(root.clone(), &["--base".into(), "missing".into()]),
            ExitCode::from(2)
        );
        assert_eq!(
            local_ready(root.clone(), &["--base".into()]),
            ExitCode::from(2)
        );
        std::fs::write(root.join("notes.md"), "Dirty\n").unwrap();
        assert_eq!(local_ready(root.clone(), &[]), ExitCode::FAILURE);
        git(&["checkout", "--", "notes.md"]);
        std::fs::create_dir_all(root.join("builder/src")).unwrap();
        std::fs::write(root.join("builder/src/untracked.rs"), "fn f() {}\n").unwrap();
        assert_eq!(local_ready(root.clone(), &[]), ExitCode::FAILURE);
        std::fs::remove_file(root.join("builder/src/untracked.rs")).unwrap();

        // The same committed selection must defer a rename's source deletion,
        // even though its destination and review waiver both claim Markdown.
        git(&["mv", "code.rs", "code.md"]);
        git(&[
            "commit",
            "-qm",
            "move\n\nReview-waiver: docs-only\nChecks: prose\n",
        ]);
        assert_eq!(
            affected::run_selected(
                &root,
                &[
                    "--base".into(),
                    "main".into(),
                    "--committed-only".into(),
                    "--run".into()
                ],
                true
            ),
            affected::RunOutcome::NeedsHost
        );
        assert_eq!(
            local_ready(root.clone(), &["--record-only".into()]),
            ExitCode::FAILURE
        );
        // Exercise the actual ready handoff with a bounded fake. A mapping
        // regression above can only fail this test, never submit real work.
        let forwarded = std::cell::Cell::new(false);
        assert_eq!(
            run_at(root.clone(), &custom, false, |request| {
                assert_eq!(request, ["ready", "--base", "main"]);
                forwarded.set(true);
                ExitCode::from(23)
            }),
            ExitCode::from(23)
        );
        assert!(forwarded.get());
    }

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
    fn each_frontier_family_is_required() {
        assert_eq!(
            probs("s\n\nReviewed-by: subagent/opus-5.5\nChecks: green\n"),
            ["no gpt-sol review"]
        );
        assert_eq!(
            probs("s\n\nReviewed-by: td-agent/openai/gpt-6.1-sol\nChecks: green\n"),
            ["no opus review"]
        );
    }

    #[test]
    fn either_transport_can_supply_both_models() {
        for transport in ["subagent", "td-agent"] {
            let message = format!("s\n\nReviewed-by: {transport}/anthropic/claude-opus-5.5\nReviewed-by: {transport}/openai/gpt-6.1-sol\nChecks: green\n");
            assert!(probs(&message).is_empty());
        }
    }

    #[test]
    fn two_versions_of_one_family_are_one_opinion() {
        assert_eq!(probs("s\n\nReviewed-by: subagent/opus-5.5\nReviewed-by: td-agent/opus-4.8\nChecks: green\n"), ["no gpt-sol review"]);
    }

    #[test]
    fn docs_only_waives_the_reviews_but_is_checked() {
        let message = "s\n\nReview-waiver: docs-only\nChecks: none needed\n";
        assert!(problems(message, &code(&["AGENTS.md", "README.md"])).is_empty());
        assert_eq!(
            problems(message, &code(&["AGENTS.md", "builder/src/ready.rs"])),
            ["docs-only waiver touches builder/src/ready.rs"]
        );
    }

    /// A rename must not launder a source file into a docs-only waiver.
    ///
    /// Driven against a REAL commit rather than a pinned argv, because what is
    /// being asserted is git's behaviour: with rename detection on, the source
    /// path is simply absent from the listing, and every path the gate then
    /// judges ends in `.md`.
    #[test]
    fn a_rename_cannot_hide_a_source_file_from_the_docs_only_waiver() {
        let _root_cleanup = crate::test_temp::TempDir::new("td-ready-rename").unwrap();
        let root = _root_cleanup.join("fixture");
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
        assert!(p.iter().any(|s| s.contains("malformed Reviewed-by")));
        assert!(p.iter().any(|s| s == "no opus review"));
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
        assert_eq!(tools, vec!["subagent", "legacy-cli"]);
        assert!(probs(m).is_empty());
    }

    fn repo_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .to_path_buf()
    }

    /// The fixture two crates share: `td-builder ready` gates on the record and
    /// td-review displays it, using the same parser and policy. The fixture pins both entry points.
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

//! End-to-end cover for the landing sequence, driven through the real binary's
//! non-interactive `--land` mode: a scratch repo with two pushable remotes and
//! one `no_push` remote, a branch with two commits, and the assertion that
//! landing it produces exactly one squash commit carrying both messages and
//! that every pushable remote ends up at that commit.

use std::error::Error;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

type Res<T> = Result<T, Box<dyn Error>>;

const BIN: &str = env!("CARGO_BIN_EXE_td-review");

/// Scratch directory removed when the test ends.
struct TempDir(PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

impl TempDir {
    fn new(tag: &str) -> Res<TempDir> {
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let path =
            std::env::temp_dir().join(format!("td-review-{tag}-{}-{nanos}", std::process::id()));
        fs::create_dir_all(&path)?;
        Ok(TempDir(path))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

/// The fixtures must not inherit the developer's git config: a global
/// `commit.gpgsign`, `merge.tool`, hook path or alias would otherwise decide
/// whether these tests pass. Identity comes from the environment so the repos
/// that are cloned mid-test need no per-repo setup.
const CLEAN_ENV: [(&str, &str); 7] = [
    ("GIT_CONFIG_GLOBAL", "/dev/null"),
    ("GIT_CONFIG_SYSTEM", "/dev/null"),
    ("GIT_CONFIG_NOSYSTEM", "1"),
    ("GIT_AUTHOR_NAME", "Test Integrator"),
    ("GIT_AUTHOR_EMAIL", "integrator@example.invalid"),
    ("GIT_COMMITTER_NAME", "Test Integrator"),
    ("GIT_COMMITTER_EMAIL", "integrator@example.invalid"),
];

fn git_env(dir: &Path, args: &[&str], env: &[(&str, &str)]) -> Res<String> {
    let mut cmd = Command::new("git");
    cmd.current_dir(dir).args(args);
    for (k, v) in CLEAN_ENV.iter().chain(env) {
        cmd.env(k, v);
    }
    let out = cmd.output()?;
    if !out.status.success() {
        return Err(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr)
        )
        .into());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn git(dir: &Path, args: &[&str]) -> Res<String> {
    git_env(dir, args, &[])
}

/// Feed `input` to git on stdin — the only way to write a commit object whose
/// message git has not already transcoded.
fn git_stdin(dir: &Path, args: &[&str], input: &[u8]) -> Res<String> {
    let mut cmd = Command::new("git");
    cmd.current_dir(dir).args(args).stdin(Stdio::piped()).stdout(Stdio::piped());
    for (k, v) in CLEAN_ENV {
        cmd.env(k, v);
    }
    let mut child = cmd.spawn()?;
    match child.stdin.as_mut() {
        Some(pipe) => pipe.write_all(input)?,
        None => return Err("git stdin was not piped".into()),
    }
    drop(child.stdin.take());
    let out = child.wait_with_output()?;
    if !out.status.success() {
        return Err(format!("git {} failed", args.join(" ")).into());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Undecoded stdout — a commit message is not necessarily UTF-8.
fn git_bytes(dir: &Path, args: &[&str]) -> Res<Vec<u8>> {
    let mut cmd = Command::new("git");
    cmd.current_dir(dir).args(args);
    for (k, v) in CLEAN_ENV {
        cmd.env(k, v);
    }
    let out = cmd.output()?;
    if !out.status.success() {
        return Err(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr)
        )
        .into());
    }
    Ok(out.stdout)
}

/// A complete review record, for a commit whose READY cell has to read `ok` —
/// the only cell `!merge` replaces.
const RECORD: &str = "Reviewed-by: subagent/opus-5\nReviewed-by: codex/gpt-5.6-sol\n\
                      Reviewed-by: agy/gemini-3.1-pro\nChecks: green";

fn commit(dir: &Path, file: &str, body: &str, message: &str, when: &str) -> Res<()> {
    fs::write(dir.join(file), body)?;
    git(dir, &["add", file])?;
    git_env(
        dir,
        &["commit", "-m", message],
        &[("GIT_AUTHOR_DATE", when), ("GIT_COMMITTER_DATE", when)],
    )?;
    Ok(())
}

struct Scenario {
    _tmp: TempDir,
    work: PathBuf,
    origin: PathBuf,
    backup: PathBuf,
}

/// A work tree on `main` with `origin` + `backup` pushable, `archive` marked
/// `no_push`, and a two-commit branch present only as a remote-tracking ref —
/// the shape the integrator actually sees after `git fetch`.
fn scenario(tag: &str) -> Res<Scenario> {
    let tmp = TempDir::new(tag)?;
    let root = tmp.path().to_path_buf();
    let origin = root.join("origin.git");
    let backup = root.join("backup.git");
    let work = root.join("work");

    for bare in [&origin, &backup] {
        git(&root, &["init", "--bare", "-b", "main", &bare.to_string_lossy()])?;
    }
    git(&root, &["init", "-b", "main", &work.to_string_lossy()])?;
    for (k, v) in [
        ("user.name", "Test Integrator"),
        ("user.email", "integrator@example.invalid"),
        ("commit.gpgsign", "false"),
        ("tag.gpgsign", "false"),
    ] {
        git(&work, &["config", k, v])?;
    }

    commit(&work, "README", "base\n", "base: initial commit", "2024-01-01T00:00:00 +0000")?;
    git(&work, &["remote", "add", "origin", &origin.to_string_lossy()])?;
    git(&work, &["remote", "add", "backup", &backup.to_string_lossy()])?;
    // A real FETCH url (so its tracking refs exist) with a `no_push` PUSH url:
    // the delete/push skip must come from the push url, not from an absent ref.
    git(&work, &["remote", "add", "archive", &origin.to_string_lossy()])?;
    git(&work, &["remote", "set-url", "--push", "archive", "no_push"])?;
    git(&work, &["push", "origin", "main"])?;
    git(&work, &["push", "backup", "main"])?;

    git(&work, &["checkout", "-b", "work-0001-feature"])?;
    commit(&work, "a.txt", "alpha\n", "feature: first step\n\nrationale one", "2024-01-02T00:00:00 +0000")?;
    commit(&work, "b.txt", "beta\n", "feature: second step\n\nrationale two", "2024-01-03T00:00:00 +0000")?;
    git(&work, &["push", "origin", "work-0001-feature"])?;
    git(&work, &["fetch", "-q", "archive"])?;

    // Land from a work tree that only has the remote-tracking ref.
    git(&work, &["checkout", "main"])?;
    git(&work, &["branch", "-D", "work-0001-feature"])?;

    Ok(Scenario { _tmp: tmp, work, origin, backup })
}

/// The `.git/SQUASH_MSG` git would offer a hand-finished commit.
fn squash_msg(work: &Path) -> Res<String> {
    let dir = git(work, &["rev-parse", "--absolute-git-dir"])?.trim().to_string();
    Ok(fs::read_to_string(Path::new(&dir).join("SQUASH_MSG"))?)
}

fn review(work: &Path, args: &[&str]) -> Res<(bool, String)> {
    let mut cmd = Command::new(BIN);
    cmd.arg("--repo").arg(work).args(args);
    for (k, v) in CLEAN_ENV {
        cmd.env(k, v);
    }
    let out = cmd.output()?;
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    Ok((out.status.success(), text))
}

#[test]
fn lands_squash_commit_and_pushes_every_pushable_remote() -> Res<()> {
    let s = scenario("land")?;
    let (ok, output) = review(
        &s.work,
        &["--land", "origin/work-0001-feature", "--squash", "--yes", "--push"],
    )?;
    assert!(ok, "landing should succeed:\n{output}");

    // Exactly one new commit on main, not the branch's two.
    let count = git(&s.work, &["rev-list", "--count", "main"])?.trim().to_string();
    assert_eq!(count, "2", "expected one squash commit on top of the base\n{output}");

    // The message is the branch's own commit messages, in order.
    let message = git(&s.work, &["log", "-1", "--format=%B", "main"])?;
    assert!(message.contains("feature: first step"), "message: {message}");
    assert!(message.contains("rationale one"), "message: {message}");
    assert!(message.contains("feature: second step"), "message: {message}");
    assert!(message.contains("rationale two"), "message: {message}");
    let first = message.find("feature: first step");
    let second = message.find("feature: second step");
    assert!(first < second, "branch commits must appear oldest-first: {message}");

    // Both files landed.
    assert!(s.work.join("a.txt").exists());
    assert!(s.work.join("b.txt").exists());

    // Every pushable remote is at the new commit; the no_push one is skipped.
    let head = git(&s.work, &["rev-parse", "main"])?.trim().to_string();
    for remote in [&s.origin, &s.backup] {
        let there = git(remote, &["rev-parse", "main"])?.trim().to_string();
        assert_eq!(there, head, "{} should carry the landed commit", remote.display());
    }
    assert!(
        output.contains("skipped: no_push"),
        "the no_push remote must be skipped, not pushed:\n{output}"
    );

    // A landing leaves nothing behind to clean up.
    assert_eq!(git(&s.work, &["status", "--porcelain"])?.trim(), "");
    Ok(())
}

/// `r` in the pane, the default here: the same work as the branch's own history
/// rather than one commit. A branch already sitting on the base tip replays as
/// a fast-forward, so what lands is the very commits that were reviewed —
/// messages, authors and object ids alike.
#[test]
fn a_replay_lands_the_branchs_own_commits_rather_than_one() -> Res<()> {
    let s = scenario("replay")?;
    let tip = git(&s.work, &["rev-parse", "refs/remotes/origin/work-0001-feature"])?
        .trim()
        .to_string();
    let (ok, output) = review(
        &s.work,
        &["--land", "origin/work-0001-feature", "--yes", "--push"],
    )?;
    assert!(ok, "the replay should succeed:\n{output}");

    // Two commits on top of the base, not one squash carrying both.
    let count = git(&s.work, &["rev-list", "--count", "main"])?.trim().to_string();
    assert_eq!(count, "3", "expected the branch's two commits on the base\n{output}");
    let head = git(&s.work, &["rev-parse", "main"])?.trim().to_string();
    assert_eq!(head, tip, "a branch on the base tip must land unrewritten\n{output}");

    // Each commit keeps its own subject, body and author date, oldest first.
    let subjects = git(&s.work, &["log", "--reverse", "--format=%s", "-2", "main"])?;
    assert_eq!(
        subjects.lines().collect::<Vec<_>>(),
        vec!["feature: first step", "feature: second step"],
        "output: {output}"
    );
    let bodies = git(&s.work, &["log", "--format=%b", "-2", "main"])?;
    assert!(bodies.contains("rationale one"), "bodies: {bodies}");
    assert!(bodies.contains("rationale two"), "bodies: {bodies}");
    let dates = git(&s.work, &["log", "--reverse", "--format=%ad", "--date=short", "-2", "main"])?;
    assert_eq!(dates.lines().collect::<Vec<_>>(), vec!["2024-01-02", "2024-01-03"]);

    assert!(s.work.join("a.txt").exists());
    assert!(s.work.join("b.txt").exists());
    for remote in [&s.origin, &s.backup] {
        let there = git(remote, &["rev-parse", "main"])?.trim().to_string();
        assert_eq!(there, head, "{} should carry the replayed commits", remote.display());
    }
    assert_eq!(git(&s.work, &["status", "--porcelain"])?.trim(), "");
    Ok(())
}

/// A base that has moved on is a real replay rather than a fast-forward: every
/// commit is rewritten onto the new tip, and what they add up to has to be the
/// merge result the review pane diffed — the same tree the squash would stage.
#[test]
fn a_replay_onto_a_moved_base_keeps_every_commit_and_the_reviewed_tree() -> Res<()> {
    let s = scenario("replay-moved")?;
    commit(&s.work, "c.txt", "gamma\n", "base: unrelated work", "2024-01-04T00:00:00 +0000")?;
    let base = git(&s.work, &["rev-parse", "main"])?.trim().to_string();
    let tip = git(&s.work, &["rev-parse", "refs/remotes/origin/work-0001-feature"])?
        .trim()
        .to_string();
    let reviewed =
        git(&s.work, &["merge-tree", "--write-tree", &base, &tip])?.trim().to_string();

    let (ok, output) = review(&s.work, &["--land", "origin/work-0001-feature", "--yes"])?;
    assert!(ok, "the replay should succeed:\n{output}");

    let gained = git(&s.work, &["rev-list", "--count", &format!("{base}..main")])?;
    assert_eq!(gained.trim(), "2", "both commits must land\n{output}");
    assert_eq!(
        git(&s.work, &["rev-parse", "main^{tree}"])?.trim(),
        reviewed,
        "the replay must add up to the tree the pane diffed\n{output}"
    );
    // Rewritten onto the new tip, so not the branch's own commits any more.
    assert_ne!(git(&s.work, &["rev-parse", "main"])?.trim(), tip);
    assert_eq!(git(&s.work, &["status", "--porcelain"])?.trim(), "");
    Ok(())
}

/// cherry-pick drops a commit whose patch the base already carries — right, but
/// it means the replay lands fewer commits than the pane listed, which must be
/// said out loud rather than read as a miscount (or reported as one).
#[test]
fn a_replay_skips_a_commit_the_base_already_carries() -> Res<()> {
    let s = scenario("replay-skip")?;
    let first = git(&s.work, &["rev-parse", "refs/remotes/origin/work-0001-feature~1"])?
        .trim()
        .to_string();
    git(&s.work, &["cherry-pick", &first])?;
    let base = git(&s.work, &["rev-parse", "main"])?.trim().to_string();

    let (ok, output) = review(&s.work, &["--land", "origin/work-0001-feature", "--yes"])?;
    assert!(ok, "the rest of the branch must still land:\n{output}");
    let gained = git(&s.work, &["rev-list", "--count", &format!("{base}..main")])?;
    assert_eq!(gained.trim(), "1", "the already-landed commit must not land twice\n{output}");
    assert_eq!(
        git(&s.work, &["log", "-1", "--format=%s", "main"])?.trim(),
        "feature: second step"
    );
    assert!(output.contains("are already on main"), "the skip must be reported:\n{output}");
    assert!(output.contains("replayed 1 commit"), "output: {output}");
    Ok(())
}

/// The property that makes `r` safe to press: a replay that stops partway has
/// already committed everything before the commit that stopped it, and unlike a
/// staged squash those commits are one `p` from being published. So the base
/// goes back to where it started, with no sequencer state left to finish.
#[test]
fn a_replay_that_stops_partway_leaves_the_base_where_it_was() -> Res<()> {
    let s = scenario("replay-conflict")?;
    let root = git(&s.work, &["rev-parse", "main"])?.trim().to_string();

    // A branch whose first commit replays cleanly and whose second does not,
    // while its two ends still merge cleanly onto the base — so the pane has a
    // landing tree to show, and the stop can only be found partway through,
    // with a commit already on the base by then.
    git(&s.work, &["checkout", "-b", "work-0002-mid", &root])?;
    commit(&s.work, "a.txt", "alpha\n", "mid: first step", "2024-01-05T00:00:00 +0000")?;
    commit(&s.work, "README", "sideways\n", "mid: second step", "2024-01-06T00:00:00 +0000")?;
    commit(&s.work, "README", "moved\n", "mid: third step", "2024-01-07T00:00:00 +0000")?;
    git(&s.work, &["checkout", "main"])?;
    commit(&s.work, "README", "moved\n", "base: moves README too", "2024-01-08T00:00:00 +0000")?;
    let before = git(&s.work, &["rev-parse", "main"])?.trim().to_string();

    let (ok, output) = review(&s.work, &["--land", "work-0002-mid", "--yes"])?;
    assert!(!ok, "a stopped replay must not report success:\n{output}");
    assert_eq!(
        git(&s.work, &["rev-parse", "main"])?.trim(),
        before,
        "the commits applied before the stop were left on the base:\n{output}"
    );
    assert_eq!(
        git(&s.work, &["status", "--porcelain"])?.trim(),
        "",
        "the stopped replay was left in the work tree:\n{output}"
    );
    let dir = git(&s.work, &["rev-parse", "--absolute-git-dir"])?.trim().to_string();
    assert!(
        !Path::new(&dir).join("sequencer").exists(),
        "sequencer state was left to finish by hand:\n{output}"
    );
    assert!(output.contains("nothing was applied"), "output: {output}");
    Ok(())
}

/// A commit that is NOT ours arriving on the base mid-replay. The count no
/// longer matches what was picked, so `head_before..HEAD` is no longer ours
/// alone — and a rollback would take that commit with it. The landing must
/// leave everything standing, say so, and warn that `p` would publish it: the
/// tree is clean, so no bail-out prompt will offer to undo anything.
#[test]
fn a_replay_that_gained_a_commit_it_did_not_make_is_not_rolled_back() -> Res<()> {
    let s = scenario("replay-foreign")?;
    // Moved, so the replay makes commits of its own: a fast-forward creates
    // none, and post-commit would never fire.
    commit(&s.work, "c.txt", "gamma\n", "base: unrelated work", "2024-01-04T00:00:00 +0000")?;
    let dir = git(&s.work, &["rev-parse", "--absolute-git-dir"])?.trim().to_string();
    let hooks = Path::new(&dir).join("hooks");
    fs::create_dir_all(&hooks)?;
    // cherry-pick runs post-commit for each commit it makes, so this fires
    // inside the sequencer. Plumbing only: the index and work tree belong to
    // the sequencer while it runs, and neither is needed to add a commit.
    let hook = hooks.join("post-commit");
    fs::write(
        &hook,
        "#!/bin/sh\n\
         d=$(git rev-parse --git-dir)\n\
         [ -e \"$d/intruded\" ] && exit 0\n\
         : > \"$d/intruded\"\n\
         t=$(git rev-parse 'HEAD^{tree}')\n\
         c=$(git commit-tree \"$t\" -p HEAD -m 'other: landed mid-replay')\n\
         git update-ref refs/heads/main \"$c\"\n",
    )?;
    let mut perms = fs::metadata(&hook)?.permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
    fs::set_permissions(&hook, perms)?;

    let (ok, output) = review(&s.work, &["--land", "origin/work-0001-feature", "--yes"])?;
    assert!(!ok, "a base that gained a foreign commit must not report success:\n{output}");
    assert!(output.contains("left standing"), "output: {output}");
    assert!(output.contains("gained 3"), "the count must be named:\n{output}");
    // The whole point: the intruder is still there.
    let log = git(&s.work, &["log", "--format=%s", "main"])?;
    assert!(
        log.contains("other: landed mid-replay"),
        "the rollback destroyed a commit it did not make:\n{log}"
    );
    // And the operator is told that `p` would publish what was just refused.
    assert!(output.contains("WILL PUBLISH"), "output: {output}");
    Ok(())
}

/// The replay landed exactly the commits it picked, and their total is still
/// not the merge result the pane diffed. Nothing else is on the base, so these
/// commits ARE ours to remove — and unlike the case above, they must be.
#[test]
fn a_replay_whose_tree_is_not_the_reviewed_one_is_rolled_back() -> Res<()> {
    let s = scenario("replay-divergent")?;
    commit(&s.work, "c.txt", "gamma\n", "base: unrelated work", "2024-01-04T00:00:00 +0000")?;
    let before = git(&s.work, &["rev-parse", "main"])?.trim().to_string();
    let dir = git(&s.work, &["rev-parse", "--absolute-git-dir"])?.trim().to_string();
    let hooks = Path::new(&dir).join("hooks");
    fs::create_dir_all(&hooks)?;
    // Rewrites the commit the sequencer just made into one carrying an extra
    // file, keeping its parent — so the COUNT still matches what was picked and
    // only the tree gives it away. A private index, because the sequencer owns
    // the real one; no work-tree writes at all. On the LAST of the two picks:
    // done earlier, the rewritten HEAD no longer matches the work tree and the
    // next pick refuses instead.
    let hook = hooks.join("post-commit");
    fs::write(
        &hook,
        "#!/bin/sh\n\
         d=$(git rev-parse --git-dir)\n\
         n=$(cat \"$d/picks\" 2>/dev/null || echo 0)\n\
         n=$((n+1)); echo \"$n\" > \"$d/picks\"\n\
         [ \"$n\" -lt 2 ] && exit 0\n\
         GIT_INDEX_FILE=\"$d/tmp-index\"; export GIT_INDEX_FILE\n\
         git read-tree HEAD\n\
         b=$(printf sneaked | git hash-object -w --stdin)\n\
         git update-index --add --cacheinfo 100644,\"$b\",sneaky.txt\n\
         t=$(git write-tree)\n\
         c=$(git commit-tree \"$t\" -p HEAD^ -m \"$(git log -1 --format=%s)\")\n\
         git update-ref refs/heads/main \"$c\"\n",
    )?;
    let mut perms = fs::metadata(&hook)?.permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
    fs::set_permissions(&hook, perms)?;

    let (ok, output) = review(&s.work, &["--land", "origin/work-0001-feature", "--yes"])?;
    assert!(!ok, "a tree that is not the reviewed one must not land:\n{output}");
    assert!(output.contains("is not the merge result"), "output: {output}");
    assert_eq!(
        git(&s.work, &["rev-parse", "main"])?.trim(),
        before,
        "the unreviewed commits were left on the base:\n{output}"
    );
    assert!(output.contains("rolled back"), "output: {output}");
    assert_eq!(git(&s.work, &["status", "--porcelain"])?.trim(), "");
    Ok(())
}

/// `r` promises the branch's own commits, and a deliberately empty one is one
/// of them — without `--allow-empty` the sequencer stops on it and the whole
/// landing aborts. A commit that BECOMES empty against this base still stops,
/// which is the case that must not be quietly dropped.
#[test]
fn a_replay_keeps_a_commit_that_was_empty_to_begin_with() -> Res<()> {
    let s = scenario("replay-empty")?;
    let root = git(&s.work, &["rev-parse", "main"])?.trim().to_string();

    git(&s.work, &["checkout", "-b", "work-0004-empty", &root])?;
    commit(&s.work, "a.txt", "alpha\n", "empty: first step", "2024-01-05T00:00:00 +0000")?;
    git_env(
        &s.work,
        &["commit", "--allow-empty", "-m", "empty: a marker commit"],
        &[
            ("GIT_AUTHOR_DATE", "2024-01-06T00:00:00 +0000"),
            ("GIT_COMMITTER_DATE", "2024-01-06T00:00:00 +0000"),
        ],
    )?;
    commit(&s.work, "b2.txt", "beta\n", "empty: third step", "2024-01-07T00:00:00 +0000")?;
    git(&s.work, &["checkout", "main"])?;
    // Moved, so this is a real replay rather than a fast-forward.
    commit(&s.work, "c.txt", "gamma\n", "base: unrelated work", "2024-01-08T00:00:00 +0000")?;
    let base = git(&s.work, &["rev-parse", "main"])?.trim().to_string();

    let (ok, output) = review(&s.work, &["--land", "work-0004-empty", "--yes"])?;
    assert!(ok, "an empty commit must not stop the replay:\n{output}");
    let gained = git(&s.work, &["rev-list", "--count", &format!("{base}..main")])?;
    assert_eq!(gained.trim(), "3", "every commit must land, empty one included\n{output}");
    let subjects = git(&s.work, &["log", "--reverse", "--format=%s", "-3", "main"])?;
    assert_eq!(
        subjects.lines().collect::<Vec<_>>(),
        vec!["empty: first step", "empty: a marker commit", "empty: third step"]
    );
    Ok(())
}

/// A cherry-pick that stopped because a commit replayed to NOTHING leaves a
/// clean work tree, so the dirty-tree guard does not see it — but whoever
/// aborts that sequence rewinds the base to where IT began, past commits this
/// run never made. So a landing refuses to start on one at all, in either mode:
/// the alternative is a recovery step that destroys work and then reports the
/// commits it removed as left standing.
#[test]
fn a_landing_refuses_to_start_on_someone_elses_stopped_replay() -> Res<()> {
    let s = scenario("inherited-stop")?;
    let root = git(&s.work, &["rev-parse", "main"])?.trim().to_string();

    // A branch whose one commit is already on the base by patch: replaying it
    // lands nothing, which stops the sequencer with the tree clean.
    git(&s.work, &["checkout", "-b", "work-0003-dup", &root])?;
    commit(&s.work, "c.txt", "gamma\n", "dup: a step", "2024-01-05T00:00:00 +0000")?;
    let dup = git(&s.work, &["rev-parse", "HEAD"])?.trim().to_string();
    git(&s.work, &["checkout", "main"])?;
    git(&s.work, &["cherry-pick", &dup])?;
    // A commit of the base's own, the one an inherited abort would destroy.
    commit(&s.work, "d.txt", "delta\n", "base: later work", "2024-01-06T00:00:00 +0000")?;
    let before = git(&s.work, &["rev-parse", "main"])?.trim().to_string();

    // Someone leaves a stopped sequence behind by hand.
    let stopped = Command::new("git")
        .current_dir(&s.work)
        .args(["cherry-pick", &dup])
        .envs(CLEAN_ENV)
        .output()?;
    assert!(!stopped.status.success(), "the fixture must leave a stopped cherry-pick");
    assert_eq!(
        git(&s.work, &["status", "--porcelain"])?.trim(),
        "",
        "the fixture's stop must leave a CLEAN tree, or it proves nothing"
    );

    for args in [
        vec!["--land", "origin/work-0001-feature", "--yes"],
        vec!["--land", "origin/work-0001-feature", "--squash", "--yes"],
    ] {
        let (ok, output) = review(&s.work, &args)?;
        assert!(!ok, "{args:?} must refuse on an inherited sequence:\n{output}");
        assert!(output.contains("already in progress"), "output: {output}");
        assert_eq!(
            git(&s.work, &["rev-parse", "main"])?.trim(),
            before,
            "{args:?} rewound the base past commits it never made:\n{output}"
        );
    }
    Ok(())
}

/// A branch that does not merge cleanly has no landing tree for the pane to
/// show, so it showed the BRANCH's own diff and said so. A squash cannot land
/// blind from there — the merge conflicts too — but a replay's successive
/// merges can resolve where the one-shot merge does not, and would commit work
/// whose diff nobody has seen. So `r` refuses before the sequencer starts.
#[test]
fn a_replay_refuses_a_branch_that_does_not_merge_cleanly() -> Res<()> {
    let s = scenario("replay-unmergeable")?;
    commit(&s.work, "b.txt", "conflicting\n", "base: touches b.txt", "2024-01-04T00:00:00 +0000")?;
    let before = git(&s.work, &["rev-parse", "main"])?.trim().to_string();

    let (ok, output) = review(&s.work, &["--land", "origin/work-0001-feature", "--yes"])?;
    assert!(!ok, "an unmergeable branch must not replay:\n{output}");
    assert!(output.contains("no reviewed tree to hold a replay to"), "output: {output}");
    assert_eq!(git(&s.work, &["rev-parse", "main"])?.trim(), before);
    assert_eq!(git(&s.work, &["status", "--porcelain"])?.trim(), "");

    // The squash path reaches the conflict instead, as it always did.
    let (ok, output) = review(&s.work, &["--land", "origin/work-0001-feature", "--squash", "--yes"])?;
    assert!(!ok, "{output}");
    assert!(output.contains("conflicts"), "output: {output}");
    Ok(())
}

/// cherry-pick replays one parent's changes and a merge commit has two, with
/// nothing in the range saying which. Refused rather than guessed at.
#[test]
fn a_replay_refuses_a_branch_that_carries_a_merge() -> Res<()> {
    let s = scenario("replay-merge")?;
    git(&s.work, &["checkout", "-b", "side", "main"])?;
    commit(&s.work, "c.txt", "gamma\n", "side: a commit", "2024-01-04T00:00:00 +0000")?;
    git(&s.work, &["checkout", "-B", "merged", "refs/remotes/origin/work-0001-feature"])?;
    git(&s.work, &["merge", "--no-ff", "-m", "feature: merge side", "side"])?;
    git(&s.work, &["checkout", "main"])?;
    let before = git(&s.work, &["rev-parse", "main"])?.trim().to_string();

    let (ok, output) = review(&s.work, &["--land", "merged", "--yes"])?;
    assert!(!ok, "a branch with a merge must not replay:\n{output}");
    assert!(output.contains("merge commit"), "output: {output}");
    assert!(output.contains("squash it instead"), "output: {output}");
    assert_eq!(git(&s.work, &["rev-parse", "main"])?.trim(), before);
    // And the same branch still lands the other way.
    let (ok, output) = review(&s.work, &["--land", "merged", "--squash", "--yes"])?;
    assert!(ok, "squashing a merged branch must still work:\n{output}");
    Ok(())
}

/// The squash path learns this from an empty index after the merge. A replay
/// has to know it before the sequencer starts: a commit that replays to nothing
/// stops the sequence rather than being dropped from it.
#[test]
fn a_replay_refuses_a_branch_that_changes_nothing() -> Res<()> {
    let s = scenario("replay-noop")?;
    let (ok, output) = review(&s.work, &["--land", "origin/work-0001-feature", "--squash", "--yes"])?;
    assert!(ok, "first landing should succeed:\n{output}");
    let before = git(&s.work, &["rev-parse", "main"])?.trim().to_string();

    let (ok, output) = review(&s.work, &["--land", "origin/work-0001-feature", "--yes"])?;
    assert!(!ok, "an already-landed branch must not replay:\n{output}");
    assert!(output.contains("nothing to replay"), "output: {output}");
    assert_eq!(git(&s.work, &["rev-parse", "main"])?.trim(), before);
    assert_eq!(git(&s.work, &["status", "--porcelain"])?.trim(), "");
    Ok(())
}

/// Both modes are pinned to the commit that was reviewed, and to the base it was
/// reviewed against.
#[test]
fn a_replay_refuses_when_either_side_moved_since_the_review() -> Res<()> {
    let s = scenario("replay-moved-pins")?;
    let tip = git(&s.work, &["rev-parse", "refs/remotes/origin/work-0001-feature"])?
        .trim()
        .to_string();
    let before = git(&s.work, &["rev-parse", "main"])?.trim().to_string();

    let (ok, output) = review(
        &s.work,
        &["--land", "origin/work-0001-feature", "--yes", "--expect", "deadbeef"],
    )?;
    assert!(!ok, "a branch that moved must not replay:\n{output}");
    assert!(output.contains("moved since it was reviewed"), "output: {output}");

    let (ok, output) = review(
        &s.work,
        &[
            "--land",
            "origin/work-0001-feature",
            "--yes",
            "--expect",
            &tip,
            "--expect-base",
            "deadbeef",
        ],
    )?;
    assert!(!ok, "a base that moved must not be replayed onto:\n{output}");
    assert!(output.contains("the rebase would differ"), "output: {output}");
    assert_eq!(git(&s.work, &["rev-parse", "main"])?.trim(), before);
    Ok(())
}

#[test]
fn refuses_to_land_with_a_dirty_work_tree() -> Res<()> {
    let s = scenario("dirty")?;
    fs::write(s.work.join("untracked.txt"), "scratch\n")?;

    let (ok, output) = review(&s.work, &["--land", "origin/work-0001-feature", "--squash", "--yes"])?;
    assert!(!ok, "a dirty work tree must block the landing:\n{output}");
    assert!(output.contains("working tree not clean"), "output: {output}");
    assert_eq!(git(&s.work, &["rev-list", "--count", "main"])?.trim(), "1");
    Ok(())
}

#[test]
fn refuses_to_land_onto_the_wrong_branch() -> Res<()> {
    let s = scenario("wrongbranch")?;
    git(&s.work, &["checkout", "-b", "scratch"])?;

    let (ok, output) = review(&s.work, &["--land", "origin/work-0001-feature", "--squash", "--yes"])?;
    assert!(!ok, "landing must require the base to be checked out:\n{output}");
    assert!(output.contains("HEAD is on 'scratch'"), "output: {output}");
    Ok(())
}

#[test]
fn refuses_a_branch_that_changes_nothing() -> Res<()> {
    let s = scenario("noop")?;
    // Land it once, then try again: the second attempt has no staged change.
    let (ok, output) = review(&s.work, &["--land", "origin/work-0001-feature", "--squash", "--yes"])?;
    assert!(ok, "first landing should succeed:\n{output}");

    let (ok, output) = review(&s.work, &["--land", "origin/work-0001-feature", "--squash", "--yes"])?;
    assert!(!ok, "an already-landed branch must not land again:\n{output}");
    assert!(output.contains("nothing staged"), "output: {output}");
    // The refusal cleaned up after itself.
    assert_eq!(git(&s.work, &["status", "--porcelain"])?.trim(), "");
    assert_eq!(git(&s.work, &["rev-list", "--count", "main"])?.trim(), "2");
    Ok(())
}

#[test]
fn land_requires_explicit_confirmation() -> Res<()> {
    let s = scenario("confirm")?;
    let (ok, output) = review(&s.work, &["--land", "origin/work-0001-feature", "--squash"])?;
    assert!(!ok, "--land without --yes must refuse:\n{output}");
    assert!(output.contains("--yes"), "output: {output}");
    assert_eq!(git(&s.work, &["rev-list", "--count", "main"])?.trim(), "1");
    Ok(())
}

#[test]
fn list_shows_remote_branches_newest_first() -> Res<()> {
    let s = scenario("list")?;
    // A second, older branch so the ordering assertion is meaningful.
    git(&s.work, &["checkout", "-b", "work-0000-older"])?;
    commit(&s.work, "c.txt", "gamma\n", "older: a change", "2023-06-01T00:00:00 +0000")?;
    git(&s.work, &["push", "origin", "work-0000-older"])?;
    git(&s.work, &["checkout", "main"])?;

    let (ok, output) = review(&s.work, &["--list"])?;
    assert!(ok, "--list should succeed:\n{output}");
    // age, ahead/behind, review record, then the refname.
    let branches: Vec<&str> = output
        .lines()
        .filter_map(|l| l.split_whitespace().nth(3))
        .collect();
    // `archive` mirrors origin's refs, so each branch appears once per remote;
    // the assertion is on the committer-date ordering.
    assert_eq!(
        branches,
        vec![
            "archive/work-0001-feature",
            "origin/work-0001-feature",
            "origin/work-0000-older"
        ],
        "newest committer date first; got:\n{output}"
    );
    // main is the landing target, never a candidate.
    assert!(!output.contains("origin/main"), "output: {output}");
    Ok(())
}

#[test]
fn landed_message_is_byte_identical_to_the_branch_messages() -> Res<()> {
    let s = scenario("bytes")?;
    let base = git(&s.work, &["merge-base", "main", "origin/work-0001-feature"])?
        .trim()
        .to_string();
    let expected = git(
        &s.work,
        &["log", "--reverse", "--format=%B", &format!("{base}..origin/work-0001-feature")],
    )?;

    let (ok, output) = review(&s.work, &["--land", "origin/work-0001-feature", "--squash", "--yes"])?;
    assert!(ok, "{output}");

    // `git log %B` and `git commit --cleanup=whitespace` differ only in the
    // trailing blank lines git strips; compare with those normalised away.
    let landed = git(&s.work, &["log", "-1", "--format=%B", "main"])?;
    assert_eq!(landed.trim_end(), expected.trim_end(), "landed message drifted");
    Ok(())
}

/// `git commit` transcodes a message it cannot read as UTF-8, so the only way a
/// non-UTF-8 message reaches the store is a hand-built object — which is what an
/// importer produces. Decoding it lossily would land U+FFFD.
#[test]
fn a_non_utf8_commit_message_lands_without_replacement_chars() -> Res<()> {
    let s = scenario("latin1")?;
    git(&s.work, &["checkout", "-q", "-b", "work-0002-latin1"])?;
    fs::write(s.work.join("c.txt"), "gamma\n")?;
    git(&s.work, &["add", "c.txt"])?;
    git(&s.work, &["commit", "-q", "-m", "placeholder"])?;
    let tree = git(&s.work, &["rev-parse", "HEAD^{tree}"])?.trim().to_string();
    let parent = git(&s.work, &["rev-parse", "HEAD^"])?.trim().to_string();

    let mut object = format!(
        "tree {tree}\nparent {parent}\n\
         author Test Integrator <integrator@example.invalid> 1700000000 +0000\n\
         committer Test Integrator <integrator@example.invalid> 1700000000 +0000\n\n"
    )
    .into_bytes();
    // 0xe9/0xe8 are latin-1 e-acute/e-grave and not valid UTF-8 on their own.
    object.extend_from_slice(b"feature: caf\xe9 handling\n\nlatin-1 body \xe9\xe8\n");
    let oid = git_stdin(&s.work, &["hash-object", "-t", "commit", "-w", "--stdin"], &object)?;
    git(&s.work, &["update-ref", "refs/heads/work-0002-latin1", &oid])?;
    git(&s.work, &["push", "-q", "origin", "work-0002-latin1"])?;
    git(&s.work, &["checkout", "-q", "main"])?;
    git(&s.work, &["branch", "-D", "work-0002-latin1"])?;

    let (ok, output) = review(&s.work, &["--land", "origin/work-0002-latin1", "--squash", "--yes"])?;
    assert!(ok, "{output}");

    let landed = git_bytes(&s.work, &["log", "-1", "--format=%B", "main"])?;
    assert!(
        !landed.windows(3).any(|w| w == [0xef, 0xbf, 0xbd]),
        "the message was decoded lossily: {}",
        String::from_utf8_lossy(&landed)
    );
    // git transcodes the latin-1 bytes it is handed; what must not happen is
    // handing it U+FFFD, which is valid UTF-8 and would be stored as-is.
    assert_eq!(
        String::from_utf8_lossy(&landed).trim_end(),
        "feature: caf\u{e9} handling\n\nlatin-1 body \u{e9}\u{e8}"
    );
    Ok(())
}

#[test]
fn a_conflict_leaves_the_branch_message_in_squash_msg() -> Res<()> {
    let s = scenario("conflict")?;
    // Make main and the branch touch the same line.
    git(&s.work, &["checkout", "-q", "-b", "work-0002-conflict", "origin/work-0001-feature"])?;
    fs::write(s.work.join("shared.txt"), "branch side\n")?;
    git(&s.work, &["add", "shared.txt"])?;
    git_env(
        &s.work,
        &["commit", "-m", "conflict: branch writes shared.txt"],
        &[("GIT_AUTHOR_DATE", "2024-02-01T00:00:00 +0000"),
          ("GIT_COMMITTER_DATE", "2024-02-01T00:00:00 +0000")],
    )?;
    git(&s.work, &["push", "origin", "work-0002-conflict"])?;
    git(&s.work, &["checkout", "-q", "main"])?;
    git(&s.work, &["branch", "-D", "work-0002-conflict"])?;
    fs::write(s.work.join("shared.txt"), "main side\n")?;
    git(&s.work, &["add", "shared.txt"])?;
    git(&s.work, &["commit", "-m", "base: main writes shared.txt"])?;

    let (ok, output) = review(&s.work, &["--land", "origin/work-0002-conflict", "--squash", "--yes"])?;
    assert!(!ok, "a conflicted squash must not report success:\n{output}");
    assert!(output.contains("conflicts"), "output: {output}");

    // The index really is conflicted -- this is what separates Conflict from Failed.
    let unmerged = git(&s.work, &["diff", "--name-only", "--diff-filter=U"])?;
    assert_eq!(unmerged.trim(), "shared.txt");

    // The alias's key property: a hand-finished commit still gets the branch's
    // message, not git's "Squashed commit of the following" template.
    let msg = squash_msg(&s.work)?;
    assert!(msg.contains("conflict: branch writes shared.txt"), "SQUASH_MSG: {msg}");
    assert!(!msg.contains("Squashed commit of the following"), "SQUASH_MSG: {msg}");
    assert!(msg.contains("feature: first step"), "SQUASH_MSG: {msg}");
    Ok(())
}

#[test]
fn a_non_conflict_merge_failure_is_not_reported_as_a_conflict() -> Res<()> {
    let s = scenario("mergefail")?;
    // Diverge main (no textual conflict), then merge.ff=only makes
    // `merge --squash` refuse outright without touching the index.
    commit(&s.work, "unrelated.txt", "x\n", "base: an unrelated commit", "2024-01-04T00:00:00 +0000")?;
    git(&s.work, &["config", "merge.ff", "only"])?;

    let (ok, output) = review(&s.work, &["--land", "origin/work-0001-feature", "--squash", "--yes"])?;
    assert!(!ok, "{output}");
    assert!(
        output.contains("merge failed") && output.contains("the work tree was not modified"),
        "a clean-index merge refusal must not be called a conflict:\n{output}"
    );
    assert!(!output.contains("hit conflicts"), "output: {output}");
    assert_eq!(git(&s.work, &["status", "--porcelain"])?.trim(), "");
    Ok(())
}

#[test]
fn refuses_a_branch_that_moved_since_it_was_reviewed() -> Res<()> {
    // The TUI pins the reviewed tip and passes it as --expect; a concurrent
    // fetch must not slip new commits into the landing.
    let s = scenario("moved")?;
    let stale = git(&s.work, &["rev-parse", "origin/work-0001-feature~1"])?
        .trim()
        .to_string();

    let (ok, output) = review(
        &s.work,
        &["--land", "origin/work-0001-feature", "--squash", "--expect", &stale, "--yes"],
    )?;
    assert!(!ok, "a branch that moved since the review must be refused:\n{output}");
    assert!(output.contains("moved since it was reviewed"), "output: {output}");
    assert_eq!(git(&s.work, &["rev-list", "--count", "main"])?.trim(), "1");

    // The same landing goes through once the pin names the current tip.
    let tip = git(&s.work, &["rev-parse", "origin/work-0001-feature"])?.trim().to_string();
    let (ok, output) = review(
        &s.work,
        &["--land", "origin/work-0001-feature", "--squash", "--expect", &tip, "--yes"],
    )?;
    assert!(ok, "an up-to-date pin must land:\n{output}");
    Ok(())
}

#[test]
fn refuses_a_branch_with_no_commits_beyond_the_base() -> Res<()> {
    let s = scenario("nocommits")?;
    // main itself has nothing beyond main.
    let (ok, output) = review(&s.work, &["--land", "main", "--squash", "--yes"])?;
    assert!(!ok, "{output}");
    assert!(output.contains("no commits beyond"), "output: {output}");
    assert_eq!(git(&s.work, &["rev-list", "--count", "main"])?.trim(), "1");
    Ok(())
}

/// The message is built before the merge so a blank one refuses while the work
/// tree is still pristine.
#[test]
fn refuses_a_branch_whose_commits_carry_no_message() -> Res<()> {
    let s = scenario("nomsg")?;
    git(&s.work, &["checkout", "-q", "-b", "work-0003-silent"])?;
    fs::write(s.work.join("d.txt"), "delta\n")?;
    git(&s.work, &["add", "d.txt"])?;
    git(&s.work, &["commit", "-q", "--allow-empty-message", "-m", ""])?;
    git(&s.work, &["push", "-q", "origin", "work-0003-silent"])?;
    git(&s.work, &["checkout", "-q", "main"])?;
    git(&s.work, &["branch", "-D", "work-0003-silent"])?;

    let (ok, output) = review(&s.work, &["--land", "origin/work-0003-silent", "--squash", "--yes"])?;
    assert!(!ok, "a messageless branch must not land:\n{output}");
    assert!(output.contains("empty message"), "output: {output}");
    assert_eq!(git(&s.work, &["rev-list", "--count", "main"])?.trim(), "1");
    // Refused before the merge: the work tree is untouched.
    assert!(git(&s.work, &["status", "--porcelain"])?.trim().is_empty(), "tree was touched");
    Ok(())
}

#[test]
fn refuses_when_head_is_detached() -> Res<()> {
    let s = scenario("detached")?;
    let head = git(&s.work, &["rev-parse", "HEAD"])?.trim().to_string();
    git(&s.work, &["checkout", "-q", &head])?;

    let (ok, output) = review(&s.work, &["--land", "origin/work-0001-feature", "--squash", "--yes"])?;
    assert!(!ok, "{output}");
    assert!(output.contains("detached"), "output: {output}");
    Ok(())
}

#[test]
fn a_hook_rejection_leaves_the_squash_staged_and_says_so() -> Res<()> {
    let s = scenario("hook")?;
    let hooks = git(&s.work, &["rev-parse", "--absolute-git-dir"])?.trim().to_string();
    let hook = Path::new(&hooks).join("hooks").join("pre-commit");
    fs::create_dir_all(Path::new(&hooks).join("hooks"))?;
    fs::write(&hook, "#!/bin/sh\nexit 1\n")?;
    let mut perms = fs::metadata(&hook)?.permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
    fs::set_permissions(&hook, perms)?;

    let (ok, output) = review(&s.work, &["--land", "origin/work-0001-feature", "--squash", "--yes"])?;
    assert!(!ok, "a rejected commit must not report success:\n{output}");
    assert!(output.contains("still staged"), "output: {output}");
    // HEAD did not move, and the squash is recoverable by hand.
    assert_eq!(git(&s.work, &["rev-list", "--count", "main"])?.trim(), "1");
    assert!(squash_msg(&s.work)?.contains("feature: first step"));
    Ok(())
}

/// `--preview` is the non-TUI review path, so its diff has to stay readable:
/// a newline is a control character and blanket scrubbing folds the whole thing
/// onto one line.
#[test]
fn the_preview_keeps_its_line_breaks() -> Res<()> {
    let s = scenario("preview-lines")?;
    let (ok, output) = review(&s.work, &["--preview", "origin/work-0001-feature"])?;
    assert!(ok, "{output}");
    assert!(!output.contains('\u{b7}'), "line breaks were scrubbed away:\n{output}");
    assert!(output.lines().count() > 10, "the output collapsed:\n{output}");
    Ok(())
}

/// The pane and the commit must read the message from the same query: only the
/// landing's is pinned to UTF-8, so ambient `i18n.logOutputEncoding` would
/// otherwise show a message the commit does not carry.
#[test]
fn the_preview_shows_the_message_the_commit_will_carry() -> Res<()> {
    let s = scenario("preview-encoding")?;
    let raw: &[u8] = b"feature: caf\xe9 handling\n\nlatin-1 body\n";
    let msg = s.work.join("msg.raw");
    fs::write(&msg, raw)?;
    git(&s.work, &["checkout", "-q", "-b", "work-0004-latin1"])?;
    fs::write(s.work.join("e.txt"), "epsilon\n")?;
    git(&s.work, &["add", "e.txt"])?;
    // Records the bytes verbatim plus an `encoding ISO-8859-1` header.
    git(
        &s.work,
        &["-c", "i18n.commitEncoding=ISO-8859-1", "commit", "-q", "-F", &msg.to_string_lossy()],
    )?;
    fs::remove_file(&msg)?;
    git(&s.work, &["push", "-q", "origin", "work-0004-latin1"])?;
    git(&s.work, &["checkout", "-q", "main"])?;
    git(&s.work, &["branch", "-D", "work-0004-latin1"])?;
    // Ambient config that would hand the preview raw latin-1 bytes.
    git(&s.work, &["config", "i18n.logOutputEncoding", "ISO-8859-1"])?;

    let (ok, output) = review(&s.work, &["--preview", "origin/work-0004-latin1"])?;
    assert!(ok, "{output}");
    assert!(output.contains("caf\u{e9} handling"), "the preview mangled the message:\n{output}");
    Ok(())
}

/// A push publishes every local commit on the base, not only the one reviewed.
#[test]
fn the_push_says_which_commits_it_publishes() -> Res<()> {
    let s = scenario("unpushed")?;
    git(&s.work, &["branch", "--set-upstream-to=origin/main", "main"])?;
    commit(
        &s.work,
        "local.txt",
        "local\n",
        "main: a hand-made local commit",
        "2024-01-04T00:00:00 +0000",
    )?;

    let (ok, output) =
        review(&s.work, &["--land", "origin/work-0001-feature", "--squash", "--yes", "--push"])?;
    assert!(ok, "{output}");
    assert!(output.contains("publishing 2 commits"), "output: {output}");
    assert!(output.contains("a hand-made local commit"), "output: {output}");
    Ok(())
}

/// A `pre-commit` hook (or a concurrent `git add`) can put content in the index
/// that the review pane never showed, and git commits the index as it finds it.
/// The commit is checked against the tree that was staged, so it cannot ride
/// along — and the commit that carried it is rolled back, not left on the base.
#[test]
fn content_staged_during_the_commit_does_not_ride_along() -> Res<()> {
    let s = scenario("sneak")?;
    let dir = git(&s.work, &["rev-parse", "--absolute-git-dir"])?.trim().to_string();
    let hooks = Path::new(&dir).join("hooks");
    fs::create_dir_all(&hooks)?;
    let hook = hooks.join("pre-commit");
    fs::write(&hook, "#!/bin/sh\nprintf sneaked > sneaky.txt\ngit add sneaky.txt\n")?;
    let mut perms = fs::metadata(&hook)?.permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
    fs::set_permissions(&hook, perms)?;

    let main_before = git(&s.work, &["rev-parse", "main"])?.trim().to_string();
    let origin_before = git(&s.origin, &["rev-parse", "main"])?.trim().to_string();
    let (ok, output) =
        review(&s.work, &["--land", "origin/work-0001-feature", "--squash", "--yes", "--push"])?;
    assert!(!ok, "unreviewed content must not land:\n{output}");
    assert!(output.contains("not the staged"), "output: {output}");
    assert_eq!(git(&s.work, &["rev-parse", "main"])?.trim(), main_before, "not rolled back");
    assert_eq!(git(&s.origin, &["rev-parse", "main"])?.trim(), origin_before, "it was published");
    Ok(())
}

/// Several agents work this repo at once. A base that advances after the squash
/// is staged would get a commit whose tree predates it, silently reverting the
/// work that arrived in between — so that commit must never be created.
#[test]
fn a_base_that_moves_while_the_squash_is_staged_is_refused() -> Res<()> {
    let s = scenario("toctou")?;
    let tree = git(&s.work, &["rev-parse", "HEAD^{tree}"])?.trim().to_string();
    let head = git(&s.work, &["rev-parse", "HEAD"])?.trim().to_string();
    let intruder = git(
        &s.work,
        &["commit-tree", &tree, "-p", &head, "-m", "other: landed mid-squash"],
    )?
    .trim()
    .to_string();

    // `git merge --squash` writes ORIG_HEAD, so this fires inside the window
    // between reading HEAD and committing. The flag file makes it fire once —
    // the update-ref below re-enters the hook.
    let dir = git(&s.work, &["rev-parse", "--absolute-git-dir"])?.trim().to_string();
    let hooks = Path::new(&dir).join("hooks");
    fs::create_dir_all(&hooks)?;
    let flag = hooks.join("race-once");
    fs::write(&flag, "")?;
    let hook = hooks.join("reference-transaction");
    fs::write(
        &hook,
        format!(
            "#!/bin/sh\ntest \"$1\" = committed || exit 0\ntest -f {flag} || exit 0\n\
             rm -f {flag}\ngit update-ref refs/heads/main {intruder}\n",
            flag = flag.to_string_lossy()
        ),
    )?;
    let mut perms = fs::metadata(&hook)?.permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
    fs::set_permissions(&hook, perms)?;

    let origin_before = git(&s.origin, &["rev-parse", "main"])?.trim().to_string();
    let (ok, output) =
        review(&s.work, &["--land", "origin/work-0001-feature", "--squash", "--yes", "--push"])?;
    assert!(!ok, "a raced landing must not report success:\n{output}");
    assert!(output.contains("while the squash was staged"), "output: {output}");
    // The remedy must not rewind past the commit the guard just protected.
    assert!(
        !output.contains(&format!("reset --hard {}", head.get(..12).unwrap_or_default())),
        "the remedy would discard the concurrent commit:\n{output}"
    );
    // Nothing was committed on top of the intruder, and nothing was published.
    assert_eq!(git(&s.work, &["rev-parse", "main"])?.trim(), intruder);
    assert_eq!(
        git(&s.origin, &["rev-parse", "main"])?.trim(),
        origin_before,
        "the raced commit was published"
    );
    Ok(())
}

/// `git push` writes to EVERY configured pushurl, so the sentinel counts
/// wherever it sits in the list — not just first.
#[test]
fn a_no_push_sentinel_behind_a_real_pushurl_still_blocks_the_remote() -> Res<()> {
    let s = scenario("latepush")?;
    let before = git(&s.backup, &["rev-parse", "main"])?.trim().to_string();
    git(&s.work, &["remote", "set-url", "--push", "backup", &s.backup.to_string_lossy()])?;
    git(&s.work, &["remote", "set-url", "--add", "--push", "backup", "no_push"])?;

    let (ok, output) =
        review(&s.work, &["--land", "origin/work-0001-feature", "--squash", "--yes", "--push"])?;
    assert!(ok, "{output}");
    assert!(output.contains("backup (skipped: no_push)"), "output: {output}");
    assert_eq!(
        git(&s.backup, &["rev-parse", "main"])?.trim(),
        before,
        "backup was pushed to despite a no_push pushurl"
    );
    Ok(())
}

/// `remote.<name>.skipPushAll` takes a remote out of `--push`/`P` and out of
/// nothing else — the difference from `no_push`, which is a push url nothing
/// can push to. The push by name at the end is the whole point of the option:
/// a laptop that is usually asleep is still a remote you push to on the days
/// it is awake.
#[test]
fn a_remote_that_opts_out_of_push_all_is_left_out_and_still_pushable_by_name() -> Res<()> {
    let s = scenario("skip-pushall")?;
    let before = git(&s.backup, &["rev-parse", "main"])?.trim().to_string();
    git(&s.work, &["config", "remote.backup.skipPushAll", "true"])?;

    let (ok, output) =
        review(&s.work, &["--land", "origin/work-0001-feature", "--squash", "--yes", "--push"])?;
    assert!(ok, "{output}");
    assert!(
        output.contains("backup left out: remote.backup.skipPushAll is set"),
        "the run must say which remote it left out, and why:\n{output}"
    );
    assert!(!output.contains("==> backup"), "the opted-out remote was pushed to:\n{output}");
    assert_eq!(
        git(&s.backup, &["rev-parse", "main"])?.trim(),
        before,
        "backup was published to despite opting out of the push-all"
    );
    // Still an ordinary remote: what it opted out of was the push-all.
    let head = git(&s.work, &["rev-parse", "main"])?.trim().to_string();
    git(&s.work, &["push", "backup", "main"])?;
    assert_eq!(git(&s.backup, &["rev-parse", "main"])?.trim(), head);
    Ok(())
}

/// `--delete-landed` sweeps the remotes the push REACHED, which is no longer
/// every pushable one: a remote that sat the push-all out never took the
/// landing, so deleting its copy of the branch would leave that remote with
/// neither. The TUI's sweep was already scoped this way for a mirror that
/// rejected a push; an opt-out is the same shape.
#[test]
fn the_landed_sweep_spares_a_branch_on_a_remote_the_push_all_left_out() -> Res<()> {
    let s = scenario("skip-pushall-sweep")?;
    // backup carries the branch too, so there is something to spare.
    git(&s.work, &["push", "backup", "origin/work-0001-feature:refs/heads/work-0001-feature"])?;
    git(&s.work, &["fetch", "-q", "backup"])?;
    git(&s.work, &["config", "remote.backup.skipPushAll", "true"])?;

    let (ok, output) = review(
        &s.work,
        &["--land", "origin/work-0001-feature", "--squash", "--yes", "--push", "--delete-landed"],
    )?;
    assert!(ok, "{output}");
    let heads = git(&s.backup, &["for-each-ref", "--format=%(refname:short)", "refs/heads"])?;
    assert!(
        heads.contains("work-0001-feature"),
        "a remote the push-all left out must keep its branch: {heads}\n{output}"
    );
    // Kept is not the same as forgotten: the run says so, as the push says
    // which remotes it left out.
    assert!(
        output.contains("backup/work-0001-feature kept: the push did not reach backup"),
        "the run must say the branch was kept, and why:\n{output}"
    );
    let heads = git(&s.origin, &["for-each-ref", "--format=%(refname:short)", "refs/heads"])?;
    assert!(
        !heads.contains("work-0001-feature"),
        "the remote that took the landing should have been swept: {heads}\n{output}"
    );
    Ok(())
}

/// `on` is a boolean to git and to nothing that compares against the string
/// `true`, so this is what pins `--type=bool` itself: without it the value
/// comes back as `on`, the remote is pushed to, and every other test here
/// still passes.
#[test]
fn a_spelling_only_git_reads_as_true_still_opts_the_remote_out() -> Res<()> {
    let s = scenario("skip-pushall-on")?;
    let before = git(&s.backup, &["rev-parse", "main"])?.trim().to_string();
    git(&s.work, &["config", "remote.backup.skipPushAll", "on"])?;

    let (ok, output) =
        review(&s.work, &["--land", "origin/work-0001-feature", "--squash", "--yes", "--push"])?;
    assert!(ok, "{output}");
    assert!(output.contains("backup left out"), "`on` must opt the remote out:\n{output}");
    assert_eq!(
        git(&s.backup, &["rev-parse", "main"])?.trim(),
        before,
        "backup was published to despite `skipPushAll = on`"
    );
    Ok(())
}

/// A landing nobody would take is still a landing that published nothing, so
/// `--push` fails as it does when every remote is `no_push`. What changes is
/// the reason it gives: a config the operator wrote, not one nobody meant.
#[test]
fn a_push_all_every_remote_opted_out_of_fails_saying_so() -> Res<()> {
    let s = scenario("skip-pushall-all")?;
    for remote in ["origin", "backup", "archive"] {
        git(&s.work, &["config", &format!("remote.{remote}.skipPushAll"), "true"])?;
    }

    let (ok, output) =
        review(&s.work, &["--land", "origin/work-0001-feature", "--squash", "--yes", "--push"])?;
    assert!(!ok, "a commit published nowhere is not a successful --push:\n{output}");
    assert!(
        output.contains("every remote asked to be left out"),
        "the reason must be the opt-out, not ineligibility:\n{output}"
    );
    // The landing itself happened: only the publish did not.
    assert_eq!(git(&s.work, &["rev-list", "--count", "main"])?.trim(), "2");
    Ok(())
}

/// git parses the value, so a repeated key resolves to its LAST value as git
/// resolves one.
#[test]
fn the_opt_out_is_a_git_boolean_and_the_last_value_wins() -> Res<()> {
    let s = scenario("skip-pushall-bool")?;
    // The `false` added after the opt-out is what decides, as it would for any
    // git config.
    git(&s.work, &["config", "remote.backup.skipPushAll", "yes"])?;
    git(&s.work, &["config", "--add", "remote.backup.skipPushAll", "false"])?;

    let (ok, output) =
        review(&s.work, &["--land", "origin/work-0001-feature", "--squash", "--yes", "--push"])?;
    assert!(ok, "{output}");
    assert!(!output.contains("backup left out"), "the last value must win:\n{output}");
    assert_eq!(
        git(&s.backup, &["rev-parse", "main"])?.trim(),
        git(&s.work, &["rev-parse", "main"])?.trim(),
        "backup opted back in and was not published to"
    );
    Ok(())
}

/// A value git cannot read as a boolean is not permission to push: the remote
/// is left out and git's own diagnostic — which names the key and the value —
/// is what says so. The landing itself still succeeds and still publishes
/// everywhere else.
#[test]
fn an_opt_out_that_is_not_a_boolean_leaves_the_remote_out_and_says_why() -> Res<()> {
    let s = scenario("skip-pushall-junk")?;
    let before = git(&s.backup, &["rev-parse", "main"])?.trim().to_string();
    git(&s.work, &["config", "remote.backup.skipPushAll", "sometimes"])?;

    let (ok, output) =
        review(&s.work, &["--land", "origin/work-0001-feature", "--squash", "--yes", "--push"])?;
    assert!(ok, "an unreadable opt-out must not fail the landing:\n{output}");
    assert!(output.contains("backup left out:"), "output: {output}");
    assert!(output.contains("sometimes"), "git's diagnostic must name the value:\n{output}");
    assert_eq!(
        git(&s.backup, &["rev-parse", "main"])?.trim(),
        before,
        "a remote whose opt-out cannot be read was pushed to"
    );
    assert_eq!(
        git(&s.origin, &["rev-parse", "main"])?.trim(),
        git(&s.work, &["rev-parse", "main"])?.trim(),
        "the other remotes must still be published to"
    );
    Ok(())
}

#[test]
fn a_partial_push_failure_is_reported_as_failure() -> Res<()> {
    let s = scenario("partialpush")?;
    // Diverge backup so its push is rejected while origin's succeeds.
    git(&s.backup, &["branch", "-f", "main", "HEAD"])?;
    let clone = s.work.parent().unwrap_or(&s.work).join("diverge");
    git(s.work.parent().unwrap_or(&s.work), &["clone", "-q", &s.backup.to_string_lossy(), &clone.to_string_lossy()])?;
    for (k, v) in [("user.name", "Other"), ("user.email", "o@e.invalid")] {
        git(&clone, &["config", k, v])?;
    }
    fs::write(clone.join("divergent.txt"), "elsewhere\n")?;
    git(&clone, &["add", "divergent.txt"])?;
    git(&clone, &["commit", "-qm", "backup: a commit td-review has never seen"])?;
    git(&clone, &["push", "-q", "origin", "main"])?;

    let (ok, output) = review(
        &s.work,
        &["--land", "origin/work-0001-feature", "--squash", "--yes", "--push"],
    )?;
    assert!(!ok, "a rejected push must fail the run:\n{output}");
    // git prints `! [rejected] … (fetch first)` above a generic
    // `error: failed to push some refs`; the summary must carry the reason.
    let summary = output.lines().find(|l| l.contains("push to backup failed")).unwrap_or_default();
    assert!(summary.contains("[rejected]"), "the reason was not reported: {summary}");
    // origin still got it, and the local commit stands.
    let head = git(&s.work, &["rev-parse", "main"])?.trim().to_string();
    assert_eq!(git(&s.origin, &["rev-parse", "main"])?.trim(), head);
    Ok(())
}

#[test]
fn every_remote_marked_no_push_is_not_reported_as_published() -> Res<()> {
    let s = scenario("nopush")?;
    for remote in ["origin", "backup"] {
        git(&s.work, &["remote", "set-url", "--push", remote, "no_push"])?;
    }
    let (ok, output) = review(
        &s.work,
        &["--land", "origin/work-0001-feature", "--squash", "--yes", "--push"],
    )?;
    assert!(!ok, "publishing nothing must not exit 0:\n{output}");
    assert!(output.contains("local only") || output.contains("no remote was eligible"),
            "output: {output}");
    // The commit is real, it just was not published.
    assert_eq!(git(&s.work, &["rev-list", "--count", "main"])?.trim(), "2");
    Ok(())
}

#[test]
fn lands_onto_a_base_other_than_main() -> Res<()> {
    let s = scenario("altbase")?;
    git(&s.work, &["checkout", "-q", "-b", "release"])?;

    let (ok, output) = review(
        &s.work,
        &["--base", "release", "--land", "origin/work-0001-feature", "--squash", "--yes"],
    )?;
    assert!(ok, "{output}");
    assert_eq!(git(&s.work, &["rev-list", "--count", "release"])?.trim(), "2");
    // main was not touched.
    assert_eq!(git(&s.work, &["rev-list", "--count", "main"])?.trim(), "1");
    Ok(())
}

#[test]
fn flag_combinations_that_do_nothing_are_rejected() -> Res<()> {
    let s = scenario("flags")?;
    // Each row pins the message it must be rejected WITH: several of these
    // combinations also fail for want of a tty, which would green the assertion
    // without the guard ever running.
    for (args, want) in [
        (vec!["--list", "--land", "origin/work-0001-feature", "--squash", "--yes"], "mutually exclusive"),
        (vec!["--push"], "--push is only meaningful with --land"),
        (vec!["--squash"], "--squash is only meaningful with --land"),
        (
            vec!["--yes"],
            "--yes is only meaningful with --land, --delete or --prune-worktrees",
        ),
        (vec!["--expect", "deadbeef"], "--expect/--expect-base are only meaningful with --land"),
        (vec!["--list", "--delete", "origin/work-0001-feature", "--yes"], "mutually exclusive"),
        (
            vec![
                "--delete",
                "origin/work-0001-feature",
                "--land",
                "origin/work-0001-feature", "--squash",
                "--yes",
            ],
            "mutually exclusive",
        ),
        // Deleting the branch is only safe once its work is published.
        (
            vec!["--delete-landed", "--land", "origin/work-0001-feature", "--squash", "--yes"],
            "--delete-landed needs --land <branch> --push",
        ),
    ] {
        let (ok, output) = review(&s.work, &args)?;
        assert!(!ok, "{args:?} should be rejected, not silently ignored:\n{output}");
        assert!(
            output.contains(want),
            "{args:?} was rejected for the wrong reason (wanted {want:?}):\n{output}"
        );
        // Rejected means nothing happened, not "errored after acting".
        assert_eq!(
            git(&s.work, &["rev-list", "--count", "main"])?.trim(),
            "1",
            "{args:?} changed the repo before erroring:\n{output}"
        );
        assert!(remote_has_branch(&s.origin)?, "{args:?} deleted the branch:\n{output}");
    }
    Ok(())
}

/// Push the branch to `backup` as well, so a delete has more than one remote to
/// clear, and refresh the tracking refs the delete reads.
fn publish_branch_everywhere(s: &Scenario) -> Res<()> {
    git(&s.work, &["push", "backup", "origin/work-0001-feature:refs/heads/work-0001-feature"])?;
    git(&s.work, &["fetch", "backup"])?;
    Ok(())
}

#[test]
fn a_remote_head_symref_is_not_offered_as_a_branch() -> Res<()> {
    let s = scenario("symref")?;
    // What `git remote set-head` leaves behind. git shortens it to plain
    // "origin", so it does not look like a `*/HEAD` alias in the listing.
    git(&s.work, &["symbolic-ref", "refs/remotes/origin/HEAD", "refs/remotes/origin/main"])?;

    let (ok, listed) = review(&s.work, &["--list"])?;
    assert!(ok, "{listed}");
    for line in listed.lines() {
        let name = line.split_whitespace().nth(2).unwrap_or_default();
        assert_ne!(name, "origin", "the HEAD symref must not be listed:\n{listed}");
        assert_ne!(name, "origin/HEAD", "the HEAD symref must not be listed:\n{listed}");
    }
    assert!(listed.contains("origin/work-0001-feature"), "real branches still listed:\n{listed}");
    Ok(())
}

/// The reported row, through the shipped binary. `--list` printed the review
/// record and nothing else, so a branch whose commits main already carries read
/// `ok` — a cell that means "ready to land" beside a landing that refuses it.
/// Its tip no longer merges (the base moved onto a file it also added), which
/// is what stops the tree question answering and what left this row saying `ok`
/// after the TUI's had learnt to say `landed`.
#[test]
fn list_says_landed_for_a_branch_the_base_carries_and_no_longer_merges() -> Res<()> {
    let s = scenario("list-carried")?;
    // The base moves first, so the replay rewrites the oids rather than
    // fast-forwarding onto them — without this the branch is simply an
    // ancestor and the plain tree question answers.
    commit(&s.work, "c.txt", "gamma\n", "base: moves on", "2024-01-04T00:00:00 +0000")?;
    let (ok, output) = review(&s.work, &["--land", "origin/work-0001-feature", "--yes"])?;
    assert!(ok, "the replay landing should succeed:\n{output}");
    // And then onto a file the branch also adds, so its tip cannot merge.
    commit(&s.work, "a.txt", "gamma\n", "base: the same file again", "2024-01-05T00:00:00 +0000")?;

    let (ok, listed) = review(&s.work, &["--list"])?;
    assert!(ok, "{listed}");
    let row = listed
        .lines()
        .find(|l| l.contains("origin/work-0001-feature"))
        .unwrap_or_default()
        .to_string();
    assert!(row.contains("landed"), "the row must say there is nothing here:\n{listed}");
    assert!(!row.contains(" ok "), "and must not read as ready to land:\n{listed}");
    Ok(())
}

/// The other cell of the shared rule, also through the shipped binary: a branch
/// that still carries work and no longer merges. Nothing but this covers
/// `!merge` on the `--list` side, and the column's width was widened for it.
#[test]
fn list_says_merge_for_a_branch_that_still_has_work_and_does_not_merge() -> Res<()> {
    let s = scenario("list-conflicting")?;
    // A second branch, forked before the base moves and touching the same file,
    // with a COMPLETE record — `!merge` replaces `ok` and nothing else.
    git(&s.work, &["checkout", "-b", "work-0002-open", "main"])?;
    commit(
        &s.work,
        "a.txt",
        "open\n",
        &format!("open: step\n\n{RECORD}"),
        "2024-01-04T00:00:00 +0000",
    )?;
    git(&s.work, &["push", "origin", "work-0002-open"])?;
    git(&s.work, &["checkout", "main"])?;
    git(&s.work, &["branch", "-D", "work-0002-open"])?;
    // The base takes that file to a third value, so the branch cannot merge.
    commit(&s.work, "a.txt", "gamma\n", "base: the same file", "2024-01-05T00:00:00 +0000")?;

    let (ok, listed) = review(&s.work, &["--list"])?;
    assert!(ok, "{listed}");
    let row = listed
        .lines()
        .find(|l| l.contains("origin/work-0002-open"))
        .unwrap_or_default()
        .to_string();
    assert!(row.contains("!merge"), "a branch that will not merge must say so:\n{listed}");
    assert!(!row.contains(" ok "), "and must not read as ready to land:\n{listed}");
    // The cell is padded, not run into the branch name: the width this needed
    // is the one `--list` was widened to.
    assert!(row.contains("!merge  origin/work-0002-open"), "column not padded:\n{row}");
    Ok(())
}

fn remote_has_branch(remote: &Path) -> Res<bool> {
    Ok(git(remote, &["for-each-ref", "--format=%(refname)", "refs/heads/work-0001-feature"])?
        .trim()
        .is_empty()
        .eq(&false))
}

#[test]
fn deletes_the_branch_from_every_pushable_remote() -> Res<()> {
    let s = scenario("delete")?;
    publish_branch_everywhere(&s)?;
    assert!(remote_has_branch(&s.origin)?);
    assert!(remote_has_branch(&s.backup)?);

    // Unqualified: every pushable remote carrying the name.
    let (ok, output) = review(&s.work, &["--delete", "work-0001-feature", "--yes"])?;
    assert!(ok, "delete should succeed:\n{output}");

    assert!(!remote_has_branch(&s.origin)?, "origin should no longer carry the branch:\n{output}");
    assert!(!remote_has_branch(&s.backup)?, "backup should no longer carry the branch:\n{output}");
    // `archive` DOES carry the branch (it mirrors origin) and is skipped purely
    // because its push url is `no_push` — the same rule `git pushall` applies.
    assert!(!output.contains("==> archive"), "the no_push remote must be left alone:\n{output}");

    // The deleted remotes' tracking refs go with it, so those rows leave the list.
    let (_, listed) = review(&s.work, &["--list"])?;
    assert!(
        !listed.contains("origin/work-0001-feature"),
        "stale tracking ref left behind:\n{listed}"
    );
    Ok(())
}

/// The landing is a three-way squash onto the base, not a replay of the
/// branch's own diff. Rename a file on the BASE side after the branch forked:
/// the branch still edits the old path, but the squash stages the edit under
/// the NEW one. A preview showing the branch's diff would name a path the
/// landing does not touch — the integrator would approve the wrong thing.
#[test]
fn the_preview_shows_the_merge_result_not_the_branches_own_diff() -> Res<()> {
    let s = scenario("preview-rename")?;
    // Base side: README -> DOCS, after the branch's merge base.
    git(&s.work, &["mv", "README", "DOCS"])?;
    git(&s.work, &["commit", "-m", "base: rename README to DOCS"])?;

    // Branch side: an edit to the OLD path, which merges onto the new one.
    git(&s.work, &["checkout", "-q", "-b", "edits-readme", "origin/work-0001-feature"])?;
    commit(&s.work, "README", "base\nedited\n", "feature: edit README", "2024-01-04T00:00:00 +0000")?;
    git(&s.work, &["push", "-q", "origin", "edits-readme"])?;
    git(&s.work, &["checkout", "-q", "main"])?;
    git(&s.work, &["branch", "-D", "edits-readme"])?;

    let (ok, output) = review(&s.work, &["--preview", "origin/edits-readme"])?;
    assert!(ok, "preview should succeed:\n{output}");
    assert!(
        output.contains("DOCS"),
        "the preview must name the path the squash actually stages:\n{output}"
    );

    // And the claim is exactly right: landing it stages DOCS, not README.
    let (ok, land) = review(&s.work, &["--land", "origin/edits-readme", "--squash", "--yes"])?;
    assert!(ok, "land should succeed:\n{land}");
    let staged = git(&s.work, &["show", "--stat", "--format=", "HEAD"])?;
    assert!(staged.contains("DOCS"), "the landing staged: {staged}\npreview said:\n{output}");
    assert!(!staged.contains("README"), "the landing staged: {staged}");
    Ok(())
}

/// `git rev-parse origin/x` searches refs/tags BEFORE refs/remotes, so a tag
/// named after the remote-tracking ref would decide what gets landed. The list
/// row means the remote branch; so must the landing.
#[test]
fn a_tag_named_like_the_branch_does_not_hijack_the_landing() -> Res<()> {
    let s = scenario("tag-shadow")?;
    // A tag with the ref's exact name, parked on the base (nothing to land).
    git(&s.work, &["tag", "origin/work-0001-feature", "main"])?;

    let (ok, output) = review(&s.work, &["--land", "origin/work-0001-feature", "--squash", "--yes"])?;
    assert!(ok, "the remote branch, not the tag, should land:\n{output}");
    let landed = git(&s.work, &["show", "--stat", "--format=%s", "HEAD"])?;
    assert!(landed.contains("b.txt"), "the branch's content must be what landed: {landed}");
    Ok(())
}

/// The post-land cleanup filters remotes by the oid that was landed, and that
/// oid must come from the same ref the landing used: git's DWIM searches tags
/// before remote-tracking refs, so a shadowing tag would leave every remote
/// looking "not the landed commit" and the branch undeleted.
#[test]
fn a_tag_named_like_the_branch_does_not_derail_the_cleanup() -> Res<()> {
    let s = scenario("tag-cleanup")?;
    git(&s.work, &["tag", "origin/work-0001-feature", "main"])?;

    let (ok, output) = review(
        &s.work,
        &["--land", "origin/work-0001-feature", "--squash", "--yes", "--push", "--delete-landed"],
    )?;
    assert!(ok, "{output}");
    assert!(!remote_has_branch(&s.origin)?, "the landed branch was left on origin:\n{output}");
    Ok(())
}

#[test]
fn flags_accept_both_the_spaced_and_the_equals_spelling() -> Res<()> {
    let s = scenario("flags-equals")?;
    git(&s.work, &["checkout", "-b", "release"])?;

    let (ok, output) = review(&s.work, &["--base=release", "--list"])?;
    assert!(ok, "--base=release should be accepted:\n{output}");
    assert!(output.contains("work-0001-feature"), "the branch should still list:\n{output}");

    // A value on a switch is a typo; silently ignoring it would hide the mistake.
    let (ok, output) = review(&s.work, &["--list=yes"])?;
    assert!(!ok, "--list=yes should be rejected:\n{output}");
    assert!(output.contains("--list takes no value"), "wrong reason:\n{output}");
    Ok(())
}

/// `origin/x` names one remote's ref. Pressing `D` on that row — or asking for
/// it by name — must not take the same branch off every other remote too.
#[test]
fn a_qualified_delete_touches_only_the_remote_it_names() -> Res<()> {
    let s = scenario("delete-qualified")?;
    publish_branch_everywhere(&s)?;

    let (ok, output) = review(&s.work, &["--delete", "backup/work-0001-feature", "--yes"])?;
    assert!(ok, "delete should succeed:\n{output}");

    assert!(!remote_has_branch(&s.backup)?, "backup was named, it should be gone:\n{output}");
    assert!(remote_has_branch(&s.origin)?, "origin was not named, it must survive:\n{output}");
    assert!(!output.contains("==> origin"), "origin must not even be contacted:\n{output}");
    Ok(())
}

/// The base guard alone is scoped to whatever `--base` says, so a run based on
/// some other branch could delete `main` off a mirror. `receive.denyDeleteCurrent`
/// is the server's default, not ours to rely on.
#[test]
fn delete_refuses_an_integration_branch_whatever_the_base_is() -> Res<()> {
    let s = scenario("delete-protected")?;
    git(&s.work, &["checkout", "-b", "release"])?;

    let (ok, output) = review(&s.work, &["--base", "release", "--delete", "main", "--yes"])?;
    assert!(!ok, "deleting main must be refused:\n{output}");
    assert!(output.contains("never deleted by this tool"), "wrong reason:\n{output}");
    assert!(!output.contains("==> "), "no remote may be contacted:\n{output}");
    assert!(
        !git(&s.origin, &["rev-parse", "--verify", "--quiet", "refs/heads/main"])?.trim().is_empty(),
        "origin lost main:\n{output}"
    );
    Ok(())
}

/// The local `refs/remotes/<remote>/HEAD` only exists after a `set-head`, so a
/// guard that consulted it alone would fail OPEN on any remote that never had
/// one — the common case. Without it, the remote itself must be asked.
#[test]
fn delete_refuses_a_default_branch_with_no_local_head_ref() -> Res<()> {
    let s = scenario("delete-no-head-ref")?;
    git(&s.work, &["push", "backup", "main:refs/heads/trunk"])?;
    git(&s.backup, &["symbolic-ref", "HEAD", "refs/heads/trunk"])?;
    git(&s.work, &["fetch", "backup"])?;
    // Whatever the fetch may have created, this test is about NOT having it.
    // `symbolic-ref --delete`, not `update-ref -d`: the latter dereferences and
    // would delete refs/remotes/backup/trunk instead.
    let _ = git(&s.work, &["symbolic-ref", "--delete", "refs/remotes/backup/HEAD"]);
    assert!(
        git(&s.work, &["rev-parse", "--verify", "--quiet", "refs/remotes/backup/HEAD"])
            .unwrap_or_default()
            .trim()
            .is_empty(),
        "the local HEAD ref must be absent for this test to mean anything"
    );

    let (ok, output) = review(&s.work, &["--delete", "backup/trunk", "--yes"])?;
    assert!(!ok, "deleting a remote's default branch must be refused:\n{output}");
    assert!(output.contains("backup's default branch"), "wrong reason:\n{output}");
    assert!(!output.contains("==> backup"), "the remote must not be contacted:\n{output}");
    assert!(
        !git(&s.backup, &["rev-parse", "--verify", "--quiet", "refs/heads/trunk"])?.trim().is_empty(),
        "backup lost its default branch:\n{output}"
    );
    Ok(())
}

/// A remote that cannot be asked is not evidence that the branch is safe to
/// delete. Refuse, do not assume.
#[test]
fn delete_refuses_when_the_remote_cannot_be_reached() -> Res<()> {
    let s = scenario("delete-unreachable")?;
    publish_branch_everywhere(&s)?;
    // Tracking refs already fetched; now leave the default branch answerable
    // only over the network, and make the network fail.
    let _ = git(&s.work, &["symbolic-ref", "--delete", "refs/remotes/backup/HEAD"]);
    git(&s.work, &["remote", "set-url", "backup", "/nonexistent/nowhere.git"])?;

    let (ok, output) = review(&s.work, &["--delete", "backup/work-0001-feature", "--yes"])?;
    assert!(!ok, "an unanswerable remote must refuse:\n{output}");
    assert!(
        output.contains("could not be established"),
        "it must say why it refused, not fail some other way:\n{output}"
    );
    assert!(!output.contains("deleted work-0001-feature"), "nothing may be deleted:\n{output}");
    Ok(())
}

/// Same protection where the name is not one of the well-known ones: a mirror
/// whose HEAD points at the branch is refused on that ground alone.
#[test]
fn delete_refuses_a_remotes_own_default_branch() -> Res<()> {
    let s = scenario("delete-remote-head")?;
    git(&s.work, &["push", "backup", "main:refs/heads/trunk"])?;
    git(&s.backup, &["symbolic-ref", "HEAD", "refs/heads/trunk"])?;
    git(&s.work, &["fetch", "backup"])?;
    git(&s.work, &["remote", "set-head", "backup", "-a"])?;

    let (ok, output) =
        review(&s.work, &["--delete", "backup/trunk", "--yes"])?;
    assert!(!ok, "deleting a remote's default branch must be refused:\n{output}");
    assert!(output.contains("backup's default branch"), "wrong reason:\n{output}");
    // Without this the test passes on the SERVER's denyDeleteCurrent refusal,
    // which would hold even with the local guard deleted.
    assert!(!output.contains("==> backup"), "the remote must not be contacted:\n{output}");
    assert!(
        !git(&s.backup, &["rev-parse", "--verify", "--quiet", "refs/heads/trunk"])?.trim().is_empty(),
        "backup lost its default branch:\n{output}"
    );
    Ok(())
}

/// The local refs/remotes/<r>/HEAD symref records whatever the last `set-head`
/// saw, and the delete goes to the PUSH url — which need not even be the same
/// repository. A stale local answer must not authorise an irreversible delete.
#[test]
fn a_stale_head_symref_does_not_authorise_deleting_the_real_default() -> Res<()> {
    let s = scenario("stale-head")?;
    git(&s.work, &["push", "backup", "main:refs/heads/trunk"])?;
    git(&s.backup, &["symbolic-ref", "HEAD", "refs/heads/trunk"])?;
    git(&s.work, &["fetch", "-q", "backup"])?;
    // Says main; backup's actual default is trunk.
    git(&s.work, &["symbolic-ref", "refs/remotes/backup/HEAD", "refs/remotes/backup/main"])?;

    let (ok, output) = review(&s.work, &["--delete", "backup/trunk", "--yes"])?;
    assert!(!ok, "a stale symref must not authorise the delete:\n{output}");
    assert!(output.contains("backup's default branch"), "wrong reason:\n{output}");
    assert!(
        !git(&s.backup, &["rev-parse", "--verify", "--quiet", "refs/heads/trunk"])?.trim().is_empty(),
        "backup lost its default branch:\n{output}"
    );
    Ok(())
}

/// A `HEAD` symref pointing at a branch that does not exist — what a bare repo
/// initialised as `master` and only ever pushed `main` to is left with. It
/// resolves to nothing, so `ls-remote --symref` prints NO line for it, and
/// reading that silence as "unanswerable" refused every delete on that remote.
#[test]
fn a_remote_whose_head_dangles_does_not_block_a_delete() -> Res<()> {
    let s = scenario("delete-dangling-head")?;
    publish_branch_everywhere(&s)?;
    for bare in [&s.origin, &s.backup] {
        git(bare, &["symbolic-ref", "HEAD", "refs/heads/master"])?;
        assert!(
            git(bare, &["ls-remote", "--symref", ".", "HEAD"])?.trim().is_empty(),
            "the fixture must leave HEAD unresolvable for this test to mean anything"
        );
    }

    let (ok, output) = review(&s.work, &["--delete", "work-0001-feature", "--yes"])?;
    assert!(ok, "a HEAD that resolves to nothing protects nothing:\n{output}");
    assert!(!remote_has_branch(&s.origin)?, "origin still carries the branch:\n{output}");
    assert!(!remote_has_branch(&s.backup)?, "backup still carries the branch:\n{output}");
    Ok(())
}

/// No symref line at all — a detached `HEAD`, or a server too old for the
/// symref capability — leaves the oid as the only evidence. At the branch's own
/// tip that is indistinguishable from `HEAD` being the branch, so it refuses.
#[test]
fn an_unnamed_head_at_the_branch_tip_still_refuses() -> Res<()> {
    let s = scenario("delete-unnamed-head")?;
    publish_branch_everywhere(&s)?;
    let tip =
        git(&s.work, &["rev-parse", "refs/remotes/origin/work-0001-feature"])?.trim().to_string();
    // `--no-deref`, or this writes through HEAD to the branch it names.
    git(&s.backup, &["update-ref", "--no-deref", "HEAD", &tip])?;

    let (ok, output) = review(&s.work, &["--delete", "backup/work-0001-feature", "--yes"])?;
    assert!(!ok, "an unnamed HEAD at the tip must refuse:\n{output}");
    assert!(output.contains("without naming a branch"), "wrong reason:\n{output}");
    assert!(!output.contains("==> backup"), "the remote must not be pushed to:\n{output}");
    assert!(remote_has_branch(&s.backup)?, "backup lost the branch:\n{output}");
    Ok(())
}

/// A `HEAD` symrefed outside `refs/heads` is not a branch, so it settles the
/// question by itself — even parked on this branch's own commit, where an oid
/// comparison alone would read it as the default branch and refuse.
#[test]
fn a_head_symrefed_to_a_tag_does_not_block_a_delete() -> Res<()> {
    let s = scenario("delete-head-tag")?;
    publish_branch_everywhere(&s)?;
    let tip =
        git(&s.work, &["rev-parse", "refs/remotes/origin/work-0001-feature"])?.trim().to_string();
    git(&s.backup, &["update-ref", "refs/tags/v1", &tip])?;
    git(&s.backup, &["symbolic-ref", "HEAD", "refs/tags/v1"])?;

    let (ok, output) = review(&s.work, &["--delete", "backup/work-0001-feature", "--yes"])?;
    assert!(ok, "a HEAD that is not a branch protects no branch:\n{output}");
    assert!(!remote_has_branch(&s.backup)?, "backup still carries the branch:\n{output}");
    Ok(())
}

/// The trade this guard makes: a live `HEAD` withheld by `uploadpack.hideRefs`
/// looks exactly like one that dangles, so the delete is attempted. The server's
/// own refusal to delete its checked-out branch is what keeps the branch — and
/// that failure must be reported as a failure, not as a delete.
#[test]
fn a_hidden_live_head_is_caught_by_the_server_and_reported() -> Res<()> {
    let s = scenario("delete-hidden-head")?;
    publish_branch_everywhere(&s)?;
    git(&s.backup, &["symbolic-ref", "HEAD", "refs/heads/work-0001-feature"])?;
    git(&s.backup, &["config", "uploadpack.hideRefs", "HEAD"])?;

    let (ok, output) = review(&s.work, &["--delete", "backup/work-0001-feature", "--yes"])?;
    assert!(!ok, "a rejected delete must not report success:\n{output}");
    assert!(output.contains("delete on backup failed"), "the rejection must be shown:\n{output}");
    assert!(remote_has_branch(&s.backup)?, "the server's refusal must keep the branch:\n{output}");
    Ok(())
}

/// A ref the server does not advertise is not a ref the server does not have.
/// With no symref line and the branch hidden there is no oid to clear `HEAD`
/// with, and an unadvertised ref must not read as an absent one.
#[test]
fn a_hidden_branch_ref_is_not_evidence_the_delete_is_safe() -> Res<()> {
    let s = scenario("delete-hidden-ref")?;
    publish_branch_everywhere(&s)?;
    let base = git(&s.work, &["rev-parse", "refs/heads/main"])?.trim().to_string();
    // Detached, so no symref line is sent; and the branch is withheld, so its
    // oid cannot rule HEAD out. `uploadpack.hideRefs` is fetch-side only.
    git(&s.backup, &["update-ref", "--no-deref", "HEAD", &base])?;
    git(&s.backup, &["config", "uploadpack.hideRefs", "refs/heads/work-0001-feature"])?;

    let (ok, output) = review(&s.work, &["--delete", "backup/work-0001-feature", "--yes"])?;
    assert!(!ok, "an unanswerable endpoint must refuse:\n{output}");
    assert!(output.contains("without naming a branch"), "wrong reason:\n{output}");
    assert!(remote_has_branch(&s.backup)?, "backup lost the branch:\n{output}");
    Ok(())
}

/// `git push` writes to EVERY pushurl, so every one must be asked: a second
/// endpoint whose HEAD IS the branch has to refuse the whole delete, even
/// though the first endpoint cleared it.
#[test]
fn every_pushurl_is_asked_before_a_delete() -> Res<()> {
    let s = scenario("delete-two-pushurls")?;
    publish_branch_everywhere(&s)?;
    let root = match s.origin.parent() {
        Some(root) => root.to_path_buf(),
        None => return Err("the scenario has no root".into()),
    };
    let mirror = root.join("mirror.git");
    git(&root, &["init", "--bare", "-b", "main", &mirror.to_string_lossy()])?;
    git(
        &s.work,
        &[
            "push",
            &mirror.to_string_lossy(),
            "origin/work-0001-feature:refs/heads/work-0001-feature",
        ],
    )?;
    git(&mirror, &["symbolic-ref", "HEAD", "refs/heads/work-0001-feature"])?;
    // backup's own url stays FIRST, and clears the branch; the added one must
    // still be consulted.
    git(&s.work, &["remote", "set-url", "--push", "backup", &s.backup.to_string_lossy()])?;
    git(&s.work, &["remote", "set-url", "--add", "--push", "backup", &mirror.to_string_lossy()])?;

    let (ok, output) = review(&s.work, &["--delete", "backup/work-0001-feature", "--yes"])?;
    assert!(!ok, "the second endpoint's default branch must refuse:\n{output}");
    assert!(output.contains("backup's default branch"), "wrong reason:\n{output}");
    assert!(!output.contains("==> backup"), "the remote must not be pushed to:\n{output}");
    assert!(remote_has_branch(&s.backup)?, "backup lost the branch:\n{output}");
    Ok(())
}

/// The same detached `HEAD`, parked anywhere else: nothing about the branch
/// being deleted, so it must not refuse.
#[test]
fn an_unnamed_head_elsewhere_does_not_block_a_delete() -> Res<()> {
    let s = scenario("delete-unnamed-head-elsewhere")?;
    publish_branch_everywhere(&s)?;
    let base = git(&s.work, &["rev-parse", "refs/heads/main"])?.trim().to_string();
    git(&s.backup, &["update-ref", "--no-deref", "HEAD", &base])?;

    let (ok, output) = review(&s.work, &["--delete", "backup/work-0001-feature", "--yes"])?;
    assert!(ok, "a HEAD at some other commit protects nothing:\n{output}");
    assert!(!remote_has_branch(&s.backup)?, "backup still carries the branch:\n{output}");
    Ok(())
}

#[test]
fn delete_requires_an_explicit_yes() -> Res<()> {
    let s = scenario("delete-noyes")?;
    let (ok, output) = review(&s.work, &["--delete", "origin/work-0001-feature"])?;
    assert!(!ok, "an unconfirmed delete must be refused:\n{output}");
    assert!(remote_has_branch(&s.origin)?, "the branch must survive a refused delete");
    Ok(())
}

#[test]
fn delete_refuses_the_base_branch() -> Res<()> {
    let s = scenario("delete-base")?;
    for name in ["main", "origin/main"] {
        let (ok, output) = review(&s.work, &["--delete", name, "--yes"])?;
        assert!(!ok, "deleting the base ({name}) must be refused:\n{output}");
        // Refused locally, by us — not bounced back by a remote.
        assert!(output.contains("it is the base branch"), "output: {output}");
        assert!(!output.contains("==> origin"), "no remote should be contacted: {output}");
    }
    // Both remotes still have main, and it still points at the base commit.
    for remote in [&s.origin, &s.backup] {
        git(remote, &["rev-parse", "refs/heads/main"])?;
    }
    Ok(())
}

#[test]
fn delete_reports_when_no_remote_carries_the_branch() -> Res<()> {
    let s = scenario("delete-missing")?;
    let (ok, output) = review(&s.work, &["--delete", "origin/nonexistent", "--yes"])?;
    assert!(!ok, "a delete that removes nothing is not a success:\n{output}");
    assert!(output.contains("no pushable remote carries"), "output: {output}");
    Ok(())
}

#[test]
fn delete_landed_removes_the_branch_only_after_it_is_published() -> Res<()> {
    let s = scenario("delete-landed")?;
    publish_branch_everywhere(&s)?;

    let (ok, output) = review(
        &s.work,
        &["--land", "origin/work-0001-feature", "--squash", "--yes", "--push", "--delete-landed"],
    )?;
    assert!(ok, "land + push + delete should succeed:\n{output}");

    // The work is on every pushable remote before the branch goes away.
    let head = git(&s.work, &["rev-parse", "main"])?.trim().to_string();
    for remote in [&s.origin, &s.backup] {
        assert_eq!(git(remote, &["rev-parse", "main"])?.trim(), head);
        assert!(!remote_has_branch(remote)?, "branch should be gone from {}", remote.display());
    }
    Ok(())
}

/// A branch that only a `no_push` mirror carries has nothing to clean up. The
/// oid filter doing its job is not a failed cleanup.
#[test]
fn delete_landed_is_not_a_failure_when_there_is_nothing_to_delete() -> Res<()> {
    let s = scenario("nothing-to-delete")?;
    // archive shares origin's fetch url, so it tracks the branch; origin's own
    // tracking ref is dropped so no pushable remote carries it.
    git(&s.work, &["update-ref", "-d", "refs/remotes/origin/work-0001-feature"])?;

    let (ok, output) = review(
        &s.work,
        &["--land", "archive/work-0001-feature", "--squash", "--yes", "--push", "--delete-landed"],
    )?;
    assert!(ok, "an empty cleanup is not a failed landing:\n{output}");
    assert!(output.contains("no remote this push reached carries the landed"), "output: {output}");
    Ok(())
}

#[test]
fn a_failed_push_leaves_the_branch_alone() -> Res<()> {
    let s = scenario("delete-nopush")?;
    publish_branch_everywhere(&s)?;
    // Diverge backup so its push is rejected: the landing is only half
    // published, so the branch is still the only complete copy of that work.
    let clone = s.work.parent().unwrap_or(&s.work).join("diverge");
    git(
        s.work.parent().unwrap_or(&s.work),
        &["clone", "-q", &s.backup.to_string_lossy(), &clone.to_string_lossy()],
    )?;
    for (k, v) in [("user.name", "Other"), ("user.email", "o@e.invalid")] {
        git(&clone, &["config", k, v])?;
    }
    fs::write(clone.join("divergent.txt"), "elsewhere\n")?;
    git(&clone, &["add", "divergent.txt"])?;
    git(&clone, &["commit", "-qm", "backup: a commit td-review has never seen"])?;
    git(&clone, &["push", "-q", "origin", "main"])?;

    let (ok, output) = review(
        &s.work,
        &["--land", "origin/work-0001-feature", "--squash", "--yes", "--push", "--delete-landed"],
    )?;
    assert!(!ok, "a half-published landing must not report success:\n{output}");
    assert!(remote_has_branch(&s.origin)?, "the branch must survive a failed push:\n{output}");
    assert!(remote_has_branch(&s.backup)?, "the branch must survive a failed push:\n{output}");
    Ok(())
}

#[test]
fn delete_does_not_mistake_a_slashed_branch_name_for_a_remote() -> Res<()> {
    let s = scenario("delete-slash")?;
    // Two branches whose names collide once a leading component is stripped:
    // `feature/keep` must never be read as remote `feature` + branch `keep`.
    for name in ["feature/keep", "keep"] {
        git(&s.work, &["push", "origin", &format!("origin/work-0001-feature:refs/heads/{name}")])?;
    }
    git(&s.work, &["fetch", "origin"])?;

    let (ok, output) = review(&s.work, &["--delete", "feature/keep", "--yes"])?;
    assert!(ok, "deleting a slashed branch name should work:\n{output}");

    let left = git(&s.origin, &["for-each-ref", "--format=%(refname)", "refs/heads/"])?;
    assert!(!left.contains("refs/heads/feature/keep"), "feature/keep should be gone:\n{left}");
    assert!(left.contains("refs/heads/keep"), "the unrelated `keep` must survive:\n{left}");
    Ok(())
}

#[test]
fn delete_refuses_a_branch_that_moved_since_the_last_fetch() -> Res<()> {
    let s = scenario("delete-stale")?;
    // A tip pushed by someone else after our fetch: our tracking ref is stale,
    // and deleting would drop work nobody here has ever seen.
    let clone = s.work.parent().unwrap_or(&s.work).join("other");
    git(
        s.work.parent().unwrap_or(&s.work),
        &["clone", "-q", "-b", "work-0001-feature", &s.origin.to_string_lossy(), &clone.to_string_lossy()],
    )?;
    for (k, v) in [("user.name", "Other"), ("user.email", "o@e.invalid")] {
        git(&clone, &["config", k, v])?;
    }
    fs::write(clone.join("late.txt"), "work nobody fetched\n")?;
    git(&clone, &["add", "late.txt"])?;
    git(&clone, &["commit", "-qm", "feature: a third step, pushed after the fetch"])?;
    git(&clone, &["push", "-q", "origin", "work-0001-feature"])?;

    let (ok, output) = review(&s.work, &["--delete", "origin/work-0001-feature", "--yes"])?;
    assert!(!ok, "a stale delete must be refused:\n{output}");
    assert!(remote_has_branch(&s.origin)?, "the branch must survive:\n{output}");
    Ok(())
}

#[test]
fn landing_refuses_when_the_base_moved_since_the_review() -> Res<()> {
    // Worktrees share refs, so another agent can advance the base while a
    // review pane is open — the staged squash would then differ from the diff
    // that was approved.
    let s = scenario("base-moved")?;
    let reviewed = git(&s.work, &["rev-parse", "main"])?.trim().to_string();
    commit(
        &s.work,
        "late.txt",
        "on main\n",
        "base: a commit made after the review opened",
        "2024-01-04T00:00:00 +0000",
    )?;

    let (ok, output) = review(
        &s.work,
        &["--land", "origin/work-0001-feature", "--squash", "--expect-base", &reviewed, "--yes"],
    )?;
    assert!(!ok, "landing onto a moved base must be refused:\n{output}");
    assert!(output.contains("moved since the review"), "output: {output}");
    assert_eq!(git(&s.work, &["rev-list", "--count", "main"])?.trim(), "2");

    // Re-reviewing against the new base lands normally.
    let now = git(&s.work, &["rev-parse", "main"])?.trim().to_string();
    let (ok, output) = review(
        &s.work,
        &["--land", "origin/work-0001-feature", "--squash", "--expect-base", &now, "--yes"],
    )?;
    assert!(ok, "an up-to-date base pin must land:\n{output}");
    Ok(())
}

#[test]
fn delete_landed_spares_a_same_named_branch_on_another_remote() -> Res<()> {
    let s = scenario("delete-diverged")?;
    // `backup` carries a DIFFERENT branch under the same name. Remotes are
    // matched by name, so nothing but the oid check keeps this work alive.
    git(&s.work, &["checkout", "-q", "-b", "unrelated"])?;
    commit(&s.work, "u.txt", "unrelated\n", "unrelated: other work", "2024-01-05T00:00:00 +0000")?;
    git(&s.work, &["push", "backup", "unrelated:refs/heads/work-0001-feature"])?;
    git(&s.work, &["checkout", "-q", "main"])?;
    git(&s.work, &["branch", "-qD", "unrelated"])?;
    git(&s.work, &["fetch", "-q", "backup"])?;
    let survivor = git(&s.backup, &["rev-parse", "refs/heads/work-0001-feature"])?
        .trim()
        .to_string();

    let (ok, output) = review(
        &s.work,
        &["--land", "origin/work-0001-feature", "--squash", "--yes", "--push", "--delete-landed"],
    )?;
    assert!(ok, "land + push + delete should succeed:\n{output}");

    assert!(!remote_has_branch(&s.origin)?, "the landed branch should be gone from origin");
    assert_eq!(
        git(&s.backup, &["rev-parse", "refs/heads/work-0001-feature"])?.trim(),
        survivor,
        "backup's unrelated branch must survive:\n{output}"
    );
    assert!(
        output.contains("not the landed commit"),
        "the skip must be reported, not silent:\n{output}"
    );
    Ok(())
}

/// The record column is what the integrator reads before opening a branch, so
/// it has to come from the commits themselves — a branch whose commits carry
/// the record reads `ok`, one whose commits do not says how many.
#[test]
fn list_shows_the_review_record_per_branch() -> Res<()> {
    let s = scenario("record")?;
    let record = "\n\nReviewed-by: subagent/opus-4.8\nReviewed-by: codex/gpt-5.6-sol\n\
                  Reviewed-by: agy/gemini-3.1-pro\nChecks: affected-checks (green)";

    git(&s.work, &["checkout", "-b", "reviewed-rolling"])?;
    commit(
        &s.work,
        "r.txt",
        "one\n",
        &format!("reviewed: an increment{record}"),
        "2024-02-01T00:00:00 +0000",
    )?;
    git(&s.work, &["push", "origin", "reviewed-rolling"])?;
    git(&s.work, &["checkout", "main"])?;

    let (ok, output) = review(&s.work, &["--list"])?;
    assert!(ok, "--list should succeed:\n{output}");

    let column = |branch: &str| -> Option<String> {
        output
            .lines()
            .find(|l| l.contains(branch))
            .and_then(|l| l.split_whitespace().nth(2))
            .map(str::to_string)
    };
    assert_eq!(
        column("origin/reviewed-rolling").as_deref(),
        Some("ok"),
        "a fully recorded branch reads ok:\n{output}"
    );
    // The scenario's own branch carries no record at all: both its commits.
    assert_eq!(
        column("origin/work-0001-feature").as_deref(),
        Some("2/2!"),
        "an unreviewed branch names how many commits are missing it:\n{output}"
    );
    Ok(())
}

/// The sweep removes the worktree of a branch whose every commit has landed,
/// and keeps everything it cannot prove is finished with.
#[test]
fn prune_worktrees_removes_only_landed_clean_unpushed_worktrees() -> Res<()> {
    let s = scenario("prune")?;
    let trees = s.work.join("trees");

    // 1. Landed, clean, never pushed — the one case that is swept.
    git(&s.work, &["checkout", "-b", "landed-work"])?;
    commit(&s.work, "l.txt", "l\n", "landed: a change", "2024-02-01T00:00:00 +0000")?;
    git(&s.work, &["checkout", "main"])?;
    // Land it by replaying the patch onto main, as a rebase landing does: the
    // object id differs, so only patch equivalence can see that it landed.
    git(&s.work, &["cherry-pick", "landed-work"])?;
    let landed = trees.join("landed");
    git(&s.work, &["worktree", "add", &landed.to_string_lossy(), "landed-work"])?;

    // 2. A rolling branch, equally landed — kept, because that is its normal state.
    git(&s.work, &["branch", "keep-rolling", "landed-work"])?;
    let rolling = trees.join("rolling");
    git(&s.work, &["worktree", "add", &rolling.to_string_lossy(), "keep-rolling"])?;

    // 3. Unlanded work — kept.
    git(&s.work, &["checkout", "-b", "unlanded-work"])?;
    commit(&s.work, "u.txt", "u\n", "unlanded: a change", "2024-02-02T00:00:00 +0000")?;
    git(&s.work, &["checkout", "main"])?;
    let unlanded = trees.join("unlanded");
    git(&s.work, &["worktree", "add", &unlanded.to_string_lossy(), "unlanded-work"])?;

    // 4. Landed but dirty — kept.
    git(&s.work, &["branch", "dirty-work", "landed-work"])?;
    let dirty = trees.join("dirty");
    git(&s.work, &["worktree", "add", &dirty.to_string_lossy(), "dirty-work"])?;
    fs::write(dirty.join("l.txt"), "edited\n")?;

    let (ok, plan) = review(&s.work, &["--prune-worktrees"])?;
    assert!(ok, "the plan should succeed:\n{plan}");
    assert!(plan.contains("remove  "), "something should be removable:\n{plan}");
    for (path, why) in [
        (&rolling, "rolling branch"),
        (&unlanded, "not on the base"),
        (&dirty, "uncommitted changes"),
    ] {
        let name = path.to_string_lossy().to_string();
        let line = plan
            .lines()
            .find(|l| l.contains(&name))
            .unwrap_or_else(|| panic!("no plan line for {name}:\n{plan}"));
        assert!(line.starts_with("keep"), "{name} must be kept:\n{line}");
        assert!(line.contains(why), "{name} must say why ({why}):\n{line}");
    }
    assert!(
        plan.lines().any(|l| l.starts_with("remove") && l.contains(&landed.to_string_lossy().to_string())),
        "the landed worktree should be swept:\n{plan}"
    );
    // A plan alone removes nothing.
    assert!(landed.exists(), "the plan must not have removed anything");

    let (ok, done) = review(&s.work, &["--prune-worktrees", "--yes"])?;
    assert!(ok, "the sweep should succeed:\n{done}");
    assert!(!landed.exists(), "the landed worktree should be gone:\n{done}");
    assert!(rolling.exists(), "the rolling worktree must survive:\n{done}");
    assert!(unlanded.exists(), "the unlanded worktree must survive:\n{done}");
    assert!(dirty.exists(), "the dirty worktree must survive:\n{done}");
    Ok(())
}

/// A rolling branch is landed over and over, so the automatic post-land sweep
/// must not delete it — that would take the branch out from under the agent
/// still working the workstream.
#[test]
fn the_landed_sweep_keeps_a_rolling_branch() -> Res<()> {
    let s = scenario("rolling")?;
    git(&s.work, &["checkout", "-b", "ui-rolling"])?;
    commit(&s.work, "u.txt", "u\n", "ui: an increment", "2024-02-01T00:00:00 +0000")?;
    git(&s.work, &["push", "origin", "ui-rolling"])?;
    git(&s.work, &["checkout", "main"])?;
    git(&s.work, &["branch", "-D", "ui-rolling"])?;

    let (ok, output) = review(
        &s.work,
        &["--land", "origin/ui-rolling", "--yes", "--push", "--delete-landed"],
    )?;
    assert!(ok, "landing a rolling branch should succeed:\n{output}");
    assert!(
        output.contains("rolling branch — kept"),
        "the sweep must say it kept it, not silently skip:\n{output}"
    );
    assert!(
        git(&s.origin, &["rev-parse", "refs/heads/ui-rolling"]).is_ok(),
        "origin must still carry the rolling branch after landing:\n{output}"
    );
    Ok(())
}

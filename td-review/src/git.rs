//! Capturing wrapper around the `git` CLI plus the few queries the review TUI
//! needs. Everything runs with the pager and colour disabled so parsed output
//! is plain text, and with `GIT_TERMINAL_PROMPT=0` so a credential prompt fails
//! loudly instead of deadlocking behind the alternate screen.

use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::record;

/// Field separator for `for-each-ref` formats. Records are newline-separated and
/// the free-text fields (author, subject) come last, so commit text cannot forge
/// a row or shift a structural field.
const FS: char = '\x1f';

/// A remote's push URL set to this literal means "never push here" — the
/// convention `git pushall` already honours.
pub const NO_PUSH: &str = "no_push";

/// `git config remote.<name>.skipPushAll true` takes a remote out of the
/// push-all (`P`, `--push`) and out of nothing else. Named after git's own
/// `remote.<name>.skipFetchAll`, which is the same opt-out on the fetch side.
/// It is the [`NO_PUSH`] sentinel's opposite number: that one is a push URL
/// nothing can push to, for a remote that must never be written to, and it
/// disables `git push <remote>` by hand as much as anything else. This is for
/// a remote that is merely usually unreachable — a laptop that sleeps — where
/// the push must still work on the days it is asked for by name.
pub const SKIP_PUSH_ALL: &str = "skipPushAll";

/// The suffix marking a workstream branch that outlives its own landings
/// (AGENTS.md, "Parallel work"). Landing one does not finish it: the agent
/// rebases onto the new base and keeps going, so the automatic post-push sweep
/// must leave it alone even though its commits are provably published.
pub const ROLLING: &str = "-rolling";

pub fn is_rolling(short: &str) -> bool {
    short.ends_with(ROLLING)
}

pub struct Git {
    repo: PathBuf,
}

/// One captured `git` invocation with stdout/stderr left as bytes.
pub struct RawRun {
    pub ok: bool,
    pub code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

/// Result of one captured `git` invocation.
pub struct Run {
    pub ok: bool,
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl Run {
    /// Trimmed stdout — the common shape for single-value plumbing queries.
    pub fn line(&self) -> &str {
        self.stdout.trim()
    }

    /// One-line failure summary. Callers log git's full output separately, so
    /// this must stay a single row: a multi-line blob would be collapsed into
    /// one clipped smear by the frame renderer.
    pub fn failure(&self) -> String {
        let text = if self.stderr.trim().is_empty() { &self.stdout } else { &self.stderr };
        // Prefer git's own diagnostic: it routinely prints warnings ahead of the
        // fatal line, and the first line alone would report the wrong cause.
        // `!` first, because on a rejected push the useful line is
        // `! [remote rejected] … (deletion of the current branch prohibited)`
        // while the `error:` line is only "failed to push some refs".
        let mut lines = text.lines().map(str::trim).filter(|l| !l.is_empty());
        let first = lines
            .clone()
            .find(|l| l.starts_with('!'))
            .or_else(|| lines.clone().find(|l| l.starts_with("fatal:") || l.starts_with("error:")));
        match first.or_else(|| lines.next()) {
            Some(line) => line.to_string(),
            None => match self.code {
                Some(c) => format!("git exited with status {c}"),
                None => "git terminated by signal".to_string(),
            },
        }
    }
}

/// Outcome of a trial squash-merge.
pub enum MergeResult {
    /// Tree oid the landing would stage.
    Clean(String),
    Conflicted,
    /// git could not compute it (too old for `merge-tree --write-tree`, a bad
    /// ref, …) — carries git's own message.
    Unavailable(String),
}

/// What the endpoints of a remote claim about one branch's `HEAD`.
#[derive(Debug, PartialEq, Eq)]
pub enum HeadClaim {
    /// Some endpoint's `HEAD` is a symref to it: its default branch.
    Default,
    /// Every endpoint answered, and each named a `HEAD` that is not this branch
    /// — or advertised none at all. A live `HEAD` withheld by
    /// `uploadpack.hideRefs` cannot be told on the wire from one that dangles or
    /// is unborn, so this permits the delete and leans on the server's own
    /// refusal to delete its checked-out branch.
    NotDefault,
    /// An endpoint resolved `HEAD` without naming a branch and the oids do not
    /// rule this one out: a server too old for the `symref` capability, or one
    /// hiding refs from the advertisement. Nothing on the wire tells that apart
    /// from `HEAD` being the branch, so it counts against the delete.
    Unnamed,
}

/// Which remote a plain refresh resolves to. The two "not one remote" answers
/// are distinct because only one of them makes fetching every remote a useful
/// thing to suggest.
#[derive(Debug, PartialEq, Eq)]
pub enum DefaultRemote {
    Remote(String),
    NoRemotes,
    /// Several remotes and none distinguished: picking one would silently
    /// refresh the wrong mirror.
    Ambiguous,
}

/// One remote-tracking branch as shown in the list.
pub struct Branch {
    pub refname: String,
    pub commit: String,
    pub committed_unix: i64,
    pub author: String,
    pub subject: String,
    /// Commits ahead of / behind the base. `None` when git could not report
    /// them (pre-2.41, no `%(ahead-behind:)`).
    pub counts: Option<(u32, u32)>,
}

impl Branch {
    /// Compact age ("26m", "4h", "3d") relative to `now`.
    pub fn age(&self, now: i64) -> String {
        age_label(now.saturating_sub(self.committed_unix))
    }

    pub fn counts_label(&self) -> String {
        match self.counts {
            Some((a, b)) => format!("{a}/{b}"),
            None => "?/?".to_string(),
        }
    }

    /// Known to contribute nothing — dimmed in the list. Unknown counts are
    /// not treated as empty.
    pub fn nothing_ahead(&self) -> bool {
        matches!(self.counts, Some((0, _)))
    }
}

/// Split a remote-tracking ref into (remote, branch name) — the branch name is
/// what a delete refspec names. Splits ONLY on a leading component that is a
/// configured remote, so `origin/feature/x` -> `("origin", "feature/x")` while a
/// local `feature/x` is left whole.
pub fn split_remote<'a>(refname: &'a str, remotes: &[String]) -> (Option<&'a str>, &'a str) {
    // `git remote add foo/bar` is accepted, so the remote is not necessarily the
    // first path component: take the LONGEST configured remote that prefixes the
    // ref. Splitting on the first `/` would hand a delete the wrong branch name.
    let mut best = 0usize;
    for r in remotes {
        if r.len() > best
            && refname.starts_with(r.as_str())
            && refname.get(r.len()..r.len().saturating_add(1)) == Some("/")
        {
            best = r.len();
        }
    }
    match (refname.get(..best), refname.get(best.saturating_add(1)..)) {
        (Some(name), Some(rest)) if best > 0 => (Some(name), rest),
        _ => (None, refname),
    }
}

/// Render a duration in seconds as a fixed-width-ish compact label.
pub fn age_label(secs: i64) -> String {
    if secs < 0 {
        return "future".to_string();
    }
    const MINUTE: i64 = 60;
    const HOUR: i64 = 60 * MINUTE;
    const DAY: i64 = 24 * HOUR;
    const WEEK: i64 = 7 * DAY;
    const YEAR: i64 = 365 * DAY;
    if secs < MINUTE {
        format!("{secs}s")
    } else if secs < HOUR {
        format!("{}m", secs / MINUTE)
    } else if secs < DAY {
        format!("{}h", secs / HOUR)
    } else if secs < WEEK {
        format!("{}d", secs / DAY)
    } else if secs < YEAR {
        format!("{}w", secs / WEEK)
    } else {
        format!("{}y", secs / YEAR)
    }
}

pub fn now_unix() -> i64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_secs() as i64,
        Err(_) => 0,
    }
}

impl Git {
    pub fn new(repo: PathBuf) -> Git {
        Git { repo }
    }

    /// Locate the work tree root containing `start`.
    pub fn discover(start: &Path) -> io::Result<Git> {
        let out = Command::new("git")
            .current_dir(start)
            .args(["rev-parse", "--show-toplevel"])
            .stdin(Stdio::null())
            .output()
            .map_err(|e| io::Error::new(e.kind(), format!("running git: {e}")))?;
        let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !out.status.success() || stdout.is_empty() {
            let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("{} is not inside a git work tree: {err}", start.display()),
            ));
        }
        Ok(Git::new(PathBuf::from(stdout)))
    }

    pub fn repo(&self) -> &Path {
        &self.repo
    }

    /// Run git with output captured. Never fails on a non-zero exit status —
    /// inspect [`Run::ok`].
    pub fn run(&self, args: &[&str]) -> io::Result<Run> {
        let out = self.run_raw(args)?;
        Ok(Run {
            ok: out.ok,
            code: out.code,
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        })
    }

    /// One git invocation with stdout left undecoded. A commit message is copied
    /// through verbatim, and `from_utf8_lossy` would rewrite the bytes of a
    /// message that is not valid UTF-8 into U+FFFD.
    pub fn run_raw(&self, args: &[&str]) -> io::Result<RawRun> {
        let out = Command::new("git")
            .current_dir(&self.repo)
            .arg("--no-pager")
            // color.ui=false alone is overridden by a per-command
            // `color.diff=always`, which would bleed escapes into the panes.
            .args(["-c", "color.ui=false", "-c", "color.diff=never"])
            .args(args)
            .stdin(Stdio::null())
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_PAGER", "cat")
            .output()
            .map_err(|e| io::Error::new(e.kind(), format!("running git: {e}")))?;
        Ok(RawRun {
            ok: out.status.success(),
            code: out.status.code(),
            stdout: out.stdout,
            stderr: out.stderr,
        })
    }

    /// The squash message exactly as git stored it. Returned as bytes: it is
    /// written to SQUASH_MSG and committed verbatim, so it must not be decoded.
    pub fn message_bytes(&self, range: &str) -> io::Result<Vec<u8>> {
        // Pin the output encoding: git re-encodes a message that declares a
        // legacy `encoding` header into `i18n.logOutputEncoding`, and the
        // committed result must be the UTF-8 `git commit` will record, not
        // whatever the ambient config asks for.
        let run = self.run_raw(&[
            "-c",
            "i18n.logOutputEncoding=UTF-8",
            "log",
            "--reverse",
            "--format=%B",
            range,
            "--",
        ])?;
        if run.ok {
            return Ok(run.stdout);
        }
        Err(io::Error::other(String::from_utf8_lossy(&run.stderr).trim().to_string()))
    }

    /// Every commit in `range`, with its message and the paths it touches, for
    /// the review-record scan. Record-separated (`%x1e`) with a field separator
    /// after the oid and after the body: a message is free text, and nothing
    /// line-based can delimit it. The encoding is pinned for the same reason
    /// `message_bytes` pins it — the record is read out of the committed bytes.
    pub fn commit_records(&self, range: &str) -> io::Result<Vec<record::Commit>> {
        let out = self.run_ok(&[
            "-c",
            "i18n.logOutputEncoding=UTF-8",
            "log",
            "--reverse",
            "--format=%x1e%H%x1f%P%x1f%B%x1f",
            "--name-only",
            range,
            "--",
        ])?;
        record::parse_commits(&out).ok_or_else(|| {
            io::Error::other(format!("{range}: commit log did not parse as records"))
        })
    }

    /// Any uncommitted change in another worktree, untracked files included: the
    /// sweep is about to delete the directory, so "not worth committing" is not
    /// its call to make.
    pub fn worktree_dirty(&self, path: &str) -> io::Result<bool> {
        // `--untracked-files=all` explicitly: `--porcelain` alone obeys
        // `status.showUntrackedFiles`, and so does `git worktree remove`'s own
        // dirty check — so under that config BOTH layers go blind at once and
        // the sweep deletes a directory with somebody's untracked notes in it.
        Ok(!self
            .run_ok(&["-C", path, "status", "--porcelain", "--untracked-files=all"])?
            .trim()
            .is_empty())
    }

    /// Commits on `branch` with no equivalent on `base`. `git cherry` compares
    /// by PATCH, not ancestry, which is what makes it right after a rebase
    /// landing: the commits that landed have new object ids and would otherwise
    /// all look unlanded.
    pub fn unlanded_commits(&self, base: &str, branch: &str) -> io::Result<usize> {
        // Fully qualified: a TAG sharing the branch's name would otherwise
        // resolve here, and a worktree judged landed against the wrong object
        // is a worktree removed with work in it.
        let base = format!("refs/heads/{base}");
        let branch = format!("refs/heads/{branch}");
        let out = self.run_ok(&["cherry", &base, &branch])?;
        Ok(out.lines().filter(|l| l.starts_with('+')).count())
    }

    /// Commits on `branch` at all, landed or not. A branch with none has not
    /// finished — it has not STARTED, and `git cherry` cannot tell those apart:
    /// both report zero unlanded commits.
    pub fn commits_over(&self, base: &str, branch: &str) -> io::Result<usize> {
        let range = format!("refs/heads/{base}..refs/heads/{branch}");
        Ok(self.run_ok(&["rev-list", "--count", &range])?.trim().parse().unwrap_or(0))
    }

    /// Remotes that still carry `branch`, by the same split a delete uses — a
    /// remote whose name contains `/` is not guessed at.
    pub fn remotes_carrying(&self, branch: &str) -> io::Result<Vec<String>> {
        let names = self.remote_names()?;
        let out = self.run_ok(&["for-each-ref", "--format=%(refname:short)", "refs/remotes"])?;
        Ok(out
            .lines()
            .filter_map(|r| {
                let (remote, short) = split_remote(r.trim(), &names);
                (short == branch).then(|| remote.map(str::to_string)).flatten()
            })
            .collect())
    }

    /// Run git, turning a non-zero exit into an `Err` carrying git's own message.
    pub fn run_ok(&self, args: &[&str]) -> io::Result<String> {
        let run = self.run(args)?;
        if run.ok {
            Ok(run.stdout)
        } else {
            Err(io::Error::other(format!(
                "git {}: {}",
                args.join(" "),
                run.failure()
            )))
        }
    }

    /// Run git with the terminal handed over (pager, colour, prompts) for the
    /// "open this diff in my own pager" escape hatch. Stdio is bound to
    /// `/dev/tty` rather than inherited, so the pager still works when
    /// td-review's own stdin or stdout is redirected.
    pub fn run_interactive(&self, args: &[&str]) -> io::Result<()> {
        let open = |write: bool| -> io::Result<std::fs::File> {
            std::fs::OpenOptions::new().read(!write).write(write).open("/dev/tty")
        };
        let status = Command::new("git")
            .current_dir(&self.repo)
            .args(args)
            .stdin(Stdio::from(open(false)?))
            .stdout(Stdio::from(open(true)?))
            .stderr(Stdio::from(open(true)?))
            .status()
            .map_err(|e| io::Error::new(e.kind(), format!("running git: {e}")))?;
        if status.success() {
            Ok(())
        } else {
            Err(io::Error::other(match status.code() {
                Some(c) => format!("git {} exited with status {c}", args.join(" ")),
                None => format!("git {} terminated by signal", args.join(" ")),
            }))
        }
    }

    /// Uncommitted changes, including untracked files — the same porcelain
    /// check `git squash-in` refuses on.
    pub fn dirty_entries(&self) -> io::Result<Vec<String>> {
        let out = self.run_ok(&["status", "--porcelain"])?;
        Ok(out.lines().map(str::to_string).collect())
    }

    /// Currently checked-out branch, or `None` when HEAD is detached.
    pub fn head_branch(&self) -> io::Result<Option<String>> {
        let run = self.run(&["symbolic-ref", "--quiet", "--short", "HEAD"])?;
        if run.ok {
            Ok(Some(run.line().to_string()))
        } else {
            Ok(None)
        }
    }

    pub fn git_dir(&self) -> io::Result<PathBuf> {
        Ok(PathBuf::from(
            self.run_ok(&["rev-parse", "--absolute-git-dir"])?.trim(),
        ))
    }

    /// `--verify`: resolve exactly one object, and never read `rev` as a flag.
    pub fn rev_parse(&self, rev: &str) -> io::Result<String> {
        Ok(self.run_ok(&["rev-parse", "--verify", "--end-of-options", rev])?.trim().to_string())
    }

    /// Resolve a branch argument the way the list means it. `rev-parse` DWIM
    /// searches `refs/tags/<name>` before `refs/remotes/<name>`, so a tag named
    /// `origin/work-1` would outrank the remote-tracking ref and the pane, the
    /// pin and the merge could then name different commits.
    pub fn branch_oid(&self, name: &str) -> io::Result<String> {
        for qualified in [format!("refs/remotes/{name}"), format!("refs/heads/{name}")] {
            let run =
                self.run(&["rev-parse", "--verify", "--quiet", "--end-of-options", &qualified])?;
            if run.ok {
                return Ok(run.line().to_string());
            }
        }
        self.rev_parse(name)
    }

    pub fn merge_base(&self, a: &str, b: &str) -> io::Result<String> {
        let run = self.run(&["merge-base", a, b])?;
        if !run.ok {
            return Err(io::Error::other(format!("no merge base between {a} and {b}")));
        }
        Ok(run.line().to_string())
    }

    /// Remote-tracking branches, newest commit first, excluding the `*/HEAD`
    /// symrefs and every remote's copy of the base branch.
    ///
    /// `%(ahead-behind:)` needs git >= 2.41; on an older git the whole query
    /// fails, so retry without it and leave the counts unknown rather than
    /// refusing to start.
    pub fn branches(&self, base: &str) -> io::Result<Vec<Branch>> {
        let fmt = |counts: &str| {
            format!(
                // Full refname, not %(refname:short): git's shortening drops a
                // trailing /HEAD, which would smuggle the symref in as a branch.
                "--format=%(refname){FS}%(objectname){FS}%(committerdate:unix){FS}\
                 {counts}{FS}%(authorname){FS}%(contents:subject)"
            )
        };
        let with_counts = fmt(&format!("%(ahead-behind:refs/heads/{base})"));
        let run = self.run(&["for-each-ref", "--sort=-committerdate", &with_counts, "refs/remotes"])?;
        let remotes = self.remote_names()?;
        if run.ok {
            return Ok(parse_branches(&run.stdout, base, &remotes));
        }
        // Leave the counts field empty so the record shape is unchanged.
        let out = self.run_ok(&["for-each-ref", "--sort=-committerdate", &fmt(""), "refs/remotes"])?;
        Ok(parse_branches(&out, base, &remotes))
    }

    /// How far the local base has fallen behind its upstream, if it has one.
    /// Landing onto a stale base produces a commit every remote then rejects.
    pub fn base_behind_upstream(&self, base: &str) -> io::Result<Option<(String, u32)>> {
        let upstream = self.run(&["rev-parse", "--abbrev-ref", &format!("{base}@{{upstream}}")])?;
        if !upstream.ok {
            return Ok(None);
        }
        let name = upstream.line().to_string();
        let counts = self.run(&["rev-list", "--count", &format!("{base}..{name}")])?;
        if !counts.ok {
            return Ok(None);
        }
        match counts.line().parse::<u32>() {
            Ok(0) | Err(_) => Ok(None),
            Ok(n) => Ok(Some((name, n))),
        }
    }

    /// Commits on `base` that its upstream does not have, oldest first. A push
    /// publishes all of them, not just the one that was reviewed.
    pub fn unpushed(&self, base: &str) -> io::Result<Vec<String>> {
        let upstream = self.run(&["rev-parse", "--abbrev-ref", &format!("{base}@{{upstream}}")])?;
        if !upstream.ok {
            return Ok(Vec::new());
        }
        let name = upstream.line().to_string();
        let run = self.run(&[
            "log",
            "--reverse",
            "--oneline",
            "--no-decorate",
            &format!("{name}..{base}"),
            "--",
        ])?;
        if !run.ok {
            return Ok(Vec::new());
        }
        Ok(run.stdout.lines().map(str::to_string).collect())
    }

    /// Whether `tip` carries `commit` — `merge-base --is-ancestor` answers with
    /// its exit status. Anything git could not answer (an unknown oid, say)
    /// reads as "no": the callers use this to gate an irreversible delete.
    pub fn contains(&self, tip: &str, commit: &str) -> io::Result<bool> {
        Ok(self.run(&["merge-base", "--is-ancestor", commit, tip])?.ok)
    }

    /// Commits on `base` that `remote`'s copy of it lacks, oldest first, as of
    /// the last fetch. `None` when there is no `refs/remotes/<remote>/<base>`:
    /// nothing local then says what that remote holds, and "no commits" would
    /// read as "nothing to publish" when the whole branch may be new to it.
    pub fn unpushed_to(&self, remote: &str, base: &str) -> io::Result<Option<Vec<String>>> {
        if self.remote_branch_oid(remote, base)?.is_none() {
            return Ok(None);
        }
        let range = format!("refs/remotes/{remote}/{base}..refs/heads/{base}");
        let run = self.run(&["log", "--reverse", "--oneline", "--no-decorate", &range, "--"])?;
        if !run.ok {
            // The tracking ref is there, so this is git declining to walk the
            // range — not the same state as "no copy here", and it must not be
            // reported as one.
            return Err(io::Error::other(run.failure()));
        }
        Ok(Some(run.stdout.lines().map(str::to_string).collect()))
    }

    /// The oid of `refs/remotes/<remote>/<short>`, or None when it does not exist.
    pub fn remote_branch_oid(&self, remote: &str, short: &str) -> io::Result<Option<String>> {
        let refname = format!("refs/remotes/{remote}/{short}");
        let run = self.run(&["rev-parse", "--verify", "--quiet", &refname])?;
        Ok(if run.ok { Some(run.line().to_string()) } else { None })
    }

    /// The tree `git merge --squash <branch>` would stage onto `base`, computed
    /// without touching the index or work tree. `Conflicted` and `Unavailable`
    /// both mean "no tree to diff" — the caller says which, because they read
    /// very differently to someone about to approve a landing.
    pub fn merge_tree(&self, base: &str, branch: &str) -> io::Result<MergeResult> {
        let run = self.run(&["merge-tree", "--write-tree", base, branch])?;
        let tree = run.stdout.lines().next().unwrap_or_default().trim();
        let is_oid = tree.len() >= 40 && tree.chars().all(|c| c.is_ascii_hexdigit());
        Ok(match (run.ok, run.code) {
            (true, _) if is_oid => MergeResult::Clean(tree.to_string()),
            // 1 is "merged with conflicts"; anything else is git declining.
            (false, Some(1)) => MergeResult::Conflicted,
            _ => MergeResult::Unavailable(run.failure()),
        })
    }

    /// Every URL `git push <remote>` writes to, in that order. `--all`, because
    /// a push reaches all of them and only the first is reported without it.
    pub fn push_urls(&self, remote: &str) -> io::Result<Vec<String>> {
        let run = self.run(&["remote", "get-url", "--push", "--all", remote])?;
        if !run.ok {
            return Err(io::Error::other(run.failure()));
        }
        let urls: Vec<String> =
            run.stdout.lines().map(str::trim).filter(|l| !l.is_empty()).map(str::to_string).collect();
        if urls.is_empty() {
            return Err(io::Error::other(format!("{remote} has no push URL")));
        }
        Ok(urls)
    }

    /// Whether `short` is the default branch of any endpoint a delete would
    /// reach, asked live. The local `refs/remotes/<remote>/HEAD` symref is not
    /// consulted: it is whatever the last `set-head` recorded and the delete
    /// goes to the PUSH URLs, which need not be the repository that symref
    /// describes. `Err` means an endpoint could not be asked at all — a delete
    /// must refuse rather than assume, since "unknown" is exactly the state in
    /// which the branch could be the one that must not go.
    pub fn head_claim(&self, remote: &str, short: &str) -> io::Result<HeadClaim> {
        let refname = format!("refs/heads/{short}");
        let mut unnamed = false;
        for url in self.push_urls(remote)? {
            let run =
                self.run(&["ls-remote", "--symref", "--end-of-options", &url, "HEAD", &refname])?;
            if !run.ok {
                return Err(io::Error::other(run.failure()));
            }
            match parse_head_claim(&run.stdout, &refname) {
                // Decisive: one endpoint protecting the branch is enough.
                HeadClaim::Default => return Ok(HeadClaim::Default),
                HeadClaim::Unnamed => unnamed = true,
                HeadClaim::NotDefault => {}
            }
        }
        Ok(if unnamed { HeadClaim::Unnamed } else { HeadClaim::NotDefault })
    }

    /// The one remote a plain refresh means: whichever `branch.<base>.remote`
    /// names, else `origin`, else the sole remote. A `branch.<base>.remote`
    /// holding a URL rather than a remote name falls through — bare `git fetch`
    /// would take it, but a URL has no tracking refs to prune.
    pub fn default_remote(&self, base: &str) -> io::Result<DefaultRemote> {
        let remotes = self.remote_names()?;
        if remotes.is_empty() {
            return Ok(DefaultRemote::NoRemotes);
        }
        // `.` (a local-branch upstream) is not a remote, so it fails this test.
        let configured = self.run(&["config", "--get", &format!("branch.{base}.remote")])?;
        if configured.ok && remotes.iter().any(|r| r == configured.line()) {
            return Ok(DefaultRemote::Remote(configured.line().to_string()));
        }
        if remotes.iter().any(|r| r == "origin") {
            return Ok(DefaultRemote::Remote("origin".to_string()));
        }
        Ok(match remotes.as_slice() {
            [only] => DefaultRemote::Remote(only.clone()),
            _ => DefaultRemote::Ambiguous,
        })
    }

    /// Configured remote names, in `git remote` order.
    pub fn remote_names(&self) -> io::Result<Vec<String>> {
        Ok(self
            .run_ok(&["remote"])?
            .lines()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect())
    }

    /// Every configured remote paired with its push URL.
    pub fn remotes(&self) -> io::Result<Vec<(String, String)>> {
        let names = self.run_ok(&["remote"])?;
        let mut out = Vec::new();
        for name in names.lines().map(str::trim).filter(|s| !s.is_empty()) {
            let run = self.run(&["remote", "get-url", "--push", "--all", name])?;
            // `git push` uses EVERY configured pushurl, so `--all`: without it
            // only the first is seen and a later sentinel would be missed.
            // If any of them is the sentinel the remote is not pushable.
            let url = if run.ok && run.stdout.lines().any(|l| l.trim() == NO_PUSH) {
                NO_PUSH.to_string()
            } else if run.ok {
                run.line().to_string()
            } else {
                String::new()
            };
            out.push((name.to_string(), url));
        }
        Ok(out)
    }

    /// Whether `remote` has opted out of the push-all with [`SKIP_PUSH_ALL`].
    /// git parses the value, so every spelling of a boolean it takes anywhere
    /// else (`true`, `yes`, `on`, `1`, the bare key) is taken here, and a
    /// repeated key resolves to its LAST value as git resolves one. `Err`
    /// carries git's own diagnostic for a value that is not a boolean at all:
    /// the caller reports it and leaves the remote out, since a stated intent
    /// nobody can read is not permission to push on a keystroke.
    pub fn skips_push_all(&self, remote: &str) -> io::Result<bool> {
        let key = format!("remote.{remote}.{SKIP_PUSH_ALL}");
        let run = self.run(&["config", "--type=bool", "--get-all", &key])?;
        match (run.ok, run.code) {
            (true, _) => {
                Ok(run.stdout.lines().map(str::trim).rfind(|l| !l.is_empty()) == Some("true"))
            }
            // 1 is git's "no such key", which is every remote that never opted
            // out — not a failure to report.
            (false, Some(1)) => Ok(false),
            _ => Err(io::Error::other(run.failure())),
        }
    }
}

/// Split `for-each-ref` output into records. Kept free of `Git` so it is
/// directly testable.
const REMOTES: &str = "refs/remotes/";

pub fn parse_branches(out: &str, base: &str, remotes: &[String]) -> Vec<Branch> {
    let mut branches = Vec::new();
    for record in out.lines() {
        if record.trim().is_empty() {
            continue;
        }
        // Exactly 6 fields; the last absorbs any FS the subject carries.
        let mut fields = record.splitn(6, FS);
        let refname = match fields.next().map(|r| r.strip_prefix(REMOTES).unwrap_or(r)) {
            Some(r) if !r.is_empty() => r.to_string(),
            _ => continue,
        };
        let commit = fields.next().unwrap_or_default().to_string();
        let committed_unix =
            fields.next().unwrap_or_default().trim().parse::<i64>().unwrap_or(0);
        let counts = parse_ahead_behind(fields.next().unwrap_or_default());
        let author = fields.next().unwrap_or_default().to_string();
        let subject = fields.next().unwrap_or_default().to_string();

        // `*/HEAD` is a symref alias, and every remote's copy of the base
        // branch is the landing target rather than a candidate. Split the way a
        // delete does, or a remote whose name contains `/` slips both through.
        let short = split_remote(&refname, remotes).1;
        if short == "HEAD" || short == base {
            continue;
        }
        branches.push(Branch { refname, commit, committed_unix, author, subject, counts });
    }
    branches
}

/// Classify one `ls-remote --symref <url> HEAD refs/heads/<short>` answer. A
/// `HEAD` that does not resolve — dangling at a deleted branch, or an empty
/// repository — produces NO line for it at all, so its absence is not evidence
/// that the branch is the default; a symref line or an oid is.
///
/// Ref names are compared whole. ls-remote matches its patterns by tail, so the
/// exact compare is what keeps `refs/heads/refs/heads/<short>` from answering
/// for `<short>` — not any assumption about what a caller passed in.
fn parse_head_claim(out: &str, refname: &str) -> HeadClaim {
    let mut head_oid = None;
    let mut branch_oid = None;
    for line in out.lines() {
        let (value, name) = match line.split_once('\t') {
            Some((v, n)) => (v.trim(), n.trim()),
            None => continue,
        };
        match (value.strip_prefix("ref: "), name) {
            // A symref for HEAD settles it either way, including one aimed
            // outside refs/heads: whatever it is, it is not this branch.
            (Some(target), "HEAD") => {
                return if target == refname { HeadClaim::Default } else { HeadClaim::NotDefault }
            }
            (Some(_), _) => {}
            (None, "HEAD") => head_oid = Some(value),
            (None, n) if n == refname => branch_oid = Some(value),
            (None, _) => {}
        }
    }
    match (head_oid, branch_oid) {
        (None, _) => HeadClaim::NotDefault,
        (Some(head), Some(branch)) if head != branch => HeadClaim::NotDefault,
        // HEAD resolved but named nothing, and the oids leave it open: it sits
        // at this branch's tip, or the branch was not advertised to compare.
        _ => HeadClaim::Unnamed,
    }
}

/// `%(ahead-behind:...)` renders as "<ahead> <behind>"; empty when the field
/// was not requested or the base ref is missing.
fn parse_ahead_behind(field: &str) -> Option<(u32, u32)> {
    let mut parts = field.split_whitespace();
    let ahead = parts.next()?.parse().ok()?;
    let behind = parts.next()?.parse().ok()?;
    Some((ahead, behind))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(branches: &[Branch]) -> Vec<&str> {
        branches.iter().map(|b| b.refname.as_str()).collect()
    }

    /// `git remote add foo/bar` is accepted, and a delete that guessed the
    /// remote from the first path component would name the wrong branch.
    #[test]
    fn a_ref_is_split_on_the_longest_remote_that_prefixes_it() {
        let remotes = ["origin".to_string(), "origin/mirror".to_string()];
        assert_eq!(split_remote("origin/work-1", &remotes), (Some("origin"), "work-1"));
        assert_eq!(
            split_remote("origin/mirror/work-1", &remotes),
            (Some("origin/mirror"), "work-1")
        );
        // A slashed BRANCH name on a plain remote still keeps its slashes.
        assert_eq!(split_remote("origin/feature/x", &remotes), (Some("origin"), "feature/x"));
        // Nothing matches: the whole thing is the branch name.
        assert_eq!(split_remote("work-1", &remotes), (None, "work-1"));
        assert_eq!(split_remote("upstream/work-1", &remotes), (None, "upstream/work-1"));
        // A remote name alone is not a branch on that remote.
        assert_eq!(split_remote("origin", &remotes), (None, "origin"));
    }

    /// The list must hide the same rows a delete would refuse: a remote whose
    /// name contains `/` otherwise leaks its HEAD symref and its copy of the
    /// base branch into the candidates.
    #[test]
    fn a_slashed_remote_still_hides_its_head_and_the_base() {
        let remotes = ["origin".to_string(), "foo/bar".to_string()];
        let out = [
            record("foo/bar/HEAD", "0 0", "a", "s"),
            record("foo/bar/main", "0 0", "a", "s"),
            record("foo/bar/work-1", "1 0", "a", "s"),
        ]
        .join("\n");
        assert_eq!(names(&parse_branches(&out, "main", &remotes)), vec!["foo/bar/work-1"]);
    }

    /// One `for-each-ref` record in the shipped field order.
    fn record(refname: &str, counts: &str, author: &str, subject: &str) -> String {
        format!("refs/remotes/{refname}{FS}abc123{FS}1700000000{FS}{counts}{FS}{author}{FS}{subject}\n")
    }

    #[test]
    fn parses_records_and_skips_head_and_base() {
        let out = format!(
            "{}{}{}{}",
            record("origin/work-1", "3 4", "Ada", "subject one"),
            // git shortens refs/remotes/origin/HEAD to plain "origin"; parsing
            // the full refname is what keeps it out of the list.
            record("origin/HEAD", "0 0", "Ada", "sym"),
            record("origin/main", "0 0", "Ada", "base"),
            record("other/work-2", "1 0", "Bob", "subject two"),
        );
        let branches = parse_branches(&out, "main", &["origin".to_string()]);
        assert_eq!(names(&branches), vec!["origin/work-1", "other/work-2"]);
        assert_eq!(
            branches.first().map(|b| (b.subject.as_str(), b.counts)),
            Some(("subject one", Some((3, 4))))
        );
    }

    #[test]
    fn missing_ahead_behind_is_unknown_not_zero() {
        // The pre-2.41 fallback leaves the field empty; "0 ahead" must not be
        // implied, or an unlandable branch would look dimmed and landable.
        let out = record("origin/w", "", "Ada", "s");
        let branches = parse_branches(&out, "main", &["origin".to_string()]);
        let b = branches.first();
        assert_eq!(b.and_then(|b| b.counts), None);
        assert_eq!(b.map(Branch::counts_label), Some("?/?".to_string()));
        assert_eq!(b.map(Branch::nothing_ahead), Some(false));
    }

    #[test]
    fn a_subject_cannot_forge_a_row_or_shift_a_structural_field() {
        // A subject stuffed with field separators and a whole fake record.
        let hostile = format!("evil{FS}deadbeef{FS}1{FS}9 9{FS}Mallory{FS}spoofed");
        let out = record("origin/w", "2 0", "Ada", &hostile);
        let branches = parse_branches(&out, "main", &["origin".to_string()]);
        assert_eq!(names(&branches), vec!["origin/w"]);
        // The structural fields all precede the free text, so they survive.
        assert_eq!(branches.first().and_then(|b| b.counts), Some((2, 0)));
        assert_eq!(branches.first().map(|b| b.author.as_str()), Some("Ada"));
        assert_eq!(branches.first().map(|b| b.subject.as_str()), Some(hostile.as_str()));
    }

    #[test]
    fn subject_may_contain_slashes_and_spaces() {
        let out = record("origin/feat/x", "2 0", "A B", "a: b/c d");
        let branches = parse_branches(&out, "main", &["origin".to_string()]);
        assert_eq!(
            branches.first().map(|b| (b.refname.as_str(), b.subject.as_str())),
            Some(("origin/feat/x", "a: b/c d"))
        );
    }

    fn claim(out: &str) -> HeadClaim {
        parse_head_claim(out, "refs/heads/work-1")
    }

    #[test]
    fn a_symref_naming_the_branch_is_the_default_branch() {
        let out = "ref: refs/heads/work-1\tHEAD\naaa\tHEAD\naaa\trefs/heads/work-1\n";
        assert_eq!(claim(out), HeadClaim::Default);
    }

    #[test]
    fn a_symref_naming_anything_else_clears_the_branch() {
        // Same oid throughout, so only the symref distinguishes these.
        let out = "ref: refs/heads/main\tHEAD\naaa\tHEAD\naaa\trefs/heads/work-1\n";
        assert_eq!(claim(out), HeadClaim::NotDefault);
        // Aimed outside refs/heads, so the oids must not be consulted at all:
        // whatever HEAD is, it is not this branch.
        let tag = "ref: refs/tags/v1\tHEAD\naaa\tHEAD\naaa\trefs/heads/work-1\n";
        assert_eq!(claim(tag), HeadClaim::NotDefault);
    }

    /// ls-remote matches patterns by tail, so a branch whose name ends in the
    /// one being deleted is advertised too. Only the whole name may answer.
    #[test]
    fn a_tail_matching_branch_does_not_answer_for_the_branch() {
        let out = "aaa\tHEAD\naaa\trefs/heads/refs/heads/work-1\n";
        assert_eq!(claim(out), HeadClaim::Unnamed);
    }

    /// A HEAD dangling at a branch that no longer exists reports NOTHING — not
    /// an error, not an oid. Reading that as "unknown" refused every delete on
    /// a remote whose HEAD had gone stale.
    #[test]
    fn a_head_that_resolves_to_nothing_protects_nothing() {
        assert_eq!(claim(""), HeadClaim::NotDefault);
        assert_eq!(claim("aaa\trefs/heads/work-1\n"), HeadClaim::NotDefault);
    }

    /// No symref capability: the oid is the only evidence either way.
    #[test]
    fn an_unnamed_head_counts_only_where_the_oids_do_not_clear_it() {
        assert_eq!(claim("aaa\tHEAD\naaa\trefs/heads/work-1\n"), HeadClaim::Unnamed);
        assert_eq!(claim("bbb\tHEAD\naaa\trefs/heads/work-1\n"), HeadClaim::NotDefault);
        // HEAD resolved but the branch was not advertised, so there is no oid to
        // clear it with. A hidden ref must not read as an absent one.
        assert_eq!(claim("aaa\tHEAD\n"), HeadClaim::Unnamed);
    }

    #[test]
    fn age_labels_scale() {
        assert_eq!(age_label(30), "30s");
        assert_eq!(age_label(90), "1m");
        assert_eq!(age_label(3 * 3600 + 5), "3h");
        assert_eq!(age_label(50 * 3600), "2d");
        assert_eq!(age_label(20 * 86400), "2w");
        assert_eq!(age_label(800 * 86400), "2y");
    }
}

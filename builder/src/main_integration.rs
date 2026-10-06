//! `td-builder main-integration`: the integration tier's runner on main.
//! No branch's `ready` boots the qemu oracles (`affected::boot_path` only
//! names a boot-path change), nor runs a recipe check whose key differs
//! from a pass on record only in the builder engine or evaluator, so this
//! is where they run: after landings, on the newest `origin/main`, in a
//! worktree of its own.
//!
//! `run` fetches the remote's main, and when its head has no verdict yet
//! resets the runner's detached worktree to it and runs that commit's own
//! `td-builder check recipe-checks integration` there: every recipe check,
//! then the oracles. Heads that land while a run is
//! going are not queued: the next pass takes whatever is newest, so a burst
//! of landings costs one run, and a red one is bisected by whoever heals it
//! (`ci/revert-suspect.sh`). A run killed by a signal records no verdict and
//! runs again; any exit is a verdict, since the tier's 1 cannot tell an
//! oracle's failure from the check host's. `status` reads what was recorded
//! and fetches nothing.
//!
//! The state lives outside every checkout, in `TD_MAIN_INTEGRATION_DIR` or
//! `~/.local/state/td/main-integration`: `tree/` (the worktree, whose
//! ignored build caches and oracle memo survive between runs),
//! `verdicts/SHA`, `logs/SHA.log`, `running` while a run is in progress,
//! and `runner.lock`, held for a runner's whole life so two cannot share
//! the tree. The repository is named by its common git directory, so a
//! runner started from an agent's worktree survives that worktree.

use std::fs::{self, File, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const USAGE: &str = "usage: td-builder main-integration run [--once] [--again] \
                     [--interval SECS] [--remote NAME]\n       \
                     td-builder main-integration status [--remote NAME]";

/// Between fetches when main has not moved.
const DEFAULT_INTERVAL: Duration = Duration::from_secs(300);

/// `status`'s exit when the newest main has no verdict yet (unrun,
/// running, or aborted): not a pass a script may take for green.
const NO_VERDICT: u8 = 3;

/// How far back `status` and a red run look for the last green commit.
const GREEN_HORIZON: usize = 500;

pub(crate) fn main(args: &[String]) -> ExitCode {
    let code = match args.first().map(String::as_str) {
        Some("run") => parse_run(args.get(1..).unwrap_or(&[])).and_then(|opts| run(&opts)),
        Some("status") => parse_remote(args.get(1..).unwrap_or(&[])).and_then(|r| status(&r)),
        Some("-h" | "--help") => {
            println!("{USAGE}");
            Ok(0)
        }
        _ => Err(USAGE.to_string()),
    };
    match code {
        Ok(code) => ExitCode::from(code),
        Err(e) => {
            eprintln!("td-builder main-integration: {e}");
            ExitCode::from(2)
        }
    }
}

#[derive(Debug, PartialEq)]
struct RunOpts {
    once: bool,
    again: bool,
    interval: Duration,
    remote: String,
}

fn parse_run(args: &[String]) -> Result<RunOpts, String> {
    let mut opts = RunOpts {
        once: false,
        again: false,
        interval: DEFAULT_INTERVAL,
        remote: "origin".to_string(),
    };
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--once" => opts.once = true,
            "--again" => opts.again = true,
            "--interval" => {
                let secs = it
                    .next()
                    .and_then(|s| s.parse::<u64>().ok())
                    .filter(|s| *s > 0)
                    .ok_or("--interval takes a positive number of seconds")?;
                opts.interval = Duration::from_secs(secs);
            }
            "--remote" => opts.remote = remote_name(it.next())?,
            other => return Err(format!("unknown argument {other:?}\n{USAGE}")),
        }
    }
    if opts.again && !opts.once {
        return Err("--again re-runs one head: it goes with --once".to_string());
    }
    Ok(opts)
}

fn parse_remote(args: &[String]) -> Result<String, String> {
    match args {
        [] => Ok("origin".to_string()),
        [flag, name] if flag == "--remote" => remote_name(Some(name)),
        _ => Err(USAGE.to_string()),
    }
}

/// A remote name git will read as a name, not an option or a path.
fn remote_name(name: Option<&String>) -> Result<String, String> {
    match name {
        Some(n)
            if !n.is_empty()
                && !n.starts_with('-')
                && n.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
                && n != "."
                && n != ".." =>
        {
            Ok(n.clone())
        }
        _ => Err("--remote takes a git remote name".to_string()),
    }
}

/// Where the runner keeps its worktree and records.
fn state_dir() -> Result<PathBuf, String> {
    state_dir_from(
        std::env::var_os("TD_MAIN_INTEGRATION_DIR").as_deref(),
        std::env::var_os("HOME").as_deref(),
    )
}

fn state_dir_from(
    explicit: Option<&std::ffi::OsStr>,
    home: Option<&std::ffi::OsStr>,
) -> Result<PathBuf, String> {
    let dir = match (explicit, home) {
        (Some(d), _) if !d.is_empty() => PathBuf::from(d),
        (_, Some(h)) if !h.is_empty() => Path::new(h).join(".local/state/td/main-integration"),
        _ => return Err("neither TD_MAIN_INTEGRATION_DIR nor HOME is set".to_string()),
    };
    if dir.is_absolute() {
        Ok(dir)
    } else {
        Err(format!("{} is not an absolute path", dir.display()))
    }
}

/// A run's outcome as the tier's exit code says it.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Verdict {
    Pass,
    /// No oracle could run on this host: not a pass, and not main's fault.
    HostGap,
    Fail,
}

impl Verdict {
    fn of(code: Option<i32>) -> Self {
        match code {
            Some(0) => Verdict::Pass,
            Some(td_engine::exit::EXIT_UNPROVISIONED) => Verdict::HostGap,
            _ => Verdict::Fail,
        }
    }

    /// How `run --once` and `status` exit on this verdict: 0, the tier's own
    /// unprovisioned code, or 1.
    fn exit_code(self) -> u8 {
        match self {
            Verdict::Pass => 0,
            Verdict::HostGap => u8::try_from(td_engine::exit::EXIT_UNPROVISIONED).unwrap_or(1),
            Verdict::Fail => 1,
        }
    }

    fn word(self) -> &'static str {
        match self {
            Verdict::Pass => "pass",
            Verdict::HostGap => "host-gap",
            Verdict::Fail => "fail",
        }
    }

    fn from_word(word: &str) -> Option<Self> {
        match word {
            "pass" => Some(Verdict::Pass),
            "host-gap" => Some(Verdict::HostGap),
            "fail" => Some(Verdict::Fail),
            _ => None,
        }
    }
}

/// One recorded run: `WORD EXIT SECONDS FINISHED_UNIX`.
#[derive(Debug, PartialEq)]
struct Record {
    verdict: Verdict,
    exit: Option<i32>,
    secs: u64,
    finished: u64,
}

impl Record {
    fn line(&self) -> String {
        let exit = self
            .exit
            .map_or_else(|| "signal".to_string(), |c| c.to_string());
        format!(
            "{} {exit} {} {}\n",
            self.verdict.word(),
            self.secs,
            self.finished
        )
    }

    fn parse(text: &str) -> Option<Self> {
        let mut words = text.split_whitespace();
        let verdict = Verdict::from_word(words.next()?)?;
        let exit = match words.next()? {
            "signal" => None,
            n => Some(n.parse().ok()?),
        };
        let secs = words.next()?.parse().ok()?;
        let finished = words.next()?.parse().ok()?;
        words.next().is_none().then_some(Record {
            verdict,
            exit,
            secs,
            finished,
        })
    }
}

/// The runner's directory layout.
struct State {
    dir: PathBuf,
}

impl State {
    fn tree(&self) -> PathBuf {
        self.dir.join("tree")
    }

    fn verdict_path(&self, sha: &str) -> PathBuf {
        self.dir.join("verdicts").join(sha)
    }

    fn log_path(&self, sha: &str) -> PathBuf {
        self.dir.join("logs").join(format!("{sha}.log"))
    }

    fn running_path(&self) -> PathBuf {
        self.dir.join("running")
    }

    fn verdict(&self, sha: &str) -> Option<Record> {
        Record::parse(&fs::read_to_string(self.verdict_path(sha)).ok()?)
    }

    fn write_verdict(&self, sha: &str, record: &Record) -> Result<(), String> {
        let path = self.verdict_path(sha);
        let tmp = path.with_extension("tmp");
        fs::write(&tmp, record.line())
            .and_then(|()| fs::rename(&tmp, &path))
            .map_err(|e| format!("could not record {}: {e}", path.display()))
    }
}

fn is_sha(s: &str) -> bool {
    matches!(s.len(), 40 | 64) && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// git in `dir` with the caller's environment minus anything that would
/// point it at another repository.
fn git(dir: &Path) -> Command {
    let mut cmd = Command::new("git");
    cmd.current_dir(dir)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE");
    cmd
}

/// git's trimmed stdout, or its stderr as the error.
fn git_text(dir: &Path, args: &[&str]) -> Result<String, String> {
    let out = git(dir)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("could not run git {}: {e}", args.join(" ")))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    } else {
        Err(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

fn git_ok(dir: &Path, args: &[&str]) -> Result<(), String> {
    git_text(dir, args).map(drop)
}

/// The repository this runner serves: the one the caller stands in.
/// Its common git directory, not the caller's checkout: a runner started
/// from an agent's worktree must outlive that worktree's removal.
fn repo_root() -> Result<PathBuf, String> {
    let cwd = std::env::current_dir().map_err(|e| format!("no working directory: {e}"))?;
    common_dir(&cwd)
}

fn common_dir(dir: &Path) -> Result<PathBuf, String> {
    git_text(
        dir,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )
    .map(PathBuf::from)
}

/// The remote's main as this repository last fetched it.
fn remote_head(repo: &Path, remote: &str) -> Result<String, String> {
    let sha = git_text(
        repo,
        &[
            "rev-parse",
            "--verify",
            &format!("refs/remotes/{remote}/main^{{commit}}"),
        ],
    )?;
    if is_sha(&sha) {
        Ok(sha)
    } else {
        Err(format!("{remote}/main resolved to {sha:?}, not a commit"))
    }
}

/// The newest first-parent ancestor of `head` (itself included) whose
/// recorded verdict is a pass, with how many commits it trails `head`.
fn last_green(repo: &Path, state: &State, head: &str) -> Option<(String, usize)> {
    let list = git_text(
        repo,
        &[
            "rev-list",
            "--first-parent",
            &format!("--max-count={GREEN_HORIZON}"),
            head,
        ],
    )
    .ok()?;
    newest_passing(list.lines(), |sha| state.verdict(sha).map(|r| r.verdict))
}

fn newest_passing<'a>(
    shas: impl Iterator<Item = &'a str>,
    verdict: impl Fn(&str) -> Option<Verdict>,
) -> Option<(String, usize)> {
    shas.enumerate()
        .find(|(_, sha)| verdict(sha) == Some(Verdict::Pass))
        .map(|(behind, sha)| (sha.to_string(), behind))
}

/// Point the runner's worktree at `sha`, creating it on first use. Ignored
/// files (`target/`, `.td-build-cache/`, the oracle memo) are kept: they
/// are what makes the next run cheaper than the first.
fn prepare_tree(repo: &Path, state: &State, sha: &str) -> Result<PathBuf, String> {
    let tree = state.tree();
    if tree.join(".git").exists() {
        let owner = common_dir(&tree)?;
        if !same_dir(&owner, repo) {
            return Err(format!(
                "{} is a worktree of {}, not of {}: remove it to let the runner \
                 make its own",
                tree.display(),
                owner.display(),
                repo.display()
            ));
        }
    } else {
        // A tree removed by hand leaves its registration behind.
        git_ok(repo, &["worktree", "prune"])?;
        let path = tree
            .to_str()
            .ok_or_else(|| format!("{} is not UTF-8", tree.display()))?;
        git_ok(repo, &["worktree", "add", "--detach", "--quiet", path, sha])?;
    }
    git_ok(&tree, &["checkout", "--detach", "--force", "--quiet", sha])?;
    git_ok(&tree, &["clean", "-fd", "--quiet"])?;
    let at = git_text(&tree, &["rev-parse", "HEAD"])?;
    if at == sha {
        Ok(tree)
    } else {
        Err(format!("{} is at {at}, not {sha}", tree.display()))
    }
}

fn same_dir(a: &Path, b: &Path) -> bool {
    match (fs::canonicalize(a), fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// The caller's environment, less what would aim the tier somewhere other
/// than the runner's tree: a target directory (the tree's own `target/` is
/// the cache that makes the next run cheap), the caller's rustc flags, a
/// recipe-check scope, a branch run's deferral of engine-only recipe
/// checks (main is where they run), a gate-disable list that could drop
/// the recipe gate and leave a green with nothing checked, a
/// `TD_CHECK_FULL` that would boot past the memo every run, and git's
/// repository overrides.
fn scrub(cmd: &mut Command) -> &mut Command {
    for var in [
        "CARGO_TARGET_DIR",
        "RUSTFLAGS",
        "CARGO_ENCODED_RUSTFLAGS",
        "TD_CHECK_FULL",
        "CARGO_BUILD_TARGET_DIR",
        crate::check_loop::CHECK_SCOPE_ENV,
        crate::check_loop::CHECK_DEFER_ENV,
        "TD_CHECK_DISABLE",
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_COMMON_DIR",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    ] {
        cmd.env_remove(var);
    }
    cmd
}

/// The tier as `sha` defines it: its own builder, built in its own tree,
/// then run as this process's child, so that killing the runner ends the
/// run (and with it the check host's request) rather than leaving it in a
/// tree the next runner resets.
fn run_tier(tree: &Path, log: &Path) -> Result<Option<i32>, String> {
    let mut out = OpenOptions::new()
        .create(true)
        .append(true)
        .open(log)
        .map_err(|e| format!("{}: {e}", log.display()))?;
    let _ = writeln!(out, "== td-builder main-integration: run at {}", now_unix());
    let child = |cmd: &mut Command| -> Result<Option<i32>, String> {
        let stdout = out
            .try_clone()
            .map_err(|e| format!("{}: {e}", log.display()))?;
        let stderr = out
            .try_clone()
            .map_err(|e| format!("{}: {e}", log.display()))?;
        crate::host_bin::arm_check_child(cmd);
        scrub(cmd)
            .current_dir(tree)
            .stdin(Stdio::null())
            .stdout(stdout)
            .stderr(stderr)
            .status()
            .map(|s| s.code())
            .map_err(|e| format!("could not start {cmd:?} in {}: {e}", tree.display()))
    };
    let built = child(Command::new("cargo").args([
        "build",
        "--release",
        "--manifest-path",
        "builder/Cargo.toml",
    ]))?;
    if built != Some(0) {
        // A landed commit whose builder does not build is red; a signal
        // is not a verdict.
        return Ok(built);
    }
    child(Command::new(tree.join("target/release/td-builder")).args(TIER_ARGS))
}

/// What a run asks the head's builder: every recipe check, unscoped and
/// undeferred, which a branch's `ready` leaves to main when only the
/// builder engine or evaluator changed since a check's last pass, then the
/// qemu oracles, which run only once the gates pass.
const TIER_ARGS: &[&str] = &["check", "recipe-checks", crate::integration::GOAL];

/// What one pass did.
#[derive(Debug, PartialEq)]
enum Pass {
    /// The head already has this verdict.
    Idle(String, Verdict),
    Ran(String, Verdict),
    /// The run died by a signal (the runner or the tier killed, the host
    /// out of memory): no verdict, so the next pass runs the head again.
    Aborted(String),
}

/// Runs the tier in a prepared tree, writing to a log: its exit code, or
/// None for a signal.
type Tier<'a> = dyn FnMut(&Path, &Path) -> Result<Option<i32>, String> + 'a;

/// Fetch, and run the tier on the newest head if it has no verdict (or
/// `again`).
fn pass_once(
    repo: &Path,
    state: &State,
    remote: &str,
    again: bool,
    tier: &mut Tier<'_>,
) -> Result<Pass, String> {
    // The destination is spelled out: a single-branch or narrowed fetch
    // config would otherwise leave `REMOTE/main` missing or stale.
    let refspec = format!("+refs/heads/main:refs/remotes/{remote}/main");
    git_ok(repo, &["fetch", "--quiet", remote, &refspec])?;
    let head = remote_head(repo, remote)?;
    if let Some(record) = state.verdict(&head).filter(|_| !again) {
        return Ok(Pass::Idle(head, record.verdict));
    }
    for sub in ["verdicts", "logs"] {
        fs::create_dir_all(state.dir.join(sub))
            .map_err(|e| format!("{}: {e}", state.dir.join(sub).display()))?;
    }
    let tree = prepare_tree(repo, state, &head)?;
    let log = state.log_path(&head);
    let started_unix = now_unix();
    fs::write(state.running_path(), format!("{head} {started_unix}\n"))
        .map_err(|e| format!("{}: {e}", state.running_path().display()))?;
    let started = Instant::now();
    let exit = tier(&tree, &log);
    let _ = fs::remove_file(state.running_path());
    let Some(code) = exit? else {
        return Ok(Pass::Aborted(head));
    };
    let verdict = Verdict::of(Some(code));
    state.write_verdict(
        &head,
        &Record {
            verdict,
            exit: Some(code),
            secs: started.elapsed().as_secs(),
            finished: now_unix(),
        },
    )?;
    Ok(Pass::Ran(head, verdict))
}

fn lock_file(state: &State) -> Result<(File, PathBuf), String> {
    fs::create_dir_all(&state.dir).map_err(|e| format!("{}: {e}", state.dir.display()))?;
    let path = state.dir.join("runner.lock");
    OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .map(|f| (f, path.clone()))
        .map_err(|e| format!("{}: {e}", path.display()))
}

/// Hold the runner's lock for the process's life, or say who has it. A
/// `running` file left by a runner that was killed is cleared here. A
/// `status` holds the lock shared for an instant, so a busy lock is asked
/// again for a second before it means another runner.
fn hold_lock(state: &State) -> Result<File, String> {
    let (mut file, path) = lock_file(state)?;
    let mut tries = 0;
    loop {
        match file.try_lock() {
            Ok(()) => break,
            Err(std::fs::TryLockError::WouldBlock) if tries < 10 => {
                tries += 1;
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(std::fs::TryLockError::WouldBlock) => {
                return Err(format!(
                    "another runner holds {} (its pid is in the file)",
                    path.display()
                ))
            }
            Err(std::fs::TryLockError::Error(e)) => return Err(format!("{}: {e}", path.display())),
        }
    }
    let _ = file.set_len(0);
    let _ = writeln!(file, "{}", std::process::id());
    let _ = fs::remove_file(state.running_path());
    Ok(file)
}

/// Whether a runner holds the state now.
/// Read-only and shared, so asking creates nothing and cannot hold off a
/// runner that is starting for longer than `hold_lock` waits.
fn runner_live(state: &State) -> bool {
    File::open(state.dir.join("runner.lock")).is_ok_and(|file| {
        matches!(
            file.try_lock_shared(),
            Err(std::fs::TryLockError::WouldBlock)
        )
    })
}

fn run(opts: &RunOpts) -> Result<u8, String> {
    let repo = repo_root()?;
    let state = State { dir: state_dir()? };
    let _lock = hold_lock(&state)?;
    println!(
        ">> main-integration: {}/main in {}, state in {}",
        opts.remote,
        repo.display(),
        state.dir.display()
    );
    let mut idle_said: Option<String> = None;
    loop {
        let pass = pass_once(&repo, &state, &opts.remote, opts.again, &mut |tree, log| {
            println!(
                ">> main-integration: running the tier at {} (log {})",
                tree.display(),
                log.display()
            );
            run_tier(tree, log)
        });
        let code = match &pass {
            Ok(Pass::Idle(head, verdict)) => {
                if idle_said.as_deref() != Some(head) {
                    println!(
                        ">> main-integration: {} already has a verdict ({}); waiting for \
                         main to move",
                        short(head),
                        verdict.word()
                    );
                    idle_said = Some(head.clone());
                }
                verdict.exit_code()
            }
            Ok(Pass::Ran(head, verdict)) => {
                idle_said = Some(head.clone());
                report(&repo, &state, head, *verdict);
                verdict.exit_code()
            }
            Ok(Pass::Aborted(head)) => {
                println!(
                    ">> main-integration: {} ABORTED by a signal; no verdict, the next \
                     pass runs it again (log {})",
                    short(head),
                    state.log_path(head).display()
                );
                1
            }
            Err(e) => {
                eprintln!("td-builder main-integration: {e}");
                1
            }
        };
        if opts.once {
            return Ok(code);
        }
        std::thread::sleep(opts.interval);
    }
}

fn short(sha: &str) -> &str {
    sha.get(..12).unwrap_or(sha)
}

/// One run's outcome, and on red what may have broken it.
fn report(repo: &Path, state: &State, head: &str, verdict: Verdict) {
    println!(
        ">> main-integration: {} {} (log {})",
        short(head),
        verdict.word().to_uppercase(),
        state.log_path(head).display()
    );
    if verdict != Verdict::Fail {
        return;
    }
    match last_green(repo, state, head) {
        Some((green, _)) => {
            println!("   suspects since the last green {}:", short(&green));
            let range = format!("{green}..{head}");
            let log =
                git_text(repo, &["log", "--oneline", "--first-parent", &range]).unwrap_or_default();
            for line in log.lines() {
                println!("     {line}");
            }
        }
        None => println!("   no green run within {GREEN_HORIZON} commits to bound the suspects"),
    }
    println!("   heal: ci/revert-suspect.sh --ref <suspect>, landed as usual");
}

fn status(remote: &str) -> Result<u8, String> {
    let repo = repo_root()?;
    let state = State { dir: state_dir()? };
    let head = remote_head(&repo, remote)?;
    let subject = git_text(&repo, &["log", "-1", "--format=%s", &head]).unwrap_or_default();
    println!("{remote}/main {} {subject} (as last fetched)", short(&head));
    let live = runner_live(&state);
    let running = fs::read_to_string(state.running_path())
        .ok()
        .and_then(|t| {
            let mut w = t.split_whitespace();
            Some((w.next()?.to_string(), w.next()?.parse::<u64>().ok()?))
        })
        .filter(|_| live);
    match running {
        Some((sha, started)) => println!(
            "  running: {} for {}m",
            short(&sha),
            now_unix().saturating_sub(started) / 60
        ),
        None if live => println!("  runner: waiting for main to move"),
        None => println!("  runner: none holds {}", state.dir.display()),
    }
    let code = match state.verdict(&head) {
        Some(record) => {
            println!(
                "  verdict: {} in {}m (log {})",
                record.verdict.word(),
                record.secs / 60,
                state.log_path(&head).display()
            );
            record.verdict.exit_code()
        }
        None => {
            println!("  verdict: none yet");
            NO_VERDICT
        }
    };
    match last_green(&repo, &state, &head) {
        Some((sha, 0)) => println!("  last green: {} (this head)", short(&sha)),
        Some((sha, behind)) => println!("  last green: {} ({behind} behind)", short(&sha)),
        None => println!("  last green: none within {GREEN_HORIZON} commits"),
    }
    Ok(code)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    fn s(args: &[&str]) -> Vec<String> {
        args.iter().map(|a| a.to_string()).collect()
    }

    /// Main runs every recipe check in full before the oracles: the recipe
    /// gate is named, so its failure reds the run, and nothing the caller
    /// exported may scope or defer it.
    #[test]
    fn the_tier_runs_every_recipe_check_then_the_oracles() {
        assert_eq!(TIER_ARGS, &["check", "recipe-checks", "integration"]);
        let mut cmd = Command::new("true");
        cmd.env(crate::check_loop::CHECK_DEFER_ENV, "1")
            .env(crate::check_loop::CHECK_SCOPE_ENV, "td-sh")
            .env("TD_CHECK_DISABLE", "recipe-checks")
            .env("TD_CHECK_FULL", "1");
        scrub(&mut cmd);
        let removed: Vec<&OsStr> = cmd
            .get_envs()
            .filter(|(_, v)| v.is_none())
            .map(|(k, _)| k)
            .collect();
        for var in [
            crate::check_loop::CHECK_DEFER_ENV,
            crate::check_loop::CHECK_SCOPE_ENV,
            "TD_CHECK_DISABLE",
            "TD_CHECK_FULL",
        ] {
            assert!(removed.contains(&OsStr::new(var)), "{var} not scrubbed");
        }
    }

    #[test]
    fn run_arguments_parse_and_refuse() {
        let opts = parse_run(&s(&["--once", "--again", "--interval", "60"])).unwrap();
        assert_eq!(
            opts,
            RunOpts {
                once: true,
                again: true,
                interval: Duration::from_secs(60),
                remote: "origin".to_string(),
            }
        );
        assert_eq!(parse_run(&[]).unwrap().interval, DEFAULT_INTERVAL);
        for bad in [
            &["--again"][..],
            &["--interval", "0"],
            &["--interval"],
            &["--remote", "--upload-pack=x"],
            &["--remote", "../x"],
            &["--bogus"],
        ] {
            assert!(parse_run(&s(bad)).is_err(), "{bad:?}");
        }
        assert_eq!(parse_remote(&s(&["--remote", "up"])).unwrap(), "up");
        assert!(parse_remote(&s(&["up"])).is_err());
    }

    #[test]
    fn the_state_dir_is_explicit_or_under_home_and_absolute() {
        assert_eq!(
            state_dir_from(Some(OsStr::new("/x")), Some(OsStr::new("/h"))).unwrap(),
            PathBuf::from("/x")
        );
        assert_eq!(
            state_dir_from(None, Some(OsStr::new("/h"))).unwrap(),
            PathBuf::from("/h/.local/state/td/main-integration")
        );
        assert!(state_dir_from(Some(OsStr::new("rel")), None).is_err());
        assert!(state_dir_from(None, None).is_err());
    }

    #[test]
    fn a_record_round_trips_and_a_garbled_one_is_none() {
        for record in [
            Record {
                verdict: Verdict::Pass,
                exit: Some(0),
                secs: 1500,
                finished: 7,
            },
            Record {
                verdict: Verdict::Fail,
                exit: None,
                secs: 3,
                finished: 9,
            },
        ] {
            assert_eq!(Record::parse(&record.line()), Some(record));
        }
        for bad in ["", "pass 0 1", "maybe 0 1 2", "pass x 1 2", "pass 0 1 2 3"] {
            assert_eq!(Record::parse(bad), None, "{bad:?}");
        }
        assert_eq!(Verdict::of(Some(0)), Verdict::Pass);
        assert_eq!(
            Verdict::of(Some(td_engine::exit::EXIT_UNPROVISIONED)),
            Verdict::HostGap
        );
        assert_eq!(Verdict::of(Some(1)), Verdict::Fail);
        assert_eq!(Verdict::of(None), Verdict::Fail);
    }

    #[test]
    fn the_last_green_skips_reds_gaps_and_unrun_commits() {
        let verdict = |sha: &str| match sha {
            "c" => Some(Verdict::Fail),
            "b" => Some(Verdict::HostGap),
            "a" => Some(Verdict::Pass),
            _ => None,
        };
        assert_eq!(
            newest_passing(["d", "c", "b", "a"].into_iter(), verdict),
            Some(("a".to_string(), 3))
        );
        assert_eq!(newest_passing(["d", "c"].into_iter(), verdict), None);
    }

    /// A bare origin and a clone: a pass runs the newest head once, records
    /// its verdict, idles on it while main stands still, and after two
    /// landings runs only the newer one; `--again` re-runs a head with a
    /// verdict; a signal death records none, so the next pass runs it
    /// again. The runner is anchored on the clone's common git directory,
    /// found from a linked worktree that is then removed, and its fetch
    /// names its destination, so a fetch config that maps nothing to
    /// `origin/main` still moves it.
    #[test]
    fn a_runner_takes_the_newest_head_once() {
        if Command::new("git").arg("--version").output().is_err() {
            eprintln!("SKIP: no git (the in-sandbox gate)");
            return;
        }
        let base = std::env::temp_dir().join(format!("td-main-integration-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        struct Rm(PathBuf);
        impl Drop for Rm {
            fn drop(&mut self) {
                let _ = fs::remove_dir_all(&self.0);
            }
        }
        let _rm = Rm(base.clone());
        let (origin, work, clone) = (base.join("origin"), base.join("work"), base.join("repo"));
        for d in [&origin, &work] {
            fs::create_dir_all(d).unwrap();
        }
        let sh = |dir: &Path, args: &[&str]| {
            let out = git(dir)
                .args(args)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@example.invalid")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@example.invalid")
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}: {out:?}");
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        let land = |n: u32| {
            fs::write(work.join("f"), n.to_string()).unwrap();
            sh(&work, &["add", "f"]);
            sh(&work, &["commit", "-qm", &format!("c{n}")]);
            sh(&work, &["push", "-q", "origin", "HEAD:main"]);
            sh(&work, &["rev-parse", "HEAD"])
        };
        sh(&origin, &["init", "-q", "--bare", "--initial-branch=main"]);
        sh(&work, &["init", "-q"]);
        sh(
            &work,
            &["remote", "add", "origin", origin.to_str().unwrap()],
        );
        let first = land(1);
        sh(&base, &["clone", "-q", origin.to_str().unwrap(), "repo"]);
        // A fetch config that maps nothing to origin/main, and no such ref.
        sh(
            &clone,
            &[
                "config",
                "remote.origin.fetch",
                "+refs/heads/none:refs/remotes/origin/none",
            ],
        );
        sh(&clone, &["update-ref", "-d", "refs/remotes/origin/main"]);
        let linked = base.join("linked");
        sh(
            &clone,
            &[
                "worktree",
                "add",
                "-q",
                "--detach",
                linked.to_str().unwrap(),
            ],
        );
        let repo = common_dir(&linked).unwrap();
        assert!(same_dir(&repo, &clone.join(".git")), "{}", repo.display());
        fs::remove_dir_all(&linked).unwrap();
        let state = State {
            dir: base.join("state"),
        };
        let mut ran: Vec<String> = Vec::new();
        let mut tier = |tree: &Path, log: &Path| {
            ran.push(fs::read_to_string(tree.join("f")).unwrap());
            fs::write(log, "log").unwrap();
            assert!(state_running(&base));
            Ok(Some(0))
        };
        fn state_running(base: &Path) -> bool {
            base.join("state/running").exists()
        }
        assert_eq!(
            pass_once(&repo, &state, "origin", false, &mut tier).unwrap(),
            Pass::Ran(first.clone(), Verdict::Pass)
        );
        assert_eq!(
            pass_once(&repo, &state, "origin", false, &mut tier).unwrap(),
            Pass::Idle(first.clone(), Verdict::Pass)
        );
        land(2);
        let third = land(3);
        // An untracked file in the tree goes; an ignored cache would stay.
        fs::write(state.tree().join("stray"), "x").unwrap();
        assert_eq!(
            pass_once(&repo, &state, "origin", false, &mut tier).unwrap(),
            Pass::Ran(third.clone(), Verdict::Pass)
        );
        assert!(!state.tree().join("stray").exists());
        let mut red = |_: &Path, _: &Path| Ok(Some(1));
        assert_eq!(
            pass_once(&repo, &state, "origin", true, &mut red).unwrap(),
            Pass::Ran(third.clone(), Verdict::Fail)
        );
        assert_eq!(
            pass_once(&repo, &state, "origin", false, &mut red).unwrap(),
            Pass::Idle(third.clone(), Verdict::Fail)
        );
        assert_eq!(ran, ["1", "3"]);
        assert!(!state.running_path().exists());
        assert_eq!(state.verdict(&third).unwrap().verdict, Verdict::Fail);
        assert_eq!(last_green(&repo, &state, &third), Some((first.clone(), 2)));
        // A tier that cannot start records nothing and clears `running`.
        let fourth = land(4);
        let mut broken = |_: &Path, _: &Path| Err("no cargo".to_string());
        assert!(pass_once(&repo, &state, "origin", false, &mut broken).is_err());
        assert!(!state.running_path().exists());
        // A signal death is no verdict: the next pass runs the head again.
        let mut killed = |_: &Path, _: &Path| Ok(None);
        assert_eq!(
            pass_once(&repo, &state, "origin", false, &mut killed).unwrap(),
            Pass::Aborted(fourth.clone())
        );
        assert!(state.verdict(&fourth).is_none());
        assert!(!state.running_path().exists());
        assert_eq!(
            pass_once(&repo, &state, "origin", false, &mut red).unwrap(),
            Pass::Ran(fourth.clone(), Verdict::Fail)
        );

        // A tree that is another repository's worktree is refused.
        let other = State {
            dir: base.join("other-state"),
        };
        sh(
            &work,
            &[
                "worktree",
                "add",
                "-q",
                "--detach",
                other.tree().to_str().unwrap(),
            ],
        );
        let err = pass_once(&repo, &other, "origin", false, &mut red).unwrap_err();
        assert!(err.contains("is a worktree of"), "{err}");

        // The lock: held means live; taking it clears a killed runner's
        // `running`; released means none. Asking creates nothing.
        let fresh = State {
            dir: base.join("fresh-state"),
        };
        assert!(!runner_live(&fresh));
        assert!(!fresh.dir.exists());
        fs::write(state.running_path(), "x 1\n").unwrap();
        assert!(!runner_live(&state));
        let held = hold_lock(&state).unwrap();
        assert!(!state.running_path().exists());
        assert!(runner_live(&state));
        assert!(hold_lock(&state).is_err());
        drop(held);
        assert!(!runner_live(&state));
    }
}

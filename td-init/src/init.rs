//! `init` — td's PID 1: run the inittab, then supervise and reap forever.
//!
//! Two duties. The first is the inittab: `sysinit` and `wait` jobs run to
//! completion in order, `once` jobs are started and left alone, `respawn` jobs
//! are restarted whenever they exit. The second is reaping: every orphan the
//! kernel reparents onto PID 1 must be collected or the process table fills with
//! zombies, and `wait4(-1, ...)` is the only way to see children this process
//! never spawned — `Child::wait` is a targeted `waitpid` and cannot.
//!
//! Supervision uses no signals at all: a blocking `wait4` IS the event loop, so
//! there is no handler surface and no self-pipe. That is also why `ctrlaltdel`,
//! `shutdown` and `restart` inittab actions are unsupported — each is a signal
//! contract, and a signal handler is a separate reviewed increment.
//!
//! `init --dry-run [-f FILE]` prints on stdout the jobs it would run, preceded
//! by any complaint about the table so a report read through a pipe is never
//! jobs alone. It exits non-zero on any such complaint, including a table that
//! yielded no jobs and fell back to the built-in one.

use crate::sys;
use std::fs::OpenOptions;
use std::os::unix::fs::OpenOptionsExt;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const DEFAULT_PATH: &str = "/etc/inittab";

/// The environment every inittab job starts with, matching busybox init. td's
/// own `/bin` is the uutils farm, so the search path is the usual four.
const DEFAULT_ENV: &[(&str, &str)] = &[
    ("HOME", "/"),
    ("PATH", "/sbin:/usr/sbin:/bin:/usr/bin"),
    ("SHELL", "/bin/sh"),
    ("USER", "root"),
];

/// The table used when `/etc/inittab` is unreadable: bring the system up, then
/// keep a shell on the console. `cttyhack` is what makes that shell a session
/// leader with a controlling terminal, so job control works in a rescue.
const DEFAULT_TABLE: &str = "\
::sysinit:/etc/init.d/rcS
::respawn:/bin/cttyhack /bin/sh
";

/// A respawn job that dies faster than this is restarted only after
/// `RESPAWN_DELAY`, so a broken command cannot spin the machine.
const MIN_UPTIME: Duration = Duration::from_secs(1);
const RESPAWN_DELAY: Duration = Duration::from_secs(1);
/// The bounded slice used only while a restart is pending; the settled loop
/// blocks in `wait4` instead.
const POLL_SLICE: Duration = Duration::from_millis(100);
/// How long to sleep when this process has no children at all.
const IDLE_SLICE: Duration = Duration::from_secs(1);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    SysInit,
    Wait,
    Once,
    Respawn,
}

impl Action {
    fn parse(text: &str) -> Option<Action> {
        match text {
            "sysinit" => Some(Action::SysInit),
            "wait" => Some(Action::Wait),
            "once" => Some(Action::Once),
            "respawn" => Some(Action::Respawn),
            _ => None,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Action::SysInit => "sysinit",
            Action::Wait => "wait",
            Action::Once => "once",
            Action::Respawn => "respawn",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// The inittab id field: the terminal this job's stdio is opened on, bare
    /// (`ttyS0`) or absolute. Empty means inherit init's own stdio.
    pub tty: String,
    pub action: Action,
    pub argv: Vec<String>,
    /// Per-word: would a shell have left this argument alone? Only the
    /// `--dry-run` note reads it — such a word is not a shell/exec divergence.
    /// Quoting is most of the answer but not all of it; see `split_argv_marked`.
    pub quoted: Vec<bool>,
}

fn log(msg: &str) {
    crate::emit_err(&format!("init: {msg}\n"));
}

// ── the inittab ─────────────────────────────────────────────────────────────

/// Split a process field into argv, flagging each word that arrived QUOTED.
/// Whitespace separates, and single quotes, double quotes and backslash group —
/// enough for the `sh -c 'a; b'` entries real inittabs carry. There is no
/// variable, glob or redirection handling: init execs the command directly, it
/// does not run a shell for you. The flag exists for the `--dry-run` note
/// alone, and it means "a shell would have left this word alone too" — which is
/// NOT the same as "it was quoted": double quotes still expand `$` and command
/// substitution, so `"$HOME"` IS a divergence between td's literal exec and the
/// shell busybox's init would have run, while `'$HOME'` and `\$HOME` are not.
fn split_argv_marked(text: &str) -> Result<Vec<(String, bool)>, String> {
    let mut out: Vec<(String, bool)> = Vec::new();
    let mut word = String::new();
    let mut quoted = false;
    // Set by anything a shell would still act on, quoted or not — the half of
    // the answer quoting alone cannot give.
    let mut expands = false;
    let mut started = false;
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        match c {
            ' ' | '\t' => {
                if started {
                    out.push((std::mem::take(&mut word), quoted && !expands));
                    started = false;
                    quoted = false;
                    expands = false;
                }
            }
            '\'' => {
                started = true;
                quoted = true;
                loop {
                    match chars.next() {
                        Some('\'') => break,
                        Some(q) => word.push(q),
                        None => return Err("unterminated single quote".to_string()),
                    }
                }
            }
            '"' => {
                started = true;
                quoted = true;
                loop {
                    match chars.next() {
                        Some('"') => break,
                        // POSIX: inside double quotes a backslash escapes only
                        // these four. Before anything else it stays literal, so
                        // `"\n"` is backslash-n — matching what a shell would
                        // have handed the same command line.
                        Some('\\') => match chars.next() {
                            Some(e) if matches!(e, '"' | '\\' | '$' | '`') => word.push(e),
                            Some(e) => {
                                word.push('\\');
                                word.push(e);
                            }
                            None => return Err("trailing backslash".to_string()),
                        },
                        // Double quotes stop globbing and word splitting but
                        // NOT expansion: these two still run inside them.
                        Some(q) => {
                            if q == '$' || q == '`' {
                                expands = true;
                            }
                            word.push(q);
                        }
                        None => return Err("unterminated double quote".to_string()),
                    }
                }
            }
            '\\' => {
                started = true;
                // A backslash escape is as literal as a quote, and was being
                // counted as neither.
                quoted = true;
                match chars.next() {
                    Some(e) => word.push(e),
                    None => return Err("trailing backslash".to_string()),
                }
            }
            other => {
                started = true;
                // Unquoted, so a shell would act on it — even if some OTHER
                // part of this same word was quoted.
                if METACHARACTERS.contains(&other) {
                    expands = true;
                }
                word.push(other);
            }
        }
    }
    if started {
        out.push((word, quoted && !expands));
    }
    Ok(out)
}

/// Parse `id:runlevels:action:process` lines. Rejected lines are collected as
/// diagnostics rather than aborting: PID 1 boots what it understood and reports
/// what it did not, because a machine that refuses to start over one bad line
/// cannot be repaired from its own console.
///
/// The runlevels field is accepted and ignored — td has no runlevels — so the
/// familiar four-field shape keeps working.
pub fn parse_inittab(text: &str) -> (Vec<Entry>, Vec<String>) {
    let mut entries = Vec::new();
    let mut problems = Vec::new();
    for (index, raw) in text.lines().enumerate() {
        let number = index + 1;
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut fields = line.splitn(4, ':');
        let parsed = (fields.next(), fields.next(), fields.next(), fields.next());
        let (id, action, process) = match parsed {
            (Some(id), Some(_runlevels), Some(action), Some(process)) => (id, action, process),
            _ => {
                problems.push(format!("line {number}: expected id:runlevels:action:process"));
                continue;
            }
        };
        let Some(action) = Action::parse(action) else {
            problems.push(format!(
                "line {number}: unsupported action '{action}' (sysinit, wait, once, respawn)"
            ));
            continue;
        };
        let marked = match split_argv_marked(process) {
            Ok(marked) => marked,
            Err(e) => {
                problems.push(format!("line {number}: {e}"));
                continue;
            }
        };
        if marked.is_empty() {
            problems.push(format!("line {number}: no command"));
            continue;
        }
        let (argv, quoted) = marked.into_iter().unzip();
        entries.push(Entry {
            tty: id.to_string(),
            action,
            argv,
            quoted,
        });
    }
    (entries, problems)
}

/// The built-in table plus the reason it is being used, prefixed to whatever
/// else the caller had to say.
fn fall_back(why: String) -> (Vec<Entry>, Vec<String>) {
    let (entries, problems) = parse_inittab(DEFAULT_TABLE);
    let mut all = vec![why];
    all.extend(problems);
    (entries, all)
}

fn load(path: &str) -> (Vec<Entry>, Vec<String>) {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) => return fall_back(format!("{path}: {e}; using the built-in table")),
    };
    let (entries, problems) = parse_inittab(&text);
    if !entries.is_empty() {
        return (entries, problems);
    }
    // A table that yields NO jobs is as unusable as one that would not open:
    // supervise() would idle forever with no console shell, which is a machine
    // that is up and cannot be repaired from its own console. An empty file, an
    // all-comment file, and a table whose every action td rejects all land here.
    // busybox init installs its defaults on an empty action list for the same
    // reason.
    let (entries, note) = fall_back(format!("{path}: no usable entries; using the built-in table"));
    let mut all = problems;
    all.extend(note);
    (entries, all)
}

// ── running jobs ────────────────────────────────────────────────────────────

fn describe(entry: &Entry) -> String {
    entry.argv.join(" ")
}

/// `O_NOCTTY` — this open must not make the terminal INIT's own.
const O_NOCTTY: i32 = 0o400;

/// Start a job. When the entry names a terminal, the child's stdio is opened on
/// it — `Stdio::from(File)` is the whole mechanism, so no `dup2` is needed.
///
/// The open carries `O_NOCTTY` because it happens in the PARENT: init is a
/// session leader with no controlling terminal, so without the flag the first
/// `respawn ttyS0` entry would silently make that tty init's own — and a ^C on
/// the console would then be delivered against PID 1. The child claims it
/// deliberately instead, which is exactly what `cttyhack` is for.
fn start(entry: &Entry) -> Result<Child, String> {
    let prog = entry
        .argv
        .first()
        .ok_or_else(|| "empty command".to_string())?;
    let mut cmd = Command::new(prog);
    cmd.args(entry.argv.get(1..).unwrap_or(&[]));
    // The kernel hands PID 1 only `HOME=/` and `TERM=linux` (plus any
    // `key=value` word on the kernel command line) — no PATH, so without this
    // every job inherits a shell where `mount` is ENOENT. These
    // are busybox init's four, and they are DEFAULTS: a value already in init's
    // environment (one the kernel command line set) wins.
    for (key, value) in DEFAULT_ENV {
        // Empty counts as unset: a kernel command line that produces `PATH=`
        // would otherwise leave every job with no search path at all.
        if std::env::var_os(key).is_none_or(|v| v.is_empty()) {
            cmd.env(key, value);
        }
    }
    if !entry.tty.is_empty() {
        let path = if entry.tty.starts_with('/') {
            entry.tty.clone()
        } else {
            format!("/dev/{}", entry.tty)
        };
        match OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(O_NOCTTY)
            .open(&path)
        {
            Ok(tty) => match (tty.try_clone(), tty.try_clone()) {
                (Ok(out), Ok(err)) => {
                    cmd.stdin(Stdio::from(tty))
                        .stdout(Stdio::from(out))
                        .stderr(Stdio::from(err));
                }
                _ => log(&format!("{path}: could not duplicate the handle")),
            },
            Err(e) => log(&format!("{path}: {e}; inheriting init's stdio")),
        }
    }
    cmd.spawn().map_err(|e| format!("{prog}: {e}"))
}

/// Run a job to completion. Used only before the reaping loop starts, so the
/// targeted `waitpid` inside `Child::wait` cannot race the `wait4(-1)` there.
fn run_to_completion(entry: &Entry) {
    match start(entry) {
        Ok(mut child) => match child.wait() {
            Ok(status) => {
                if !status.success() {
                    log(&format!("{}: {status}", describe(entry)));
                }
            }
            Err(e) => log(&format!("{}: {e}", describe(entry))),
        },
        Err(e) => log(&e),
    }
}

fn earliest(current: Option<Instant>, candidate: Instant) -> Option<Instant> {
    Some(match current {
        Some(at) if at < candidate => at,
        _ => candidate,
    })
}

struct Respawn {
    entry: Entry,
    pid: Option<i32>,
    started: Option<Instant>,
    retry_at: Option<Instant>,
    /// Consecutive too-fast exits (or failures to start). Reset by one run that
    /// lasts `MIN_UPTIME`.
    fast_failures: u32,
}

/// After this many consecutive fast failures the job is held rather than retried
/// every second — busybox's escalation, and for the same reason: an inittab line
/// that can never work must not scroll a serial console forever, burying the
/// diagnostics needed to fix it.
const RESPAWN_BURST: u32 = 5;
const RESPAWN_HOLD: Duration = Duration::from_secs(300);

/// How long to wait before the next restart, and whether to say anything. The
/// burst is reported line by line, the switch to holding once, and the holding
/// itself in silence.
fn throttle(fast_failures: u32) -> (Duration, bool) {
    if fast_failures <= RESPAWN_BURST {
        (RESPAWN_DELAY, true)
    } else {
        (RESPAWN_HOLD, fast_failures == RESPAWN_BURST + 1)
    }
}

/// The supervision loop. Never returns: PID 1 exiting panics the kernel.
fn supervise(entries: Vec<Entry>, mut once: Vec<(i32, String)>) -> ! {
    let mut jobs: Vec<Respawn> = entries
        .into_iter()
        .map(|entry| Respawn {
            entry,
            pid: None,
            started: None,
            retry_at: None,
            fast_failures: 0,
        })
        .collect();

    loop {
        // Start every job that is down and off its throttle, tracking the
        // earliest pending restart so the reap below stays bounded.
        let now = Instant::now();
        let mut next_retry: Option<Instant> = None;
        for job in &mut jobs {
            if job.pid.is_some() {
                continue;
            }
            if let Some(at) = job.retry_at {
                if at > now {
                    next_retry = earliest(next_retry, at);
                    continue;
                }
            }
            match start(&job.entry) {
                Ok(child) => {
                    job.pid = Some(child.id() as i32);
                    job.started = Some(Instant::now());
                    job.retry_at = None;
                    // The pid is reaped by `wait4(-1)` below, so this handle must
                    // not also wait on it. Dropping a `Child` reaps nothing.
                    drop(child);
                }
                Err(e) => {
                    job.fast_failures = job.fast_failures.saturating_add(1);
                    let (delay, say) = throttle(job.fast_failures);
                    if say {
                        log(&format!("{e}; retrying in {}s", delay.as_secs()));
                    }
                    let at = Instant::now() + delay;
                    job.retry_at = Some(at);
                    next_retry = earliest(next_retry, at);
                }
            }
        }

        match sys::wait_any(next_retry.is_some()) {
            Ok(sys::Reaped::Child { pid, status }) => {
                collect(&mut jobs, &mut once, pid, status);
            }
            // With no signal handler there is nothing to wake us, so a pending
            // restart means polling: PID 1 wakes 10x/s until the timer expires.
            // That is cheap during the one-second burst, and it persists for the
            // whole five-minute hold of a job that can never start — the cost of
            // holding a broken entry rather than spinning on it.
            Ok(sys::Reaped::NotYet) => {
                let slice = match next_retry {
                    Some(at) => {
                        let left = at.saturating_duration_since(Instant::now());
                        // Poll finely only when the restart is imminent. A job
                        // held for five minutes would otherwise keep PID 1
                        // waking ten times a second for the whole hold.
                        left.min(if left > IDLE_SLICE { IDLE_SLICE } else { POLL_SLICE })
                    }
                    None => POLL_SLICE,
                };
                std::thread::sleep(slice);
            }
            // Nothing left to supervise: no children and every job throttled or
            // absent. Sleeping here is what keeps a table with no respawn job
            // (or a wholly failed one) from spinning.
            Ok(sys::Reaped::NoChildren) => std::thread::sleep(match next_retry {
                Some(at) => at.saturating_duration_since(Instant::now()).min(IDLE_SLICE),
                None => IDLE_SLICE,
            }),
            Err(e) => {
                log(&format!("wait: {e}"));
                std::thread::sleep(POLL_SLICE);
            }
        }
    }
}

/// Account for one reaped pid. An unknown pid is an ORPHAN the kernel reparented
/// onto us; collecting it silently is precisely PID 1's job.
fn collect(jobs: &mut [Respawn], once: &mut Vec<(i32, String)>, pid: i32, status: i32) {
    for job in jobs.iter_mut() {
        if job.pid != Some(pid) {
            continue;
        }
        job.pid = None;
        let brief = job
            .started
            .is_some_and(|at| at.elapsed() < MIN_UPTIME);
        if brief {
            job.fast_failures = job.fast_failures.saturating_add(1);
            let (delay, say) = throttle(job.fast_failures);
            if say {
                log(&format!(
                    "{} ({}) exited after less than {}s; restarting in {}s",
                    describe(&job.entry),
                    sys::status_text(status),
                    MIN_UPTIME.as_secs(),
                    delay.as_secs()
                ));
            }
            job.retry_at = Some(Instant::now() + delay);
        } else {
            job.fast_failures = 0;
        }
        job.started = None;
        return;
    }
    // In place: PID 1 reaps for the life of the machine, so this runs on every
    // orphan the kernel reparents onto init, not just on a `once` job's exit.
    once.retain(|(once_pid, name)| {
        if *once_pid != pid {
            return true;
        }
        if sys::exit_code(status) != Some(0) {
            log(&format!("{name}: {}", sys::status_text(status)));
        }
        false
    });
}

fn boot(entries: Vec<Entry>) -> ! {
    for action in [Action::SysInit, Action::Wait] {
        for entry in entries.iter().filter(|e| e.action == action) {
            run_to_completion(entry);
        }
    }
    let mut once = Vec::new();
    for entry in entries.iter().filter(|e| e.action == Action::Once) {
        match start(entry) {
            Ok(child) => {
                once.push((child.id() as i32, describe(entry)));
                drop(child);
            }
            Err(e) => log(&e),
        }
    }
    let respawns: Vec<Entry> = entries
        .into_iter()
        .filter(|e| e.action == Action::Respawn)
        .collect();
    supervise(respawns, once)
}

/// A word that only a shell would act on. busybox init hands such a process
/// field to `sh -c`; td's execs it directly, so the word arrives as a plain
/// argument. `--dry-run` is where that difference is cheap to notice; rejecting
/// the entry would refuse tables busybox accepts.
///
/// Matched anywhere in a word: the splitter groups `cmd>/dev/log` and `a|b`
/// into ONE word each.
const METACHARACTERS: [char; 14] =
    ['>', '<', '|', '&', ';', '(', ')', '$', '`', '*', '?', '[', ']', '~'];

/// Does a `-c` in this argv introduce a script? `mytool -c cfg > /dev/log`
/// hands the redirect over literally, so the answer turns on which program
/// actually runs, not on a `-c` appearing somewhere.
///
/// Wrappers are transparent because a real inittab is mostly wrappers — the
/// built-in table's own `cttyhack /bin/sh -c ...` does run a shell, and calling
/// that field "passed through as an argument" would be the opposite of true.
/// Each execs the rest of its argv, so step over it and ask again of what is
/// left. busybox is the same shape with its applet in argv[1].
fn invokes_a_shell(argv: &[String]) -> bool {
    const SHELLS: [&str; 5] = ["sh", "bash", "ash", "dash", "td-sh"];
    const WRAPPERS: [&str; 5] = ["cttyhack", "setsid", "env", "nohup", "chroot"];
    // `su` runs the target user's login shell whatever else it is given, so it
    // never reaches the wrapper walk below.
    const ALWAYS_A_SHELL: &str = "su";
    let mut words = argv.iter();
    let mut wrapped = false;
    // A wrapper chain is short by nature; the bound only stops a pathological
    // table from walking a long argv here.
    for _ in 0..argv.len().min(16) {
        let Some(word) = words.next() else {
            return false;
        };
        // A wrapper's own flags and `env`'s assignments sit between it and the
        // program it runs, so step over them rather than giving up: `env FOO=bar
        // sh -c ...` does run a shell.
        if wrapped && (word.starts_with('-') || word.contains('=')) {
            continue;
        }
        let prog = crate::basename(word);
        if SHELLS.contains(&prog) || prog == ALWAYS_A_SHELL {
            return true;
        }
        // `busybox` alone is not a shell — `busybox sh` is.
        if prog == "busybox" {
            return words.next().is_some_and(|a| SHELLS.contains(&crate::basename(a)));
        }
        if !WRAPPERS.contains(&prog) {
            return false;
        }
        // `chroot NEWROOT prog ...` — the operand is not the program.
        if prog == "chroot" && words.next().is_none() {
            return false;
        }
        wrapped = true;
    }
    false
}

/// Does this word introduce a shell's script operand? `-c` bare, or anywhere
/// in a short-flag cluster. Position in the cluster does not matter: the
/// option-argument follows the whole run either way, so `sh -xc 'cmd'` and
/// `sh -cx 'cmd'` are the same call, and `-c` is the only clusterable short
/// flag spelled `c` in any of the shells listed above.
fn introduces_a_script(word: &str) -> bool {
    match word.strip_prefix('-') {
        Some(flags) => !flags.starts_with('-') && flags.contains('c'),
        None => false,
    }
}

fn shell_word<'a>(argv: &'a [String], quoted: &[bool]) -> Option<&'a String> {
    // A leading `NAME=value` is a shell assignment. td's init execs it as the
    // program, so the command busybox would have run is never reached at all.
    if let Some(first) = argv.first() {
        if let Some((name, _)) = first.split_once('=') {
            if !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                return Some(first);
            }
        }
    }
    let shell = invokes_a_shell(argv);
    // Only the ONE operand after a shell's `-c` is that shell's to interpret;
    // noting `sh -c 'echo $HOME'` would be plainly false. Words PAST it are
    // `$0 $1 ...` to the script, so a redirect there is still handed over
    // literally and still worth the note.
    // ...and only the FIRST `-c`. A later one is an argument to the script, so
    // treating it as another introducer would swallow the word after it.
    let mut script = false;
    let mut consumed = false;
    for (index, word) in argv.iter().enumerate() {
        if script {
            script = false;
            consumed = true;
            continue;
        }
        if shell && !consumed && introduces_a_script(word) {
            script = true;
            continue;
        }
        // A quoted word is literal to a shell too, so it is not a divergence.
        if !quoted.get(index).copied().unwrap_or(false) && word.contains(METACHARACTERS) {
            return Some(word);
        }
    }
    None
}

fn dry_run(entries: &[Entry], problems: &[String]) -> Result<u8, String> {
    crate::emit(&report(entries, problems))?;
    Ok(if problems.is_empty() { 0 } else { 1 })
}

/// The `--dry-run` report, built as a string so a test can read it.
fn report(entries: &[Entry], problems: &[String]) -> String {
    let mut out = String::new();
    // On stdout, ahead of the jobs, because one of these is "your table yielded
    // nothing, so what follows is the BUILT-IN table" — and an operator reading
    // the report through a pipe would otherwise see jobs that are not in their
    // file with the explanation on a stream they are not looking at.
    for problem in problems {
        out.push_str(&format!("# {problem}\n"));
    }
    for entry in entries {
        let tty = if entry.tty.is_empty() { "-" } else { &entry.tty };
        out.push_str(&format!(
            "{} {tty} {}\n",
            entry.action.name(),
            describe(entry)
        ));
        if let Some(word) = shell_word(&entry.argv, &entry.quoted) {
            out.push_str(&format!(
                "  note: '{word}' is passed through as an argument; init execs the command directly and runs no shell\n"
            ));
        }
    }
    out
}

/// Whether an argument is meant as an option. A lone `-` is not.
fn is_option(arg: &str) -> bool {
    arg.starts_with('-') && arg.len() > 1
}

struct Options {
    path: String,
    dry: bool,
}

/// Parse init's own arguments under the one rule that governs this program: **as
/// PID 1 there is no such thing as a fatal argument.** The kernel panics the
/// moment PID 1 exits, so every fault that would be an error anywhere else is
/// downgraded to a logged note and the boot continues — including `--dry-run`,
/// which is a request to exit and so cannot be honoured. Off PID 1 the same
/// faults are ordinary errors: a mistyped option there must fail rather than
/// start a real supervision loop that hangs whoever ran it.
///
/// `pid1` is a parameter rather than a call to `std::process::id` so both halves
/// of that rule are testable; only `run` decides which we are.
fn parse_args(args: &[String], pid1: bool) -> Result<(Options, Vec<String>), String> {
    let mut opts = Options {
        path: DEFAULT_PATH.to_string(),
        dry: false,
    };
    let mut notes = Vec::new();
    let mut rest = args.iter();
    while let Some(a) = rest.next() {
        match a.as_str() {
            "--dry-run" => opts.dry = true,
            "-f" | "--inittab" => match rest.next() {
                Some(p) => opts.path = p.clone(),
                None => {
                    let e = format!("option '{a}' needs a FILE argument");
                    if !pid1 {
                        return Err(e);
                    }
                    // The path already parsed is kept, so the note has to name
                    // it rather than the default it may well not be.
                    notes.push(format!("{e}; using {}", opts.path));
                }
            },
            other if is_option(other) => {
                let e = format!("unrecognised option '{other}'");
                if !pid1 {
                    return Err(e);
                }
                notes.push(e);
            }
            // Bare words are tolerated everywhere: the kernel passes init
            // whatever is left on its command line, and those are not options.
            other => notes.push(format!("ignoring argument '{other}'")),
        }
    }
    if opts.dry && pid1 {
        notes.push("ignoring --dry-run: PID 1 must not exit".to_string());
        opts.dry = false;
    }
    Ok((opts, notes))
}

pub fn run(args: &[String]) -> Result<u8, String> {
    let pid1 = std::process::id() == 1;
    let (opts, notes) = parse_args(args, pid1)?;
    for note in &notes {
        log(note);
    }
    let (entries, problems) = load(&opts.path);
    if opts.dry {
        return dry_run(&entries, &problems);
    }
    // Supervising from anywhere but PID 1 is never what the caller meant, and it is
    // actively destructive: sysinit would re-run (remounting filesystems), every
    // respawn job would gain a duplicate, and the loop never returns. busybox and
    // sysvinit both refuse for the same reason, and this applet now has a /bin entry
    // any root shell can reach — `--dry-run` above is the way to inspect a table.
    if !pid1 {
        return Err("must be run as PID 1 (use --dry-run to check a table)".to_string());
    }
    for problem in &problems {
        log(problem);
    }
    boot(entries)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]
    use super::*;

    fn split_argv(text: &str) -> Result<Vec<String>, String> {
        Ok(split_argv_marked(text)?.into_iter().map(|(w, _)| w).collect())
    }

    fn entry(tty: &str, action: Action, argv: &[&str]) -> Entry {
        Entry {
            tty: tty.to_string(),
            action,
            argv: argv.iter().map(|s| (*s).to_string()).collect(),
            quoted: vec![false; argv.len()],
        }
    }

    #[test]
    fn the_four_field_form_parses_with_runlevels_ignored() {
        let (entries, problems) = parse_inittab(
            "::sysinit:/etc/init.d/rcS\nttyS0:2345:respawn:/bin/cttyhack /bin/sh\n",
        );
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(
            entries,
            vec![
                entry("", Action::SysInit, &["/etc/init.d/rcS"]),
                entry("ttyS0", Action::Respawn, &["/bin/cttyhack", "/bin/sh"]),
            ]
        );
    }

    #[test]
    fn blank_lines_and_comments_are_skipped() {
        let (entries, problems) = parse_inittab("\n  # a comment\n\n::once:/bin/true\n");
        assert!(problems.is_empty());
        assert_eq!(entries.len(), 1);
    }

    /// A colon in the command must not be eaten by field splitting: `PATH=a:b`
    /// and `sh -c 'a; b'` are ordinary inittab commands.
    #[test]
    fn only_the_first_three_colons_are_field_separators() {
        let (entries, problems) = parse_inittab("::once:/bin/sh -c 'a:b; c:d'\n");
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(entries[0].argv, vec!["/bin/sh", "-c", "a:b; c:d"]);
    }

    /// One bad line must not cost the whole table — the machine still boots and
    /// says what it rejected.
    #[test]
    fn a_rejected_line_is_reported_and_the_rest_still_parse() {
        let (entries, problems) = parse_inittab(
            "::ctrlaltdel:/sbin/reboot\ngarbage\n::respawn:\n::once:/bin/sh -c 'oops\n::wait:/bin/true\n",
        );
        assert_eq!(entries, vec![entry("", Action::Wait, &["/bin/true"])]);
        assert_eq!(problems.len(), 4, "{problems:?}");
        assert!(problems[0].contains("unsupported action 'ctrlaltdel'"));
        assert!(problems[1].contains("expected id:runlevels:action:process"));
        assert!(problems[2].contains("no command"));
        assert!(problems[3].contains("unterminated single quote"));
        // Diagnostics name the line they came from, counting from 1.
        assert!(problems[0].starts_with("line 1:"));
        assert!(problems[3].starts_with("line 4:"));
    }

    #[test]
    fn argv_splitting_honours_quotes_and_backslashes() {
        assert_eq!(split_argv("/bin/sh").unwrap(), vec!["/bin/sh"]);
        assert_eq!(
            split_argv("  /bin/sh   -c   true  ").unwrap(),
            vec!["/bin/sh", "-c", "true"]
        );
        assert_eq!(
            split_argv("/bin/sh -c 'echo one two'").unwrap(),
            vec!["/bin/sh", "-c", "echo one two"]
        );
        assert_eq!(
            split_argv(r#"/bin/sh -c "echo \"hi\"""#).unwrap(),
            vec!["/bin/sh", "-c", r#"echo "hi""#]
        );
        // Inside double quotes a backslash escapes only " \ $ ` — before
        // anything else it is literal, as a shell would have left it.
        assert_eq!(split_argv(r#"/bin/echo "a\nb""#).unwrap(), vec!["/bin/echo", r"a\nb"]);
        assert_eq!(split_argv(r#"/bin/echo "a\$b""#).unwrap(), vec!["/bin/echo", "a$b"]);
        assert_eq!(split_argv(r#"/bin/echo "a\\b""#).unwrap(), vec!["/bin/echo", r"a\b"]);
        assert_eq!(split_argv(r"/bin/echo a\ b").unwrap(), vec!["/bin/echo", "a b"]);
        // An empty quoted word is still a word.
        assert_eq!(split_argv("/bin/echo ''").unwrap(), vec!["/bin/echo", ""]);
        assert!(split_argv("").unwrap().is_empty());
        assert!(split_argv("   ").unwrap().is_empty());
    }

    #[test]
    fn unterminated_quoting_is_an_error_not_a_truncated_command() {
        assert!(split_argv("/bin/sh -c 'oops").is_err());
        assert!(split_argv("/bin/sh -c \"oops").is_err());
        assert!(split_argv(r"/bin/sh \").is_err());
    }

    /// The built-in table is what an image with no `/etc/inittab` boots, so it
    /// must parse cleanly and keep a shell on the console.
    #[test]
    fn the_built_in_table_is_valid() {
        let (entries, problems) = parse_inittab(DEFAULT_TABLE);
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[1].action, Action::Respawn);
        assert!(entries[1].argv.first().unwrap().ends_with("cttyhack"));
    }

    #[test]
    fn a_missing_inittab_falls_back_to_the_built_in_table() {
        let (entries, problems) = load("/nonexistent/inittab");
        assert_eq!(entries.len(), 2);
        assert!(problems.first().unwrap().contains("using the built-in table"));
    }

    /// A table that OPENS but yields no jobs is just as unusable as one that
    /// does not: PID 1 would idle forever with no console shell, which is a
    /// machine that is up and cannot be repaired from its own console.
    #[test]
    fn a_table_with_no_usable_entries_falls_back_too() {
        let dir = std::env::temp_dir().join(format!("td-init-tab-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let write = |name: &str, text: &str| {
            let p = dir.join(name);
            std::fs::write(&p, text).unwrap();
            p.to_string_lossy().into_owned()
        };

        for (name, text) in [
            ("empty", ""),
            ("comments", "# nothing but a comment\n\n"),
            // Every line rejected — the shape an inittab written for busybox's
            // richer action set has when td-init reads it.
            ("rejected", "::ctrlaltdel:/bin/reboot\n::shutdown:/bin/umount -a -r\n"),
        ] {
            let (entries, problems) = load(&write(name, text));
            assert_eq!(entries.len(), 2, "{name} did not fall back: {problems:?}");
            assert!(
                problems.iter().any(|p| p.contains("using the built-in table")),
                "{name}: {problems:?}"
            );
        }

        // A table with even ONE usable job is the operator's, and is left alone.
        let (entries, problems) = load(&write("one", "::once:/bin/true\n::bogus:/bin/false\n"));
        assert_eq!(entries.len(), 1);
        assert!(!problems.iter().any(|p| p.contains("built-in")), "{problems:?}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `--dry-run` is the validation entry point: it exits 1 exactly when the
    /// table had a rejected line.
    #[test]
    fn dry_run_reports_a_bad_table_through_its_exit_code() {
        let (good, none) = parse_inittab("::once:/bin/true\n");
        assert_eq!(dry_run(&good, &none), Ok(0));
        let (some, problems) = parse_inittab("nonsense\n");
        assert_eq!(dry_run(&some, &problems), Ok(1));
        // A table that yields no jobs is unusable even with nothing rejected, and
        // `load` quietly substitutes the built-in one — so the validator has to
        // fail on it too, or `-f /dev/null` reads as a clean bill of health.
        let (fell_back, why) = load("/nonexistent/inittab");
        assert_eq!(dry_run(&fell_back, &why), Ok(1));
    }

    /// Supervising from a process that is not PID 1 re-runs sysinit and duplicates
    /// every respawn job, so `run` refuses — but `--dry-run` still has to work there,
    /// since a test process, a shell and the image's own greeter probe are all not
    /// PID 1. The test process is not PID 1, so this exercises the real refusal.
    #[test]
    fn supervising_is_refused_anywhere_but_pid_1_while_dry_run_still_works() {
        assert_ne!(std::process::id(), 1, "this test relies on not being PID 1");
        let err = run(&[]).unwrap_err();
        assert!(
            err.contains("must be run as PID 1"),
            "expected a PID-1 refusal, got {err:?}"
        );
        // …and the validation path is unaffected by that refusal.
        assert_eq!(run(&["--dry-run".into(), "-f".into(), "/nonexistent".into()]), Ok(1));
    }

    /// The jobs printed may not be the operator's at all: `load` substitutes the
    /// built-in table for one it could not use. The reason therefore rides
    /// stdout WITH the jobs and ahead of them — logged to stderr it is a caption
    /// on a stream nobody reading the report through a pipe is looking at.
    #[test]
    fn the_report_says_up_front_when_the_jobs_are_not_the_operators() {
        let (entries, problems) = load("/nonexistent/inittab");
        let text = report(&entries, &problems);
        let first = text.lines().next().unwrap_or("");
        assert!(first.starts_with("# "), "not a leading note: {text}");
        assert!(first.contains("using the built-in table"), "{text}");
        // ...and the jobs are still there, after it.
        assert!(text.contains("respawn"), "{text}");
        let at = |needle: &str| text.match_indices(needle).next().map(|(i, _)| i).unwrap();
        assert!(at("built-in table") < at("respawn"), "{text}");
        // A table with nothing to say about it is jobs alone.
        let (good, none) = parse_inittab("::once:/bin/true\n");
        assert_eq!(report(&good, &none), "once - /bin/true\n");
    }

    fn args(xs: &[&str]) -> Vec<String> {
        xs.iter().map(|s| (*s).to_string()).collect()
    }

    /// busybox init hands a process field containing metacharacters to `sh -c`;
    /// td's execs it directly. `--dry-run` is sold as the inittab validator, so
    /// it is where that difference has to be visible — quietly passing `>` to
    /// the program as an argument is the silent-breakage case at the cutover.
    #[test]
    fn a_word_only_a_shell_would_act_on_is_called_out() {
        // Nothing quoted, which is the default these cases exercise.
        fn sw(argv: &[String]) -> Option<&String> {
            shell_word(argv, &[])
        }
        assert_eq!(sw(&args(&["/bin/foo", ">", "/dev/log"])), Some(&">".to_string()));
        assert_eq!(sw(&args(&["/bin/foo", "|", "logger"])), Some(&"|".to_string()));
        assert_eq!(sw(&args(&["/bin/foo", "$HOME"])), Some(&"$HOME".to_string()));
        // The compact forms, which the splitter leaves as ONE word each. These
        // are how a real table writes a redirect or a pipe, and requiring the
        // whole word to be operators missed every one of them.
        for compact in [">/dev/log", "a|b", "x&&y", "*.conf", "cmd;next", "f?.log"] {
            assert_eq!(
                sw(&args(&["/bin/foo", compact])),
                Some(&compact.to_string()),
                "missed {compact}"
            );
        }
        // The quoted forms a real inittab carries are NOT shell words: they have
        // already been grouped into one argument by the parser.
        assert_eq!(sw(&args(&["/bin/sh", "-c", "exit 0"])), None);
        // ...and a script handed to a shell is that shell's to expand, so noting
        // it would be plainly false. This is the commonest real inittab line
        // there is, and the `exit 0` case above is the one shape of it that
        // dodges the question.
        assert_eq!(sw(&args(&["/bin/sh", "-c", "echo $HOME >/dev/log"])), None);
        assert_eq!(sw(&args(&["/bin/sh", "-c", "exec `which getty`"])), None);
        // Before the `-c` the words are still init's to pass through literally.
        assert_eq!(sw(&args(&["$SHELL", "-c", "true"])), Some(&"$SHELL".to_string()));
        // ...and `-c` only means "a script follows" to a SHELL. To most programs
        // it is a config flag, and the redirect after one is still handed over
        // literally — exactly what the note exists to say.
        assert_eq!(
            sw(&args(&["/bin/mytool", "-c", "cfg", ">", "/dev/log"])),
            Some(&">".to_string())
        );
        assert_eq!(sw(&args(&["/bin/bash", "-c", "x >/dev/log"])), None);
        // A wrapper runs the shell behind it, so the note must not claim
        // otherwise. cttyhack is the one the BUILT-IN table itself uses.
        for wrapped in [
            ["/bin/cttyhack", "/bin/sh", "-c", "echo $HOME"],
            ["/sbin/setsid", "/bin/sh", "-c", "echo $HOME"],
            ["/usr/bin/env", "/bin/sh", "-c", "echo $HOME"],
            ["/bin/nohup", "/bin/dash", "-c", "echo $HOME"],
        ] {
            assert_eq!(sw(&args(&wrapped)), None, "{wrapped:?}");
        }
        // Quoting is provenance the note must respect: `-t 'init[1]'` reaches
        // the program identically under busybox's shell and under td's exec.
        let (entries, _) = parse_inittab("::once:/bin/logger -t 'init[1]' hello\n");
        assert_eq!(shell_word(&entries[0].argv, &entries[0].quoted), None);
        let (entries, _) = parse_inittab("::once:/bin/echo '$HOME'\n");
        assert_eq!(shell_word(&entries[0].argv, &entries[0].quoted), None);
        // DOUBLE quotes are not that: a shell expands `$` and a backtick inside
        // them, so `"$HOME"` really does reach the program differently under
        // busybox's shell than under td's literal exec, and suppressing the
        // note for it hid the divergence the note exists to report. Escaping
        // the `$` makes it literal again, in either kind of quotes or none.
        for expanded in ["\"$HOME\"", "\"`id`\"", "\"a $HOME b\""] {
            let (entries, _) = parse_inittab(&format!("::once:/bin/echo {expanded}\n"));
            assert!(
                shell_word(&entries[0].argv, &entries[0].quoted).is_some(),
                "{expanded}"
            );
        }
        for literal in ["\"\\$HOME\"", "\\$HOME", "'`id`'", "\"a b\""] {
            let (entries, _) = parse_inittab(&format!("::once:/bin/echo {literal}\n"));
            assert_eq!(
                shell_word(&entries[0].argv, &entries[0].quoted),
                None,
                "{literal}"
            );
        }
        // A quoted FRAGMENT does not launder the rest of its word.
        let (entries, _) = parse_inittab("::once:/bin/echo $HOME'x'\n");
        assert!(shell_word(&entries[0].argv, &entries[0].quoted).is_some());
        // ...and an UNquoted one still is.
        let (entries, _) = parse_inittab("::once:/bin/echo $HOME\n");
        assert_eq!(
            shell_word(&entries[0].argv, &entries[0].quoted),
            Some(&"$HOME".to_string())
        );
        // chroot's NEWROOT operand, and su, which always execs a login shell.
        assert_eq!(
            sw(&args(&["/usr/sbin/chroot", "/newroot", "/bin/sh", "-c", "echo $HOME"])),
            None
        );
        assert_eq!(sw(&args(&["/bin/su", "-c", "echo $HOME", "root"])), None);
        // A wrapper's own flags and assignments sit before the program it runs.
        assert_eq!(
            sw(&args(&["/usr/bin/env", "FOO=bar", "/bin/sh", "-c", "echo $HOME"])),
            None
        );
        assert_eq!(
            sw(&args(&["/sbin/setsid", "-w", "/bin/sh", "-c", "echo $HOME"])),
            None
        );
        // ...but a wrapper around a non-shell is still literal.
        assert_eq!(
            sw(&args(&["/sbin/setsid", "/bin/mytool", "$HOME"])),
            Some(&"$HOME".to_string())
        );
        // Clustered short flags: `-xc` is the same call as `-x -c`. So is
        // `-cx` — the option-argument follows the whole cluster, not the `c`.
        assert_eq!(sw(&args(&["/bin/sh", "-xc", "echo $HOME"])), None);
        assert_eq!(sw(&args(&["/bin/sh", "-cx", "echo $HOME"])), None);
        // td's own shell counts; a long option ending in c does not.
        assert_eq!(sw(&args(&["/bin/td-sh", "-c", "echo $HOME"])), None);
        assert_eq!(
            sw(&args(&["/bin/sh", "--noprofilec", "$HOME"])),
            Some(&"$HOME".to_string())
        );
        // A second `-c` is an argument TO the script, not another introducer.
        assert_eq!(
            sw(&args(&["/bin/sh", "-c", "true", "-c", "$HOME"])),
            Some(&"$HOME".to_string())
        );
        // Flags may precede the `-c`, and the shell is decided by argv[0]: a
        // `sh` sitting in argv[1] of something else is that program's argument.
        assert_eq!(sw(&args(&["/bin/sh", "-x", "-c", "echo $HOME"])), None);
        assert_eq!(
            sw(&args(&["/bin/mytool", "sh", "-c", "echo $HOME"])),
            Some(&"echo $HOME".to_string())
        );
        // Past the script the words are `$0 $1 ...` to it, NOT more script: this
        // redirect is one init passes through literally, so it earns the note.
        assert_eq!(
            sw(&args(&["/bin/sh", "-c", "true", ">", "/dev/log"])),
            Some(&">".to_string())
        );
        // busybox's `-c` is an applet's, not a shell's — unless the applet IS a
        // shell, which its multicall spells in argv[1].
        assert_eq!(
            sw(&args(&["/bin/busybox", "-c", "echo $HOME"])),
            Some(&"echo $HOME".to_string())
        );
        assert_eq!(sw(&args(&["/bin/busybox", "sh", "-c", "echo $HOME"])), None);
        // A leading assignment is a shell construct too, and the one that fails
        // hardest: init execs "FOO=bar" as the program and never runs getty.
        assert_eq!(
            sw(&args(&["FOO=bar", "/sbin/getty", "ttyS0"])),
            Some(&"FOO=bar".to_string())
        );
        // Globs a shell would expand, in the two spellings the operator set above
        // did not already cover.
        assert_eq!(
            sw(&args(&["/bin/foo", "[ab].conf"])),
            Some(&"[ab].conf".to_string())
        );
        assert_eq!(
            sw(&args(&["~/bin/tool"])),
            Some(&"~/bin/tool".to_string())
        );
        assert_eq!(sw(&args(&["/sbin/getty", "-L", "115200", "ttyS0"])), None);
        assert_eq!(sw(&args(&["/bin/foo", "--flag=a,b"])), None);

        // It is a note, not a rejection: the entry still runs, and dry-run's
        // exit status still reports only rejected lines.
        let (entries, problems) = parse_inittab("::once:/bin/foo > /dev/log\n");
        assert!(problems.is_empty());
        assert_eq!(dry_run(&entries, &problems), Ok(0));
    }

    /// Off PID 1, an argument fault is an ordinary error. A mistyped `--dry-run`
    /// especially: silently starting a real supervision loop would hang whoever
    /// ran it instead of failing.
    #[test]
    fn an_argument_fault_is_fatal_anywhere_but_pid_1() {
        assert!(parse_args(&args(&["-f"]), false).is_err());
        assert!(parse_args(&args(&["--dryrun"]), false).is_err());
        assert!(parse_args(&args(&["-x"]), false).is_err());
        // A bare word is not an option and is tolerated everywhere: the kernel
        // passes init leftover command-line words.
        assert!(parse_args(&args(&["single"]), false).is_ok());
        let (opts, _) = parse_args(&args(&["--dry-run", "-f", "/tmp/t"]), false).unwrap();
        assert!(opts.dry);
        assert_eq!(opts.path, "/tmp/t");
    }

    /// As PID 1 there is no fatal argument: the kernel panics the moment PID 1
    /// exits, so every one of the faults above is logged and the boot continues.
    /// `--dry-run` is a request to EXIT, so it is refused rather than obeyed.
    #[test]
    fn pid_1_never_exits_over_an_argument() {
        for bad in [vec!["-f"], vec!["--dryrun"], vec!["-x"], vec!["single"]] {
            let (opts, notes) = parse_args(&args(&bad), true).unwrap();
            assert!(!opts.dry, "{bad:?}");
            assert!(!notes.is_empty(), "{bad:?} should be reported, not silent");
        }
        // A missing -f operand keeps the default table rather than aborting.
        let (opts, _) = parse_args(&args(&["-f"]), true).unwrap();
        assert_eq!(opts.path, DEFAULT_PATH);
        // A trailing bare -f keeps the path ALREADY given, so the note has to
        // name that one — reporting the default while using another table sends
        // whoever is reading the console after the wrong file.
        let (opts, notes) = parse_args(&args(&["-f", "/custom/tab", "-f"]), true).unwrap();
        assert_eq!(opts.path, "/custom/tab");
        assert!(notes.iter().any(|n| n.contains("using /custom/tab")), "{notes:?}");
        // ...and an explicit --dry-run is refused with a note saying why.
        let (opts, notes) = parse_args(&args(&["--dry-run"]), true).unwrap();
        assert!(!opts.dry);
        assert!(notes.iter().any(|n| n.contains("PID 1 must not exit")), "{notes:?}");
    }

    /// Respawn accounting: a job's own pid clears its slot, and a brief life
    /// arms the throttle. An orphan's pid must leave every slot untouched.
    #[test]
    fn reaping_clears_the_right_job_and_throttles_a_brief_one() {
        let mut jobs = vec![
            Respawn {
                entry: entry("", Action::Respawn, &["/bin/sh"]),
                pid: Some(11),
                started: Some(Instant::now()),
                retry_at: None,
                fast_failures: 0,
            },
            Respawn {
                entry: entry("", Action::Respawn, &["/bin/getty"]),
                pid: Some(12),
                started: Some(Instant::now()),
                retry_at: None,
                fast_failures: 0,
            },
        ];
        let mut once = Vec::new();

        collect(&mut jobs, &mut once, 99, 0);
        assert_eq!(jobs[0].pid, Some(11), "an orphan must not clear a job");
        assert_eq!(jobs[1].pid, Some(12));

        collect(&mut jobs, &mut once, 12, 0);
        assert_eq!(jobs[0].pid, Some(11));
        assert_eq!(jobs[1].pid, None);
        // It died immediately, so the restart is throttled rather than instant.
        assert!(jobs[1].retry_at.is_some());
    }

    /// A job that ran long enough is restarted with no delay.
    #[test]
    fn a_long_lived_job_restarts_without_a_throttle() {
        let mut jobs = vec![Respawn {
            entry: entry("", Action::Respawn, &["/bin/sh"]),
            pid: Some(11),
            started: Some(Instant::now() - MIN_UPTIME - Duration::from_secs(1)),
            retry_at: None,
            fast_failures: 3,
        }];
        collect(&mut jobs, &mut Vec::new(), 11, 0);
        assert_eq!(jobs[0].pid, None);
        assert!(jobs[0].retry_at.is_none());
        // ...and it clears the escalation the earlier failures had built up.
        assert_eq!(jobs[0].fast_failures, 0);
    }

    /// A job that can never work must not scroll the console forever: the first
    /// few failures are reported once a second, then the entry is held for five
    /// minutes and says so exactly once.
    #[test]
    fn a_hopeless_job_escalates_to_a_quiet_hold() {
        for n in 1..=RESPAWN_BURST {
            assert_eq!(throttle(n), (RESPAWN_DELAY, true), "failure {n}");
        }
        assert_eq!(throttle(RESPAWN_BURST + 1), (RESPAWN_HOLD, true));
        for n in [RESPAWN_BURST + 2, RESPAWN_BURST + 50, u32::MAX] {
            assert_eq!(throttle(n), (RESPAWN_HOLD, false), "failure {n}");
        }
    }

    /// The counter drives that escalation, so it has to survive repeated brief
    /// exits of the same job rather than resetting on each one.
    #[test]
    fn consecutive_brief_exits_accumulate() {
        let mut jobs = vec![Respawn {
            entry: entry("", Action::Respawn, &["/bin/sh"]),
            pid: Some(11),
            started: Some(Instant::now()),
            retry_at: None,
            fast_failures: 0,
        }];
        for expected in 1..=3 {
            jobs[0].pid = Some(11);
            jobs[0].started = Some(Instant::now());
            collect(&mut jobs, &mut Vec::new(), 11, 0);
            assert_eq!(jobs[0].fast_failures, expected);
        }
    }

    /// A `once` job is remembered only until it is reaped, so the table cannot
    /// grow without bound on a long-running system.
    #[test]
    fn a_reaped_once_job_is_forgotten() {
        let mut once = vec![(21, "/bin/one".to_string()), (22, "/bin/two".to_string())];
        collect(&mut [], &mut once, 21, 0);
        assert_eq!(once, vec![(22, "/bin/two".to_string())]);
        collect(&mut [], &mut once, 99, 0);
        assert_eq!(once.len(), 1, "an orphan must not disturb the once table");
    }
}

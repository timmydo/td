//! Which long check this worktree is running, so it can be stopped by NAME.
//!
//! The gap this fills is not convenience. Every worktree invokes the same
//! relative path, so `ps` shows several identical `./target/release/td-builder
//! ready` lines and a command line carries no cwd: there is no pattern that
//! selects one worktree's run. An agent that reaches for `pkill -f` to end its
//! own build therefore ends everyone's, and has no way to know it did — the
//! kill audit records the signals td-builder SENDS, not the one it received,
//! so the victim leaves behind only its swept gates and no cause.
//!
//! That is what happened, and `check-host-stop` already carries the lesson in
//! its own comment: giving an operator something to ASK is what removed the
//! old signal, because a stop that does not exist is what sends them to
//! `kill`. So a run writes down who it is, and `stop` reads that.
//!
//! Three properties do the work:
//!
//! * The record lives in the worktree that ran, so a `stop` cannot name
//!   another worktree's run: it never sees its record. The scope follows the
//!   worktree you are standing in rather than a pattern you have to get
//!   right. "Standing in" is `git rev-parse --show-toplevel`, so any
//!   subdirectory of the worktree will do, but a `stop` run from somewhere
//!   else entirely finds a different set — which is why it names the root it
//!   looked in.
//! * The record carries the pid's `starttime`, so a pid the kernel has since
//!   recycled is recognised as a different process and left alone. A bare
//!   pidfile would reintroduce the original bug in a smaller window.
//! * It carries the boot id too, because that pair is unique within a boot
//!   and repeatable across one, and these records outlive a reboot.

use crate::sys::{self, KillTarget};
use std::io;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

/// Under the per-worktree build cache, beside the other run state.
const DIR: &str = ".td-build-cache/runs";

/// Where process state is read from.
const PROC: &str = "/proc";

/// A value that changes on every boot, so a record can say which boot's pid
/// space it is talking about.
const BOOT_ID: &str = "/proc/sys/kernel/random/boot_id";

/// This boot's identity. An error, never a guess: `stop_all` turns it into a
/// problem and stops nothing, and `Guard::record` declines to record.
fn boot_id() -> io::Result<String> {
    Ok(std::fs::read_to_string(BOOT_ID)?.trim().to_string())
}

/// How long `stop` waits to see a signalled run actually go. A signal is a
/// request, and reporting "stopped" for a process still holding the check host
/// would break the one workflow this command exists for: `stop` in front of a
/// start.
const CONFIRM: Duration = Duration::from_secs(5);

/// How often it looks while waiting.
const POLL: Duration = Duration::from_millis(50);

/// `ESRCH`. `/proc/<pid>/stat` is a seq_file: a process that exits between the
/// `open` and the `read` fails the READ with ESRCH rather than the open with
/// ENOENT, and std maps errno 3 to no named `ErrorKind`, so it arrives as
/// `Uncategorized` and a `NotFound` match alone misses it. `kill(2)` answers
/// the same way for a pid that has just been reaped. This is
/// `td-svc/src/procfs.rs`'s constant and rule; see `is_gone`.
const ESRCH: i32 = 3;

/// Does this error mean "that process is gone"? ENOENT from an open and ESRCH
/// from a read or a signal are the two ways the kernel says so. Everything
/// else — EACCES, EIO — is a fault and must propagate, because reporting a
/// live run as finished is the fail-open shape this module exists to avoid.
///
/// An unmounted `/proc` is the one case this CANNOT see: every stat there is
/// ENOENT, so this rule calls it gone rather than a fault. That is why
/// `starttime_of_in` looks for `/proc/self/stat` before believing an absence.
/// The look is not what saves `stop`, which has an outer guard in `boot_id` —
/// also a `/proc` read, and taken first. It keeps `starttime_of` correct in
/// its own right instead of correct only by way of its callers.
fn is_gone(e: &io::Error) -> bool {
    e.kind() == io::ErrorKind::NotFound || e.raw_os_error() == Some(ESRCH)
}

/// A live run, as its record describes it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Record {
    pub pid: i64,
    /// Field 22 of `/proc/<pid>/stat`, in clock ticks since boot. Constant for
    /// a process and not reused with its pid, which is the whole point.
    pub starttime: u64,
    /// Which run this is — see `long_run_verb` for the set.
    pub verb: String,
    /// The boot this pid and starttime belong to. Both repeat across reboots,
    /// and these records outlive one.
    pub boot: String,
}

impl Record {
    fn render(&self) -> String {
        format!(
            "pid {}\nstarttime {}\nverb {}\nboot {}\n",
            self.pid, self.starttime, self.verb, self.boot
        )
    }

    /// `None` for anything this cannot read as a record. The caller treats
    /// that as a fault rather than an absence — see `stop`.
    fn parse(text: &str) -> Option<Record> {
        let mut pid = None;
        let mut starttime = None;
        let mut verb = None;
        let mut boot = None;
        let (mut saw_pid, mut saw_starttime, mut saw_verb) = (false, false, false);
        let mut saw_boot = false;
        for line in text.lines() {
            let (key, value) = line.split_once(' ')?;
            // A repeated key is corruption, not an override. "Repeated"
            // means the key appeared twice, not that its slot is still unset:
            // guarding on the value would let `pid x` followed by `pid 12`
            // through, which is exactly the "pick a pid out of a file we did
            // not write" this refuses.
            match key {
                "pid" if !saw_pid => {
                    saw_pid = true;
                    pid = value.parse().ok();
                }
                "starttime" if !saw_starttime => {
                    saw_starttime = true;
                    starttime = value.parse().ok();
                }
                "verb" if !saw_verb => {
                    saw_verb = true;
                    verb = Some(value.to_string());
                }
                "boot" if !saw_boot => {
                    saw_boot = true;
                    boot = Some(value.to_string());
                }
                _ => return None,
            }
        }
        let record = Record {
            pid: pid?,
            starttime: starttime?,
            verb: verb?,
            boot: boot?,
        };
        // 0 and negatives name a process GROUP or every process the caller may
        // signal, and a value past `pid_t` wraps negative on the way to the
        // syscall. `sys::kill_pid` already refuses all three, so this is the
        // second of two locks rather than the only one — but a record holding
        // one was never written by us, and the cheapest place to say so is
        // here, before anything treats it as a process.
        if record.pid <= 0 || i32::try_from(record.pid).is_err() {
            return None;
        }
        Some(record)
    }
}

/// `starttime` for a pid: `None` when the process is gone, a zombie
/// included.
///
/// An unreadable or unparseable `stat` is an ERROR and never an absence. This
/// is `td-svc/src/procfs.rs`'s rule, mirrored rather than shared because
/// `td-svc` is a standalone crate and this one is in the dependency-free
/// workspace. Reporting "gone" for a process we merely failed to inspect
/// would retire a live run's record and exit 0 having signalled nothing,
/// which is the fail-open shape that rule exists to forbid.
pub fn starttime_of(pid: i64) -> io::Result<Option<u64>> {
    starttime_of_in(Path::new(PROC), pid)
}

/// `starttime_of` against a given `/proc`, so the unmounted case is testable.
///
/// ENOENT means "that process is gone" only if `/proc` is there to say so.
/// Without it every `stat` is ENOENT and every record would read as a run
/// that had ended — the fail-open shape the ENOENT rule exists to prevent,
/// arrived at through the rule itself. `td-svc/src/procfs.rs` keeps an
/// `is_mounted` for exactly this.
///
/// This is defence in depth and not the outer guard: `stop_all` reads
/// `boot_id` from `/proc` before it checks a single record against `/proc`,
/// so an unmounted `/proc` already fails that command closed, with a problem
/// and a non-zero exit. What the look buys is `starttime_of`'s own contract — `None` means
/// gone — holding for every caller rather than for the one that happens to
/// read `/proc` first. It costs nothing on the common path, because it is
/// only taken once the answer is already "gone".
fn starttime_of_in(proc_root: &Path, pid: i64) -> io::Result<Option<u64>> {
    let answer = starttime_at(&proc_root.join(pid.to_string()).join("stat"))?;
    if answer.is_none() && !proc_root.join("self").join("stat").exists() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "{} is not mounted: a finished run and an unreadable one look the same",
                proc_root.display()
            ),
        ));
    }
    Ok(answer)
}

/// The half of `starttime_of` that takes a path, so a test can hand it a
/// `stat` the kernel would never write. Without this seam the fail-closed
/// branch below is unreachable from a test and the rule above is only a
/// comment.
fn starttime_at(path: &Path) -> io::Result<Option<u64>> {
    match std::fs::read_to_string(path) {
        Ok(text) => match parse_stat(&text) {
            // A zombie has stopped being a process in any useful sense but
            // keeps its stat entry, its state and its starttime until someone
            // reaps it. Reading it as alive makes `stop` signal a corpse, wait
            // out the whole confirmation window, and then report a run that
            // "has not exited" with a non-zero exit — for a run that ended.
            // `td-svc/src/procfs.rs` carries the same rule for the same
            // reason; this is the second half of the mirror.
            Some((ZOMBIE, _)) => Ok(None),
            Some((_, starttime)) => Ok(Some(starttime)),
            None => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{}: unparseable", path.display()),
            )),
        },
        Err(e) if is_gone(&e) => Ok(None),
        Err(e) => Err(e),
    }
}

/// The state a reaped-but-unwaited process is left in.
const ZOMBIE: char = 'Z';

/// Fields 3 (state) and 22 (starttime) of a `/proc/<pid>/stat` line.
///
/// Split after the LAST `)`, never on whitespace from the start: field 2 is the
/// executable name, in parentheses, and it may contain both spaces and
/// parentheses — `(td builder)` and `(x) y` are legal comms, and counting
/// fields from the left mis-parses either.
fn parse_stat(text: &str) -> Option<(char, u64)> {
    let close = text.rfind(')')?;
    let tail = text.get(close + 1..)?;
    // The tail begins at field 3, so state is its first field and field 22 is
    // index 19 — index 18 of what is left once the state is taken.
    let mut fields = tail.split_whitespace();
    let state = fields.next()?.chars().next()?;
    let starttime = fields.nth(18)?.parse().ok()?;
    Some((state, starttime))
}

/// Whether the pid still names the process the record was written for.
fn is_same_process(record: &Record) -> io::Result<bool> {
    Ok(starttime_of(record.pid)? == Some(record.starttime))
}

fn dir(root: &Path) -> PathBuf {
    root.join(DIR)
}

fn path_for(root: &Path, pid: i64) -> PathBuf {
    dir(root).join(format!("{pid}.run"))
}

/// Where a record is written before it is published under `path_for`.
///
/// It must not end in `.run`: `records` selects on that extension, and a
/// half-written file wearing it would be read as a corrupt record.
fn staging_path(root: &Path, pid: i64) -> PathBuf {
    dir(root).join(format!(".{pid}.run.tmp"))
}

/// A run's record, removed when the run ends.
///
/// Removal is best-effort by construction: a `Drop` cannot run for a process
/// that is SIGKILLed, nor for one taking SIGTERM's default disposition, and
/// those are precisely the processes an operator most wants to have left a
/// trace. `stop` leans on the same tolerance deliberately, leaving the record
/// of a run it has signalled. So the reader must never assume a record exists
/// only while its run does — `is_same_process` is what makes that safe.
pub struct Guard {
    path: PathBuf,
}

impl Guard {
    /// Write the record for THIS process. `None` when it cannot be written:
    /// a run that cannot be recorded still runs, because refusing to build
    /// because we could not write a note would be a worse trade than being
    /// unstoppable by name.
    pub fn record(root: &Path, verb: &str) -> Option<Guard> {
        let pid = i64::from(std::process::id());
        let starttime = starttime_of(pid).ok().flatten()?;
        let record = Record {
            pid,
            starttime,
            verb: verb.to_string(),
            boot: boot_id().ok()?,
        };
        std::fs::create_dir_all(dir(root)).ok()?;
        let path = path_for(root, pid);
        // Write-then-rename, because `fs::write` publishes the name before the
        // bytes: a reader arriving in between sees an empty file, which parses
        // as nothing, which `records` reports as a problem — so a run merely
        // STARTING would make a concurrent `stop` exit non-zero over a record
        // nobody can account for. The temporary carries the pid too, so two
        // starts cannot share one.
        let staging = staging_path(root, pid);
        std::fs::write(&staging, record.render()).ok()?;
        if std::fs::rename(&staging, &path).is_err() {
            let _ = std::fs::remove_file(&staging);
            return None;
        }
        Some(Guard { path })
    }
}

/// The verbs a `stop` is for, recorded wherever they run. See
/// `should_record` for which process records, and `long_run_verb` for which
/// invocations count.
pub fn record_if_long_run(args: &[String]) -> Option<Guard> {
    let hosted = std::env::var_os(crate::check_memory::HOST_CHILD_ENV).is_some();
    let verb = should_record(args, hosted)?;
    let root = crate::affected::resolve_root();
    let guard = Guard::record(&root, verb);
    if guard.is_none() {
        // The trade is deliberate — a run that cannot write a note still runs
        // — but silence would leave an operator with a long build that `stop`
        // later reports as "nothing to stop", exit 0, having never said why.
        eprintln!(
            "td-builder: warning: this {verb} cannot be stopped by name: no run record under {}/{DIR}",
            root.display()
        );
    }
    guard
}

/// The whole record-or-not decision, with the environment passed in so both
/// halves can be tested — including the hosted one, whose absence is what
/// made the first attempt record a useless pid 1.
fn should_record(args: &[String], hosted: bool) -> Option<&str> {
    // Inside the check host a pid namespace makes this process pid 1: a number
    // that names nothing from outside and is identical for every worktree's
    // run. The CLIENT that submitted the request is the process an operator
    // can signal, and the host already treats its going away as cancelling the
    // request, so the client is the copy of this code that records.
    if hosted {
        return None;
    }
    long_run_verb(args)
}

/// Which verb, if any, this argv is a stoppable run of.
///
/// DERIVED from the forwarding predicate rather than restated beside it. The
/// check host already decides which invocations are long enough to be worth
/// hosting — it excludes `ready --record-only`, a bare `affected-checks`, the
/// `gate-run` listings and every `--help` — and a second hand-written copy of
/// that judgement would be a thing to keep in step. A long verb added there
/// and forgotten here would leave that run nameable only by `pkill`, which is
/// the failure this module exists to remove, and no test over a list of cases
/// someone thought of would catch it.
///
/// `hosted` is passed as false deliberately: whether THIS process should be
/// the one recording is `should_record`'s question, already answered above.
///
/// The exclusions below are the whole difference, and they are about KIND
/// rather than duration: a control message is not a run, so `stop` must not
/// claim to have stopped one.
fn long_run_verb(args: &[String]) -> Option<&str> {
    let verb = args.first().map(String::as_str)?;
    if matches!(verb, "daemon-request") {
        return None;
    }
    crate::check_host::should_forward_with_host_state(args, false).then_some(verb)
}

impl Drop for Guard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Every record in this worktree, with the path it came from.
///
/// A file that cannot be read or parsed comes back as an `Err` FOR THAT FILE,
/// never as a failure of the whole listing. Silently skipping it would report
/// "nothing to stop" for a run that is still going; but refusing to return the
/// others would turn one unreadable byte into a worktree whose live runs can
/// no longer be stopped at all — and an operator who cannot stop a run is an
/// operator reaching for `pkill`, which is the thing this module exists to
/// remove. So: report it, stop everything else, and exit non-zero.
///
/// The outer `Err` is reserved for the directory itself being unreadable,
/// where there is no per-file answer to give.
fn records(root: &Path) -> io::Result<Vec<(PathBuf, Result<Record, String>)>> {
    let mut out = Vec::new();
    let entries = match std::fs::read_dir(dir(root)) {
        Ok(entries) => entries,
        // No directory is the ordinary case: nothing has ever run here.
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(e),
    };
    for entry in entries {
        // Not `?`: throwing the whole listing away here would discard every
        // record already collected and stop nothing, which is the shape this
        // function's contract above exists to forbid — reached, otherwise,
        // through a `getdents` that fails partway on EIO or a stale NFS
        // handle.
        let entry = match entry {
            Ok(entry) => entry,
            Err(e) => {
                out.push((
                    dir(root),
                    Err(format!("{}: reading the directory: {e}", dir(root).display())),
                ));
                continue;
            }
        };
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("run") {
            continue;
        }
        // A directory named `x.run` would fail the read with EISDIR and be
        // reported as an unreadable record, making `stop` exit non-zero over
        // something that is not a record at all. Only a regular file is one.
        //
        // `file_type` is free when readdir supplies `d_type`, but falls back to
        // an `lstat` where it does not, and that lstat races a finishing run
        // exactly as the read below does. Same answer in both places.
        match entry.file_type() {
            Ok(kind) if kind.is_file() => {}
            Ok(_) => continue,
            Err(e) if is_gone(&e) => continue,
            Err(e) => {
                out.push((path.clone(), Err(format!("{}: {e}", path.display()))));
                continue;
            }
        }
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            // The run finished and its guard took the record away between the
            // directory read and this one. That is the ordinary end of a run,
            // not the unreadable-record problem below: an absent file says the
            // run is over, where an unparseable one says we do not know.
            Err(e) if is_gone(&e) => continue,
            Err(e) => {
                out.push((path.clone(), Err(format!("{}: {e}", path.display()))));
                continue;
            }
        };
        let parsed = Record::parse(&text).ok_or_else(|| {
            format!(
                "{}: not a run record, or one a different version wrote; if no run owns it, delete it",
                path.display()
            )
        });
        out.push((path, parsed));
    }
    out.sort_by(|(a, _), (b, _)| a.cmp(b));
    Ok(out)
}

/// What one `stop` did, in the words it will use.
#[derive(Default, Debug)]
struct Tally {
    /// One line per run seen to stop.
    stopped: Vec<String>,
    /// Records for runs that had already ended, retired here.
    stale: usize,
    /// Everything that went wrong. Non-empty means a non-zero exit, and each
    /// entry is printed: a `stop` that could not account for something must
    /// say which something, and name the file, because an operator who has to
    /// clean up by hand needs the path and not just the pid.
    problems: Vec<String>,
}

/// Is this record's run still there to be signalled? Retires it if not.
fn still_running(path: &Path, record: &Record, boot: &str) -> io::Result<bool> {
    // A record from a previous boot names nothing. `starttime` is measured in
    // ticks since boot, so the pid/starttime pair is unique WITHIN a boot and
    // repeatable across them: these records outlive a reboot in the build
    // cache, and without this a leftover could match an unrelated process and
    // send it a SIGTERM. Checked before the pid is looked at at all.
    if record.boot != boot || !is_same_process(record)? {
        let _ = std::fs::remove_file(path);
        return Ok(false);
    }
    Ok(true)
}

/// Watch signalled runs go, round-robin, until they are gone or `deadline`.
///
/// Every pending run is polled on every pass, so one that refuses SIGTERM
/// cannot eat the window its neighbour needed: confirming them one after
/// another would leave the second with a single look once the first had run
/// the clock down. A signal is a request — a process in `D` state does not
/// take it until it leaves, and one that traps SIGTERM may take its time or
/// refuse — so reporting success without watching would break `stop && ready`,
/// which is the workflow this command is for.
fn confirm_all(mut pending: Vec<(PathBuf, Record)>, deadline: Instant, tally: &mut Tally) {
    while !pending.is_empty() {
        let mut still = Vec::new();
        for (path, record) in pending {
            match is_same_process(&record) {
                Ok(false) => {
                    let _ = std::fs::remove_file(&path);
                    tally
                        .stopped
                        .push(format!("stopped {} (pid {})", record.verb, record.pid));
                }
                Ok(true) => still.push((path, record)),
                Err(e) => tally.problems.push(format!(
                    "{}: pid {} ({}): {e}",
                    path.display(),
                    record.pid,
                    record.verb
                )),
            }
        }
        pending = still;
        if pending.is_empty() || Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(POLL);
    }
    for (path, record) in pending {
        // The record STAYS, so a second `stop` can re-signal it. Deleting it
        // here would answer that second call with "nothing to stop" while the
        // run continued — the fail-open shape this module is supposed to
        // forbid. It also covers what a guard cannot: SIGTERM taken by its
        // default disposition runs no destructor, so on the ordinary path the
        // guard never gets to remove anything.
        tally.problems.push(format!(
            "{}: pid {} ({}) has not exited; its record is kept, so `stop` again to re-signal",
            path.display(),
            record.pid,
            record.verb
        ));
    }
}

/// Stop every run recorded in `root`, and keep going past the ones that fail.
///
/// SIGTERM to the recorded pid, and nothing else needs killing: a recorded run
/// is by construction one the check host would take (`long_run_verb` derives
/// from `should_forward`), so the client owns no build tree of its own. The
/// hosted tree is the host's, armed to die with the HOST rather than with this
/// client, and it comes down on the host's own client-went-away cancellation
/// once this process is gone. That teardown is asynchronous and this command
/// does not wait for it: what `stop` confirms is that the process it was able
/// to name has ended.
///
/// The signal goes through `sys::kill_all_recorded`, which reads every command
/// line, sends every signal, and only then writes the audit — so a stalled
/// audit filesystem cannot stand between the first run and the last, and the
/// stop still lands in the audit naming who asked and why: the record a
/// `pkill` from outside does not leave.
fn stop_all(root: &Path, wait: Duration) -> Tally {
    let mut tally = Tally::default();
    let found = match records(root) {
        Ok(found) => found,
        Err(e) => {
            tally
                .problems
                .push(format!("{}: {e}", dir(root).display()));
            return tally;
        }
    };
    let boot = match boot_id() {
        Ok(boot) => boot,
        // Fail closed: without it a record from a previous boot cannot be told
        // from one of ours, and signalling on that guess is the whole hazard.
        Err(e) => {
            tally.problems.push(format!("{BOOT_ID}: {e}"));
            return tally;
        }
    };

    let mut live = Vec::new();
    for (path, parsed) in found {
        let record = match parsed {
            Ok(record) => record,
            Err(why) => {
                tally.problems.push(why);
                continue;
            }
        };
        match still_running(&path, &record, &boot) {
            Ok(true) => live.push((path, record)),
            Ok(false) => tally.stale += 1,
            Err(e) => tally.problems.push(format!(
                "{}: pid {} ({}): {e}",
                path.display(),
                record.pid,
                record.verb
            )),
        }
    }

    let kills: Vec<(KillTarget, String)> = live
        .iter()
        .map(|(_, record)| {
            (
                KillTarget::Pid(record.pid),
                format!(
                    "td-builder stop: {} run recorded by this worktree",
                    record.verb
                ),
            )
        })
        .collect();
    let results = sys::kill_all_recorded(&kills, sys::SIGTERM);

    let mut pending = Vec::new();
    for ((path, record), result) in live.into_iter().zip(results) {
        match result {
            Ok(()) => pending.push((path, record)),
            // It ended between the check above and the signal. Nothing was
            // stopped, and nothing is wrong.
            Err(e) if is_gone(&e) => {
                let _ = std::fs::remove_file(&path);
                tally.stale += 1;
            }
            Err(e) => tally.problems.push(format!(
                "{}: pid {} ({}): {e}",
                path.display(),
                record.pid,
                record.verb
            )),
        }
    }

    confirm_all(pending, Instant::now() + wait, &mut tally);
    tally
}

const HELP: &str = "\
usage: td-builder stop

Stop the long check run this worktree started — `ready`,
`check`, `affected-checks --run`, or `gate-run`. Reads the run
records under .td-build-cache/runs/ and SIGTERMs each live one.

A recorded run is one the shared check host took, so the process
signalled here owns no build tree of its own: the hosted tree is
the host's, and it comes down on the host's own client-went-away
cancellation once this process is gone. That teardown is
asynchronous and `stop` does not wait for it.

Only this worktree's runs: the records live in it, so another
worktree's build cannot be named from here. This is the reason
not to reach for `pkill -f` — every worktree runs the same
binary path, so no pattern can tell two runs apart.

A signal is a request, not a death, so `stop` waits to see each
process go before reporting it stopped, and polls them all
together rather than one after another. One that has not gone
keeps its record and is reported as unfinished, with a non-zero
exit: run `stop` again to re-signal it, and `stop && ready`
will not start over a client that is still running. Nothing to
stop is success on its own.

A record it cannot read is reported and the other runs are
stopped anyway; the exit is non-zero. One unreadable file must
never be able to make a worktree's runs unstoppable.

Short forms are not runs and are not recorded: `ready
--record-only`, a bare `affected-checks`, `gate-run --list`. Nor
are `build` and `realize`, which the check host does not take.
Signal a pid you recorded yourself — never a `pkill -f` pattern,
which cannot tell two worktrees apart and also ends the shared
check host.

To stop the shared check host instead, see `check-host-stop`.
";

/// The whole command, with its inputs passed in so its exit codes are
/// testable: 0 stopped everything it found, 2 bad usage, 1 something is
/// unaccounted for.
fn stop_main(args: &[String], root: &Path, wait: Duration) -> u8 {
    match args {
        [] => {}
        [flag] if flag == "-h" || flag == "--help" => {
            print!("{HELP}");
            return 0;
        }
        // Matched on the whole slice, not just the first: `stop --help junk`
        // silently ignoring `junk` would let a typo look like it worked.
        _ => {
            eprintln!("td-builder: stop: unexpected arguments: {}", args.join(" "));
            eprint!("{HELP}");
            return 2;
        }
    }
    let tally = stop_all(root, wait);
    for line in &tally.stopped {
        println!("{line}");
    }
    // Always, not only when nothing stopped: a swept record is a thing that
    // happened, and an operator counting runs should see it.
    if tally.stale > 0 {
        println!("retired {} stale record(s)", tally.stale);
    }
    for problem in &tally.problems {
        eprintln!("td-builder: stop: {problem}");
    }
    if tally.stopped.is_empty() && tally.stale == 0 && tally.problems.is_empty() {
        // Success, not failure. "Nothing is running" is what the operator
        // wanted to be true, and a non-zero exit here would make `stop` unsafe
        // to put in front of a start. The root is named because it is the one
        // thing that could be wrong: a `stop` from the wrong cwd looks exactly
        // like a `stop` with nothing to do.
        println!("nothing to stop in {}", root.display());
    }
    if tally.problems.is_empty() {
        0
    } else {
        1
    }
}

pub fn stop_cli(args: &[String]) -> ExitCode {
    ExitCode::from(stop_main(args, &crate::affected::resolve_root(), CONFIRM))
}

// Opting out LOCALLY, which is what AGENTS.md allows inline test code to do,
// and only here: a fixture that cannot unwrap itself is written around the
// lint rather than around the behaviour. Nothing above this line takes it —
// the module's production surface returns `io::Result` and `Option`
// throughout, and this is a new module, so none of the crate's grandfathered
// file-level allowances apply to it.
#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    /// A real `/proc/self/stat`, so the parser is checked against the kernel's
    /// format rather than against my idea of it.
    #[test]
    fn our_own_starttime_is_readable_and_stable() {
        let pid = i64::from(std::process::id());
        let first = starttime_of(pid).expect("stat").expect("we are alive");
        let second = starttime_of(pid).expect("stat").expect("still alive");
        assert_eq!(first, second, "a starttime moved under a live process");
    }

    #[test]
    fn a_comm_full_of_spaces_and_parens_does_not_shift_the_fields() {
        // The trap this parser exists to avoid. Both are legal `comm` values,
        // and splitting from the left counts fields inside the name.
        let tail = " R 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 4242 rest";
        assert_eq!(parse_stat(&format!("1 (td builder){tail}")), Some(('R', 4242)));
        assert_eq!(parse_stat(&format!("1 (a) b) c){tail}")), Some(('R', 4242)));
        // The state travels with it, and comes from field 3, not from the name.
        let zombie = " Z 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 4242 rest";
        assert_eq!(parse_stat(&format!("1 (R){zombie}")), Some(('Z', 4242)));
        // And a line with no closing paren at all is unparseable, not zero.
        assert_eq!(parse_stat("1 td-builder R 2 3"), None);
        assert_eq!(parse_stat(""), None);
    }

    /// Block until `pid` has actually installed its SIGTERM trap.
    ///
    /// A shell needs a moment to parse and run `trap`, and a SIGTERM landing
    /// in that window takes the DEFAULT disposition and kills it. Signalling
    /// straight after the spawn is therefore a race whose usual outcome is a
    /// dead child — a test that would then prove the opposite of what it says.
    fn wait_until_term_is_ignored(pid: i64) {
        let term_bit: u64 = 1u64 << (sys::SIGTERM as u32 - 1);
        for _ in 0..500 {
            let ignored = std::fs::read_to_string(format!("/proc/{pid}/status"))
                .ok()
                .and_then(|status| {
                    status
                        .lines()
                        .find_map(|line| line.strip_prefix("SigIgn:"))
                        .and_then(|hex| u64::from_str_radix(hex.trim(), 16).ok())
                })
                .unwrap_or(0);
            if ignored & term_bit != 0 {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("the child never installed its SIGTERM trap");
    }

    /// The state letter from a live `/proc/<pid>/stat`.
    fn state_of(pid: i64) -> Option<char> {
        let text = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        parse_stat(&text).map(|(state, _)| state)
    }

    #[test]
    fn a_zombie_is_gone_however_much_of_its_stat_survives() {
        // A reaped-but-unwaited process keeps its stat entry, its state and
        // its starttime. Reading it as alive makes `stop` signal a corpse,
        // wait out the whole confirmation window, and then report a run that
        // "has not exited" with a non-zero exit — for a run that ended.
        let root = tempdir("zombie");
        let mut child = std::process::Command::new("sleep")
            .arg("60")
            .spawn()
            .expect("spawn");
        let pid = i64::from(child.id());
        let starttime = starttime_of(pid).expect("stat").expect("child is alive");
        // Killed and deliberately NOT reaped.
        sys::kill_recorded(KillTarget::Pid(pid), sys::SIGKILL, "test: make a zombie")
            .expect("kill");
        let mut state = None;
        for _ in 0..500 {
            state = state_of(pid);
            if state == Some(ZOMBIE) {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(state, Some(ZOMBIE), "the child never became a zombie");
        // Its stat is still readable and still carries the same starttime,
        // which is exactly why a starttime comparison alone is not enough...
        let text = std::fs::read_to_string(format!("/proc/{pid}/stat")).expect("stat");
        assert_eq!(parse_stat(&text).map(|(_, when)| when), Some(starttime));
        // ...and it is nonetheless not a process any more.
        assert_eq!(starttime_of(pid).expect("stat"), None, "a zombie read as alive");
        let record = Record {
            pid,
            starttime,
            verb: "ready".to_string(),
            boot: boot_id().expect("boot id"),
        };
        assert!(
            !is_same_process(&record).expect("check"),
            "a zombie matched the record of the run it used to be"
        );
        let path = path_for(&root, pid);
        std::fs::create_dir_all(dir(&root)).expect("mkdir");
        std::fs::write(&path, record.render()).expect("write");
        assert!(
            !still_running(&path, &record, &record.boot).expect("check"),
            "a corpse read as a live run instead of being retired"
        );
        assert!(!path.exists(), "a zombie's record was kept");
        let _ = child.wait();
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_unmounted_proc_is_a_fault_and_not_a_worktree_of_finished_runs() {
        // Without `/proc`, every `<pid>/stat` is ENOENT, so the ENOENT rule
        // would read every record as a run that had ended. What is pinned here
        // is that contract alone: `stop` fails closed earlier anyway, because
        // `stop_all` reads `boot_id` from `/proc` too. The absence of `/proc`
        // has to be told apart from the absence of a process, which is why
        // `td-svc/src/procfs.rs` keeps an `is_mounted` too.
        let root = tempdir("noproc");
        let fake = root.join("proc");
        std::fs::create_dir_all(&fake).expect("mkdir");
        assert!(
            starttime_of_in(&fake, 1).is_err(),
            "an unmounted /proc read as a finished run"
        );
        // With it mounted, a pid that is simply not there is an ordinary
        // absence again.
        std::fs::create_dir_all(fake.join("self")).expect("mkdir");
        std::fs::write(fake.join("self").join("stat"), "1 (x) R 2 3").expect("write");
        assert_eq!(starttime_of_in(&fake, 1).expect("absent"), None);
        // And a process that IS there still reads normally.
        let tail = " R 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 4242 rest";
        std::fs::create_dir_all(fake.join("7")).expect("mkdir");
        std::fs::write(fake.join("7").join("stat"), format!("7 (x){tail}")).expect("write");
        assert_eq!(starttime_of_in(&fake, 7).expect("present"), Some(4242));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_pid_that_is_gone_is_absent_and_not_an_error() {
        // An unallocatable pid: one past the maximum is never a live process.
        let max: i64 = std::fs::read_to_string("/proc/sys/kernel/pid_max")
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(4_194_304);
        assert_eq!(starttime_of(max + 1).expect("not an error"), None);
    }

    #[test]
    fn a_record_survives_a_round_trip_and_nothing_else_parses() {
        let record = Record {
            pid: 12,
            starttime: 34,
            verb: "ready".to_string(),
            boot: boot_id().expect("boot id"),
        };
        assert_eq!(Record::parse(&record.render()), Some(record));
        // A file that is not a record must not read as one: `stop` treats an
        // unparseable record as a fault, and that only helps if this says so.
        assert_eq!(Record::parse("pid 12\nwhat 3\n"), None);
        assert_eq!(Record::parse("pid twelve\nstarttime 3\nverb ready\nboot b\n"), None);
        assert_eq!(Record::parse("pid 12\n"), None, "a partial record parsed");
        // Strict about keys, not merely about the three it needs: a file
        // holding a whole record plus something else is not a record we
        // wrote, and reading it anyway would be guessing at a pid to signal.
        assert_eq!(
            Record::parse("pid 12\nstarttime 34\nverb ready\nboot b\njunk 1\n"),
            None,
            "a record with an unknown key parsed"
        );
        // A repeated key is corruption, not an override: taking the last one
        // would pick a pid to signal out of a file we did not write.
        assert_eq!(
            Record::parse("pid 12\npid 99\nstarttime 34\nverb ready\nboot b\n"),
            None,
            "a duplicate key parsed"
        );
        // Including when the FIRST occurrence is the malformed one: guarding
        // on "the value is still unset" rather than "the key was seen" would
        // let this through with pid 12.
        assert_eq!(
            Record::parse("pid x\npid 12\nstarttime 34\nverb ready\nboot b\n"),
            None,
            "a duplicate key with a malformed first value parsed"
        );
        // 0 names our own process GROUP and -1 names everything we may
        // signal. `sys::kill_pid` refuses both, and so does this.
        // And a value past `pid_t`, which wraps negative on the way to the
        // syscall. Without it the record parses, `starttime_of` reads a path
        // that cannot exist, and the record is silently retired as stale
        // rather than reported as the corruption it is.
        for pid in ["0", "-1", "99999999999", "2147483648"] {
            assert_eq!(
                Record::parse(&format!("pid {pid}\nstarttime 34\nverb ready\nboot b\n")),
                None,
                "a pid that is not a pid parsed: {pid}"
            );
        }
    }

    #[test]
    fn a_guard_writes_its_run_and_takes_it_away_again() {
        let root = tempdir("guard");
        let path = {
            let guard = Guard::record(&root, "ready").expect("recorded");
            let path = guard.path.clone();
            assert!(path.is_file(), "no record was written");
            let found = records(&root).expect("read back");
            assert_eq!(found.len(), 1);
            let Some((_, Ok(record))) = found.first() else {
                panic!("no record: {found:?}");
            };
            assert_eq!(record.pid, i64::from(std::process::id()));
            assert_eq!(record.verb, "ready");
            path
        };
        assert!(!path.exists(), "the record outlived its run");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_recorded_pid_whose_starttime_differs_is_not_touched() {
        // The recycled-pid case, which is the reason the record is not just a
        // pidfile. Our own pid with somebody else's starttime must read as a
        // different process — and be left alone rather than signalled.
        let root = tempdir("recycled");
        let pid = i64::from(std::process::id());
        let real = starttime_of(pid).expect("stat").expect("alive");
        let record = Record {
            pid,
            starttime: real.wrapping_add(1),
            verb: "ready".to_string(),
            boot: boot_id().expect("boot id"),
        };
        assert!(!is_same_process(&record).expect("check"), "a stale pid matched");
        let path = path_for(&root, pid);
        std::fs::create_dir_all(dir(&root)).expect("mkdir");
        std::fs::write(&path, record.render()).expect("write");
        // `still_running` has no signalling path, so this asserts the
        // decision and not the consequence; the consequence — that a stale
        // record is retired without a signal reaching our own pid — is what
        // `what_stop_exits_with_is_what_happened` drives through the command.
        assert!(
            !still_running(&path, &record, &record.boot).expect("check"),
            "a stale record read as a live run"
        );
        assert!(!path.exists(), "a stale record was left behind");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_stat_we_cannot_read_is_a_fault_and_not_a_dead_process() {
        // The fail-closed rule this module borrows from td-svc/src/procfs.rs.
        // Reporting "gone" for a process we merely failed to inspect makes
        // `stop` answer "nothing to stop" while the build runs on.
        let root = tempdir("stat");
        let garbled = root.join("stat");
        std::fs::write(&garbled, "1 (sleep) R 2 3").expect("write");
        let e = starttime_at(&garbled).expect_err("a short stat read as a time");
        assert_eq!(e.kind(), io::ErrorKind::InvalidData);
        // And a path that is simply not there is still an ordinary absence.
        assert_eq!(starttime_at(&root.join("nope")).expect("absent"), None);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_record_that_cannot_be_read_is_reported_and_not_skipped() {
        // Answering "nothing to stop" for a record we merely failed to read
        // would leave a build running and tell the operator it had not been.
        // It comes back as a problem against that one file, though — see
        // `one_unreadable_record_does_not_make_the_others_unstoppable`.
        let root = tempdir("garbled");
        std::fs::create_dir_all(dir(&root)).expect("mkdir");
        std::fs::write(dir(&root).join("7.run"), "not a record").expect("write");
        let found = records(&root).expect("the listing itself failed");
        assert_eq!(found.len(), 1, "an unreadable record read as empty");
        let Some((_, Err(why))) = found.first() else {
            panic!("an unreadable record parsed: {found:?}");
        };
        assert!(why.contains("7.run"), "the problem does not name the file: {why}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_file_that_is_not_a_record_is_not_read_as_one() {
        // The directory is inside the build cache, so something else may well
        // drop a file in it. Only `.run` is ours; anything else is neither a
        // run to stop nor a fault to report.
        let root = tempdir("stray");
        std::fs::create_dir_all(dir(&root)).expect("mkdir");
        std::fs::write(dir(&root).join("README"), "not a record").expect("write");
        assert_eq!(records(&root).expect("stray read as a record"), Vec::new());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_worktree_that_never_ran_anything_has_nothing_to_stop() {
        let root = tempdir("empty");
        assert_eq!(records(&root).expect("read"), Vec::new());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A shell that refuses SIGTERM and still ends by itself.
    ///
    /// The bound is the load-bearing part: `Reaped` cannot fire if the test
    /// binary itself takes a signal, and an immortal orphan holds the
    /// harness's captured stdout open, which hangs the suite instead of
    /// failing it. The loop also keeps the child a shell, but it is not what
    /// makes it stubborn — `trap '' TERM; sleep 60` refuses SIGTERM just as
    /// well, because bash suppresses the fork for a lone final command and
    /// `SIG_IGN` survives `execve`. What is NOT optional is
    /// `wait_until_term_is_ignored`: signalling before the shell has run
    /// `trap` is what killed an early draft of this fixture.
    const STUBBORN: &str = "trap '' TERM; i=0; while [ $i -lt 60 ]; do sleep 1; i=$((i+1)); done";

    /// A spawned test child that cannot outlive its test.
    ///
    /// A `trap '' TERM` child ignores the polite signal, so an assertion
    /// failing before an explicit cleanup leaks it for the rest of its
    /// bounded life — and a leaked child holds the harness's captured stdout
    /// open, which hangs the test binary instead of failing it. Before
    /// `STUBBORN` carried that bound the leak had no end, which is how a
    /// mutation run of this module wedged for half an hour and left an orphan
    /// behind. `Drop` runs while unwinding, so this cleans up either
    /// way, and it goes through the audited kill like everything else here.
    struct Reaped(std::process::Child);

    impl Drop for Reaped {
        fn drop(&mut self) {
            let _ = sys::kill_child_recorded(&mut self.0, "test cleanup: reaping a spawned child");
            let _ = self.0.wait();
        }
    }

    /// Record a real child under `root` and return its pid, record and path.
    fn plant(root: &Path, child: &std::process::Child) -> (i64, Record, PathBuf) {
        let pid = i64::from(child.id());
        let starttime = starttime_of(pid).expect("stat").expect("child is alive");
        let record = Record {
            pid,
            starttime,
            verb: "ready".to_string(),
            boot: boot_id().expect("boot id"),
        };
        let path = path_for(root, pid);
        std::fs::create_dir_all(dir(root)).expect("mkdir");
        std::fs::write(&path, record.render()).expect("write");
        (pid, record, path)
    }

    #[test]
    fn a_live_run_is_signalled_confirmed_and_written_to_the_audit() {
        // The whole path, against a real process: record a child, stop it by
        // its record, see it die, and see the stop land in the kill audit.
        // Everything above this tests a piece.
        let root = tempdir("live");
        let child = std::process::Command::new("sleep")
            .arg("60")
            .spawn()
            .expect("spawn");
        let (pid, _record, path) = plant(&root, &child);
        // Reap in the background so the signal can be asserted from the exit
        // status below. The confirmation does not need it: a zombie already
        // reads as gone — see `a_zombie_is_gone_however_much_of_its_stat_survives`.
        let mut child = child;
        let reaper = std::thread::spawn(move || child.wait());

        let tally = stop_all(&root, Duration::from_secs(20));
        assert_eq!(tally.stopped.len(), 1, "the live run was not stopped: {tally:?}");
        assert!(tally.problems.is_empty(), "{tally:?}");
        assert!(!path.exists(), "a confirmed stop kept its record");
        let status = reaper.join().expect("reaper").expect("wait");
        assert_eq!(
            std::os::unix::process::ExitStatusExt::signal(&status),
            Some(sys::SIGTERM as i32),
            "the child did not take the signal"
        );
        // The audit is the entire reason this goes through `kill_recorded`
        // rather than a bare kill: it is the record a `pkill` from outside
        // cannot leave, and without this assertion swapping the call for
        // `sys::kill_pid` would change nothing any test could see.
        let token = format!("SIGTERM pid {pid} ");
        let lines = sys::kill_audit_sink().lock().expect("sink").clone();
        let line = lines
            .iter()
            .find(|line| line.contains(&token))
            .unwrap_or_else(|| panic!("no audit line for {token:?} among {lines:?}"));
        assert!(
            line.contains("td-builder stop: ready run recorded by this worktree"),
            "the audit line does not say who asked, or why: {line}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_run_that_will_not_go_is_not_called_stopped() {
        // A signal is a request. Reporting success for a process still holding
        // the check host would break `stop && ready`, which is the workflow
        // this command exists for. See `STUBBORN` for why the child is shaped
        // the way it is.
        let root = tempdir("stubborn");
        let child = Reaped(
            std::process::Command::new("sh")
                .args(["-c", STUBBORN])
                .spawn()
                .expect("spawn"),
        );
        wait_until_term_is_ignored(i64::from(child.0.id()));
        let (pid, _record, path) = plant(&root, &child.0);

        let first = stop_all(&root, Duration::from_millis(150));
        assert!(
            first.stopped.is_empty(),
            "an unstopped run was reported stopped: {first:?}"
        );
        assert_eq!(first.problems.len(), 1, "{first:?}");
        // Naming the pid is not enough: an operator who has to retire a record
        // by hand needs the path, and every other problem line carries one.
        let Some(problem) = first.problems.first() else {
            panic!("no problem: {first:?}");
        };
        assert!(
            problem.contains(&path.display().to_string()),
            "the problem does not name the record: {problem}"
        );
        // And it is genuinely still running, not a corpse that merely reads
        // like one. Without this the test passes against a child that died.
        let state = state_of(pid);
        assert!(
            state.is_some() && state != Some(ZOMBIE),
            "the child was not alive to refuse the signal: state {state:?}"
        );
        assert!(path.is_file(), "a run that did not stop lost its record");
        // A second `stop` re-signals it rather than answering "nothing to
        // stop", which is the whole reason the record is kept.
        let tally = stop_all(&root, Duration::from_millis(150));
        assert!(
            tally.stopped.is_empty(),
            "an unstopped run was reported stopped: {tally:?}"
        );
        assert_eq!(tally.problems.len(), 1, "{tally:?}");
        // Keeping the record and re-reporting it is not the claim; sending a
        // second signal is. Only the audit can see that, and `Reaped` sends
        // SIGKILL, so nothing else adds a SIGTERM line for this pid.
        let token = format!("SIGTERM pid {pid} ");
        let sent = sys::kill_audit_sink()
            .lock()
            .expect("sink")
            .iter()
            .filter(|line| line.contains(&token))
            .count();
        assert_eq!(sent, 2, "a second stop did not re-signal the run");
        // `child` reaps itself on the way out, panic or no panic.
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn one_unreadable_record_does_not_make_the_others_unstoppable() {
        // An unreadable record must not be the state that bricks `stop`.
        // Refusing to stop anything until someone clears a stray file would
        // send an operator straight back to the `pkill` this module exists to
        // remove. The staging rename means a run killed mid-write leaves a
        // `.tmp` no reader selects, but a zero-byte `.run` still arrives by
        // other roads: a crash after the rename without an fsync, a
        // truncation, a writer that is not this program.
        let root = tempdir("brick");
        std::fs::create_dir_all(dir(&root)).expect("mkdir");
        let child = std::process::Command::new("sleep")
            .arg("60")
            .spawn()
            .expect("spawn");
        let (pid, _, _) = plant(&root, &child);
        // One on each side of the live record in the listing order. `0.run`
        // sorts before every real pid, which is the half that matters: with
        // only a later one, an implementation that gave up at the first
        // problem would still have stopped the live run and looked correct.
        // The later name is derived from the pid so it cannot collide with it.
        std::fs::write(dir(&root).join("0.run"), "").expect("write");
        std::fs::write(dir(&root).join(format!("{pid}0.run")), "").expect("write");
        let mut child = child;
        let reaper = std::thread::spawn(move || child.wait());

        // The reasoning above is about listing order, so pin the order.
        let listed: Vec<_> = records(&root)
            .expect("read")
            .into_iter()
            .map(|(path, _)| path)
            .collect();
        let mut expected = listed.clone();
        expected.sort();
        assert_eq!(listed, expected, "records came back in no particular order");

        let tally = stop_all(&root, Duration::from_secs(20));
        assert_eq!(
            tally.stopped.len(),
            1,
            "a stray file stopped the live run being stopped: {tally:?}"
        );
        assert_eq!(
            tally.problems.len(),
            2,
            "the unreadable records were not reported: {tally:?}"
        );
        let _ = reaper.join();
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn what_stop_exits_with_is_what_happened() {
        let root = tempdir("exits");
        let wait = Duration::from_millis(50);
        let arg = |a: &str| vec![a.to_string()];
        // Nothing to stop is success, or `stop && ready` would never start.
        assert_eq!(stop_main(&[], &root, wait), 0, "nothing to stop was not 0");
        assert_eq!(stop_main(&arg("--help"), &root, wait), 0);
        assert_eq!(stop_main(&arg("-h"), &root, wait), 0);
        // Bad usage is distinct from a failed stop.
        assert_eq!(stop_main(&arg("--nope"), &root, wait), 2);
        // Including a good flag with junk after it, which must not look like
        // it worked.
        assert_eq!(
            stop_main(&["--help".to_string(), "junk".to_string()], &root, wait),
            2,
            "an ignored trailing argument exited 0"
        );
        // A record for a process that is not the one we recorded is retired,
        // and retiring it is not a failure.
        let pid = i64::from(std::process::id());
        let real = starttime_of(pid).expect("stat").expect("alive");
        let stale = Record {
            pid,
            starttime: real.wrapping_add(1),
            verb: "ready".to_string(),
            boot: boot_id().expect("boot id"),
        };
        std::fs::create_dir_all(dir(&root)).expect("mkdir");
        std::fs::write(path_for(&root, pid), stale.render()).expect("write");
        assert_eq!(stop_main(&[], &root, wait), 0, "retiring a stale record failed");
        assert!(!path_for(&root, pid).exists(), "the stale record survived");
        // Same again for a record from another boot, where the pid AND the
        // starttime are ours and only the boot differs. Both records carry
        // this process's pid, so the test surviving is the proof that a
        // retirement sends no signal.
        let other_boot = Record {
            pid,
            starttime: real,
            verb: "ready".to_string(),
            boot: "not-the-boot-we-are-on".to_string(),
        };
        std::fs::write(path_for(&root, pid), other_boot.render()).expect("write");
        assert_eq!(
            stop_main(&[], &root, wait),
            0,
            "retiring a previous boot's record failed"
        );
        assert!(
            !path_for(&root, pid).exists(),
            "a previous boot's record survived"
        );
        // Something we cannot account for is 1.
        std::fs::write(dir(&root).join("999998.run"), "garbage").expect("write");
        assert_eq!(stop_main(&[], &root, wait), 1, "an unreadable record was not 1");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_run_that_ends_as_we_look_at_it_is_gone_and_not_a_fault() {
        // Both kernel spellings of "that process is gone". ENOENT is the open
        // failing; ESRCH is the READ failing on a seq_file whose process
        // exited in between, which std leaves as `Uncategorized` — so matching
        // NotFound alone misses exactly the case that happens under load.
        assert!(is_gone(&io::Error::from(io::ErrorKind::NotFound)));
        assert!(is_gone(&io::Error::from_raw_os_error(ESRCH)));
        // And the faults that must never read as an ended run.
        assert!(!is_gone(&io::Error::from(io::ErrorKind::PermissionDenied)));
        assert!(!is_gone(&io::Error::from(io::ErrorKind::InvalidData)));
    }

    #[test]
    fn a_record_is_published_whole_or_not_at_all() {
        // `fs::write` would publish the name before the bytes, and a `stop`
        // arriving in between would read an empty file and report a problem it
        // cannot account for, exiting non-zero because a run had merely
        // started. Nothing partial may ever carry the `.run` name, and the
        // staging file must not linger.
        let root = tempdir("atomic");
        let guard = Guard::record(&root, "ready").expect("recorded");
        let found = records(&root).expect("read back");
        assert_eq!(found.len(), 1, "a staging file was read as a record");
        let pid = i64::from(std::process::id());
        assert!(
            !staging_path(&root, pid).exists(),
            "the staging file outlived the rename"
        );
        // The load-bearing half, and the one a reader depends on: whatever the
        // staging file is called, a reader must not be able to see it. Asking
        // `staging_path` rather than spelling the name here is what makes a
        // future rename ending in `.run` — which would put half-written bytes
        // back in front of `records` — fail this.
        std::fs::write(staging_path(&root, pid + 1), "").expect("write");
        assert_eq!(
            records(&root).expect("a staging file broke stop").len(),
            1,
            "a staging file was visible to a reader"
        );
        drop(guard);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_record_is_published_through_its_staging_name() {
        // Block the staging path with a directory, so a write through it must
        // fail and no record may appear. A `Guard::record` that wrote the
        // final path directly sails past this — which is what makes it a test
        // for write-then-rename, and not merely for the staging name.
        let root = tempdir("viarename");
        let pid = i64::from(std::process::id());
        std::fs::create_dir_all(staging_path(&root, pid)).expect("mkdir");
        assert!(
            Guard::record(&root, "ready").is_none(),
            "a blocked staging path still produced a record"
        );
        assert!(
            !path_for(&root, pid).exists(),
            "a record appeared without passing through staging"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn what_stop_can_name_is_what_the_host_forwards_less_the_control_message() {
        // `long_run_verb` delegates, so for non-excluded argv this is an
        // identity and cannot fail while it stays a delegation. That IS the
        // point: the cases below are a net for someone replacing the
        // delegation with a hand-written copy, which is the shape that drifted
        // before. The assertion that carries content on its own is the
        // exclusion at the bottom.
        let argv = |args: &[&str]| -> Vec<String> {
            args.iter().map(|a| (*a).to_string()).collect()
        };
        for case in [
            vec!["ready"],
            vec!["ready", "--record-only"],
            vec!["ready", "--help"],
            vec!["check"],
            vec!["check", "-h"],
            vec!["affected-checks"],
            vec!["affected-checks", "--run"],
            vec!["affected-checks", "--run", "--self-test"],
            vec!["gate-run"],
            vec!["gate-run", "--list"],
            vec!["gate-run", "list-gates"],
            vec!["gate-run", "gate-timing-report"],
            vec!["stop"],
            vec!["build"],
            vec!["realize"],
            vec![],
        ] {
            let args = argv(&case);
            assert_eq!(
                long_run_verb(&args).is_some(),
                crate::check_host::should_forward_with_host_state(&args, false),
                "the derivation disagrees with its source about {case:?}"
            );
        }
        // The whole of the difference. `daemon-request` IS forwarded, so
        // deriving without excluding it would let `stop` signal a control
        // client and report it as a stopped run.
        let control = argv(&["daemon-request", "sock", "PING"]);
        assert!(crate::check_host::should_forward_with_host_state(&control, false));
        assert_eq!(long_run_verb(&control), None, "a control message counted as a run");
    }

    #[test]
    fn the_hosted_flag_is_read_from_the_environment_not_assumed() {
        // `should_record` is tested both ways, but the line that WIRES it to
        // the environment is not reachable from a test: mutating the process
        // environment would race every other test in this binary. AGENTS.md
        // asks confinement tests to pin the source-level contract the compiler
        // cannot express, so this pins the wiring. Flipping it to `is_none()`
        // restores the useless pid-1 record the seam exists to prevent.
        let source = include_str!("run_record.rs");
        // Whitespace is stripped first, so wrapping the call across lines —
        // which rustfmt may do at any time — cannot red this. And the needle
        // is split so it does not appear contiguously in THIS line: spelled
        // whole, the test would find its own search string and pass against
        // any code at all, which is what it did on the first attempt and what
        // the mutation inverting the wiring survived.
        let dense: String = source.split_whitespace().collect();
        let needle = concat!("HOST_CHILD_ENV).", "is_some()");
        assert_eq!(
            dense.matches(needle).count(),
            1,
            "the hosted flag is no longer read from HOST_CHILD_ENV exactly once"
        );
        // `let hosted = !…is_some()` would satisfy the count above and invert
        // the meaning, so refuse the negation too.
        let negated = concat!("hosted=!std::env::var_os(", "crate::check_memory::HOST_CHILD_ENV)");
        assert!(!dense.contains(negated), "the hosted flag is read inverted");
    }

    #[test]
    fn every_run_is_signalled_before_any_is_waited_on() {
        // Confirming each run before signalling the next leaves the second one
        // running for the whole of the first one's window after the operator
        // asked. Two runs that refuse SIGTERM discriminate the two shapes by a
        // factor of two: serial costs both windows, one shared deadline costs
        // about one. The threshold sits well inside that gap so a loaded
        // machine cannot tip it.
        let root = tempdir("order");
        let window = Duration::from_millis(1000);
        let mut kids = Vec::new();
        for _ in 0..2 {
            let child = Reaped(
                std::process::Command::new("sh")
                    .args(["-c", STUBBORN])
                    .spawn()
                    .expect("spawn"),
            );
            wait_until_term_is_ignored(i64::from(child.0.id()));
            let _ = plant(&root, &child.0);
            kids.push(child);
        }
        let started = Instant::now();
        let tally = stop_all(&root, window);
        let elapsed = started.elapsed();
        assert_eq!(tally.problems.len(), 2, "{tally:?}");
        // Two-sided on purpose. The ceiling alone passes against a
        // confirmation that never waits at all — it would report both as
        // unfinished in no time — so the floor is what makes the ceiling mean
        // "they overlapped" rather than "nothing happened".
        assert!(
            elapsed >= window,
            "two stubborn runs finished in {elapsed:?}, short of the {window:?} \
             window: nothing waited for them"
        );
        // Serial confirmation costs two windows; this sits well inside that,
        // with room for a machine running the whole suite at once.
        assert!(
            elapsed < window * 9 / 5,
            "two runs took {elapsed:?} against a {window:?} window, which is one \
             confirmation after the other rather than one deadline for both"
        );
        drop(kids);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_record_from_a_previous_boot_names_nothing() {
        // `starttime` counts ticks since boot, so the pid/starttime pair is
        // unique within a boot and repeatable across one — and these records
        // outlive a reboot in the build cache. A leftover whose pair happened
        // to match would otherwise be sent a SIGTERM. Planted on OUR OWN pid
        // and starttime, so the boot is all that makes it stale — but
        // `still_running` has no signalling path, so this asserts the decision
        // and not the consequence. That a previous boot's record is retired
        // without a signal reaching our own pid is driven through the command
        // by `what_stop_exits_with_is_what_happened`.
        let root = tempdir("reboot");
        let pid = i64::from(std::process::id());
        let starttime = starttime_of(pid).expect("stat").expect("alive");
        let record = Record {
            pid,
            starttime,
            verb: "ready".to_string(),
            boot: "not-the-boot-we-are-on".to_string(),
        };
        let path = path_for(&root, pid);
        std::fs::create_dir_all(dir(&root)).expect("mkdir");
        std::fs::write(&path, record.render()).expect("write");

        let here = boot_id().expect("boot id");
        assert_ne!(record.boot, here, "the fixture is not from another boot");
        assert!(
            !still_running(&path, &record, &here).expect("check"),
            "a previous boot's record read as a live run"
        );
        assert!(!path.exists(), "a previous boot's record was left behind");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn adding_a_forwarded_verb_forces_a_decision_about_whether_it_is_a_run() {
        // `long_run_verb` derives from the forwarding predicate, which fixes
        // one direction of drift and opens the other: a verb added there
        // becomes stoppable with no thought given to whether it should be. If
        // it is another control message, `stop` will signal it and report it
        // stopped — the `daemon-request` mistake, made automatic. Pin the
        // roster so adding one has to come past this.
        let source = include_str!("check_host.rs");
        let start = source
            .find("fn should_forward_with_host_state")
            .expect("the forwarding predicate");
        let body = source
            .get(start..)
            .and_then(|rest| rest.find("\n}\n").and_then(|end| rest.get(..end)))
            .expect("its body");
        let mut verbs: Vec<&str> = body
            .match_indices("Some(\"")
            .filter_map(|(at, _)| {
                let rest = body.get(at + 6..)?;
                rest.find('"').and_then(|end| rest.get(..end))
            })
            .collect();
        verbs.sort_unstable();
        verbs.dedup();
        assert_eq!(
            verbs,
            ["affected-checks", "check", "daemon-request", "gate-run", "ready"],
            "the forwarded roster changed. Decide whether the new verb is a run \
             `stop` may signal — if it is a control message, exclude it in \
             `long_run_verb` — then update this list"
        );
    }

    #[test]
    fn the_copy_inside_the_check_host_records_nothing() {
        let argv = vec!["ready".to_string()];
        assert_eq!(should_record(&argv, false), Some("ready"));
        // A pid namespace makes the hosted process pid 1: a number that names
        // nothing from outside and is identical for every worktree's run. The
        // first attempt recorded exactly that and was useless.
        assert_eq!(should_record(&argv, true), None);
    }

    #[test]
    fn a_directory_wearing_the_run_name_is_not_a_record() {
        // Reading it would fail with EISDIR and be reported as an unreadable
        // record, making `stop` exit non-zero over something that is not one.
        let root = tempdir("isdir");
        std::fs::create_dir_all(dir(&root).join("9.run")).expect("mkdir");
        assert_eq!(records(&root).expect("a .run directory broke stop"), Vec::new());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn only_a_long_run_is_worth_recording() {
        let argv = |args: &[&str]| -> Vec<String> {
            args.iter().map(|a| (*a).to_string()).collect()
        };
        for verb in ["ready", "check", "gate-run"] {
            assert_eq!(long_run_verb(&argv(&[verb])), Some(verb));
        }
        assert_eq!(
            long_run_verb(&argv(&["affected-checks", "--run"])),
            Some("affected-checks")
        );
        // A control message is not a run. `daemon-request` is forwarded to the
        // check host exactly as `ready` is, so keying off forwarding would
        // have let `stop` claim to have stopped one.
        assert_eq!(long_run_verb(&argv(&["daemon-request", "sock", "PING"])), None);
        assert_eq!(long_run_verb(&argv(&["stop"])), None, "stop recorded itself");
        assert_eq!(long_run_verb(&argv(&[])), None);
        // Help prints and exits, wherever the flag sits.
        assert_eq!(long_run_verb(&argv(&["ready", "--help"])), None);
        assert_eq!(long_run_verb(&argv(&["affected-checks", "--run", "-h"])), None);
        // The short forms of the same verbs are over before anyone could ask
        // to stop them, and each record costs a `git rev-parse` and a write.
        assert_eq!(long_run_verb(&argv(&["ready", "--record-only"])), None);
        assert_eq!(long_run_verb(&argv(&["affected-checks"])), None);
        assert_eq!(long_run_verb(&argv(&["affected-checks", "--self-test"])), None);
        assert_eq!(long_run_verb(&argv(&["gate-run", "--list"])), None);
        assert_eq!(long_run_verb(&argv(&["gate-run", "list-gates"])), None);
        assert_eq!(long_run_verb(&argv(&["gate-run", "gate-timing-report"])), None);
    }

    fn tempdir(tag: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "td-run-record-{}-{tag}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("tempdir");
        path
    }
}

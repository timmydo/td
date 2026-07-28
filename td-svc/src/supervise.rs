//! The supervisor: start units in plan order, watch them, restart them.
//!
//! Event loop per DESIGN.md §5 — one waiter thread per running child, blocking
//! in `Child::wait`, reporting through a channel. That reaps promptly, which
//! the stop protocol (I4) depends on, and needs no polling. The main loop owns
//! all state, so nothing is shared and nothing is locked across a blocking
//! call: threads block, then send.
//!
//! Every event carries the GENERATION of the instance it describes. A service
//! that dies and restarts while a readiness probe is still running would
//! otherwise have its replacement marked ready by the dead instance's probe.

use crate::backoff;
use crate::order;
use crate::procfs::{self, Containment};
use crate::table::{Kind, Restart, Unit};
use std::io;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// How often the loop wakes when something is pending (a retry, a deadline).
/// With nothing pending it blocks on the channel instead.
const TICK: Duration = Duration::from_millis(100);
/// Gap between readiness attempts.
const PROBE_GAP: Duration = Duration::from_millis(200);
/// How long ONE readiness attempt may run before it is killed. `Command::status`
/// blocks forever, so without this a hung probe strands the unit and its
/// dependents and leaks a process per attempt.
const PROBE_ATTEMPT: Duration = Duration::from_secs(5);

/// How long a `tty=` unit waits for its dependencies before starting regardless.
///
/// I5's runtime half. Every other console defence reasons about the GRAPH — the
/// table refuses `requires=`, the plan refuses to skip — and none of them
/// engages when a dependency simply never settles. A console that is waiting
/// indefinitely is a console that is not there, which is the same outcome by a
/// slower route, so its wait is bounded and its ordering is a preference.
const CONSOLE_PATIENCE: Duration = Duration::from_secs(30);

/// How often a supervisor with NO units repeats that fact. Slow, because it is a
/// standing condition rather than news, but never silent — see `run`.
const SILENT_TABLE_COMPLAINT: Duration = Duration::from_secs(60);

/// How often a stop whose containment is still occupied re-scans `/proc`.
///
/// Survivors are not td-svc's children, so their exit produces no event; this
/// interval is the whole mechanism by which such a stop ever completes. Short
/// enough that an operator's `status` catches up promptly, long enough that a
/// wedged containment is not a `/proc` scan per loop pass.
const STOP_SWEEP_INTERVAL: Duration = Duration::from_secs(1);

/// What the socket answers an unparseable request with.
const CONTROL_USAGE: &str = "usage: status [NAME] | start NAME | stop NAME | restart NAME | \
                             reload | reboot | poweroff | halt\n";

/// Where a shutdown in flight is recorded, so a REPLACEMENT td-svc finds it.
///
/// DESIGN.md I6. PID 1 respawns td-svc unconditionally, so a supervisor that
/// dies mid-teardown must not come back and start services while an orphaned
/// `/etc/shutdown` is unmounting filesystems underneath them.
const SHUTDOWN_MARKER: &str = "/run/td-svc/shutdown";

/// How long the shutdown waits for log writers to close their files.
///
/// Bounded for the same reason every other deadline here is: a writer wedged in
/// `write(2)` on a stalled filesystem cannot be recovered, and blocking the
/// shutdown on it trades a lost log for a machine that never powers off.
const LOG_CLOSE_GRACE: Duration = Duration::from_secs(3);

/// How long every orphan of a previous supervisor gets between TERM and KILL,
/// TOTAL rather than each: they are signalled together and waited on together,
/// so a machine with eight wedged services is delayed once, not eight times.
///
/// Short, and bounded, because this is the boot path and **I5** applies —
/// nothing here may delay a console indefinitely. It is deliberately less than
/// a unit's own `stop-timeout`: an orphan has already lost its supervisor, so
/// there is nobody left for a graceful exit to report to.
const EVICT_GRACE: Duration = Duration::from_secs(5);

/// How long to keep looking after the KILL before giving up and saying so.
/// SIGKILL is not instantaneous — the task still has to be scheduled and torn
/// down — so declaring failure the moment it is sent would report a survivor
/// that is merely on its way out.
const EVICT_SETTLE: Duration = Duration::from_secs(2);

/// The teardown script, run once every service is down and before the power
/// applet. It is what actually syncs and unmounts.
const SHUTDOWN_SCRIPT: &str = "/etc/shutdown";

/// Extra time, beyond the unit's own `stop-timeout`, that a shutdown waits for
/// one service before moving on.
///
/// A stop is TERM, wait `stop-timeout`, KILL — so a unit that ignores both
/// needs a second `stop-timeout` for the KILL to land and the containment to
/// drain, plus a sweep interval for the scan that proves it. Past that the
/// shutdown proceeds anyway and says so: one service that will not die must
/// not leave the machine up forever, which is the failure this bound exists to
/// rule out.
const SHUTDOWN_UNIT_SLACK: Duration = Duration::from_secs(2);

fn unknown(name: &str) -> String {
    format!("error: no such service {name:?}\n")
}

/// `O_NOCTTY` — opening a terminal must never make it td-svc's own by side
/// effect. Same reason td-init's `start` carries it.
const O_NOCTTY: i32 = 0o400;

/// Where a `tty=` unit's own startup diagnostics go. Not the unit's terminal:
/// if it cannot open that, the message about why must still land somewhere.
const CONSOLE: &str = "/dev/console";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Never started, or waiting out a restart delay.
    Down,
    /// Spawned; for a daemon with `ready=`, not yet probed successfully.
    Starting,
    /// A oneshot that exited 0, or a daemon that is up and (if declared) probed.
    Ready,
    /// Reached a decision that is not success: exited non-zero, failed to
    /// spawn, timed out, or failed its readiness probe. Terminal for ordering
    /// purposes — dependents proceed — but a restarting daemon still retries.
    Failed,
    /// Too many consecutive fast failures; retried only at the capped interval.
    Held,
    /// Stopped because someone ASKED, over the control socket.
    ///
    /// Distinct from `Failed` because the restart policy must not see it: a
    /// `restart=always` daemon that an operator stopped has to STAY stopped, or
    /// the socket cannot stop anything. It still settles, so a dependent that
    /// was waiting on it proceeds rather than hanging on a decision an operator
    /// already made.
    Stopped,
}

impl Phase {
    /// Has this unit reached a decision? `after=` waits for one; it does NOT
    /// require success. A unit stuck at `Down` with a pending retry is NOT
    /// settled on its first attempt but IS once it has failed one, or a missing
    /// binary would block every dependent — the console included — for the ~7
    /// minutes it takes backoff to reach the hold.
    fn settled(self) -> bool {
        matches!(
            self,
            Phase::Ready | Phase::Failed | Phase::Held | Phase::Stopped
        )
    }

    /// How `status` names it. Stable strings — this is the socket's output.
    fn label(self) -> &'static str {
        match self {
            Phase::Down => "down",
            Phase::Starting => "starting",
            Phase::Ready => "ready",
            Phase::Failed => "failed",
            Phase::Held => "held",
            Phase::Stopped => "stopped",
        }
    }
}

pub struct Service {
    pub unit: Unit,
    pub phase: Phase,
    pub pid: Option<i32>,
    /// Dropped from the table by a `reload`, but still on its way down.
    ///
    /// It is no longer declared, so it is in no plan and no start order — but
    /// it is still RUNNING, and the shutdown has to wait for it like any other
    /// live process. Cleared if the name is declared again.
    pub retired: bool,
    /// `/proc` field 22 for `pid`, read at spawn. A pid alone does not identify
    /// a process: the KILL that follows a TERM is `stop-timeout` later, and in
    /// that window the child can die, be reaped, and have its pid recycled onto
    /// something unrelated. This is what the KILL checks before it fires.
    pub starttime: Option<u64>,
    /// Bumped on every spawn. Events from an older instance are ignored.
    pub generation: u64,
    pub started: Option<Instant>,
    pub retry_at: Option<Instant>,
    /// When a `oneshot`'s `timeout=` expires. `None` for a daemon or an
    /// untimed oneshot.
    pub deadline: Option<Instant>,
    /// When a unit that was TERMed for overrunning its timeout gets a KILL.
    /// DESIGN.md's stop sequence is TERM, wait, KILL; a TERM alone leaks a
    /// process that ignores it — and its waiter thread with it.
    pub kill_at: Option<Instant>,
    pub fast_failures: u32,
    /// When this unit first found itself eligible but blocked on a dependency.
    /// Only a console unit acts on it — see `CONSOLE_PATIENCE`.
    waiting_since: Option<Instant>,
    /// The plan could not order this unit but started it anyway because it
    /// provides a console. Its `after=`/`requires=` are then a preference, not
    /// a gate: the complaint says "with its ordering ignored", and waiting on
    /// dependencies the plan already called unsatisfiable would make that false.
    forced: bool,
    /// This service's log capture, created on its first spawn and kept for the
    /// life of the supervisor. Per-service and NOT per-instance: a restarting
    /// daemon would otherwise get a second writer with its own handle on the
    /// same path, and two writers rotating one file race (§7).
    log: Option<Arc<crate::logs::Capture>>,
    /// Set when the instance a probe was launched for is gone, so the probe
    /// thread stops forking attempts instead of running out its full timeout.
    cancel: Option<Arc<AtomicBool>>,
    /// A stop was REQUESTED and the process has not exited yet.
    ///
    /// The exit that follows must not be read as a failure — a `restart=always`
    /// daemon would come straight back and the socket could not stop anything.
    /// It is a separate flag rather than a phase because the unit is still
    /// `Starting`/`Ready` until it actually dies, and ordering must keep seeing
    /// that: claiming it settled before the process is gone is exactly the
    /// fail-open shape I4 forbids.
    stopping: bool,
    /// A `restart` is a stop with an intent. Set only alongside `stopping`, and
    /// consumed by the exit, so a restart that races a crash still starts once.
    start_after_stop: bool,
    /// The containment a stop was issued against, kept until the stop
    /// COMPLETES.
    ///
    /// I4: a service is stopped only when its leader is reaped AND its
    /// containment is empty. Once the leader is reaped its pid is gone, so
    /// `containment()` — which needs one — can no longer answer; without this
    /// there is nothing left to ask, and an earlier draft therefore declared
    /// the stop finished on the leader's exit alone. For the greeter that means
    /// TERMing one shell and calling it done while getty, login and a user's
    /// shell keep the console.
    stop_scope: Option<Containment>,
    /// The terminal the RUNNING instance actually got, read from the open
    /// descriptor at spawn.
    ///
    /// Not the same as resolving `tty=` later: `attach_tty` falls back to
    /// `/dev/console` when the configured terminal cannot be opened, so a
    /// containment built from the unit's name would signal a terminal this
    /// service is not on — and the node behind a path can be replaced between
    /// the spawn and the stop.
    tty_dev: Option<i32>,
    /// A KILL has already been attempted for the current teardown, so the
    /// "killing it" line is not repeated on every retry.
    killed: bool,
    /// When to re-scan a containment that was still occupied last time.
    ///
    /// The leader's exit is the ONLY event td-svc gets — survivors are not its
    /// children, so nothing wakes it when the last of them goes. Without a
    /// timer a unit whose containment outlived its leader would sit `stopping`
    /// forever, never reaching `Stopped` however long the console stayed idle.
    next_sweep: Option<Instant>,
}

impl Service {
    fn new(unit: Unit) -> Service {
        Service {
            unit,
            phase: Phase::Down,
            pid: None,
            starttime: None,
            generation: 0,
            started: None,
            retry_at: None,
            deadline: None,
            kill_at: None,
            fast_failures: 0,
            waiting_since: None,
            forced: false,
            log: None,
            cancel: None,
            stopping: false,
            start_after_stop: false,
            stop_scope: None,
            tty_dev: None,
            killed: false,
            next_sweep: None,
            retired: false,
        }
    }

    /// Where this service's processes live, for the stop path.
    ///
    /// NOT a union of group and session. A unit td-svc grouped keeps td-svc's
    /// own SESSION, so matching either would select every process in it — every
    /// other service, and td-svc's own parent. A `tty=` unit is the opposite:
    /// it `setsid()`s into a session of its own, which is the only thing that
    /// still identifies its login tree once the shell starts making job-control
    /// groups inside it.
    ///
    /// The session is read at QUERY time, not at spawn: a read immediately
    /// after `spawn` returns almost always precedes the child's `exec`, let
    /// alone its `setsid`, so it would record td-svc's own ids.
    ///
    /// A `tty=` unit that has not `setsid()` YET narrows to its pid alone. It
    /// inherited td-svc's process group and session, so both of those name the
    /// SUPERVISOR — an earlier draft returned `Group(stat.pgrp)` here, and a
    /// `tty=` oneshot that outran its `timeout=` before reaching `setsid` would
    /// have sent `kill -TERM -<td-svc's own pgid>`: td-svc, every service in
    /// its group, and the machine with it. Every containment this returns must
    /// be one td-svc is NOT in.
    pub fn containment(&self) -> io::Result<Option<Containment>> {
        let Some(pid) = self.pid else { return Ok(None) };
        if self.unit.tty.is_none() {
            // Imposed by `process_group(0)`, so pgid == pid by construction.
            return Ok(Some(Containment::Group(pid)));
        }
        // The device the UNIT named, resolved now. Deliberately not read off
        // the child: td-svc opens the tty `O_NOCTTY`, so the direct child never
        // acquires a controlling terminal and its field 7 is 0 for its whole
        // life — an earlier draft keyed on it and the console containment
        // therefore never engaged at all.
        // What the running instance ACTUALLY got, preferred over re-resolving
        // the configured name: those differ whenever `attach_tty` fell back to
        // `/dev/console`, and a containment built from the name would then
        // address a terminal this service is not on.
        // No `or_else` onto the configured name. `tty_dev` is `None` after a
        // spawn only when `attach_tty` opened NOTHING — both the unit's `tty=`
        // and `/dev/console` failed — so the child provably has no controlling
        // terminal, and addressing the device it merely NAMED would signal a
        // terminal held by someone else entirely. Fall through to the pid-keyed
        // chain, which still reaches the process td-svc spawned.
        if let Some(tty) = self.tty_dev {
            return Ok(Some(Containment::Console { leader: pid, tty }));
        }
        let Some(stat) = procfs::stat_of(pid)? else {
            return Ok(None);
        };
        Ok(Some(classify(pid, &stat)))
    }
}

/// The device actually attached to a child, read from the open descriptor
/// rather than from the path.
///
/// `attach_tty` falls back to `/dev/console` when the configured terminal
/// cannot be opened, so the path a unit NAMES and the device its process got
/// are not always the same one — and a containment built from the name would
/// then signal a terminal this service is not on.
fn attached_device(file: &std::fs::File) -> Option<i32> {
    use std::os::unix::fs::MetadataExt;
    packed_device(file.metadata().ok()?.rdev())
}

/// A `dev_t` in the packing `/proc/<pid>/stat` field 7 uses, or `None`.
fn packed_device(rdev: u64) -> Option<i32> {
    // `st_rdev` and field 7 are the same 32-bit `new_encode_dev` packing —
    // checked against /dev/tty1 (1025), /dev/ttyS0 (1088) and /dev/pts/0
    // (34816) on a live kernel. 0 is "no controlling terminal", never a target.
    //
    // The reinterpret is not a cast for convenience: the kernel holds field 7
    // in an `int` and prints it signed, so a device whose encoding sets bit 31
    // appears NEGATIVE in `/proc`. Comparing that against a `try_from` would
    // simply fail to resolve and silently drop to a containment that cannot
    // reach the login tree; matching the kernel's own reinterpretation keeps
    // the two bit-exact over the whole range.
    let packed = u32::try_from(rdev).ok()? as i32;
    (packed != 0).then_some(packed)
}

/// The next service a teardown should stop, and the cursor to resume from.
///
/// REVERSE plan order, which is the only thing that makes the order mean
/// anything: a dependent must be down before what it depends on, or the
/// teardown pulls `netup` out from under an `sshd` that is still serving.
///
/// Split out from `advance_shutdown` so the direction is testable without
/// live processes and a working `kill` — with those in the way, a walk that
/// went forwards and a walk that went backwards leave the same wreckage.
fn next_to_stop(order: &[usize], cursor: usize, live: &[bool]) -> Option<(usize, usize)> {
    let mut cursor = cursor;
    while cursor < order.len() {
        let index = order.get(order.len() - 1 - cursor).copied();
        cursor += 1;
        if let Some(index) = index {
            if live.get(index).copied().unwrap_or(false) {
                return Some((index, cursor));
            }
        }
    }
    None
}

/// Record a shutdown in flight, durably enough for a REPLACEMENT to see it.
///
/// Written to a temporary name and renamed, because a torn marker is worse
/// than none: `rename(2)` within a directory is atomic, so a resumer sees
/// either the whole word or no file at all, never half of one. The directory
/// is `control::DIR`, which `bind` already created at `0700`.
fn write_marker(path: &str, power: Power) -> io::Result<()> {
    let staging = format!("{path}.new");
    if let Some(dir) = std::path::Path::new(path).parent() {
        // `bind` normally made this, but a td-svc whose socket failed to bind
        // still has to be able to shut the machine down. An existing DIRECTORY
        // is already `Ok`; the only thing this can now report is something that
        // is not one sitting in the way, which is worth saying precisely rather
        // than rediscovering as a confusing failure of the write below.
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&staging, format!("{}\n", power.as_str()))?;
    std::fs::rename(&staging, path)
}

/// What a marker left by an earlier instance says, if there is one.
///
/// A marker that exists but cannot be read, or carries a word this does not
/// know, still means a shutdown BEGAN — so it resumes, and resumes to
/// `Reboot`. The alternative is to park, and a machine that is up with its
/// filesystems unmounted and no services is the outcome I6 exists to prevent.
/// Reboot is the recoverable end of that choice.
fn read_marker(path: &str) -> Option<Power> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return None,
        Err(e) => {
            log(&format!("{path}: {e}; resuming the shutdown as a reboot"));
            return Some(Power::Reboot);
        }
    };
    match Power::parse(text.trim()) {
        Some(power) => Some(power),
        None => {
            log(&format!(
                "{path}: does not name a power action; resuming as a reboot"
            ));
            Some(Power::Reboot)
        }
    }
}

/// Run `/etc/shutdown` and wait for it. Its failures are ITS to report — the
/// script is a tripwire that prints its own diagnostics — but a script that
/// cannot be run at all is worth a line, since the unmounts then never happen.
fn run_teardown(script: &str) {
    log(&format!("running {script}"));
    let mut cmd = Command::new(script);
    cmd.stdin(Stdio::null());
    // Explicitly the CONSOLE, not whatever td-svc inherited. The teardown's
    // last act is to print a marker the boot oracle latches, and by the time it
    // runs the greeter's session leader is gone — so the terminal it was on has
    // been vhangup'd and writes through any descriptor inherited from it return
    // EIO. The marker would simply vanish, which is indistinguishable from a
    // teardown that never ran. Observed on the wire, not theorised.
    match open_tty(CONSOLE) {
        Ok(console) => match console.try_clone() {
            Ok(errors) => {
                cmd.stdout(Stdio::from(console)).stderr(Stdio::from(errors));
            }
            Err(e) => log(&format!("{CONSOLE}: {e}; the teardown keeps our own output")),
        },
        Err(e) => log(&format!("{CONSOLE}: {e}; the teardown keeps our own output")),
    }
    match cmd.status() {
        Ok(status) if status.success() => {}
        Ok(status) => log(&format!("{script}: exited {status}; continuing anyway")),
        Err(e) => log(&format!("{script}: {e}; filesystems may not be clean")),
    }
}

/// What is left of a recorded stop scope once its leader has been reaped.
///
/// The leader's pid stops being ours the moment its waiter reaps it, and the
/// kernel may hand it straight on. Anything keyed on that pid must therefore
/// go: a `Process` scope names only the leader, so it empties, and `Console`'s
/// leader half goes the same way, leaving the terminal.
///
/// A group or session id STAYS. It was the leader's pid, but Linux keeps a pid
/// number reserved for as long as it is still in use as a pgid or sid, so it
/// cannot name a different group while that group still has a member — and if
/// it has none there is nothing there to scan or signal anyway.
fn without_reaped_leader(scope: Option<Containment>) -> Option<Containment> {
    match scope {
        // No process has pid 0, so this half now matches nothing.
        Some(Containment::Console { tty, .. }) => Some(Containment::Console { leader: 0, tty }),
        Some(Containment::Process(_)) => None,
        other => other,
    }
}

/// How many processes a stop must still account for in one scan.
///
/// Split out from `finish_stop` because the arm that matters is the one a test
/// cannot otherwise reach: making `/proc` unreadable on demand is not
/// something a unit test can arrange, so without this the I3 reading — an
/// unreadable scan is NOT an empty one — had nothing pinning it.
///
/// A scan that found nothing but hit errors is not empty either; that is what
/// `proven_empty` distinguishes, and a stop that proceeded on it would tear
/// down a service that is still running.
fn occupancy(scan: Result<&procfs::Scan, &io::Error>) -> usize {
    match scan {
        Ok(scan) if scan.proven_empty() => 0,
        Ok(scan) => scan.pids.len().max(1),
        Err(_) => 1,
    }
}

/// Which containment the delayed KILL addresses, or `None` to send nothing.
///
/// Split out from `escalate` so the CHOICE is testable without a live child
/// and a real signal — it is the part that was wrong, not the sending.
///
/// A requested stop KILLs the scope its TERM went to. Re-deriving one needs a
/// live pid, so once the leader is reaped a re-derived target is `None` and the
/// KILL silently does nothing — which is exactly the case it exists for, since
/// survivors outliving the leader are why the stop had not finished. It can
/// also be the WRONG set: a `tty=` unit whose device stopped resolving narrows
/// to the wrapper alone while the TERM went to the whole terminal.
///
/// Without a stop in flight, a leader that is no longer ours means there is
/// nothing safe left to address: every derivable id is keyed on a pid the
/// kernel may have handed on.
fn kill_target(
    stopping: bool,
    same: bool,
    recorded: Option<Containment>,
    derived: Option<Containment>,
) -> Option<Containment> {
    match (stopping, same) {
        (true, true) => recorded,
        (true, false) => without_reaped_leader(recorded),
        (false, true) => derived,
        (false, false) => None,
    }
}

/// Which containment a `tty=` child's `/proc` entry describes. Split out from
/// the read so the classification is testable without fabricating a `/proc`.
/// Is this containment provably empty? An incomplete scan is NOT emptiness
/// (I3) — it reads as "still there", the direction that refuses to start a
/// duplicate rather than the one that starts a second copy.
fn scan_is_empty(scan: procfs::Scan) -> bool {
    scan.proven_empty()
}

/// How to address a recorded orphan.
///
/// The recorded terminal comes FIRST, because it is the one thing that cannot
/// be recovered: the child never carries it (td-svc opens the console
/// `O_NOCTTY`, so its field 7 is 0 for life), so re-deriving from `/proc` would
/// narrow a console unit to its wrapper and leave the login tree running.
/// Everything else IS recoverable and is read fresh rather than trusted.
fn containment_of(entry: &crate::evict::Entry) -> Containment {
    if entry.tty != 0 {
        return Containment::Console {
            leader: entry.pid,
            tty: entry.tty,
        };
    }
    match procfs::stat_of(entry.pid) {
        Ok(Some(stat)) => classify(entry.pid, &stat),
        // It leads nothing we can prove, so address it alone rather than
        // guessing at a group that might be td-svc's own.
        _ => Containment::Process(entry.pid),
    }
}

fn classify(pid: i32, stat: &procfs::Stat) -> Containment {
    // Reached only when the unit's `tty=` device could not be resolved — see
    // `containment`, which prefers `Console` because neither the group nor the
    // session a `tty=` child leads ever contains the login tree getty makes.
    if stat.session == pid {
        Containment::Session(pid)
    } else if stat.pgrp == pid {
        // Its own group but not (yet) its own session — a `setpgid` without a
        // `setsid`. The group is safe precisely because it leads it.
        Containment::Group(pid)
    } else {
        // It leads neither, so both are inherited from td-svc.
        Containment::Process(pid)
    }
}

pub enum Event {
    /// A control-socket request, with the channel its reply goes back on.
    ///
    /// The reply travels as a message rather than a shared buffer for the same
    /// reason everything else does: the main loop owns the state and answers on
    /// its own thread, so there is nothing to lock.
    Control {
        request: String,
        reply: Sender<String>,
    },
    Exited {
        name: String,
        generation: u64,
        code: Option<i32>,
    },
    /// The Ctrl-Alt-Del sentinel died. `signal` is what killed it, if a signal
    /// did — SIGINT means the kernel delivered a press, anything else means the
    /// sentinel broke and the arming has to be rebuilt from scratch.
    ///
    /// `pid` is which sentinel, and it is not decoration: retiring one is done
    /// by closing its pipe, so every re-arm leaves a watcher thread about to
    /// report a death that has already been accounted for. Acting on that
    /// report would retire the sentinel just armed and arm another, forever.
    SentinelDied {
        pid: i32,
        signal: Option<i32>,
    },
    /// `wait` itself failed. Deliberately distinct from an exit: collapsing it
    /// into one would clear the service's identity while the child may still be
    /// running, and td-svc would then start a duplicate (DESIGN.md I3/I4).
    WaitFailed {
        name: String,
        generation: u64,
        error: String,
    },
    Ready {
        name: String,
        generation: u64,
    },
    ProbeFailed {
        name: String,
        generation: u64,
    },
}

impl Event {
    /// The service an event is about. A `Control` request is about the
    /// supervisor, not a service, and `dispatch` peels it off before asking.
    fn name(&self) -> &str {
        match self {
            Event::Exited { name, .. }
            | Event::WaitFailed { name, .. }
            | Event::Ready { name, .. }
            | Event::ProbeFailed { name, .. } => name,
            // Neither is about a service. Named rather than caught by a
            // wildcard, so a future variant has to answer this question too.
            Event::Control { .. } | Event::SentinelDied { .. } => "",
        }
    }

    fn generation(&self) -> u64 {
        match self {
            Event::Exited { generation, .. }
            | Event::WaitFailed { generation, .. }
            | Event::Ready { generation, .. }
            | Event::ProbeFailed { generation, .. } => *generation,
            Event::Control { .. } | Event::SentinelDied { .. } => 0,
        }
    }
}

/// The outcome of asking whether a unit's dependencies let it start.
enum Verdict {
    Go,
    /// Keep waiting; the instant is when to reconsider, if anything schedules
    /// that on its own rather than on the next event.
    Wait(Option<Instant>),
}

pub fn log(msg: &str) {
    crate::emit_err(&format!("td-svc: {msg}\n"));
}

/// Build the child process for a unit.
///
/// `process_group(0)` is applied to every unit EXCEPT one with `tty=`. A
/// `tty=` unit's program must `setsid()` to claim its controlling terminal, and
/// `setsid(2)` is EPERM for a process that is already a process-group leader —
/// which `process_group(0)` makes it. Grouping it would produce a console with
/// no controlling terminal on every boot. Nothing is lost: `setsid()` creates a
/// new group itself, with pgid == pid.
///
/// No `pre_exec` (DESIGN.md I2): td-svc is multithreaded, and a closure running
/// between fork and exec may touch nothing that allocates or locks.
///
/// `report` gates the terminal diagnostics. They are per-SPAWN, so a unit that
/// crash-loops would repeat them at the restart rate forever — the console
/// scrolling DESIGN.md §6 says the backoff gate exists to prevent. The restart
/// message is gated and these were not, which is worse than it sounds: the
/// ungated one is the message that explains WHY.
fn build(unit: &Unit, report: bool, captured: bool) -> Result<(Command, Vec<String>), String> {
    let mut said: Vec<String> = Vec::new();
    let prog = unit.argv.first().ok_or_else(|| "empty exec".to_string())?;
    let mut cmd = Command::new(prog);
    cmd.args(unit.argv.get(1..).unwrap_or(&[]));
    // Nothing supervised reads td-svc's stdin; a daemon that inherited it would
    // steal console input.
    cmd.stdin(Stdio::null());

    match &unit.tty {
        Some(_) => {
            // A console unit's own startup failures ("cannot open /dev/ttyS0")
            // must reach somewhere a human will look; without this they vanish
            // and the machine is up with a silent, missing greeter.
            //
            // O_NOCTTY for the same reason every other terminal open in this
            // tree carries it: td-svc must never acquire a controlling terminal
            // as a side effect of writing a diagnostic to it.
            use std::os::unix::fs::OpenOptionsExt;
            match std::fs::OpenOptions::new()
                .write(true)
                .custom_flags(O_NOCTTY)
                .open(CONSOLE)
            {
                Ok(console) => {
                    cmd.stderr(Stdio::from(console));
                }
                Err(e) => {
                    if report {
                        said.push(format!("{}: {CONSOLE}: {e}", unit.name));
                    }
                }
            }
        }
        None => {
            use std::os::unix::process::CommandExt;
            cmd.process_group(0);
        }
    }
    // Captured output goes through pipes rather than td-svc's own stdio. A
    // `tty=` unit never reaches here with a log: the table refuses the pair,
    // because a pipe is not a terminal and job control needs one (§7).
    //
    // `captured` is the writer EXISTING, not the unit merely asking for one. A
    // pipe td-svc holds and never reads is the worst outcome available here:
    // the service blocks in `write(2)` the moment it fills the buffer, which
    // is precisely the wedge the whole bounded-queue design exists to prevent.
    // Without a writer the unit inherits td-svc's stdio, exactly as it did
    // before capture existed.
    if unit.log.is_some() && captured {
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    }
    Ok((cmd, said))
}

/// Where a `tty=` value points. Bare names are under `/dev`, matching the
/// inittab id field td-init resolves the same way.
fn tty_path(tty: &str) -> String {
    if tty.starts_with('/') {
        tty.to_string()
    } else {
        format!("/dev/{tty}")
    }
}

/// Open a terminal read-write, without letting it become td-svc's own.
fn open_tty(path: &str) -> io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(O_NOCTTY)
        .open(path)
}

/// Stop a service's log writer, so it closes its file and exits.
///
/// Called whenever a `Service` stops being the thing that owns that capture —
/// dropped by a reload, or re-pointed at a different destination. Without it
/// the writer waits on a queue nothing can reach, holding a `/var` descriptor
/// that `close_logs` no longer reaches and so cannot release.
fn release_capture(service: &Service) {
    if let Some(capture) = &service.log {
        crate::logs::retire(capture);
    }
}

/// Hand a terminal to a unit that asked for one.
///
/// A failure here does NOT stop the unit: per I5 a console that cannot be
/// started is the worst outcome, so refusing to spawn it would be the skip the
/// invariant forbids. It falls back to `/dev/console` — the last-resort
/// terminal every other diagnostic here already goes to — and only if THAT
/// fails does the unit run without one.
///
/// The fallback matters because `build` has already set stdin to `null`. An
/// earlier draft logged "inheriting stdio" and left it null, so a greeter whose
/// terminal was missing got a shell reading immediate EOF: it exited at once,
/// every time, and the restart loop turned a missing device into a spin.
/// Wire a `tty=` unit's stdin/stdout to its terminal, RETURNING the diagnostics
/// it would emit rather than logging them.
///
/// Returning them is what makes the gate testable: `report` decides whether a
/// message is built at all, so a test can assert the result is empty at
/// `report=false` and non-empty at `report=true` without a log sink. Asserting
/// on `backoff::should_report` alone tests `backoff`, not this.
fn attach_tty(cmd: &mut Command, unit: &Unit, report: bool) -> Attached {
    let mut said = Vec::new();
    let Some(tty) = &unit.tty else {
        return Attached {
            said,
            device: None,
        };
    };
    let path = tty_path(tty);
    // Gated for the same reason as `build`'s: a greeter whose terminal is
    // missing restarts forever, and an ungated line here scrolls the console at
    // the restart rate with the one message that would have explained it. The
    // check wraps each `format!` rather than the push so a silent restart does
    // not build the string either.
    let opened = match open_tty(&path) {
        Ok(file) => Some(file),
        Err(e) => {
            if report {
                said.push(format!(
                    "{}: {path}: {e}; falling back to {CONSOLE}",
                    unit.name
                ));
            }
            match open_tty(CONSOLE) {
                Ok(file) => Some(file),
                Err(e) => {
                    if report {
                        said.push(format!(
                            "{}: {CONSOLE}: {e}; it will have no terminal",
                            unit.name
                        ));
                    }
                    None
                }
            }
        }
    };
    let Some(file) = opened else {
        return Attached {
            said,
            device: None,
        };
    };
    // Read off the OPEN descriptor, before anything can replace the node, and
    // after the `/dev/console` fallback has had its say.
    let device = attached_device(&file);
    match file.try_clone() {
        Ok(out) => {
            cmd.stdin(Stdio::from(file)).stdout(Stdio::from(out));
        }
        Err(e) => {
            if report {
                said.push(format!("{}: {path}: {e}", unit.name));
            }
            // The child was never wired to this terminal, so it is not its
            // containment. Recording it anyway would aim a stop at every
            // process on a device this service does not hold.
            return Attached { said, device: None };
        }
    }
    Attached { said, device }
}

/// What `attach_tty` did: anything worth saying, and the device it ACTUALLY
/// attached — which is not always the one the unit named.
struct Attached {
    said: Vec<String>,
    device: Option<i32>,
}

/// Send one signal to one target. `target` is a pid, or `-<pgid>` for a group.
///
/// **ESRCH is success.** I3 forbids inferring liveness from a signal's result,
/// and "nothing is there" is exactly such an inference — it is also racy by
/// construction, since the target may die between the `/proc` read that chose
/// it and this call. The stop path learns a service is down by watching its
/// containment empty, never from here. What DOES come back as an error is a
/// signal that could not be sent at all (EPERM, EINVAL): that means td-svc
/// cannot signal, and every "killing it" line above it would describe an
/// action that never happened.
fn send_signal(target: i32, signal: i32) -> Result<(), String> {
    // Two targets `kill(2)` accepts and td-svc never means. Neither is a
    // process or a group: `0` is the CALLER's process group — td-svc and every
    // service sharing it — and `-1` is a broadcast to every process this
    // process may signal, which is not "process group 1" however it was
    // arrived at. Both are reachable by arithmetic rather than by intent: a
    // containment whose pgid read back as 1 negates to -1, and one that read
    // back as 0 stays 0. Refused here, at the last point before the kernel,
    // because that is the only place every caller passes through.
    if target == 0 || target == -1 {
        let why = format!("refusing to signal {target}: that is not a process or a group");
        log(&why);
        return Err(why);
    }
    match crate::sys::kill(target, signal) {
        Ok(()) => Ok(()),
        // ESRCH. `io::ErrorKind` has no variant for it on stable, so match the
        // raw errno rather than a kind that does not exist yet.
        Err(e) if e.raw_os_error() == Some(ESRCH) => Ok(()),
        Err(e) => {
            let why = format!("cannot signal {target}: {e}; nothing was signalled");
            log(&why);
            Err(why)
        }
    }
}

/// `ESRCH` — no such process. Not in `std` as a named constant.
const ESRCH: i32 = 3;

/// Spawn a helper thread, reporting rather than panicking if the OS refuses.
/// `std::thread::spawn` PANICS on failure, and `panic=abort` would make that
/// the end of the supervisor.
///
/// The caller must handle `false`. Every helper here is the only thing that
/// will ever report on a service, so a thread that did not start means an
/// event that never arrives — and the unit would sit at `Starting` forever,
/// blocking its dependents with no diagnostic beyond this one line.
#[must_use]
pub fn spawn_thread<F: FnOnce() + Send + 'static>(what: &str, body: F) -> bool {
    // Named so `ps -T` can tell one waiter from another. Sanitised rather than
    // passed through: `Builder::name` PANICS on an interior NUL, and relying on
    // the table's charset validation puts a no-panic guarantee in another
    // module — one future caller with an unvalidated string aborts td-svc for a
    // debugging nicety.
    let label: String = what
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        .collect();
    match std::thread::Builder::new().name(label).spawn(body) {
        Ok(_) => true,
        Err(e) => {
            log(&format!("{what}: could not start a helper thread: {e}"));
            false
        }
    }
}

/// Where a shutdown ends. The three td-init applets that call `reboot(2)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Power {
    Reboot,
    /// `poweroff`.
    Off,
    Halt,
}

impl Power {
    /// The word the control verb uses, the word the marker carries, and the
    /// applet's basename — deliberately the same string, so a resume cannot
    /// disagree with the request that wrote it.
    fn as_str(self) -> &'static str {
        match self {
            Power::Reboot => "reboot",
            Power::Off => "poweroff",
            Power::Halt => "halt",
        }
    }

    fn parse(word: &str) -> Option<Power> {
        match word {
            "reboot" => Some(Power::Reboot),
            "poweroff" => Some(Power::Off),
            "halt" => Some(Power::Halt),
            _ => None,
        }
    }

    fn applet(self) -> String {
        format!("/bin/{}", self.as_str())
    }
}

/// A shutdown transition in flight.
///
/// Services are stopped ONE AT A TIME in reverse plan order, because that is
/// the only thing that makes the order mean anything: a dependent must be down
/// before what it depends on. Each is bounded, so a service that will not die
/// costs its own timeout rather than the machine.
struct Shutdown {
    power: Power,
    /// What to stop, in START order; the teardown walks it backwards.
    ///
    /// Captured once, at `begin_shutdown`, rather than read from `order` each
    /// pass — and it is NOT just `order`. A unit a `reload` dropped from the
    /// table is in no plan and no start order, but if it is still on its way
    /// down it is still a running process, and walking only `order` would run
    /// `/etc/shutdown` while it held a filesystem open. Those go at the END
    /// here, so the reverse walk reaches them FIRST: nothing declared depends
    /// on a unit that is no longer declared.
    walk: Vec<usize>,
    /// How far down the reversed walk we have got.
    cursor: usize,
    /// The service being stopped now, and when to give up on it.
    current: Option<(usize, Instant)>,
}

pub struct Runtime {
    services: Vec<Service>,
    /// Indices into `services`, in start order. Indices rather than names so
    /// the loop does not clone a `Vec<String>` on every wake.
    order: Vec<usize>,
    tx: Sender<Event>,
    rx: Receiver<Event>,
    self_pid: i32,
    /// td-svc's own group, session and controlling terminal, read once at
    /// startup — nothing moves it between them, since it calls neither `setsid`
    /// nor `setpgid`, and it never claims a terminal (every open it makes
    /// carries `O_NOCTTY`).
    ///
    /// `None` means `/proc/self/stat` was unreadable, and then NO group,
    /// session or terminal may be signalled at all: without knowing its own ids td-svc
    /// cannot prove a target is not itself, and the guard's whole job is to be
    /// sure. (An earlier draft substituted `self_pid` here and called that
    /// stricter. It is the reverse — a pid never equals a real pgid held by
    /// something else, so every comparison passed and the guard was off exactly
    /// when `/proc` was least trustworthy.)
    self_ids: Option<procfs::Stat>,
    /// The table this was loaded from, carried only so the "no units" complaint
    /// can name the file an operator has to go look at.
    table_path: String,
    /// The shutdown transition, once one has begun. Monotonic (**I6**): it is
    /// set once and never cleared, so nothing can start a service after the
    /// teardown has touched one.
    shutdown: Option<Shutdown>,
    /// The armed Ctrl-Alt-Del sentinel, if arming succeeded. Holding it is what
    /// keeps the sentinel blocked; `None` means presses are not being caught.
    cad: Option<crate::cad::Armed>,
    /// The two sysctls arming writes. Fields rather than the constants used
    /// directly, for the same reason `marker_path` is one — and here it is not
    /// only about testability: a test that reached the REAL sysctls would
    /// disarm Ctrl-Alt-Del on the machine running the suite, and point its
    /// kernel at a pid belonging to the test harness.
    cad_enabled_path: String,
    cad_pid_path: String,
    /// When to try arming again, and how many consecutive attempts have failed.
    /// A sentinel that cannot stay alive would otherwise be respawned as fast as
    /// the loop can turn; these throttle that to `backoff::CAP`. The count
    /// resets when a sentinel lived at least `backoff::MIN_UPTIME` — the same
    /// evidence, and the same threshold, that resets a service's restart count.
    cad_retry_at: Option<Instant>,
    cad_failures: u32,
    /// Where this supervisor records what it started, so its SUCCESSOR can
    /// evict it. A field for the same reason `marker_path` is one: the whole
    /// interesting behaviour is what happens to a machine that already has a
    /// record, and a test that reached the real path would evict processes
    /// belonging to the host running the suite.
    started_path: String,
    /// Orphans this supervisor could NOT prove gone: signalled and still there,
    /// or unverifiable. They stay in the record so the NEXT supervisor tries
    /// again — dropping them would let a td-svc that dies after a failed
    /// eviction hand its successor a clean file and a running duplicate.
    unevicted: Vec<crate::evict::Entry>,
    /// Where the I6 marker lives. A field, not the constant used directly, for
    /// the same reason `table_path` is one: every interesting property of the
    /// marker — that it is written BEFORE the first stop, that a resume reads
    /// it, that a torn one still resumes — is only testable against a path a
    /// test can write. The shipped binary always passes `SHUTDOWN_MARKER`.
    marker_path: String,
}

impl Runtime {
    pub fn new(units: Vec<Unit>, table_path: &str) -> (Runtime, Vec<String>) {
        let plan = order::plan(&units);
        let complaints = plan.complaints();
        // A unit is pre-failed only if it is skipped AND absent from the order.
        // A console unit is in BOTH — `skipped` records why its ordering could
        // not be honoured, while the order still starts it (DESIGN.md I5) — so
        // keying on `skipped` alone would mark the console failed and never
        // start it, reintroducing the hole the forcing exists to close.
        let mut services: Vec<Service> = Vec::new();
        for unit in units {
            let mut service = Service::new(unit);
            let name = &service.unit.name;
            let skipped = plan.skipped.iter().any(|(n, _)| n == name);
            let ordered = plan.order.iter().any(|n| n == name);
            if skipped && !ordered {
                service.phase = Phase::Failed;
            }
            // In BOTH is the forced console: skipped records why its ordering
            // could not be honoured, the order starts it anyway.
            service.forced = skipped && ordered;
            services.push(service);
        }
        let order = plan
            .order
            .iter()
            .filter_map(|name| services.iter().position(|s| &s.unit.name == name))
            .collect();
        let (tx, rx) = std::sync::mpsc::channel();
        let self_pid = std::process::id() as i32;
        let own = match procfs::stat_of(self_pid) {
            Ok(stat) => stat,
            Err(e) => {
                log(&format!(
                    "cannot read own /proc entry ({e}); no process group or \
                     session will be signalled"
                ));
                None
            }
        };
        (
            Runtime {
                services,
                order,
                tx,
                rx,
                self_pid,
                self_ids: own,
                table_path: table_path.to_string(),
                shutdown: None,
                cad: None,
                cad_enabled_path: crate::cad::CAD_ENABLED.to_string(),
                cad_pid_path: crate::cad::CAD_PID.to_string(),
                cad_retry_at: None,
                cad_failures: 0,
                started_path: crate::evict::STATE.to_string(),
                unevicted: Vec::new(),
                marker_path: SHUTDOWN_MARKER.to_string(),
            },
            complaints,
        )
    }

    // Named `lookup`, and written with `position` rather than the obvious
    // iterator search: every source here is embedded verbatim into the recipe,
    // and the ladder guard scans that text for host-tool names the combinator
    // happens to share. Same constraint td-util and td-sh document.
    fn lookup(&self, name: &str) -> Option<&Service> {
        let at = self.services.iter().position(|s| s.unit.name == name)?;
        self.services.get(at)
    }

    fn lookup_mut(&mut self, name: &str) -> Option<&mut Service> {
        let at = self.services.iter().position(|s| s.unit.name == name)?;
        self.services.get_mut(at)
    }

    /// Are this unit's dependencies settled? Both `after=` and `requires=`
    /// order; only `requires=` gates on success.
    fn deps_settled(&self, unit: &Unit) -> bool {
        unit.after
            .iter()
            .chain(unit.requires.iter())
            .all(|dep| match self.lookup(dep) {
                None => true,
                Some(service) => service.phase.settled(),
            })
    }

    /// May this unit start now, or must it keep waiting?
    ///
    /// `Wait` carries the instant to wake at, if there is one — a console's
    /// patience deadline. Without it the loop would only reconsider a waiting
    /// console when some other event happened to land.
    fn dependency_verdict(&self, index: usize, now: Instant) -> Verdict {
        let Some(service) = self.services.get(index) else {
            return Verdict::Wait(None);
        };
        // A console the plan could not order was started with its ordering
        // ignored; honouring it here would contradict the complaint that says
        // so, and deadlock on dependencies already called unsatisfiable.
        if service.forced {
            return Verdict::Go;
        }
        if self.deps_settled(&service.unit) {
            return Verdict::Go;
        }
        if !service.unit.is_console() {
            return Verdict::Wait(None);
        }
        // I5: a console's wait is bounded. Everything else may wait forever;
        // this one starts anyway and says what it gave up on.
        let since = service.waiting_since.unwrap_or(now);
        if now.duration_since(since) >= CONSOLE_PATIENCE {
            log(&format!(
                "{}: its dependencies have not settled in {CONSOLE_PATIENCE:?}; \
                 starting it anyway — a console is never withheld",
                service.unit.name
            ));
            return Verdict::Go;
        }
        Verdict::Wait(since.checked_add(CONSOLE_PATIENCE))
    }

    /// A strict dependency that failed skips this unit — unless it is the
    /// console, which is never skippable (DESIGN.md I5). The table rejects
    /// `requires=` on a console unit, so this is the second line of that.
    fn requires_failed(&self, unit: &Unit) -> Option<String> {
        if unit.is_console() {
            return None;
        }
        for dep in &unit.requires {
            // `Held` counts. It is a daemon that has failed so many times in a
            // row that retries are capped at five minutes — strictly worse news
            // than a single failure, and gating on `Failed` alone let a strict
            // dependent start while what it requires was crash-looping.
            // `Stopped` counts too. It settles for ORDERING (`after=` waits
            // for a decision, and an operator's stop is one), but `requires=`
            // asks whether the dependency is THERE — and a service someone
            // stopped is exactly as absent as one that failed.
            // A stop IN FLIGHT counts for the same reason `Stopped` does, and
            // it has to be read off the flag rather than the phase: the phase
            // stays `Ready` until the stop completes, so a unit whose leader is
            // already reaped would otherwise satisfy a strict dependency while
            // nothing of it is left to depend on.
            let failed = self.lookup(dep).is_some_and(|s| {
                s.stopping || matches!(s.phase, Phase::Failed | Phase::Held | Phase::Stopped)
            });
            if failed {
                return Some(dep.clone());
            }
        }
        None
    }

    fn start(&mut self, index: usize) {
        let Some(service) = self.services.get(index) else {
            return;
        };
        let unit = service.unit.clone();
        let generation = service.generation.wrapping_add(1);
        // Same gate the restart message uses: loud on the first attempt and on
        // the escalation into the hold, silent in between.
        //
        // The `+ 1` is NOT a variant of the spelling at the other two call
        // sites; all three ask "is the failure this attempt would become a loud
        // one?", and only the state differs — `on_exit` and
        // `record_start_failure` have already incremented `fast_failures`, this
        // runs BEFORE the attempt that might. Unifying the spellings would move
        // the loud spawn off the escalation attempt, and no test would notice.
        let report = backoff::should_report(service.fast_failures.saturating_add(1));

        // BEFORE the build, because the build decides whether to wire pipes
        // and that decision has to know whether anything will read them.
        let captured = self.ensure_capture(index, &unit, report);
        let mut cmd = match build(&unit, report, captured) {
            Ok((cmd, said)) => {
                for line in said {
                    log(&line);
                }
                cmd
            }
            Err(e) => {
                self.record_start_failure(index, &format!("{}: {e}", unit.name));
                return;
            }
        };
        let attached = attach_tty(&mut cmd, &unit, report);
        for said in &attached.said {
            log(said);
        }

        match cmd.spawn() {
            Ok(mut child) => {
                let pid = child.id() as i32;
                let cancel = Arc::new(AtomicBool::new(false));
                // Read before anything else: field 22 is set at fork, so it is
                // valid the instant `spawn` returns — unlike pgrp/session,
                // which the child has not had a chance to change yet.
                let starttime = procfs::stat_of(pid).ok().flatten().map(|s| s.starttime);
                if let Some(service) = self.services.get_mut(index) {
                    service.pid = Some(pid);
                    service.starttime = starttime;
                    service.tty_dev = attached.device;
                    service.killed = false;
                    service.generation = generation;
                    service.started = Some(Instant::now());
                    service.retry_at = None;
                    service.phase = Phase::Starting;
                    service.cancel = Some(Arc::clone(&cancel));
                    service.deadline = match (unit.kind, unit.timeout) {
                        (Kind::Oneshot, Some(t)) => Instant::now().checked_add(t),
                        _ => None,
                    };
                }
                // Drained before the waiter, which MOVES the child: the pipe
                // ends live on it, and an unread pipe fills and blocks the
                // service.
                //
                // Recorded as soon as the identity is known, and before the
                // waiter: everything after this point can fail, and a service
                // that is already RUNNING has to be in the record whether or
                // not the rest of the start succeeds. The window this cannot
                // close is between `spawn` returning and this write — a td-svc
                // killed inside it leaves an orphan its successor never hears
                // about — and it is unclosable, because the pid does not exist
                // to record until `spawn` returns.
                self.attach_logs(index, &mut child, &unit);
                self.persist_started();
                if !self.watch(child, unit.name.clone(), generation) {
                    self.abandon(index, pid, &unit.name);
                    return;
                }
                if unit.kind == Kind::Daemon {
                    let name = unit.name.clone();
                    if unit.ready.is_empty() {
                        if let Some(service) = self.services.get_mut(index) {
                            service.phase = Phase::Ready;
                        }
                    } else if !self.probe(unit, generation, cancel) {
                        // Same shape: readiness would never be reported.
                        if let Some(service) = self.services.get_mut(index) {
                            service.phase = Phase::Failed;
                        }
                        log(&format!("{name}: cannot probe it; not ready"));
                    }
                }
            }
            Err(e) => self.record_start_failure(index, &format!("{}: {e}", unit.name)),
        }
    }

    /// A child we spawned but cannot watch: `Builder::spawn` consumed the
    /// `Child` and then failed, so nothing can wait on it or kill it through
    /// `std` any more.
    ///
    /// This is TERMINAL — no retry, whatever `restart=` says. Retrying would
    /// start a SECOND instance of a service whose first instance is still
    /// running and now unsupervised: two sshds on one port, two greeters on one
    /// terminal. Better one degraded service than a duplicate nobody owns. The
    /// pid is signalled through `/bin/kill` because that is the only handle
    /// left; it stays a zombie until td-svc exits, which is the lesser leak.
    fn abandon(&mut self, index: usize, pid: i32, name: &str) {
        let mode = Containment::Process(pid);
        log(&format!(
            "{name}: no waiter thread; killing pid {pid} rather than leaving \
             it unsupervised, and not retrying"
        ));
        let _ = self.signal(mode, crate::sys::SIGKILL);
        if let Some(service) = self.services.get_mut(index) {
            service.phase = Phase::Failed;
            service.pid = None;
            service.starttime = None;
            service.started = None;
            service.deadline = None;
            service.kill_at = None;
            service.retry_at = None;
            if let Some(cancel) = service.cancel.take() {
                cancel.store(true, Ordering::Relaxed);
            }
        }
    }

    /// A unit that could not be spawned at all.
    ///
    /// It becomes `Failed` immediately — not `Down`-with-a-retry — so its
    /// dependents proceed at once. A missing binary and a binary that runs and
    /// exits non-zero are the same news to everything downstream, and used to
    /// differ by the ~7 minutes backoff takes to reach the hold.
    fn record_start_failure(&mut self, index: usize, message: &str) {
        let Some(service) = self.services.get_mut(index) else {
            return;
        };
        service.pid = None;
        service.starttime = None;
        service.started = None;
        service.deadline = None;
        service.kill_at = None;
        service.fast_failures = service.fast_failures.saturating_add(1);
        let restarts = service.unit.kind == Kind::Daemon
            && !matches!(service.unit.restart, Restart::Never);
        if !restarts {
            // Nothing will retry it, so say so once and stop.
            service.phase = Phase::Failed;
            service.retry_at = None;
            log(&format!("{message}; not retried"));
            return;
        }
        let delay = backoff::delay(service.fast_failures);
        let report = backoff::should_report(service.fast_failures);
        service.retry_at = Instant::now().checked_add(delay);
        // Same escalation as an exit: a binary that never appears must reach
        // the capped hold rather than retrying at the base delay forever.
        service.phase = if delay >= backoff::CAP {
            Phase::Held
        } else {
            Phase::Failed
        };
        if report {
            log(&format!("{message}; retrying in {delay:?}"));
        }
    }

    /// One waiter thread per child. It blocks in `wait` and reports; it holds no
    /// state and takes no lock, so a wedged child wedges only its thread.
    fn watch(&self, mut child: Child, name: String, generation: u64) -> bool {
        let tx = self.tx.clone();
        spawn_thread(&name.clone(), move || {
            let event = match child.wait() {
                Ok(status) => Event::Exited {
                    name,
                    generation,
                    code: status.code(),
                },
                Err(e) => Event::WaitFailed {
                    name,
                    generation,
                    error: e.to_string(),
                },
            };
            let _ = tx.send(event);
        })
    }

    /// Readiness probing runs off the main loop: a probe that hangs must not
    /// hang the supervisor, so it gets its own thread, a per-ATTEMPT deadline,
    /// and a cancel flag set when the instance it is probing dies.
    fn probe(&self, unit: Unit, generation: u64, cancel: Arc<AtomicBool>) -> bool {
        let tx = self.tx.clone();
        let name = unit.name.clone();
        spawn_thread(&name.clone(), move || {
            // A deadline that cannot be computed fails CLOSED. `is_none_or`
            // here would run the loop forever, which is the one outcome a
            // readiness timeout exists to rule out.
            let deadline = Instant::now().checked_add(unit.ready_timeout);
            while deadline.is_some_and(|end| Instant::now() < end) {
                if cancel.load(Ordering::Relaxed) {
                    return;
                }
                if probe_once(&unit) {
                    let _ = tx.send(Event::Ready { name, generation });
                    return;
                }
                std::thread::sleep(PROBE_GAP);
            }
            let _ = tx.send(Event::ProbeFailed { name, generation });
        })
    }

    /// Start every unit whose turn has come, returning the earliest pending
    /// wake-up so the loop's block stays bounded.
    fn start_eligible(&mut self) -> Option<Instant> {
        let mut next_wake: Option<Instant> = None;
        let now = Instant::now();
        // Once the teardown has begun NOTHING starts again — not a restart, not
        // a pending retry, not a console (I5 yields to I6 here: a console on a
        // machine whose filesystems are going away is not a rescue). The timer
        // collection below still runs, because the stops in flight are driven
        // by exactly those deadlines.
        let starting = self.shutdown.is_none();
        for position in 0..self.order.len() {
            if !starting {
                break;
            }
            let Some(&index) = self.order.get(position) else {
                continue;
            };
            let Some(service) = self.services.get(index) else {
                continue;
            };
            // Eligible when it has never run, or when a retry is due. `Failed`
            // with a `retry_at` is a restarting daemon, not a dead unit.
            let eligible = match service.phase {
                Phase::Down | Phase::Held => true,
                Phase::Failed => service.retry_at.is_some(),
                // Stopped is a standing decision by an operator, not a state to
                // recover from. Only an explicit `start` leaves it.
                Phase::Starting | Phase::Ready | Phase::Stopped => false,
            };
            if !eligible {
                continue;
            }
            if let Some(at) = service.retry_at {
                if at > now {
                    next_wake = Some(match next_wake {
                        Some(w) if w < at => w,
                        _ => at,
                    });
                    continue;
                }
            }
            match self.dependency_verdict(index, now) {
                Verdict::Go => {}
                Verdict::Wait(at) => {
                    if let Some(at) = at {
                        next_wake = Some(match next_wake {
                            Some(w) if w < at => w,
                            _ => at,
                        });
                    }
                    if let Some(service) = self.services.get_mut(index) {
                        service.waiting_since.get_or_insert(now);
                    }
                    continue;
                }
            }
            let Some(service) = self.services.get(index) else {
                continue;
            };
            if let Some(dep) = self.requires_failed(&service.unit) {
                let name = service.unit.name.clone();
                if let Some(service) = self.services.get_mut(index) {
                    service.phase = Phase::Failed;
                    service.retry_at = None;
                }
                log(&format!("{name}: skipped, requires '{dep}' which failed"));
                continue;
            }
            self.start(index);
        }
        // A oneshot's timeout, the KILL that follows it, and a stop's pending
        // re-scan are the other things that can need a wake-up. `next_sweep`
        // belongs here or it is not a timer at all: nothing else wakes the loop
        // when a containment drains, so the stop would wait on whatever
        // unrelated deadline happens to be nearest — a crash-looper parked at
        // the 5-minute backoff cap, with the console down for all of it.
        for service in &self.services {
            for at in [service.deadline, service.kill_at, service.next_sweep]
                .into_iter()
                .flatten()
            {
                next_wake = Some(match next_wake {
                    Some(w) if w < at => w,
                    _ => at,
                });
            }
        }
        next_wake
    }

    /// Stop any oneshot that has outrun its `timeout=`, and KILL any that has
    /// since outrun its `stop-timeout=` too. Without the first the unit stays
    /// `Starting` forever and blocks every dependent silently; without the
    /// second a process that ignores TERM runs on for good, holding its waiter
    /// thread with it.
    fn enforce_deadlines(&mut self) {
        let now = Instant::now();
        for index in 0..self.services.len() {
            let due = self
                .services
                .get(index)
                .map(|s| (s.deadline.is_some_and(|at| at <= now), s.kill_at.is_some_and(|at| at <= now)));
            match due {
                Some((true, _)) => self.time_out(index),
                Some((false, true)) => self.escalate(index),
                _ => {}
            }
            // A stop whose leader is already reaped but whose containment was
            // still occupied. Nothing will wake td-svc when the last survivor
            // exits — survivors are not its children — so this timer is the
            // only path from `stopping` to `Stopped` for those units.
            let sweep = self.services.get(index).is_some_and(|s| {
                s.stopping && s.pid.is_none() && s.next_sweep.is_some_and(|at| at <= now)
            });
            if sweep {
                self.finish_stop(index);
            }
        }
    }

    /// First half: the unit overran `timeout=`. TERM it, release its
    /// dependents, and schedule the KILL.
    fn time_out(&mut self, index: usize) {
        let Some(service) = self.services.get_mut(index) else {
            return;
        };
        let name = service.unit.name.clone();
        let timeout = service.unit.timeout;
        let containment = service.containment();
        service.deadline = None;
        service.phase = Phase::Failed;
        // Giving up is a DECISION, so it must not be undone by the exit that
        // follows it: a unit TERMed for overrunning its timeout can still exit
        // 0 in the race window, and that would flip it back to ready. Bumping
        // the generation drops the pending event as stale. The pid STAYS —
        // it is the only handle on a process that has not died yet.
        service.generation = service.generation.wrapping_add(1);
        service.kill_at = Instant::now().checked_add(service.unit.stop_timeout);
        if let Some(cancel) = service.cancel.take() {
            cancel.store(true, Ordering::Relaxed);
        }
        match timeout {
            Some(t) => log(&format!("{name}: still running after {t:?}; giving up on it")),
            None => log(&format!("{name}: giving up on it")),
        }
        // The waiter thread still owns the `Child`, so the process is reaped
        // through the normal path whenever it does die.
        match containment {
            Ok(Some(mode)) => {
                let _ = self.signal(mode, crate::sys::SIGTERM);
            }
            Ok(None) => {}
            Err(e) => log(&format!("{name}: cannot read /proc to stop it: {e}")),
        }
    }

    /// Second half: it ignored the TERM. KILL it and let go of the pid.
    ///
    /// The identity check is not optional. `stop-timeout` elapsed since the
    /// TERM, and in that window the child can have died, been reaped by its
    /// waiter, and had its pid handed to something new — which this would then
    /// kill. `/proc` field 22 is what a pid alone cannot tell us.
    fn escalate(&mut self, index: usize) {
        let Some(service) = self.services.get_mut(index) else {
            return;
        };
        let name = service.unit.name.clone();
        let stopping = service.stopping;
        let killed_before = service.killed;
        let recorded = service.stop_scope;
        let derived = service.containment();
        let same = match (service.pid, service.starttime) {
            (Some(pid), Some(starttime)) => procfs::is_same_process(pid, starttime),
            // No recorded identity means no proof it is still the same process,
            // so nothing is signalled. Failing closed here costs at most a
            // leaked process; failing open costs an unrelated one.
            _ => Ok(false),
        };
        service.kill_at = None;
        // Keep the identity. Releasing a pid we could not read means a later
        // `start` spawns a SECOND instance of a service that may still be
        // alive — the exact duplicate `abandon` refuses to create. Fails
        // closed: at worst the KILL is retried on the next sweep.
        let same = match same {
            Ok(same) => same,
            Err(e) => {
                log(&format!("{name}: cannot read /proc to kill it: {e}"));
                // Re-arm, or the promised retry never happens: `kill_at` was
                // cleared above and nothing else would wake this unit.
                if let Some(service) = self.services.get_mut(index) {
                    if service.kill_at.is_none() {
                        service.kill_at = Instant::now().checked_add(service.unit.stop_timeout);
                    }
                }
                return;
            }
        };
        let target = if stopping {
            // A requested stop never consults `derived`, so an unreadable
            // `/proc` there is not a reason to skip the KILL.
            kill_target(true, same, recorded, None)
        } else {
            match derived {
                Ok(derived) => kill_target(false, same, recorded, derived),
                Err(e) => {
                    log(&format!("{name}: cannot read /proc to kill it: {e}"));
                    // Re-arm here for the same reason as above: `kill_at` was
                    // cleared, and without a deadline this unit is never
                    // killed, never retried, and never released.
                    if let Some(service) = self.services.get_mut(index) {
                        if service.kill_at.is_none() {
                            service.kill_at =
                                Instant::now().checked_add(service.unit.stop_timeout);
                        }
                    }
                    return;
                }
            }
        };
        // Once a KILL has gone out the containment still has to be confirmed
        // empty before the unit is `Stopped`, and the survivors' exit is not an
        // event td-svc gets. Make sure a sweep is pending to do that.
        if stopping {
            let Some(service) = self.services.get_mut(index) else {
                return;
            };
            if service.next_sweep.is_none() {
                service.next_sweep = Instant::now().checked_add(STOP_SWEEP_INTERVAL);
            }
        }
        let sent = match target {
            Some(mode) => {
                // Said once per stop. `escalate` re-arms on every failed send
                // or unreadable `/proc`, so a permanent fault — no `/bin/kill`,
                // a wedged containment — would otherwise print this every
                // `stop-timeout`, forever, per unit.
                if !killed_before {
                    if same {
                        log(&format!("{name}: did not exit on TERM; killing it"));
                    } else {
                        log(&format!(
                            "{name}: its containment did not empty on TERM; killing what is left"
                        ));
                    }
                }
                if let Some(service) = self.services.get_mut(index) {
                    service.killed = true;
                }
                self.signal(mode, crate::sys::SIGKILL)
            }
            None => Ok(()),
        };
        let Some(service) = self.services.get_mut(index) else {
            return;
        };
        // A KILL that did not go out — `/bin/kill` missing, an unenumerable
        // session, or the self-containment refusal — leaves the process ALIVE.
        // Dropping the identity here would make the supervisor forget it, and
        // a later `start` would spawn a duplicate beside something still
        // running. Keep it, and leave a deadline behind: the comment on the
        // `/proc` error path promised a retry that nothing was scheduling.
        if let Err(why) = sent {
            log(&format!("{name}: {why}; keeping its identity to retry"));
            if service.kill_at.is_none() {
                service.kill_at = Instant::now().checked_add(service.unit.stop_timeout);
            }
            return;
        }
        // Only an UNREQUESTED teardown lets go here. For a requested stop the
        // waiter thread still owns the `Child` and the process may outlive the
        // KILL by a moment; clearing the pid now would make the sweep read the
        // containment as empty (a `Console` scope cannot see the leader once
        // its half is narrowed away), declare `Stopped` over a live leader,
        // and then let the late exit through `on_exit`'s restart policy —
        // restarting the very unit an operator stopped. `on_exit` is what
        // releases the identity for a stop, once the leader really is reaped.
        if !stopping {
            service.pid = None;
            service.starttime = None;
        }
    }

    /// Send a signal to a service's whole containment. `kill(1)` is used only to
    /// SEND; liveness is never inferred from its exit status (DESIGN.md I3).
    ///
    /// The first check is the backstop for the whole containment story: a
    /// signal addressed to a group or session td-svc is ITSELF in would take
    /// down the supervisor and everything it supervises. `containment()` is
    /// written never to produce one; this refuses to send it anyway, because
    /// the cost of the two being out of step once is the machine.
    fn signal(&self, mode: Containment, signal: i32) -> Result<(), String> {
        if self.contains_self(mode) {
            let why = format!("refusing to send {signal} to {mode:?}: td-svc is inside it");
            log(&why);
            return Err(why);
        }
        match mode {
            Containment::Process(pid) => send_signal(pid, signal)?,
            // The kernel's own spelling for "everyone in that group". No argv
            // between here and it, so nothing can read the minus as a flag.
            Containment::Group(pgid) => send_signal(-pgid, signal)?,
            // Neither a session nor a terminal has a kill(2) target; the
            // members are enumerated and signalled individually — the
            // `killall5` shape.
            Containment::Session(_) | Containment::Console { .. } => {
                match procfs::members(mode, self.self_pid) {
                    Ok(scan) => {
                        // Best effort, deliberately: one unreadable stranger
                        // must not stop td-svc signalling the ones it read.
                        // (Liveness is the opposite — see procfs::Scan.)
                        for e in &scan.errors {
                            log(&format!("scanning to signal {mode:?}: {e}"));
                        }
                        // Best effort per member, but a signal that is
                        // REFUSED is not a per-member problem — it means
                        // nothing was signalled at all, and the caller must
                        // not treat the escalation as done.
                        let mut failed = None;
                        for pid in scan.pids {
                            if let Err(why) = send_signal(pid, signal) {
                                failed = Some(why);
                            }
                        }
                        if let Some(why) = failed {
                            return Err(why);
                        }
                    }
                    Err(e) => {
                        let why = format!("cannot enumerate {mode:?} to signal it: {e}");
                        log(&why);
                        return Err(why);
                    }
                }
            }
        }
        Ok(())
    }

    /// Would signalling this containment reach td-svc itself? Answers TRUE
    /// when it cannot tell, which is the only safe direction: the cost of a
    /// false yes is one service that does not stop, and of a false no the
    /// supervisor and everything under it.
    fn contains_self(&self, mode: Containment) -> bool {
        match mode {
            Containment::Process(pid) => pid == self.self_pid,
            Containment::Group(pgid) => match self.self_ids {
                Some(own) => pgid == own.pgrp || pgid == self.self_pid,
                None => true,
            },
            Containment::Session(sid) => match self.self_ids {
                Some(own) => sid == own.session || sid == self.self_pid,
                None => true,
            },
            // Both halves must miss td-svc. `leader` is a pid it spawned, so
            // it can only be td-svc's own if something is badly wrong; the
            // device check is the real one, and it catches tty 0 for free
            // because td-svc holds no controlling terminal and so its own
            // field 7 IS 0.
            // Device 0 is refused OUTRIGHT, not merely because td-svc's own
            // field 7 happens to be 0: a td-svc run from a terminal (a test, an
            // operator) has a real one, and then the `own.tty_nr` comparison
            // would let 0 — the set of every daemon on the machine — through.
            Containment::Console { leader, tty } => {
                tty == 0
                    || leader == self.self_pid
                    || match self.self_ids {
                        Some(own) => tty == own.tty_nr,
                        None => true,
                    }
            }
        }
    }

    /// The leader is reaped — is the STOP finished?
    ///
    /// I4 says both halves: reaped AND the containment empty. The leader's exit
    /// is only the first, and for the greeter it is the misleading one, because
    /// the wrapper is a shell whose `getty` child holds the console. So the
    /// recorded scope is re-scanned here, and a stop that still has members
    /// stays a stop: the KILL remains armed and `escalate` will sweep the
    /// survivors.
    ///
    /// An INCOMPLETE scan (`/proc` entries that could not be read) is not
    /// emptiness. `proven_empty` is the fail-closed reading, and taking it any
    /// other way would let the teardown proceed over a service still running.
    fn finish_stop(&mut self, index: usize) {
        let (name, scope) = match self.services.get(index) {
            Some(service) => (service.unit.name.clone(), service.stop_scope),
            None => return,
        };
        // The leader is reaped by the time this runs, so anything keyed on its
        // pid would ask `/proc` about a pid that is no longer ours — and a
        // recycled one answers, keeping the containment occupied for good.
        let scope = without_reaped_leader(scope);
        let remaining = match scope {
            // Nothing was ever signalled (no containment to derive), so there
            // is nothing that could still be running under it.
            None => 0,
            Some(mode) => {
                let scan = procfs::members(mode, self.self_pid);
                if let Err(e) = &scan {
                    log(&format!("{name}: cannot confirm it stopped: {e}"));
                }
                occupancy(scan.as_ref())
            }
        };
        let Some(service) = self.services.get_mut(index) else {
            return;
        };
        if remaining > 0 {
            // Still occupied. Keep `stopping` set so a later sweep lands here
            // again, and leave `kill_at` armed so `escalate` fires on it.
            if service.kill_at.is_none() {
                service.kill_at = Instant::now().checked_add(service.unit.stop_timeout);
            }
            // Only the FIRST occupied scan says so. `enforce_deadlines` comes
            // back every `STOP_SWEEP_INTERVAL` until the containment drains,
            // and a line per pass would bury everything else in the log.
            let first = service.next_sweep.is_none();
            service.next_sweep = Instant::now().checked_add(STOP_SWEEP_INTERVAL);
            if first {
                log(&format!(
                    "{name}: leader exited but {remaining} process(es) remain in its \
                     containment; not stopped yet"
                ));
            }
            return;
        }
        service.stopping = false;
        service.stop_scope = None;
        service.kill_at = None;
        service.next_sweep = None;
        if service.start_after_stop {
            service.start_after_stop = false;
            // Down, not Stopped: `start_eligible` picks it up on the next pass.
            // The failure count is cleared too, so a `restart` of a crash-looping
            // unit tries immediately rather than serving out a backoff the
            // operator asked to interrupt.
            service.phase = Phase::Down;
            service.fast_failures = 0;
            log(&format!("{name}: stopped; restarting as asked"));
        } else {
            service.phase = Phase::Stopped;
            log(&format!("{name}: stopped"));
        }
    }

    /// Re-read the table. TRANSACTIONAL: on any diagnostic, nothing changes.
    ///
    /// The bar is exactly `td-svc check`'s — parse problems AND plan
    /// complaints — because applying the valid fragments of a table an
    /// operator got wrong is how a reload turns one broken unit into a broken
    /// machine. Keeping the last-known-good table means the worst case of a
    /// bad reload is that it did nothing, which is a state the operator can
    /// still fix from the console the old table is still running.
    ///
    /// This is what §3 assigns dependency-cycle recovery to: a unit the plan
    /// dropped cannot be rescued by `start` (nothing walks it), so the only
    /// route back is a corrected table.
    ///
    /// RUNNING processes are not disturbed. A unit whose stanza changed keeps
    /// its current process — the new definition applies at its next start —
    /// because restarting on reload would take the console down as a side
    /// effect of editing an unrelated stanza. A unit that VANISHED from the
    /// table is no longer declared, so it is stopped; it stays visible in
    /// `status` until it is down, rather than disappearing while its process
    /// is still alive.
    fn control_reload(&mut self) -> String {
        let path = self.table_path.clone();
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) => return format!("error: reload: cannot read {path}: {e}; kept the running table\n"),
        };
        let (units, problems) = crate::table::parse(&text);
        if !problems.is_empty() {
            return format!(
                "error: reload: {path} has {} problem(s); kept the running table\n{}",
                problems.len(),
                problems
                    .iter()
                    .map(|p| format!("  {p}\n"))
                    .collect::<String>()
            );
        }
        if units.is_empty() {
            return format!(
                "error: reload: {path} declares no units; kept the running table\n"
            );
        }
        let plan = order::plan(&units);
        let complaints = plan.complaints();
        if !complaints.is_empty() {
            return format!(
                "error: reload: {path} does not resolve; kept the running table\n{}",
                complaints
                    .iter()
                    .map(|c| format!("  {c}\n"))
                    .collect::<String>()
            );
        }

        // Committed. First drop any retired unit whose stop has since finished:
        // `self.order` is rebuilt from scratch below, so this is the one place
        // a `Service` can be removed without invalidating an index somebody
        // holds.
        // A dropped `Service` is the last holder of its capture's handle, so
        // its writer would wait forever on a queue nothing can reach — still
        // holding the /var descriptor that makes `umount` fail, and no longer
        // reachable from `captures()` for `close_logs` to stop.
        for service in self
            .services
            .iter()
            .filter(|s| s.retired && s.pid.is_none() && !s.stopping)
        {
            release_capture(service);
        }
        self.services
            .retain(|s| !(s.retired && s.pid.is_none() && !s.stopping));

        // Carry each surviving unit's RUNTIME state across, keyed by
        // name, and swap in its new definition.
        let mut kept: Vec<Service> = Vec::new();
        let mut retired: Vec<String> = Vec::new();
        for unit in units {
            let existing = self.services.iter().position(|s| s.unit.name == unit.name);
            let mut service = match existing {
                Some(at) => {
                    let mut service = self.services.remove(at);
                    // A capture is keyed to the destination it was opened for,
                    // so one whose `log=`/`console=` changed is retired here:
                    // keeping it would send output to the previous file and
                    // make the reload silently not apply.
                    if service.unit.log != unit.log || service.unit.console != unit.console {
                        release_capture(&service);
                        service.log = None;
                    }
                    service.unit = unit;
                    // Declared again, so no longer retired. If the stop this
                    // unit's REMOVAL started is still in flight, the table now
                    // says it should be up: reuse the restart intent so it
                    // comes back when the stop completes, rather than settling
                    // into `Stopped` — a phase only an explicit `start` leaves.
                    if service.retired && service.stopping {
                        service.start_after_stop = true;
                    }
                    service.retired = false;
                    service
                }
                None => Service::new(unit),
            };
            let name = &service.unit.name;
            let skipped = plan.skipped.iter().any(|(n, _)| n == name);
            let ordered = plan.order.iter().any(|n| n == name);
            if skipped && !ordered && service.pid.is_none() {
                service.phase = Phase::Failed;
            }
            service.forced = skipped && ordered;
            kept.push(service);
        }
        // Whatever is left in `self.services` is no longer declared. One that
        // is still UP is kept, so its stop can be driven and `status` can show
        // it going down; one that is already down is dropped here and now.
        // Keeping those too would leak a `Service` per removed unit per reload,
        // and worse: re-declaring that name later would match the corpse, adopt
        // its `Phase::Stopped` — a standing operator decision `start_eligible`
        // will not override — and the re-added unit would never start.
        let leftover: Vec<Service> = std::mem::take(&mut self.services);
        for mut service in leftover {
            if service.pid.is_none() && !service.stopping {
                log(&format!("{}: no longer in the table", service.unit.name));
                release_capture(&service);
                continue;
            }
            retired.push(service.unit.name.clone());
            service.retired = true;
            kept.push(service);
        }
        self.services = kept;
        self.order = plan
            .order
            .iter()
            .filter_map(|name| self.services.iter().position(|s| &s.unit.name == name))
            .collect();
        for name in &retired {
            // Only stop what is not already stopping. Re-issuing it would reset
            // `kill_at`, so a script reloading once a second would hold the
            // escalation off forever for that unit.
            if self
                .index_of(name)
                .and_then(|i| self.services.get(i))
                .is_some_and(|s| s.stopping)
            {
                continue;
            }
            let reply = self.control_stop(name, false);
            if let Some(why) = reply.strip_prefix("error: ") {
                log(&format!("{name}: no longer declared, and {}", why.trim_end()));
            } else {
                log(&format!("{name}: no longer in the table; stopping it"));
            }
        }
        let mut reply = format!("reloaded {path}\n");
        if !retired.is_empty() {
            reply.push_str(&format!(
                "  stopping {} unit(s) no longer declared: {}\n",
                retired.len(),
                retired.join(", ")
            ));
        }
        reply
    }

    /// Begin the transition, or report that one is already under way.
    ///
    /// The MARKER goes down before a single service is stopped (**I6**). The
    /// order is the whole point: a supervisor that dies after stopping sshd but
    /// before recording why would be respawned by PID 1 and would start sshd
    /// again, against a `/etc/shutdown` already unmounting underneath it. A
    /// marker that cannot be written is therefore fatal to the REQUEST, not
    /// something to proceed past — better a machine that stays up and says so
    /// than one torn down with no record that it was deliberate.
    fn begin_shutdown(&mut self, power: Power) -> String {
        if let Some(existing) = &self.shutdown {
            // Not an error. Two presses, or a greeter and a park handshake
            // racing, are the ordinary way this arrives twice.
            return format!(
                "shutdown already in progress ({})\n",
                existing.power.as_str()
            );
        }
        if let Err(e) = write_marker(&self.marker_path, power) {
            let path = &self.marker_path;
            return format!(
                "error: cannot record the shutdown in {path}: {e}; \
                 nothing has been stopped\n"
            );
        }
        log(&format!("{}: stopping every service", power.as_str()));
        let mut walk = self.order.clone();
        for index in 0..self.services.len() {
            let undeclared = self
                .services
                .get(index)
                .is_some_and(|s| s.retired && (s.pid.is_some() || s.stopping));
            if undeclared && !walk.contains(&index) {
                walk.push(index);
            }
        }
        self.shutdown = Some(Shutdown {
            power,
            walk,
            cursor: 0,
            current: None,
        });
        format!("{} requested\n", power.as_str())
    }

    /// Drive the teardown one step. Called once per loop pass.
    ///
    /// Reuses the ordinary stop path rather than opening a second one: each
    /// service goes through `control_stop`, so the TERM, the recorded
    /// containment, the scheduled KILL and the I4 sweep are the SAME code an
    /// operator's `stop` uses. A second teardown would be a second set of bugs.
    /// Returns the power action once every service is down — the caller runs
    /// the teardown and hands off. Split that way so this whole state machine
    /// is testable: the handoff `exec`s, so a version that performed it here
    /// could only ever be run once, by the real `run` loop.
    fn advance_shutdown(&mut self) -> Option<Power> {
        let Some(state) = &self.shutdown else {
            return None;
        };
        let power = state.power;
        let mut cursor = state.cursor;
        let walk = state.walk.clone();
        // Still waiting on the service we asked to stop?
        if let Some((index, deadline)) = state.current {
            let done = self
                .services
                .get(index)
                .is_some_and(|s| !s.stopping && s.pid.is_none());
            if !done {
                if Instant::now() < deadline {
                    return None;
                }
                if let Some(service) = self.services.get(index) {
                    log(&format!(
                        "{}: did not stop in time; going on without it",
                        service.unit.name
                    ));
                }
            }
            if let Some(state) = &mut self.shutdown {
                state.current = None;
            }
        }
        // Reverse plan order: a dependent is stopped before what it depends on.
        let live: Vec<bool> = self
            .services
            .iter()
            .map(|s| s.pid.is_some() || s.stopping)
            .collect();
        while let Some((index, next)) = next_to_stop(&walk, cursor, &live) {
            cursor = next;
            let Some(name) = self.services.get(index).map(|s| s.unit.name.clone()) else {
                continue;
            };
            let reply = self.control_stop(&name, false);
            if let Some(why) = reply.strip_prefix("error: ") {
                // A stop that could not be sent is not a reason to wait out a
                // deadline for it.
                log(&format!("{name}: {}", why.trim_end()));
                continue;
            }
            let bound = self
                .services
                .get(index)
                .map(|s| s.unit.stop_timeout)
                .unwrap_or(crate::table::DEFAULT_STOP_TIMEOUT);
            // `saturating_*` throughout: `Duration`'s `+` PANICS on overflow,
            // and a deadline is not worth a panic in the one code path whose
            // job is to end a boot cleanly.
            let wait = bound
                .saturating_mul(2)
                .saturating_add(STOP_SWEEP_INTERVAL)
                .saturating_add(SHUTDOWN_UNIT_SLACK);
            // A deadline that will not fit means DO NOT WAIT, never wait
            // forever — and the cursor moves regardless, or the next pass
            // re-selects this same unit, re-sends TERM and resets the KILL it
            // just scheduled, so the escalation never fires and the teardown
            // never advances.
            let deadline = Instant::now()
                .checked_add(wait)
                .unwrap_or_else(Instant::now);
            if let Some(state) = &mut self.shutdown {
                state.cursor = cursor;
                state.current = Some((index, deadline));
            }
            return None;
        }
        if let Some(state) = &mut self.shutdown {
            state.cursor = cursor;
        }
        Some(power)
    }

    /// Arm Ctrl-Alt-Del, or say why it is not armed and carry on.
    ///
    /// Never fatal. A machine that supervises its services but hard-resets on a
    /// key press is worse than one that does neither, but it is much better than
    /// one that refuses to boot — and on td's current `allnoconfig` kernel there
    /// is no CONFIG_VT and no input stack, so `ctrl_alt_del()` has no caller at
    /// all and a missing sysctl is the ORDINARY case, not a fault.
    fn arm_cad(&mut self) {
        // Retire any previous sentinel first. Closing the write end is how it
        // learns to exit — an explicit `drop`, because the whole mechanism
        // turns on that handle's lifetime and a field quietly going out of
        // scope is not where that should be recorded.
        //
        // No caller reaches here holding one today: `run` arms once and
        // `advance_cad` only fires after a death cleared the field. It stays
        // because arming twice is the one way to leak a sentinel AND leave the
        // kernel pointed at it, and a future second caller should not have to
        // rediscover that. A test double-arms to keep the branch honest.
        if let Some(previous) = self.cad.take() {
            log(&format!(
                "retiring the ctrl-alt-del sentinel {}",
                previous.pid
            ));
            drop(previous.keepalive);
        }
        if let Err(why) = crate::cad::disable_hard_reset(&self.cad_enabled_path) {
            // A sysctl that is not THERE is the ordinary case on td's current
            // `allnoconfig` kernel — no CONFIG_VT, so `ctrl_alt_del()` has no
            // caller — and it will not appear later. Retrying that forever is a
            // timer that can only fail, so it is said ONCE, ungated, and left.
            // Anything else is a fault, and the hard reset is still on, so
            // another attempt is worth making and the reason is throttled with
            // every other repeated one.
            if std::path::Path::new(&self.cad_enabled_path).exists() {
                self.arming_failed(&why);
            } else {
                log(&format!("ctrl-alt-del not armed: {why}"));
            }
            return;
        }
        // From here the kernel's own hard reset is OFF, so every failure below
        // leaves the machine in the state this whole module exists to avoid —
        // no reset AND no sentinel. Each schedules another attempt.
        let (child, keepalive) = match crate::cad::spawn_sentinel() {
            Ok(pair) => pair,
            Err(why) => {
                self.arming_failed(&why);
                return;
            }
        };
        let mut child = child;
        let pid = match i32::try_from(child.id()) {
            Ok(pid) => pid,
            Err(_) => {
                let why = "the sentinel's pid does not fit an i32";
                // Nothing is waiting on it yet, and a dropped `Child` is never
                // waited on — td-svc is not PID 1, so an unreaped sentinel
                // stays a zombie for the supervisor's lifetime.
                let _ = child.kill();
                let _ = child.wait();
                self.arming_failed(why);
                return;
            }
        };
        // The watcher is started BEFORE the kernel is pointed at the sentinel,
        // so that from here on the reaping is somebody's job: a later failure
        // need only drop the pipe, and the sentinel exits into a `wait` that is
        // already running. The sentinel's death also has to WAKE the loop
        // rather than be noticed on its next pass — `next_wake` can be minutes
        // out when a crash-looper sits at the backoff cap, and a press must not
        // wait on that.
        //
        // The child is handed over through a slot rather than moved directly:
        // `Builder::spawn` consumes the closure whether or not the thread
        // starts, so a `Child` moved into it is unreachable — and unreaped —
        // on the one path where no thread exists to reap it.
        let slot = std::sync::Arc::new(std::sync::Mutex::new(Some(child)));
        let watched = std::sync::Arc::clone(&slot);
        let tx = self.tx.clone();
        if !spawn_thread("cad-sentinel", move || {
            let mut child = match watched.lock() {
                Ok(mut held) => match held.take() {
                    Some(child) => child,
                    None => return,
                },
                Err(_) => return,
            };
            let signal = match child.wait() {
                Ok(status) => {
                    use std::os::unix::process::ExitStatusExt;
                    status.signal()
                }
                Err(_) => None,
            };
            let _ = tx.send(Event::SentinelDied { pid, signal });
        }) {
            if let Ok(mut held) = slot.lock() {
                if let Some(mut child) = held.take() {
                    let _ = child.kill();
                    let _ = child.wait();
                }
            }
            self.arming_failed("no thread to watch the sentinel");
            return;
        }
        if let Err(why) = crate::cad::point_kernel_at(&self.cad_pid_path, pid) {
            // Explicit: closing the write end is what EOFs the sentinel's read
            // and ends it, and the watcher above then reaps it and reports a
            // death for a pid this runtime never recorded — which
            // `on_sentinel_died` discards. Left to fall out of scope it reads
            // as an unused binding rather than as the teardown it is.
            drop(keepalive);
            self.arming_failed(&why);
            return;
        }
        self.cad = Some(crate::cad::Armed {
            keepalive,
            pid,
            since: Instant::now(),
        });
        log(&format!("ctrl-alt-del armed (sentinel {pid})"));
    }

    /// Record what is running, so a REPLACEMENT supervisor can evict it.
    ///
    /// Written from the live set on every spawn, whole, rather than appended
    /// to: an append-only record grows without bound under a crash-looping
    /// service and this lives in `/run`, which is RAM. Rewriting also makes it
    /// self-cleaning, so no exit path has to remember to remove an entry — a
    /// stale one is filtered by the identity check on the way back in.
    ///
    /// A service whose `starttime` could not be read is deliberately LEFT OUT.
    /// Recording a pid nothing can verify would hand the successor a number to
    /// kill on faith, and the whole safety of this mechanism is that it never
    /// does that. The cost is an orphan that outlives its supervisor; the
    /// alternative is signalling a stranger.
    ///
    /// Best effort. A supervisor that cannot write this still supervises — what
    /// is lost is its successor's ability to clean up after it, which is not
    /// worth refusing to run a machine over.
    fn persist_started(&self) {
        let mut entries: Vec<crate::evict::Entry> = self.unevicted.clone();
        entries.extend(self
            .services
            .iter()
            .filter_map(|service| {
                Some(crate::evict::Entry {
                    pid: service.pid?,
                    starttime: service.starttime?,
                    tty: service.tty_dev.unwrap_or(0),
                    name: service.unit.name.clone(),
                })
            }));
        if let Err(e) = crate::evict::write(&self.started_path, &entries) {
            log(&format!(
                "cannot record what is running in {}: {e}; a replacement \
                 supervisor will not know to evict it",
                self.started_path
            ));
        }
    }

    /// Point this instance's stdout and stderr at the service's capture.
    ///
    /// The capture is created on first use and then REUSED across restarts —
    /// see `Service::log`. Failing to create it is not a start failure: a
    /// service that runs without its log recorded is a better outcome than a
    /// service that does not run, which is the same trade `attach_tty` makes
    /// for a missing terminal.
    fn ensure_capture(&mut self, index: usize, unit: &Unit, report: bool) -> bool {
        let Some(path) = unit.log.clone() else {
            // Nothing asked for; nothing to fail. `build` wires no pipes.
            return false;
        };
        let Some(service) = self.services.get_mut(index) else {
            return false;
        };
        if service.log.is_none() {
            // `console=yes` copies to the same last-resort terminal every other
            // diagnostic here uses, opened O_NOCTTY for the same reason: td-svc
            // must never acquire a controlling terminal as a side effect.
            let console = if unit.console {
                match open_tty(CONSOLE) {
                    Ok(file) => Some(file),
                    Err(e) => {
                        if report {
                            log(&format!("{}: {CONSOLE}: {e}; not copying there", unit.name));
                        }
                        None
                    }
                }
            } else {
                None
            };
            let sink = crate::logs::Sink::new(&unit.name, &path, console);
            match crate::logs::start(&unit.name, sink) {
                Some(capture) => service.log = Some(capture),
                None => {
                    if report {
                        log(&format!(
                            "{}: no writer thread for {path}; running it without capture",
                            unit.name
                        ));
                    }
                    return false;
                }
            }
        }
        service.log.is_some()
    }

    /// Hand this instance's pipe ends to the service's drains.
    ///
    /// Only reached when `ensure_capture` succeeded, so the streams are only
    /// present when something is going to read them. A drain thread that
    /// cannot START drops the stream it was given, closing the read end: the
    /// service then gets EPIPE rather than blocking forever on a pipe nobody
    /// empties, which is the better of the two failures available by then.
    fn attach_logs(&mut self, index: usize, child: &mut Child, unit: &Unit) {
        let Some(capture) = self
            .services
            .get(index)
            .and_then(|service| service.log.clone())
        else {
            return;
        };
        if let Some(out) = child.stdout.take() {
            crate::logs::drain(&unit.name, out, Arc::clone(&capture));
        }
        if let Some(err) = child.stderr.take() {
            crate::logs::drain(&unit.name, err, capture);
        }
    }

    /// Every live capture, for the shutdown close.
    fn captures(&self) -> Vec<Arc<crate::logs::Capture>> {
        self.services
            .iter()
            .filter_map(|service| service.log.clone())
            .collect()
    }

    /// Release every `/var` log handle, and name what would not let go.
    ///
    /// Split out of `finish_shutdown` so it can be tested — `finish_shutdown`
    /// execs and does not return, and this is the step that decides whether
    /// `umount /var` succeeds.
    fn close_logs(&mut self) -> Vec<String> {
        let stuck = crate::logs::close_all(&self.captures(), LOG_CLOSE_GRACE);
        for name in &stuck {
            log(&format!(
                "{name}: its log writer did not close in time; if /var will not \
                 unmount, this is why"
            ));
        }
        stuck
    }

    /// Kill whatever a previous supervisor started, before starting anything.
    ///
    /// Returns the unit names it could NOT clear. Those must not be started:
    /// an unsupervised copy is still running, and a second one is the duplicate
    /// this whole mechanism exists to prevent — the same trade `abandon` makes,
    /// "better one degraded service than a duplicate nobody owns".
    ///
    /// **I5 is not violated by refusing a console here.** A greeter that
    /// survived eviction is still holding its terminal, so the machine is still
    /// repairable from its own console — what it lacks is supervision, not a
    /// console. Starting a second getty on the same device would take the
    /// working one away.
    fn evict_orphans(&mut self) -> Vec<String> {
        let (entries, problems) = crate::evict::read(&self.started_path);
        for problem in problems {
            log(&problem);
        }
        if entries.is_empty() {
            return Vec::new();
        }
        let mut live: Vec<(crate::evict::Entry, Containment)> = Vec::new();
        let mut refused: Vec<String> = Vec::new();
        for entry in entries {
            match procfs::is_same_process(entry.pid, entry.starttime) {
                // The recorded process is not there. Usually that is the whole
                // story — a supervisor that exited cleanly leaves a record full
                // of these — but its CHILDREN may still hold the group or the
                // terminal, which is the case containment exists for.
                Ok(false) => match self.survivors_of(&entry) {
                    Some(mode) => {
                        log(&format!(
                            "{}: pid {} is gone but its {mode:?} is not empty; evicting that",
                            entry.name, entry.pid
                        ));
                        live.push((entry, mode));
                        continue;
                    }
                    None => continue,
                },
                Ok(true) => {}
                Err(e) => {
                    // I3: unreadable is an error, not an emptiness. Not knowing
                    // whether the recorded process is there is not permission to
                    // assume it is not.
                    log(&format!(
                        "{}: cannot tell whether pid {} is still the process that \
                         was recorded ({e}); not starting it",
                        entry.name, entry.pid
                    ));
                    refused.push(entry.name.clone());
                    self.carry_forward(entry);
                    continue;
                }
            }
            let mode = containment_of(&entry);
            log(&format!(
                "{}: a previous supervisor left pid {} running; evicting {mode:?}",
                entry.name, entry.pid
            ));
            live.push((entry, mode));
        }
        if live.is_empty() {
            // Nothing to signal. Rewrite rather than delete: an entry that
            // could not be VERIFIED is still carried, and a later read
            // re-deciding about it is the point.
            self.persist_started();
            return refused;
        }

        // Signalled together and waited on together: a machine with eight
        // wedged services is delayed once, not eight times (I5).
        for (_, mode) in &live {
            let _ = self.signal(*mode, crate::sys::SIGTERM);
        }
        self.wait_until_evicted(&live, EVICT_GRACE);
        let stubborn: Vec<(crate::evict::Entry, Containment)> = live
            .into_iter()
            .filter(|(_, mode)| !self.evicted(*mode))
            .collect();
        // The grace period is long enough for the recorded process to exit, be
        // reaped by PID 1, and have its number reissued — and every containment
        // but a console's is keyed off that number. Re-verified before the
        // escalation, because a KILL is the one signal nothing survives to
        // complain about.
        let stubborn: Vec<(crate::evict::Entry, Containment)> = stubborn
            .into_iter()
            .filter(|(entry, mode)| match *mode {
                Containment::Console { .. } => true,
                _ => match procfs::is_same_process(entry.pid, entry.starttime) {
                    Ok(true) => true,
                    // Gone during the grace: whatever holds that number now was
                    // never ours. Its own survivors were covered by the TERM.
                    Ok(false) => false,
                    // I3: unreadable is not emptiness, but it is not a licence
                    // to KILL a number we can no longer vouch for either.
                    Err(_) => false,
                },
            })
            .collect();
        for (entry, mode) in &stubborn {
            log(&format!("{}: still there after TERM; killing it", entry.name));
            let _ = self.signal(*mode, crate::sys::SIGKILL);
        }
        self.wait_until_evicted(&stubborn, EVICT_SETTLE);
        refused.extend(self.refuse_unevicted(stubborn));
        // Rewrite, never delete: everything proven gone drops out, everything
        // that survived stays, and the successor inherits the difference.
        self.persist_started();
        refused
    }

    /// Of the orphans just signalled, which are still there?
    ///
    /// Split out because this is the branch's headline safety property — "what
    /// could not be cleared must not be started" — and a test that supplies the
    /// names itself proves only the spelling of the refusal, never the
    /// decision. Signals nothing, so a test may point it at a process it wants
    /// to keep.
    fn refuse_unevicted(
        &mut self,
        signalled: Vec<(crate::evict::Entry, Containment)>,
    ) -> Vec<String> {
        let mut refused = Vec::new();
        for (entry, mode) in signalled {
            if !self.evicted(mode) {
                log(&format!(
                    "{}: could NOT be evicted; not starting it, because a second \
                     copy of a service nobody supervises is worse than none",
                    entry.name
                ));
                refused.push(entry.name.clone());
                self.carry_forward(entry);
            }
        }
        refused
    }

    /// The containment of a recorded process that is no longer there, IF
    /// something is still in it.
    ///
    ///
    /// Safe only because the pid is wholly absent. A process group keeps its id
    /// as long as a member lives, and nothing can create group N without first
    /// holding pid N — so with pid N unused, members of group N must be what is
    /// left of the recorded service. If the number has been REISSUED that
    /// argument collapses (the new holder may lead a new group N), and this
    /// answers `None` rather than guess.
    ///
    /// Requires the scan to have actually FOUND something: an unreadable
    /// `/proc` must not turn into a teardown and a refused unit (I5).
    fn survivors_of(&self, entry: &crate::evict::Entry) -> Option<Containment> {
        if !matches!(procfs::stat_of(entry.pid), Ok(None)) {
            return None;
        }
        // A console's device is the recorded tty; every other unit is put in
        // its own group at spawn, so the group id is the recorded pid.
        let mode = if entry.tty != 0 {
            Containment::Console {
                leader: entry.pid,
                tty: entry.tty,
            }
        } else {
            Containment::Group(entry.pid)
        };
        let found = procfs::members(mode, self.self_pid)
            .is_ok_and(|scan| !scan.pids.is_empty());
        found.then_some(mode)
    }

    /// Keep an orphan in the record because it was not proven gone.
    ///
    /// Deduplicated by pid: `evict_orphans` runs once per boot, but a record
    /// naming the same pid twice must not make the successor's file grow each
    /// time a supervisor fails to clear it.
    fn carry_forward(&mut self, entry: crate::evict::Entry) {
        if !self.unevicted.iter().any(|u| u.pid == entry.pid) {
            self.unevicted.push(entry);
        }
    }

    /// Refuse to start units whose previous copy is still running.
    ///
    /// Split out of `run` so it can be tested — `run` never returns, and this
    /// is the step that decides whether a boot starts a second sshd. Same
    /// reason `resume_if_marked` is its own method.
    ///
    /// `Failed` with no `retry_at` is the one state `start_eligible` will not
    /// leave on its own, so nothing brings the duplicate back behind the
    /// operator's back; an explicit `start` still can, once they have dealt
    /// with the copy that is still there.
    fn refuse_duplicates(&mut self, names: &[String]) {
        for name in names {
            if let Some(service) = self.lookup_mut(name) {
                service.phase = Phase::Failed;
                service.retry_at = None;
            }
        }
    }

    /// Is this containment provably empty? An unreadable scan proves nothing
    /// and is NOT emptiness (I3) — so it reads as "still there", which is the
    /// direction that refuses to start a duplicate.
    fn evicted(&self, mode: Containment) -> bool {
        procfs::members(mode, self.self_pid).is_ok_and(scan_is_empty)
    }

    /// Poll until every containment is empty, or the deadline passes.
    ///
    /// Polling rather than waiting: these are PID 1's children, not td-svc's,
    /// so there is nothing to `wait` on — which is also why **I4** cannot be
    /// satisfied for them and an empty containment is the most that can be
    /// observed.
    fn wait_until_evicted(&self, live: &[(crate::evict::Entry, Containment)], within: Duration) {
        let Some(deadline) = Instant::now().checked_add(within) else {
            return;
        };
        while Instant::now() < deadline {
            if live.iter().all(|(_, mode)| self.evicted(*mode)) {
                return;
            }
            std::thread::sleep(TICK);
        }
    }

    /// Re-arm after a throttled wait, so a sentinel that cannot survive does not
    /// spin. Also the one place `cad_retry_at` is cleared.
    fn advance_cad(&mut self) {
        let Some(at) = self.cad_retry_at else {
            return;
        };
        // A timer scheduled BEFORE the teardown began still comes due during
        // it. `schedule_cad_rearm` refuses to make new ones once a shutdown is
        // in flight, but it cannot retract one already made, and firing it
        // would spawn a sentinel into a teardown whose whole job is to stop
        // processes. Dropped rather than deferred: there is nothing left to
        // arm for.
        if self.shutdown.is_some() {
            self.cad_retry_at = None;
            return;
        }
        if Instant::now() < at {
            return;
        }
        self.cad_retry_at = None;
        self.arm_cad();
    }

    /// Schedule a re-arm, backing off the way a crash-looping service does.
    ///
    /// A sentinel that dies for a PERSISTENT reason — a binary whose
    /// `cad-sentinel` verb no longer routes, say — dies again as fast as it can
    /// be spawned. Re-arming inline made that a tight fork loop that starved the
    /// event loop and filled the console; the same backoff every other repeated
    /// failure here gets bounds it at `backoff::CAP` instead.
    /// Report an arming that failed, and schedule another attempt.
    ///
    /// One entry point so the console line and the throttle cannot disagree:
    /// the REASON is gated by the same `should_report` every other repeated
    /// failure in this crate uses, which is the whole point of the backoff —
    /// an arm-then-die loop printed two lines a turn otherwise.
    fn arming_failed(&mut self, why: &str) {
        let next = self.cad_failures.saturating_add(1);
        if crate::backoff::should_report(next) {
            log(&format!("ctrl-alt-del not armed: {why}"));
        }
        self.schedule_cad_rearm();
    }

    fn schedule_cad_rearm(&mut self) {
        // Re-arm only while there is still a machine to shut down. Once the
        // teardown has begun a second press must not restart the sequence, and
        // a fresh sentinel would be one more process to stop.
        if self.shutdown.is_some() {
            return;
        }
        self.cad_failures = self.cad_failures.saturating_add(1);
        let delay = crate::backoff::delay(self.cad_failures);
        if crate::backoff::should_report(self.cad_failures) {
            log(&format!(
                "ctrl-alt-del: re-arming in {}ms (attempt {})",
                delay.as_millis(),
                self.cad_failures
            ));
        }
        self.cad_retry_at = Instant::now().checked_add(delay);
    }

    /// A press, or a sentinel that broke. Either way that arming is spent.
    fn on_sentinel_died(&mut self, pid: i32, signal: Option<i32>) {
        // News about a sentinel we already replaced. Retiring one is done by
        // closing its pipe, so a re-arm ALWAYS leaves its predecessor's watcher
        // thread about to report this; acting on it would drop the sentinel just
        // armed and arm another, which would retire that one in turn.
        if self.cad.as_ref().map(|armed| armed.pid) != Some(pid) {
            return;
        }
        // A sentinel that STAYED UP was not a failed arming, so the consecutive
        // -failure count starts over — the same rule, and the same threshold, a
        // service that ran long enough gets. Without it the count only ever
        // grows, and one much later death re-arms at `backoff::CAP`: five
        // minutes unarmed, with the hard reset off, for a fault unrelated to
        // the boot-time ones that ran the count up.
        let stayed_up = self
            .cad
            .as_ref()
            .is_some_and(|armed| armed.since.elapsed() >= crate::backoff::MIN_UPTIME);
        // The kernel points at a reaped pid now, and `cad_pid` holds a reference
        // to that dead `struct pid`, so delivery finds no task until a new pid
        // is written. Whatever happened, this sentinel is gone.
        self.cad = None;
        if stayed_up {
            self.cad_failures = 0;
        }
        if signal == Some(crate::sys::SIGINT) {
            // NOT "Ctrl-Alt-Del pressed": `cad_pid` is 0600 but any root process
            // can send SIGINT to the sentinel, so this names what was OBSERVED,
            // not a key nobody saw.
            log("shutdown requested via the ctrl-alt-del sentinel");
            let reply = self.begin_shutdown(Power::Reboot);
            if let Some(why) = reply.strip_prefix("error: ") {
                log(&format!("ctrl-alt-del: {}", why.trim_end()));
                // The press was heard and refused, so the machine is still up —
                // and it is now unarmed, with the kernel's own hard reset
                // disabled. Leaving it there makes the SECOND press do nothing
                // at all, which is the one outcome worse than either behaviour
                // this whole module chooses between.
                self.schedule_cad_rearm();
            }
            return;
        }
        let how = signal.map_or_else(
            || "exited on its own".to_string(),
            |s| format!("was killed by signal {s}"),
        );
        log(&format!("the ctrl-alt-del sentinel {how}"));
        self.schedule_cad_rearm();
    }

    /// Adopt a shutdown an earlier instance began but did not finish (**I6**).
    ///
    /// Split out of `run` so it can be tested: `run` never returns, so the one
    /// step that decides a respawned supervisor supervises nothing would
    /// otherwise be reachable only from a booted machine.
    fn resume_if_marked(&mut self) {
        let Some(power) = read_marker(&self.marker_path) else {
            return;
        };
        let path = &self.marker_path;
        log(&format!(
            "{path} found: an earlier shutdown did not finish; resuming"
        ));
        // A fresh process has nothing retired: this table has never been
        // reloaded, so the start order IS everything there is to stop.
        self.shutdown = Some(Shutdown {
            power,
            walk: self.order.clone(),
            cursor: 0,
            current: None,
        });
    }

    /// Every service is down: run the teardown, then hand off to the applet.
    ///
    /// This does not return on success — `exec` replaces td-svc. If it DOES
    /// return, the applet is missing or refused, and this PARKS: it says so on
    /// a timer and stays. Exiting would be worse, not better. PID 1 respawns
    /// td-svc unconditionally, so a fresh instance would read the marker,
    /// resume straight back to this same missing applet and exit again — a hot
    /// respawn loop through PID 1 that is no likelier to succeed on the
    /// hundredth try than the first. Parking leaves the machine diagnosable
    /// and the marker in place, so the handoff still completes if whatever was
    /// wrong is repaired.
    fn finish_shutdown(&mut self, power: Power) -> ! {
        // BEFORE the teardown, not after: `umount /var` fails EBUSY against an
        // open file, `/etc/shutdown` withholds its marker on a failed unmount,
        // and the boot oracle greps for that marker — so one stray descriptor
        // presents as a mount bug rather than a log one (§7).
        self.close_logs();
        run_teardown(SHUTDOWN_SCRIPT);
        let applet = power.applet();
        log(&format!("handing off to {applet}"));
        // `exec` only RETURNS on failure, and what it returns is the error.
        let error = std::os::unix::process::CommandExt::exec(&mut Command::new(&applet));
        log(&format!("{applet}: {error}; the machine is still up"));
        // Nothing here can fix it, and returning would resume supervising a
        // system whose filesystems `/etc/shutdown` has just unmounted.
        loop {
            std::thread::sleep(SILENT_TABLE_COMPLAINT);
            log(&format!("{applet} failed earlier; the machine is still up"));
        }
    }

    /// A sender for the event channel, so the control thread can reach the loop.
    pub fn events(&self) -> Sender<Event> {
        self.tx.clone()
    }

    /// Apply one control request and return the reply the client sees.
    ///
    /// Runs ON the main loop, so it reads and writes supervision state directly
    /// — there is no lock because there is no second owner (§5). Every reply is
    /// a complete answer: a request that changed nothing says so rather than
    /// returning an empty success, because "stopped" and "was not running" are
    /// different facts to whoever asked.
    pub fn control(&mut self, request: &str) -> String {
        let mut words = request.split_whitespace();
        let Some(verb) = words.next() else {
            return "error: empty request\n".to_string();
        };
        let target = words.next();
        if words.next().is_some() {
            return format!("error: {verb}: too many arguments\n");
        }
        match (verb, target) {
            ("status", None) => self.status(),
            ("status", Some(name)) => match self.index_of(name) {
                Some(index) => self.status_line(index),
                None => unknown(name),
            },
            // Refused once a transition is under way. Starting a service
            // against an `/etc/shutdown` that is unmounting underneath it is
            // the fail-open I6 exists to forbid, and `reload` is worse still —
            // it would rewrite the very order the teardown is walking.
            // Arity first: "reload foo" is malformed whatever the supervisor is
            // doing, and answering "a shutdown is in progress" would send
            // whoever typed it looking for a state that is not the problem.
            ("reload" | "reboot" | "poweroff" | "halt", Some(extra)) => {
                format!("error: {verb}: takes no argument (got {extra:?})\n")
            }
            ("start" | "stop" | "restart", None) => {
                format!("error: {verb}: needs a service name\n")
            }
            ("start" | "restart" | "reload", _) if self.shutdown.is_some() => {
                format!("error: {verb}: refused, a shutdown is in progress\n")
            }
            ("start", Some(name)) => self.control_start(name),
            ("stop", Some(name)) => self.control_stop(name, false),
            ("restart", Some(name)) => self.control_stop(name, true),
            ("reload", None) => self.control_reload(),
            ("reboot" | "poweroff" | "halt", None) => match Power::parse(verb) {
                Some(power) => self.begin_shutdown(power),
                // Unreachable: the arm's own pattern is the list `parse` takes.
                None => format!("error: unknown request {verb:?}\n{CONTROL_USAGE}"),
            },
            _ => format!("error: unknown request {verb:?}\n{CONTROL_USAGE}"),
        }
    }

    fn index_of(&self, name: &str) -> Option<usize> {
        self.services.iter().position(|s| s.unit.name == name)
    }

    fn status(&self) -> String {
        // One allocation for the whole reply rather than one per line: this is
        // not hot, but the shape is the one the rest of the loop uses.
        let mut out = String::with_capacity(self.services.len().saturating_mul(48));
        for index in 0..self.services.len() {
            out.push_str(&self.status_line(index));
        }
        if out.is_empty() {
            // A supervisor with no units is the failure §10 describes; saying
            // "OK" with an empty list would read as a healthy idle machine.
            return "no units\n".to_string();
        }
        out
    }

    fn status_line(&self, index: usize) -> String {
        let Some(service) = self.services.get(index) else {
            return String::new();
        };
        let pid = match service.pid {
            Some(pid) => pid.to_string(),
            None => "-".to_string(),
        };
        // A stop in flight outranks the phase. The phase deliberately does not
        // move until the stop COMPLETES (claiming otherwise is the fail-open
        // I4 forbids), but that leaves a unit whose leader is already reaped
        // reading `ready` — indistinguishable, to the one person who has to
        // decide what to do about it, from one that is actually serving.
        let state = if service.stopping {
            "stopping"
        } else {
            service.phase.label()
        };
        format!(
            "{} {} pid={} failures={}\n",
            service.unit.name, state, pid, service.fast_failures
        )
    }

    fn control_start(&mut self, name: &str) -> String {
        let Some(index) = self.index_of(name) else {
            return unknown(name);
        };
        // `start_eligible` walks `self.order`, and the plan drops a unit whose
        // dependencies could not be resolved — a cycle, or anything downstream
        // of one. Setting it `Down` would reply "starting" and then nothing
        // would ever pick it up, forever. Answer with the reason instead.
        // A retired unit is out of the order for a different reason, and saying
        // "dependency cycle" would send an operator hunting a graph problem
        // that does not exist.
        if self
            .services
            .get(index)
            .is_some_and(|s| s.retired)
        {
            return format!(
                "error: {name}: no longer in the table; reload after declaring it again\n"
            );
        }
        if !self.order.contains(&index) {
            return format!(
                "error: {name}: the plan could not order it (a dependency cycle, or a \
                 unit downstream of one), so nothing would start it; fix the table and \
                 restart the supervisor\n"
            );
        }
        let Some(service) = self.services.get_mut(index) else {
            return unknown(name);
        };
        // Checked BEFORE the pid, not inside it. A stop is still in flight
        // after its leader is reaped whenever survivors hold the containment
        // (I4), and that state has `pid == None`. Reading it as "not running"
        // cleared `stopping` and started a SECOND instance alongside the
        // processes an operator had just asked to end.
        if service.stopping {
            // Replying "already running" and doing nothing would instead leave
            // the unit STOPPED while the client exited 0 believing it started.
            service.start_after_stop = true;
            return format!("{name}: stop in progress; will start again once it exits\n");
        }
        if service.pid.is_some() {
            return format!("{name}: already running\n");
        }
        // Clearing the failure count is the point of an explicit start: an
        // operator asking again should not serve out a backoff they are
        // plainly trying to interrupt.
        service.phase = Phase::Down;
        service.fast_failures = 0;
        service.retry_at = None;
        service.stopping = false;
        service.start_after_stop = false;
        format!("{name}: starting\n")
    }

    /// `stop`, and `restart` — the same sequence with an intent recorded.
    ///
    /// This does NOT wait. TERM goes out now, the KILL is scheduled at
    /// `stop-timeout` through the same `kill_at` the timeout path uses, and the
    /// phase changes when the process actually exits. Reporting "stopped" here
    /// would be claiming a death that has not happened — the fail-open shape
    /// I4 exists to forbid — so the reply says what was sent.
    fn control_stop(&mut self, name: &str, then_start: bool) -> String {
        let Some(index) = self.index_of(name) else {
            return unknown(name);
        };
        let Some(service) = self.services.get(index) else {
            return unknown(name);
        };
        let verb = if then_start { "restart" } else { "stop" };
        let Some(pid) = service.pid else {
            // A stop already in flight is NOT finished just because the leader
            // is gone: `finish_stop` keeps `stopping` set while survivors still
            // hold the containment (I4). Falling through would clear it and
            // `kill_at` with it, stranding those processes with nothing left to
            // sweep them, and report a stop that had not happened.
            if service.stopping {
                let Some(service) = self.services.get_mut(index) else {
                    return unknown(name);
                };
                service.start_after_stop = then_start;
                return format!(
                    "{name}: {verb} requested; a stop is already in progress and its \
                     containment is not empty yet\n"
                );
            }
            // Nothing to signal. A `restart` still has to honour its intent, or
            // restarting a crashed daemon would quietly leave it down.
            if then_start {
                return self.control_start(name);
            }
            let Some(service) = self.services.get_mut(index) else {
                return unknown(name);
            };
            // Clear any stop already in flight. Without this a `restart` whose
            // TERM had gone out, followed by a `stop` before the exit arrived,
            // would leave `start_after_stop` set — and the old exit would then
            // consume that stale intent and start the unit an operator had just
            // asked to stop.
            service.stopping = false;
            service.start_after_stop = false;
            service.stop_scope = None;
            service.kill_at = None;
            service.phase = Phase::Stopped;
            service.retry_at = None;
            return format!("{name}: was not running; marked stopped\n");
        };

        // The same identity check the delayed KILL makes, for the same reason
        // and one step earlier. A waiter may already have reaped this child
        // while its `Exited` event is still in the channel, and the kernel can
        // have handed the pid to something else — at which point a `tty=` unit
        // would derive a containment from a STRANGER and TERM everything on its
        // terminal. Failing closed costs a stop that must be retried; failing
        // open costs whatever now holds that pid.
        match service.starttime {
            Some(starttime) => match procfs::is_same_process(pid, starttime) {
                Ok(true) => {}
                Ok(false) => {
                    return format!(
                        "error: {name}: pid {pid} is no longer the process we started; \
                         nothing signalled\n"
                    )
                }
                Err(e) => {
                    return format!("error: {name}: cannot confirm pid {pid} from /proc: {e}\n")
                }
            },
            None => {
                return format!(
                    "error: {name}: no recorded start time for pid {pid}; nothing signalled\n"
                )
            }
        }

        let containment = match service.containment() {
            Ok(Some(mode)) => mode,
            Ok(None) => return format!("{name}: {verb} requested; nothing to signal\n"),
            // Reported as an error, and NOTHING is recorded: marking the unit
            // stopping here would arm a KILL for a containment we never derived
            // and tell the client it worked.
            Err(e) => return format!("error: {name}: cannot read /proc to stop it: {e}\n"),
        };

        // Signal FIRST, and only record the stop if it actually went out. An
        // earlier draft set `stopping` and armed the KILL before knowing, so a
        // refused or unenumerable containment still replied without an `error:`
        // prefix — and the CLI exited 0 on a service that was never signalled.
        if let Err(why) = self.signal(containment, crate::sys::SIGTERM) {
            return format!("error: {name}: {why}\n");
        }
        let Some(service) = self.services.get_mut(index) else {
            return unknown(name);
        };
        service.stopping = true;
        service.start_after_stop = then_start;
        service.stop_scope = Some(containment);
        service.deadline = None;
        service.retry_at = None;
        // A fresh stop, so the first occupied scan reports again.
        service.next_sweep = None;
        service.kill_at = Instant::now().checked_add(service.unit.stop_timeout);
        service.killed = false;
        if let Some(cancel) = service.cancel.take() {
            cancel.store(true, Ordering::Relaxed);
        }
        format!("{name}: {verb} requested; TERM sent to {containment:?}\n")
    }

    fn on_exit(&mut self, name: &str, code: Option<i32>) {
        let Some(index) = self.services.iter().position(|s| s.unit.name == name) else {
            return;
        };
        let Some(service) = self.services.get_mut(index) else {
            return;
        };
        let brief = service
            .started
            .is_some_and(|at| at.elapsed() < backoff::MIN_UPTIME);
        let success = code == Some(0);
        let kind = service.unit.kind;
        let restart = service.unit.restart;
        service.pid = None;
        service.started = None;
        service.deadline = None;
        // It died, so the escalation has nothing left to kill.
        service.kill_at = None;
        // The instance this probe was following is gone; stop it forking.
        if let Some(cancel) = service.cancel.take() {
            cancel.store(true, Ordering::Relaxed);
        }

        // A REQUESTED stop short-circuits the restart policy entirely. Reading
        // this exit as a failure would restart a `restart=always` daemon
        // immediately, which would make `stop` a no-op with extra steps.
        if service.stopping {
            service.retry_at = None;
            service.starttime = None;
            self.finish_stop(index);
            return;
        }
        // A stop that already COMPLETED must not be undone by an exit that
        // arrives after it. `stopping` is cleared the moment the containment
        // proves empty, so a leader whose exit lands later would otherwise be
        // read as a crash and restarted — putting back the service an operator
        // stopped, with `Stopped` still on the screen that said so.
        if service.phase == Phase::Stopped {
            service.retry_at = None;
            service.starttime = None;
            return;
        }

        if kind == Kind::Oneshot {
            service.phase = if success { Phase::Ready } else { Phase::Failed };
            if !success {
                let shown = code.map_or_else(|| "a signal".into(), |c| format!("status {c}"));
                log(&format!("{name}: exited with {shown}"));
            }
            return;
        }

        let should_restart = match restart {
            Restart::Always => true,
            Restart::OnFailure => !success,
            Restart::Never => false,
        };
        if !should_restart {
            service.phase = if success { Phase::Ready } else { Phase::Failed };
            service.retry_at = None;
            return;
        }
        // The count resets only on a run that lasted AND ended cleanly. Resetting
        // on any long run let a daemon that crashes just over MIN_UPTIME restart
        // at the base delay forever, escalating never and logging never.
        if brief || !success {
            service.fast_failures = service.fast_failures.saturating_add(1);
        } else {
            service.fast_failures = 0;
        }
        let delay = backoff::delay(service.fast_failures);
        let report = backoff::should_report(service.fast_failures);
        service.retry_at = Instant::now().checked_add(delay);
        service.phase = if delay >= backoff::CAP {
            Phase::Held
        } else {
            Phase::Failed
        };
        if report {
            let how = if brief { "too quickly" } else { "unexpectedly" };
            log(&format!("{name}: exited {how}; restarting in {delay:?}"));
        }
    }

    /// Run until killed. td-svc is respawned by PID 1, so returning is not a
    /// normal outcome — but unlike PID 1 it is not fatal either.
    pub fn run(&mut self) -> ! {
        // Whatever a previous instance left running is running RIGHT NOW,
        // unsupervised, and it is running whichever way this boot goes: if we
        // go on to supervise, starting a second copy is the duplicate this
        // prevents; if we go on to finish a teardown, an orphan still holding
        // /var is a mount that will not come away. So it is evicted before
        // anything starts.
        // I6: a marker means an earlier instance began a teardown and did not
        // finish it. Resume rather than supervise — every service it had
        // already stopped is down, and `/etc/shutdown` may be part way through
        // unmounting what the rest are using. Reading it changes no process, so
        // it can precede the eviction; what must not precede it is starting
        // anything.
        self.resume_if_marked();
        // Before the eviction, which can take EVICT_GRACE + EVICT_SETTLE: until
        // this runs, Ctrl-Alt-Del is the kernel's own hard reset, and a boot
        // that spends seconds evicting is exactly when someone reaches for it.
        // The sentinel is this supervisor's child and is in no record, so the
        // eviction that follows cannot touch it.
        if self.shutdown.is_none() {
            self.arm_cad();
        }
        let surviving = self.evict_orphans();
        self.refuse_duplicates(&surviving);
        let mut complain_at = Some(Instant::now());
        loop {
            // A table that yielded NO units is the one failure this process cannot
            // recover from and cannot be told about: td-init falls back to a shell on
            // an unreadable /etc/inittab, but after the cutover the console is a unit,
            // so an unreadable table means no console to read the complaint on. Worse,
            // `run` never exits, so PID 1 never respawns us into a retry. One line at
            // startup then silence looked identical to a healthy boot on the serial
            // console; repeating it on a slow throttle at least keeps saying so.
            if self.services.is_empty() {
                if let Some(at) = complain_at {
                    if Instant::now() >= at {
                        log(&format!(
                            "no units to supervise; nothing will start. Check {}",
                            self.table_path
                        ));
                        complain_at = Instant::now().checked_add(SILENT_TABLE_COMPLAINT);
                    }
                }
            }
            self.enforce_deadlines();
            self.advance_cad();
            if let Some(power) = self.advance_shutdown() {
                self.finish_shutdown(power);
            }
            // A pending re-arm is a wake reason like any retry: without it the
            // loop can sit on the channel for a second while the machine is
            // unarmed AND the kernel's hard reset is off, which is the gap the
            // backoff exists to bound rather than to widen.
            let next_wake = match (self.start_eligible(), self.cad_retry_at) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (a, b) => a.or(b),
            };
            let timeout = match next_wake {
                Some(at) => at.saturating_duration_since(Instant::now()).max(TICK),
                None => Duration::from_secs(1),
            };
            match self.rx.recv_timeout(timeout) {
                Ok(event) => self.dispatch(event),
                Err(RecvTimeoutError::Timeout) => {}
                // Every sender is a thread this process owns; if they are all
                // gone there is nothing running, but exiting would make PID 1
                // respawn us into the same state. Idle at the SAME cadence the
                // channel would have used — pending retries still need to fire,
                // and sleeping a flat tick instead spins at 10Hz forever once
                // the last service settles.
                Err(RecvTimeoutError::Disconnected) => std::thread::sleep(timeout),
            }
        }
    }

    /// Wait for the next event, or time out. Test-only: it exists so a test can
    /// pump the loop one event at a time instead of racing `run`, which never
    /// returns. Not compiled into the shipped binary.
    #[cfg(test)]
    pub fn next_event(&self, timeout: Duration) -> Option<Event> {
        self.rx.recv_timeout(timeout).ok()
    }

    /// Apply one event, ignoring any that describes an instance we have since
    /// replaced. Without the generation check, a probe launched for a dead
    /// instance can mark its replacement ready.
    pub(crate) fn dispatch(&mut self, event: Event) {
        // Handled before the generation check, which asks "is this news about
        // an instance we have replaced?" — a question a control request has no
        // answer to. Running it through that check would drop every request
        // whose verb did not happen to name a current generation.
        if let Event::Control { request, reply } = event {
            let answer = self.control(&request);
            // A client that hung up before the answer is not an error: the
            // request still ran, which is what it asked for.
            let _ = reply.send(answer);
            return;
        }
        // Also before the check, and for the same reason: the sentinel is not a
        // service, so it has no generation to be stale against. Its pid is its
        // generation instead, and `on_sentinel_died` does that check itself.
        if let Event::SentinelDied { pid, signal } = event {
            self.on_sentinel_died(pid, signal);
            return;
        }
        let name = event.name().to_string();
        let generation = event.generation();
        if self.lookup(&name).map(|s| s.generation) != Some(generation) {
            // Stale — except that an EXIT is news about reaping, and reaping is
            // generation-independent. `time_out` bumps the generation while
            // keeping the pid, so the waiter's exit for the instance we gave up
            // on arrives here; dropping it outright left `kill_at` armed and
            // produced a false "did not exit on TERM" plus a signal to a pid
            // that had already been reaped. Nothing else can be in flight,
            // because a unit awaiting its KILL is not eligible to restart.
            //
            // Exactly ONE bump, though. `time_out` bumps by one and keeps the
            // pid, so that is the whole of the case this handles. If the unit
            // has since restarted the generation has moved further, the pid and
            // `kill_at` below belong to the NEW instance, and clearing them
            // would forget a live process and disarm its escalation on the
            // strength of news about a long-dead one.
            let one_bump = self
                .lookup(&name)
                .is_some_and(|s| s.generation == generation.wrapping_add(1));
            if matches!(event, Event::Exited { .. }) && one_bump {
                let index = self.services.iter().position(|s| s.unit.name == name);
                if let Some(service) = self.lookup_mut(&name) {
                    if service.kill_at.is_some() {
                        service.kill_at = None;
                        service.pid = None;
                        service.starttime = None;
                    }
                }
                // A stop can be in flight across a generation bump: `time_out`
                // bumps it while keeping the pid, so a unit an operator stopped
                // AFTER it overran its `timeout=` arrives here rather than in
                // `on_exit`. Returning without finishing the stop latched
                // `stopping` forever and left the operator's `status` reading
                // `failed`, with nothing ever sweeping the containment.
                if let Some(index) = index {
                    if self.services.get(index).is_some_and(|s| s.stopping) {
                        self.finish_stop(index);
                    }
                }
            }
            return;
        }
        match event {
            // Both peeled off above; naming them here keeps the match
            // exhaustive without a catch-all that would swallow a future
            // variant.
            Event::Control { .. } | Event::SentinelDied { .. } => {}
            Event::Exited { code, .. } => self.on_exit(&name, code),
            Event::WaitFailed { error, .. } => {
                // Fails CLOSED: the child may still be running, so its identity
                // is left in place and nothing is restarted on this news.
                log(&format!("{name}: cannot wait for it: {error}"));
            }
            Event::Ready { .. } => {
                if let Some(service) = self.lookup_mut(&name) {
                    if service.phase == Phase::Starting {
                        service.phase = Phase::Ready;
                    }
                }
            }
            Event::ProbeFailed { .. } => {
                // The process is up but its declared readiness was never
                // established, so it is NOT ready. Marking it so would make
                // `ready=` decorative and report a dead listener as healthy.
                // `Failed` still settles ordering, so dependents proceed.
                log(&format!(
                    "{name}: readiness probe did not succeed in time; not ready"
                ));
                if let Some(service) = self.lookup_mut(&name) {
                    if service.phase == Phase::Starting {
                        service.phase = Phase::Failed;
                    }
                }
            }
        }
    }

}

/// One readiness attempt, bounded. Returns true only on a clean exit within
/// `PROBE_ATTEMPT`; a hung probe is killed and counts as "not yet".
fn probe_once(unit: &Unit) -> bool {
    let Some(prog) = unit.ready.first() else {
        return false;
    };
    let mut cmd = Command::new(prog);
    cmd.args(unit.ready.get(1..).unwrap_or(&[]))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let Ok(mut child) = cmd.spawn() else {
        return false;
    };
    let Some(deadline) = Instant::now().checked_add(PROBE_ATTEMPT) else {
        return false;
    };
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) => {}
            Err(_) => return false,
        }
        if Instant::now() >= deadline {
            // The probe got its own process group above, so kill the GROUP: a
            // probe that forked helpers would otherwise leak one set per
            // attempt, and attempts repeat until `ready-timeout`.
            // Best effort: the probe is being abandoned either way, and the
            // `child.kill()` below is the part that must happen.
            //
            // Only when the pid CONVERTS. A fallback of 0 negates to 0, and
            // `kill(0, SIGKILL)` addresses the caller's own process group —
            // td-svc and every service it started sharing that group.
            if let Ok(pid) = i32::try_from(child.id()) {
                let _ = send_signal(-pid, crate::sys::SIGKILL);
            }
            let _ = child.kill();
            let _ = child.wait();
            return false;
        }
        std::thread::sleep(TICK);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::table::parse;

    fn runtime(text: &str) -> Runtime {
        let (units, problems) = parse(text);
        assert!(problems.is_empty(), "{problems:?}");
        Runtime::new(units, "<test>").0
    }

    /// Simulate a spawn so exit handling can be tested without a real process.
    fn mark_running(rt: &mut Runtime, name: &str, uptime: Duration) {
        let service = rt.lookup_mut(name).unwrap();
        service.phase = Phase::Starting;
        service.started = Instant::now().checked_sub(uptime);
        service.generation += 1;
    }

    #[test]
    fn ordering_dependencies_are_settled_by_failure_as_well_as_success() {
        let mut rt = runtime(
            "[a]\ntype=oneshot\nexec=/bin/true\n\
             [b]\ntype=oneshot\nexec=/bin/true\nafter=a\n",
        );
        let b = rt.lookup("b").unwrap().unit.clone();
        assert!(!rt.deps_settled(&b), "b must wait while a is Down");
        rt.lookup_mut("a").unwrap().phase = Phase::Failed;
        assert!(
            rt.deps_settled(&b),
            "after= is ordering: a failed dependency still settles it"
        );
    }

    #[test]
    fn a_failed_ordering_dependency_does_not_skip_its_dependent() {
        let mut rt = runtime(
            "[td-firstboot]\ntype=oneshot\nexec=/bin/false\n\
             [greeter]\ntype=daemon\nexec=/bin/sh\ntty=ttyS0\nafter=td-firstboot\n",
        );
        rt.lookup_mut("td-firstboot").unwrap().phase = Phase::Failed;
        let greeter = rt.lookup("greeter").unwrap().unit.clone();
        assert!(rt.deps_settled(&greeter));
        assert_eq!(rt.requires_failed(&greeter), None);
    }

    #[test]
    fn a_failed_strict_dependency_skips_an_ordinary_unit() {
        let mut rt = runtime(
            "[dep]\ntype=oneshot\nexec=/bin/false\n\
             [svc]\ntype=daemon\nexec=/bin/x\nrequires=dep\n",
        );
        rt.lookup_mut("dep").unwrap().phase = Phase::Failed;
        let svc = rt.lookup("svc").unwrap().unit.clone();
        assert_eq!(rt.requires_failed(&svc), Some("dep".into()));
    }

    /// DESIGN.md I5, enforced in four places: the table refuses `requires=` on
    /// a console unit, the plan refuses to skip one, this refuses to skip one
    /// at runtime, and `dependency_verdict` bounds how long one will wait. The
    /// first three reason about the graph; only the fourth catches a stall.
    /// The failure all four prevent is a machine that is up and cannot be
    /// repaired from its own console.
    #[test]
    fn the_console_is_never_skipped_even_with_a_failed_strict_dependency() {
        let mut rt = runtime(
            "[dep]\ntype=oneshot\nexec=/bin/false\n\
             [greeter]\ntype=daemon\nexec=/bin/sh\ntty=ttyS0\n",
        );
        rt.lookup_mut("dep").unwrap().phase = Phase::Failed;
        let mut greeter = rt.lookup("greeter").unwrap().unit.clone();
        greeter.requires = vec!["dep".into()];
        assert_eq!(rt.requires_failed(&greeter), None);
    }

    #[test]
    fn a_oneshot_that_exits_zero_becomes_ready_and_one_that_fails_does_not() {
        let mut rt = runtime("[a]\ntype=oneshot\nexec=/bin/true\n");
        mark_running(&mut rt, "a", Duration::from_millis(1));
        rt.on_exit("a", Some(0));
        assert_eq!(rt.lookup("a").unwrap().phase, Phase::Ready);

        let mut rt = runtime("[a]\ntype=oneshot\nexec=/bin/false\n");
        mark_running(&mut rt, "a", Duration::from_millis(1));
        rt.on_exit("a", Some(1));
        assert_eq!(rt.lookup("a").unwrap().phase, Phase::Failed);
    }

    #[test]
    fn restart_policy_decides_whether_an_exit_schedules_a_retry() {
        let mut rt = runtime("[a]\ntype=daemon\nexec=/x\nrestart=never\n");
        rt.on_exit("a", Some(1));
        assert!(rt.lookup("a").unwrap().retry_at.is_none());

        let mut rt = runtime("[a]\ntype=daemon\nexec=/x\nrestart=on-failure\n");
        rt.on_exit("a", Some(0));
        assert!(rt.lookup("a").unwrap().retry_at.is_none());
        rt.on_exit("a", Some(1));
        assert!(rt.lookup("a").unwrap().retry_at.is_some());

        let mut rt = runtime("[a]\ntype=daemon\nexec=/x\nrestart=always\n");
        rt.on_exit("a", Some(0));
        assert!(rt.lookup("a").unwrap().retry_at.is_some());
    }

    #[test]
    fn repeated_fast_failures_escalate_into_the_hold() {
        let mut rt = runtime("[a]\ntype=daemon\nexec=/x\nrestart=always\n");
        for _ in 0..40 {
            mark_running(&mut rt, "a", Duration::from_millis(1));
            rt.on_exit("a", Some(1));
        }
        let a = rt.lookup("a").unwrap();
        assert_eq!(a.phase, Phase::Held);
        assert!(a.fast_failures > 10);
    }

    /// The crash-loop that used to be invisible: a daemon that dies just OVER
    /// MIN_UPTIME. Resetting the counter on any long-enough run left the delay
    /// pinned at the base forever — ~50 restarts a minute, escalating never.
    #[test]
    fn a_daemon_that_crashes_just_above_min_uptime_still_escalates() {
        let mut rt = runtime("[a]\ntype=daemon\nexec=/x\nrestart=always\n");
        for _ in 0..40 {
            mark_running(&mut rt, "a", backoff::MIN_UPTIME + Duration::from_millis(200));
            rt.on_exit("a", Some(1));
        }
        let a = rt.lookup("a").unwrap();
        assert_eq!(a.phase, Phase::Held, "a slow crash-loop must reach the hold");
    }

    /// ...while a run that lasted AND ended cleanly still resets it.
    #[test]
    fn a_long_clean_run_resets_the_failure_count() {
        let mut rt = runtime("[a]\ntype=daemon\nexec=/x\nrestart=always\n");
        mark_running(&mut rt, "a", Duration::from_millis(1));
        rt.on_exit("a", Some(1));
        assert_eq!(rt.lookup("a").unwrap().fast_failures, 1);
        mark_running(&mut rt, "a", Duration::from_secs(60));
        rt.on_exit("a", Some(0));
        assert_eq!(rt.lookup("a").unwrap().fast_failures, 0);
    }

    #[test]
    fn units_excluded_by_the_plan_start_out_failed_rather_than_pending() {
        let (units, _) = parse(
            "[a]\ntype=oneshot\nexec=/x\nafter=b\n\
             [b]\ntype=oneshot\nexec=/x\nafter=a\n\
             [c]\ntype=oneshot\nexec=/x\n",
        );
        let (rt, complaints) = Runtime::new(units, "<test>");
        assert_eq!(rt.lookup("a").unwrap().phase, Phase::Failed);
        assert_eq!(rt.lookup("b").unwrap().phase, Phase::Failed);
        assert_eq!(rt.lookup("c").unwrap().phase, Phase::Down);
        assert_eq!(complaints.len(), 2);
    }

    /// A console unit the plan could not order is still startable — it must not
    /// be pre-failed by the skip list it also appears in.
    #[test]
    fn a_forced_console_is_not_pre_failed() {
        let (units, _) = parse(
            "[netup]\ntype=oneshot\nexec=/x\nafter=ghost\n\
             [greeter]\ntype=daemon\nexec=/x\ntty=ttyS0\nafter=netup\n",
        );
        let (rt, _) = Runtime::new(units, "<test>");
        assert_eq!(rt.lookup("greeter").unwrap().phase, Phase::Down);
        assert_eq!(rt.lookup("netup").unwrap().phase, Phase::Failed);
    }

    /// A spawn failure must settle immediately. It used to leave the unit
    /// `Down` with a retry, which `deps_settled` treats as unsettled — so a
    /// missing binary blocked every dependent, console included, for the ~7
    /// minutes backoff takes to reach the hold.
    #[test]
    fn a_unit_that_cannot_be_spawned_settles_at_once() {
        let mut rt = runtime(
            "[missing]\ntype=oneshot\nexec=/nonexistent/binary\n\
             [after-it]\ntype=oneshot\nexec=/bin/true\nafter=missing\n",
        );
        rt.start_eligible();
        let missing = rt.lookup("missing").unwrap();
        assert_eq!(missing.phase, Phase::Failed);
        let dependent = rt.lookup("after-it").unwrap().unit.clone();
        assert!(
            rt.deps_settled(&dependent),
            "a dependent must not wait out the backoff of a binary that does not exist"
        );
    }

    /// A oneshot cannot be retried, so a spawn failure is terminal and says so
    /// once rather than scheduling a retry nothing will honour.
    #[test]
    fn a_oneshot_that_cannot_be_spawned_is_not_retried() {
        let mut rt = runtime("[a]\ntype=oneshot\nexec=/nonexistent/binary\n");
        rt.start_eligible();
        let a = rt.lookup("a").unwrap();
        assert_eq!(a.phase, Phase::Failed);
        assert!(a.retry_at.is_none());
    }

    /// A stale event — from an instance since replaced — must not touch the
    /// live one. Without the generation check a dead instance's readiness probe
    /// marks its replacement ready.
    #[test]
    fn an_event_from_a_replaced_instance_is_ignored() {
        let mut rt = runtime("[a]\ntype=daemon\nexec=/x\nready=/bin/true\n");
        rt.lookup_mut("a").unwrap().generation = 7;
        rt.lookup_mut("a").unwrap().phase = Phase::Starting;
        rt.dispatch(Event::Ready {
            name: "a".into(),
            generation: 6,
        });
        assert_eq!(
            rt.lookup("a").unwrap().phase,
            Phase::Starting,
            "a stale readiness event must not promote the live instance"
        );
        rt.dispatch(Event::Ready {
            name: "a".into(),
            generation: 7,
        });
        assert_eq!(rt.lookup("a").unwrap().phase, Phase::Ready);
    }

    /// I5's runtime half. A dependency that never settles is not a graph
    /// problem, so none of the other three console defences engages — and the
    /// console waits forever, which is the same outcome as being skipped.
    #[test]
    fn a_console_stops_waiting_on_a_dependency_that_never_settles() {
        let mut rt = runtime(
            "[slow]\ntype=oneshot\nexec=/bin/x\n\
             [greeter]\ntype=daemon\nexec=/bin/sh\ntty=ttyS0\nafter=slow\n",
        );
        // `slow` is spawned and simply never finishes.
        rt.lookup_mut("slow").unwrap().phase = Phase::Starting;
        let greeter = rt.services.iter().position(|s| s.unit.name == "greeter").unwrap();
        let now = Instant::now();
        assert!(
            matches!(rt.dependency_verdict(greeter, now), Verdict::Wait(_)),
            "it waits at first — the ordering is still a preference worth honouring"
        );
        // ...but not past its patience.
        let later = now.checked_add(CONSOLE_PATIENCE).unwrap();
        rt.lookup_mut("greeter").unwrap().waiting_since = Some(now);
        assert!(matches!(rt.dependency_verdict(greeter, later), Verdict::Go));

        // An ordinary unit is not privileged this way and waits indefinitely.
        let mut rt = runtime(
            "[slow]\ntype=oneshot\nexec=/bin/x\n\
             [svc]\ntype=daemon\nexec=/bin/y\nafter=slow\n",
        );
        rt.lookup_mut("slow").unwrap().phase = Phase::Starting;
        let svc = rt.services.iter().position(|s| s.unit.name == "svc").unwrap();
        rt.lookup_mut("svc").unwrap().waiting_since = Some(now);
        assert!(matches!(rt.dependency_verdict(svc, later), Verdict::Wait(_)));
    }

    /// "starting it anyway, last, with its ordering ignored" has to be true at
    /// RUNTIME too. The plan moved it into the order, then `deps_settled`
    /// blocked it on the very dependencies the plan called unsatisfiable — so
    /// two consoles in a mutual cycle were reported as started and never were.
    #[test]
    fn a_forced_console_does_not_wait_on_the_ordering_it_was_told_to_ignore() {
        let (units, _) = parse(
            "[c1]\ntype=daemon\nexec=/x\ntty=ttyS0\nafter=c2\n\
             [c2]\ntype=daemon\nexec=/y\ntty=ttyS1\nafter=c1\n",
        );
        let (rt, complaints) = Runtime::new(units, "<test>");
        assert_eq!(complaints.len(), 2);
        for name in ["c1", "c2"] {
            let at = rt.services.iter().position(|s| s.unit.name == name).unwrap();
            assert!(rt.lookup(name).unwrap().forced, "{name} should be forced");
            assert!(
                matches!(rt.dependency_verdict(at, Instant::now()), Verdict::Go),
                "{name} was promised its ordering would be ignored"
            );
        }
    }

    /// A spawn's terminal diagnostics are gated by the SAME rule as the restart
    /// message, so a crash-looping unit cannot scroll the console with them.
    ///
    /// Found by running the shipped boot table through the real supervisor with a
    /// `/dev/console` it could not open: the restart message was correctly quiet
    /// after the first, while `greeter: /dev/console: Permission denied` repeated
    /// on every single restart — the console filling with the one line that had
    /// already said everything it was going to.
    #[test]
    fn a_crash_looping_units_spawn_diagnostics_are_not_repeated_every_restart() {
        // Drive the real spawn path, not `backoff` (which backoff.rs already pins):
        // a tty= unit whose terminal cannot exist, so both diagnostics are reachable.
        let unit = Unit {
            tty: Some("td-svc-no-such-tty".into()),
            ..runtime("[greeter]\ntype=daemon\nexec=/bin/true\ntty=ttyS0\n")
                .lookup("greeter")
                .unwrap()
                .unit
                .clone()
        };

        let (mut loud_cmd, loud_build) = build(&unit, true, false).unwrap();
        let loud_tty = attach_tty(&mut loud_cmd, &unit, true).said;
        assert!(
            !loud_build.is_empty() || !loud_tty.is_empty(),
            "a REPORTED spawn must say why the terminal could not be opened; with this \
             gate stuck closed a greeter that can never start explains itself nowhere"
        );

        let (mut quiet_cmd, quiet_build) = build(&unit, false, false).unwrap();
        let quiet_tty = attach_tty(&mut quiet_cmd, &unit, false).said;
        assert!(
            quiet_build.is_empty() && quiet_tty.is_empty(),
            "a GATED spawn built {:?}/{:?}; these run once per restart, so an ungated \
             line scrolls the console at the restart rate — which is the bug this gate \
             exists to stop, and it was real: the greeter printed its /dev/console \
             failure on every attempt while the restart message was correctly quiet",
            quiet_build,
            quiet_tty
        );

        // ...and the gate opens on exactly the attempts DESIGN.md §6 names.
        let loud = (0..500u32)
            .filter(|failures| backoff::should_report(failures.saturating_add(1)))
            .count();
        assert!(
            loud <= 2,
            "{loud} of 500 restarts would print their terminal diagnostics; the gate \
             allows the first attempt and the escalation into the hold, and no more"
        );
    }

    /// A oneshot that outruns `timeout=` must settle. It used to sit at
    /// `Starting` forever with the key parsed and never enforced, blocking
    /// every dependent silently.
    #[test]
    fn a_oneshot_that_outruns_its_timeout_is_given_up_on() {
        let mut rt = runtime(
            "[slow]\ntype=oneshot\nexec=/bin/x\ntimeout=1\n\
             [after-it]\ntype=oneshot\nexec=/bin/true\nafter=slow\n",
        );
        {
            let service = rt.lookup_mut("slow").unwrap();
            service.phase = Phase::Starting;
            // No pid: containment resolves to None, so nothing is signalled and
            // the test does not depend on /bin/kill existing.
            service.deadline = Instant::now().checked_sub(Duration::from_secs(1));
        }
        rt.enforce_deadlines();
        let slow = rt.lookup("slow").unwrap();
        assert_eq!(slow.phase, Phase::Failed);
        assert!(slow.deadline.is_none());
        let dependent = rt.lookup("after-it").unwrap().unit.clone();
        assert!(rt.deps_settled(&dependent));
    }

    /// ...and the exit that follows must not undo it. A unit TERMed for
    /// overrunning its timeout can still exit 0 in the race window; without a
    /// generation bump that flips the failure back to ready.
    #[test]
    fn an_exit_racing_a_timeout_does_not_undo_the_decision() {
        let mut rt = runtime("[slow]\ntype=oneshot\nexec=/bin/x\ntimeout=1\n");
        let before = {
            let service = rt.lookup_mut("slow").unwrap();
            service.phase = Phase::Starting;
            service.deadline = Instant::now().checked_sub(Duration::from_secs(1));
            service.generation
        };
        rt.enforce_deadlines();
        rt.dispatch(Event::Exited {
            name: "slow".into(),
            generation: before,
            code: Some(0),
        });
        assert_eq!(
            rt.lookup("slow").unwrap().phase,
            Phase::Failed,
            "a stale exit must not resurrect a unit we already gave up on"
        );
    }

    /// A readiness probe that never succeeded must not report Ready — that
    /// would make `ready=` decorative and a dead listener indistinguishable
    /// from a healthy one.
    #[test]
    fn a_failed_readiness_probe_does_not_report_ready() {
        let mut rt = runtime("[a]\ntype=daemon\nexec=/x\nready=/bin/false\n");
        rt.lookup_mut("a").unwrap().phase = Phase::Starting;
        let generation = rt.lookup("a").unwrap().generation;
        rt.dispatch(Event::ProbeFailed {
            name: "a".into(),
            generation,
        });
        assert_eq!(rt.lookup("a").unwrap().phase, Phase::Failed);
    }

    /// A `wait` error is not an exit. Treating it as one would clear the
    /// service's identity while the child may still be running, and td-svc
    /// would start a duplicate.
    #[test]
    fn a_wait_error_does_not_clear_the_services_identity() {
        let mut rt = runtime("[a]\ntype=daemon\nexec=/x\nrestart=always\n");
        {
            let service = rt.lookup_mut("a").unwrap();
            service.phase = Phase::Ready;
            service.pid = Some(4242);
        }
        let generation = rt.lookup("a").unwrap().generation;
        rt.dispatch(Event::WaitFailed {
            name: "a".into(),
            generation,
            error: "EINTR".into(),
        });
        let a = rt.lookup("a").unwrap();
        assert_eq!(a.pid, Some(4242), "identity must survive a wait error");
        assert_eq!(a.phase, Phase::Ready);
        assert!(a.retry_at.is_none(), "a wait error must not schedule a restart");
    }

    /// Containment is not a union. A unit td-svc grouped keeps td-svc's own
    /// session, so matching on it would select every process in the supervisor's
    /// session.
    #[test]
    fn a_grouped_unit_is_contained_by_its_group_alone() {
        let mut rt = runtime("[a]\ntype=daemon\nexec=/x\n");
        rt.lookup_mut("a").unwrap().pid = Some(4242);
        let mode = rt.lookup("a").unwrap().containment().unwrap();
        assert_eq!(mode, Some(Containment::Group(4242)));
    }

    /// The P0 this guard exists for. A `tty=` child inherits td-svc's process
    /// group AND session until it `setsid()`s, so before that BOTH name the
    /// supervisor. An earlier draft returned `Group(stat.pgrp)` here, and a
    /// `tty=` oneshot that hit its `timeout=` first would have sent
    /// `kill -TERM -<td-svc's own pgid>` — the supervisor, every service in its
    /// group, and the machine.
    #[test]
    fn a_console_unit_that_has_not_setsid_yet_contains_only_itself() {
        let rt = runtime("[greeter]\ntype=daemon\nexec=/x\ntty=ttyS0\n");
        let child = rt.self_pid.wrapping_add(1);
        // The pre-setsid state: the child leads neither its group nor its
        // session, so both fields still hold td-svc's.
        let own = rt.self_ids.unwrap_or(procfs::Stat {
            pgrp: rt.self_pid,
            session: rt.self_pid,
            tty_nr: 0,
            starttime: 0,
            zombie: false,
        });
        let (own_pgrp, own_session) = (own.pgrp, own.session);
        let inherited = procfs::Stat {
            pgrp: own_pgrp,
            session: own_session,
            tty_nr: 0,
            starttime: 1,
            zombie: false,
        };
        let mode = classify(child, &inherited);
        assert_eq!(mode, Containment::Process(child));
        assert!(
            !rt.contains_self(mode),
            "containment {mode:?} would have signalled td-svc itself"
        );

        // After setpgid but before setsid: its own group, safe to signal.
        let grouped = procfs::Stat {
            pgrp: child,
            session: own_session,
            tty_nr: 0,
            starttime: 1,
            zombie: false,
        };
        assert_eq!(classify(child, &grouped), Containment::Group(child));

        // After setsid: leads both, and the session is what holds the login
        // tree together once the shell starts making groups inside it.
        let sessioned = procfs::Stat {
            pgrp: child,
            session: child,
            tty_nr: 0,
            starttime: 1,
            zombie: false,
        };
        assert_eq!(classify(child, &sessioned), Containment::Session(child));
    }

    /// A real, live child — so the `/proc` identity checks the stop path makes
    /// have something true to check. Fabricating a pid makes `control_stop`
    /// refuse (correctly): it will not signal a pid it cannot prove is still
    /// the process td-svc started.
    struct Live {
        child: std::process::Child,
        pid: i32,
        starttime: u64,
    }

    impl Drop for Live {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    /// Poll `/proc` until `pid` is no longer a LIVE process, or give up.
    ///
    /// Liveness from `/proc`, per I3 — and `matches` already reads a zombie as
    /// absent, which is what makes this usable before the child is reaped.
    fn gone_within(pid: i32, patience: Duration) -> bool {
        let deadline = Instant::now() + patience;
        loop {
            let alive = procfs::stat_of(pid)
                .ok()
                .flatten()
                .is_some_and(|st| !st.zombie);
            if !alive {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// The signal actually LANDS on one process.
    ///
    /// Nothing else in this suite proved this. The stop-path tests drive the
    /// bookkeeping — `stopping` set, phase moved, no error returned — and every
    /// one of them stayed green with the syscall stubbed out to `Ok(())`,
    /// because a signal that is never sent is indistinguishable from one that
    /// is until something observes the target. This observes the target.
    #[test]
    fn a_signal_reaches_the_process_it_names() {
        let Some(live) = live_child() else {
            eprintln!("note: cannot spawn a child here; skipping");
            return;
        };
        assert!(
            procfs::stat_of(live.pid).ok().flatten().is_some(),
            "the fixture was not running to begin with"
        );
        send_signal(live.pid, crate::sys::SIGTERM).unwrap();
        assert!(
            gone_within(live.pid, Duration::from_secs(5)),
            "TERM did not reach the process it named"
        );
    }

    /// And on a whole process GROUP, addressed by a negative target.
    ///
    /// This is what the deleted td-svc-test leg used to prove about the uutils
    /// `kill` — that `-<pgid>` reads as a group and not as a flag. With the
    /// exec gone there is no argv to misparse, so the claim moves in-crate and
    /// becomes a property of `kill(2)` itself: the SIGN of the target is what
    /// selects a group. (Only the low 32 bits of the register reach the kernel,
    /// so this does not pin the widening cast — nothing can, and `sys.rs` says
    /// why.)
    #[test]
    fn a_negative_target_reaches_a_whole_process_group() {
        let Some(leader) = live_group_leader() else {
            eprintln!("note: cannot spawn a group leader here; skipping");
            return;
        };
        // The leader leads its own group, so the group id IS its pid.
        send_signal(-leader.pid, crate::sys::SIGTERM).unwrap();
        assert!(
            gone_within(leader.pid, Duration::from_secs(5)),
            "TERM to -pgid did not reach the group's leader"
        );
    }

    /// A signal to nothing is not an error, and is not evidence of death.
    ///
    /// ESRCH comes back as `Ok` deliberately (I3): the stop path must not learn
    /// liveness here, and the target can die between the `/proc` read that
    /// chose it and the call itself.
    #[test]
    fn signalling_a_pid_that_is_gone_is_not_an_error() {
        let Some(live) = live_child() else {
            eprintln!("note: cannot spawn a child here; skipping");
            return;
        };
        let pid = live.pid;
        drop(live); // killed and REAPED, so the pid names nothing at all
        assert_eq!(
            send_signal(pid, crate::sys::SIGTERM),
            Ok(()),
            "ESRCH must read as success, or a racing stop reports a failure it did not have"
        );
    }

    /// The two targets that are not a process or a group are refused.
    ///
    /// `kill(2)` accepts both and means something td-svc never does: `0` is the
    /// caller's own process group — td-svc and every service sharing it — and
    /// `-1` is a broadcast to every process this one may signal. Neither is
    /// reached by naming it; both are reached by ARITHMETIC, since a
    /// containment is signalled as `-pgid` and a pgid that read back as 1
    /// negates into the broadcast. Nothing downstream would report it: the
    /// signal succeeds.
    #[test]
    fn a_broadcast_or_a_self_group_target_is_refused() {
        for target in [0, -1] {
            let err = send_signal(target, crate::sys::SIGTERM)
                .err()
                .unwrap_or_else(|| "sent successfully".to_string());
            assert!(
                err.contains("not a process or a group"),
                "signalling {target} must be refused, got: {err}"
            );
        }
        // And a real target still goes through, or the guard is just a ban.
        // Our own pid with signal 0, so this half cannot skip and cannot
        // signal anything: a guard that refused everything would pass the
        // assertions above and stop the machine from ever shutting down.
        let Ok(me) = i32::try_from(std::process::id()) else {
            return;
        };
        assert_eq!(
            send_signal(me, 0),
            Ok(()),
            "the guard must refuse only 0 and -1"
        );
    }

    /// ESRCH is the ONLY errno that reads as success.
    ///
    /// The rule above is a narrow exception, and the danger is that it widens
    /// into "a signal that failed is fine". A signal td-svc could not send at
    /// all has to surface: every "killing it" line the stop path logs describes
    /// an action that then never happened, and the containment it is waiting to
    /// see empty never will.
    #[test]
    fn a_signal_that_could_not_be_sent_is_an_error() {
        let Ok(me) = i32::try_from(std::process::id()) else {
            return;
        };
        // An invalid signal NUMBER, so the refusal does not depend on whether
        // the suite runs as root — and it is refused before anything is
        // delivered, so this test does not signal itself.
        let err = send_signal(me, 65)
            .err()
            .unwrap_or_else(|| "sent successfully".to_string());
        assert!(
            err.contains("nothing was signalled"),
            "a refused signal must be reported as a failure, got: {err}"
        );
    }

    fn live_child() -> Option<Live> {
        // Blocks on a read from a pipe nothing writes to; no busy loop, and no
        // dependency on a `sleep` binary this host may not have.
        let child = Command::new("/bin/sh")
            .arg("-c")
            .arg("read x")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        let pid = i32::try_from(child.id()).ok()?;
        let starttime = procfs::stat_of(pid).ok()??.starttime;
        Some(Live {
            child,
            pid,
            starttime,
        })
    }

    /// The same, but leading its OWN process group.
    ///
    /// `Containment::Process` cannot model a survivor — it names one process,
    /// so a reaped leader empties it by construction. A group can, and it is
    /// the shape the real cases have: the greeter's console and any
    /// `process_group` unit outlive their leader through exactly this.
    fn live_group_leader() -> Option<Live> {
        use std::os::unix::process::CommandExt;
        let child = Command::new("/bin/sh")
            .arg("-c")
            .arg("read x")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .ok()?;
        let pid = i32::try_from(child.id()).ok()?;
        let starttime = procfs::stat_of(pid).ok()??.starttime;
        Some(Live {
            child,
            pid,
            starttime,
        })
    }

    /// `stop` on a `restart=always` daemon must make it STAY stopped.
    ///
    /// This is the whole point of the verb, and the restart policy is what
    /// fights it: the exit a TERM produces looks exactly like a crash, and
    /// `restart=always` answers a crash by starting it again. Without the
    /// `stopping` flag the socket's `stop` is a no-op with extra steps —
    /// TERM, exit, immediate restart, and the operator sees it running.
    #[test]
    fn stopping_an_always_restart_daemon_does_not_restart_it() {
        let mut rt = runtime("[sshd]\ntype=daemon\nexec=/bin/x\nrestart=always\n");
        let Some(live) = live_child() else { return };
        {
            let service = rt.lookup_mut("sshd").unwrap();
            service.phase = Phase::Ready;
            service.pid = Some(live.pid);
            service.starttime = Some(live.starttime);
            service.started = Some(Instant::now());
        }
        let reply = rt.control("stop sshd");
        assert!(reply.contains("stop requested"), "{reply}");
        assert!(rt.lookup("sshd").unwrap().stopping);

        // The TERM lands and the process exits — indistinguishable from a crash
        // except for the flag.
        rt.on_exit("sshd", None);
        let service = rt.lookup("sshd").unwrap();
        assert_eq!(service.phase, Phase::Stopped);
        assert!(service.retry_at.is_none(), "a stopped unit scheduled a retry");
        assert!(!service.stopping);

        // ...and nothing starts it again.
        rt.start_eligible();
        assert_eq!(rt.lookup("sshd").unwrap().phase, Phase::Stopped);
    }

    /// `restart` is the same sequence with an intent, and the intent survives
    /// the exit. It also clears the failure count: an operator restarting a
    /// crash-looping unit is plainly asking to interrupt the backoff, and
    /// serving it out anyway would make the verb useless exactly when it is
    /// most wanted.
    #[test]
    fn restart_stops_then_starts_and_interrupts_the_backoff() {
        let mut rt = runtime("[sshd]\ntype=daemon\nexec=/bin/x\nrestart=always\n");
        let Some(live) = live_child() else { return };
        {
            let service = rt.lookup_mut("sshd").unwrap();
            service.phase = Phase::Held;
            service.pid = Some(live.pid);
            service.starttime = Some(live.starttime);
            service.started = Some(Instant::now());
            service.fast_failures = 12;
            service.retry_at = Instant::now().checked_add(Duration::from_secs(300));
        }
        let reply = rt.control("restart sshd");
        assert!(reply.contains("restart requested"), "{reply}");

        rt.on_exit("sshd", None);
        let service = rt.lookup("sshd").unwrap();
        assert_eq!(
            service.phase,
            Phase::Down,
            "a restarted unit must be eligible again, not Stopped"
        );
        assert_eq!(service.fast_failures, 0, "restart did not clear the backoff");
        assert!(service.retry_at.is_none());
    }

    /// A stopped unit still SETTLES, so anything ordered after it proceeds.
    /// Treating an operator's decision as "not decided yet" would hang every
    /// dependent — the console included — on an answer that already exists.
    #[test]
    fn a_stopped_unit_settles_so_its_dependents_still_run() {
        let mut rt = runtime(
            "[a]\ntype=daemon\nexec=/bin/x\nrestart=always\n\
             [b]\ntype=oneshot\nexec=/bin/true\nafter=a\n",
        );
        let Some(live) = live_child() else { return };
        {
            let service = rt.lookup_mut("a").unwrap();
            service.phase = Phase::Ready;
            service.pid = Some(live.pid);
            service.starttime = Some(live.starttime);
            service.started = Some(Instant::now());
        }
        rt.control("stop a");
        rt.on_exit("a", None);
        assert_eq!(rt.lookup("a").unwrap().phase, Phase::Stopped);
        let b = rt.lookup("b").unwrap().unit.clone();
        assert!(
            rt.deps_settled(&b),
            "b is still waiting on a unit an operator already stopped"
        );
    }

    /// `start` on a stopped unit is the only way out of that phase, and it
    /// clears the backoff for the same reason `restart` does.
    #[test]
    fn start_is_the_only_way_out_of_stopped() {
        let mut rt = runtime("[sshd]\ntype=daemon\nexec=/bin/x\nrestart=always\n");
        {
            let service = rt.lookup_mut("sshd").unwrap();
            service.phase = Phase::Stopped;
            service.fast_failures = 9;
        }
        rt.start_eligible();
        assert_eq!(
            rt.lookup("sshd").unwrap().phase,
            Phase::Stopped,
            "the ordinary loop restarted a unit an operator stopped"
        );
        let reply = rt.control("start sshd");
        assert!(reply.contains("starting"), "{reply}");
        let service = rt.lookup("sshd").unwrap();
        assert_eq!(service.phase, Phase::Down);
        assert_eq!(service.fast_failures, 0);
    }

    /// Requests that name nothing real, or name too much, get an answer rather
    /// than silence or a panic. This is a socket an operator types at.
    #[test]
    fn malformed_requests_are_answered_not_ignored() {
        let mut rt = runtime("[a]\ntype=daemon\nexec=/bin/x\n");
        for (request, expect) in [
            ("", "empty request"),
            ("frobnicate", "unknown request"),
            ("stop", "needs a service name"),
            ("stop nosuch", "no such service"),
            ("status a b", "too many arguments"),
        ] {
            let reply = rt.control(request);
            assert!(
                reply.contains(expect),
                "request {request:?} answered {reply:?}, expected it to mention {expect:?}"
            );
        }
        // A well-formed status still works after all of that.
        assert!(rt.control("status").contains("a down"));
    }

    /// `stop` on something already down is not an error, but it must not claim
    /// to have signalled anything — and `restart` on it must still start it,
    /// or restarting a crashed daemon would quietly leave it down.
    #[test]
    fn stopping_and_restarting_a_unit_that_is_not_running() {
        let mut rt = runtime("[a]\ntype=daemon\nexec=/bin/x\nrestart=always\n");
        let reply = rt.control("stop a");
        assert!(reply.contains("was not running"), "{reply}");
        assert_eq!(rt.lookup("a").unwrap().phase, Phase::Stopped);

        let reply = rt.control("restart a");
        assert!(reply.contains("starting"), "{reply}");
        assert_eq!(rt.lookup("a").unwrap().phase, Phase::Down);
    }

    /// `start` on a unit the plan could not order must say so, not lie.
    ///
    /// `start_eligible` walks `self.order`, and the plan drops a unit in a
    /// dependency cycle (and anything downstream of one). Replying "starting"
    /// left it `Down` with nothing that would ever pick it up — a client that
    /// exited 0 on a service that never ran, forever.
    #[test]
    fn starting_a_unit_the_plan_dropped_answers_with_the_reason() {
        let mut rt = runtime(
            "[a]\ntype=oneshot\nexec=/x\nafter=b\n\
             [b]\ntype=oneshot\nexec=/x\nafter=a\n",
        );
        let reply = rt.control("start a");
        assert!(
            reply.starts_with("error:") && reply.contains("could not order it"),
            "a unit the plan dropped was told it was starting: {reply:?}"
        );
        // ...and it is still not running three passes later, which is the
        // outcome the old reply concealed.
        for _ in 0..3 {
            rt.start_eligible();
        }
        assert!(rt.lookup("a").unwrap().pid.is_none());
    }

    /// A stop that begins AFTER the unit overran its `timeout=` must still
    /// finish. `time_out` bumps the generation while keeping the pid, so the
    /// waiter's exit lands in the stale branch — which used to return without
    /// finishing the stop, latching `stopping` forever while `status` read
    /// `failed` and nothing swept the containment.
    #[test]
    fn a_stop_that_races_a_timeout_still_completes() {
        let mut rt = runtime("[slow]\ntype=oneshot\nexec=/x\ntimeout=1\n");
        let Some(live) = live_child() else { return };
        {
            let service = rt.lookup_mut("slow").unwrap();
            service.phase = Phase::Starting;
            service.pid = Some(live.pid);
            service.starttime = Some(live.starttime);
            service.deadline = Instant::now().checked_sub(Duration::from_secs(1));
        }
        // The timeout fires: TERM sent, generation bumped, pid kept.
        rt.enforce_deadlines();
        let generation = rt.lookup("slow").unwrap().generation;
        // ...and only then does an operator ask for a stop.
        rt.control("stop slow");
        assert!(rt.lookup("slow").unwrap().stopping);
        // The waiter's exit carries the OLD generation, so it is "stale".
        drop(live);
        rt.dispatch(Event::Exited {
            name: "slow".into(),
            generation: generation.wrapping_sub(1),
            code: None,
        });
        let service = rt.lookup("slow").unwrap();
        assert!(
            !service.stopping,
            "the stop latched across the generation bump; nothing would ever finish it"
        );
    }

    /// A stop must not SETTLE until the process is actually gone.
    ///
    /// Found by mutation: adding `phase = Stopped` at TERM time left every test
    /// green. That is the fail-open shape I4 forbids — the reply says "TERM
    /// sent", and anything ordered after the unit would proceed while it was
    /// still running.
    #[test]
    fn a_stop_does_not_settle_the_unit_before_the_process_exits() {
        // Without a working `kill` the stop is REFUSED before anything is
        // recorded, so the assertions below hold for the wrong reason and the
        // mutation this test exists to catch (setting `Stopped` at TERM time)
        // goes unnoticed.
        let mut rt = runtime("[a]\ntype=daemon\nexec=/x\nrestart=always\n");
        let Some(live) = live_child() else { return };
        {
            let service = rt.lookup_mut("a").unwrap();
            service.phase = Phase::Ready;
            service.pid = Some(live.pid);
            service.starttime = Some(live.starttime);
            service.started = Some(Instant::now());
        }
        rt.control("stop a");
        let service = rt.lookup("a").unwrap();
        assert_eq!(
            service.phase,
            Phase::Ready,
            "the unit settled at TERM time; the process has not exited yet"
        );
        assert!(service.pid.is_some(), "the pid was released before the exit");
    }

    /// ...and it must ARM the escalation, or a daemon that ignores TERM is left
    /// running forever while the client is told the TERM went out.
    ///
    /// Also found by mutation: deleting the `kill_at` assignment left every test
    /// green, because the only escalation test covered the `timeout=` path.
    #[test]
    fn a_stop_arms_the_kill_that_follows_an_ignored_term() {
        let mut rt = runtime("[a]\ntype=daemon\nexec=/x\nrestart=always\n");
        let Some(live) = live_child() else { return };
        {
            let service = rt.lookup_mut("a").unwrap();
            service.phase = Phase::Ready;
            service.pid = Some(live.pid);
            service.starttime = Some(live.starttime);
            service.started = Some(Instant::now());
        }
        rt.control("stop a");
        assert!(
            rt.lookup("a").unwrap().kill_at.is_some(),
            "a requested stop scheduled no KILL; a process that ignores TERM would \
             never be escalated on and the unit would sit stopping for good"
        );
    }

    /// The fail-closed direction of the console self-check, pinned.
    ///
    /// Mutation showed `None => false` passing everything: the `self_ids ==
    /// None` case was asserted for `Group` and `Session` but not for the variant
    /// this landing added. Not knowing its own ids is exactly when td-svc must
    /// refuse, because it cannot prove the target is not itself.
    #[test]
    fn a_console_containment_is_refused_when_td_svc_cannot_read_its_own_ids() {
        let mut rt = runtime("[greeter]\ntype=daemon\nexec=/x\ntty=ttyS0\n");
        let elsewhere = Containment::Console {
            leader: rt.self_pid.wrapping_add(7),
            tty: 1088,
        };
        rt.self_ids = None;
        assert!(
            rt.contains_self(elsewhere),
            "with its own ids unknown td-svc signalled a console containment anyway; \
             it cannot prove that set excludes itself"
        );
        // ...and device 0 is refused whatever the ids say, because a td-svc run
        // from a terminal HAS a real one and the comparison would let 0 through.
        rt.self_ids = Some(procfs::Stat {
            pgrp: rt.self_pid,
            session: rt.self_pid,
            tty_nr: 1088,
            starttime: 0,
            zombie: false,
        });
        assert!(
            rt.contains_self(Containment::Console {
                leader: rt.self_pid.wrapping_add(7),
                tty: 0
            }),
            "device 0 was accepted; that set is every daemon on the machine"
        );
    }

    /// I4, the half a leader's exit does not prove.
    ///
    /// A stop is complete when the leader is reaped AND the containment is
    /// empty. For the greeter the leader is a shell whose `getty` child holds
    /// the console, so its exit is exactly the misleading signal: an earlier
    /// draft marked the unit `Stopped` on it and cancelled the KILL, leaving
    /// getty, login and a user's shell on the terminal while `status` said
    /// stopped.
    #[test]
    fn a_stop_is_not_finished_while_its_containment_still_has_members() {
        let mut rt = runtime("[greeter]\ntype=daemon\nexec=/x\nrestart=always\n");
        let Some(mut live) = live_group_leader() else {
            return;
        };
        {
            let service = rt.lookup_mut("greeter").unwrap();
            service.phase = Phase::Ready;
            service.pid = Some(live.pid);
            service.starttime = Some(live.starttime);
            service.started = Some(Instant::now());
            service.stopping = true;
            // A scope that provably still has a member: the live child itself,
            // which leads this group.
            service.stop_scope = Some(Containment::Group(live.pid));
        }
        // The leader is reaped...
        rt.on_exit("greeter", None);
        let service = rt.lookup("greeter").unwrap();
        assert_ne!(
            service.phase,
            Phase::Stopped,
            "the unit was declared stopped while its containment still had members"
        );
        assert!(service.stopping, "the stop was abandoned mid-flight");
        assert!(
            service.kill_at.is_some(),
            "the KILL was disarmed while survivors remained; nothing would sweep them"
        );
        assert!(
            service.next_sweep.is_some(),
            "no re-scan was scheduled; survivors produce no event, so the unit \
             would sit stopping forever"
        );

        // ...and once the containment really is empty, the SWEEP completes it.
        // Nothing else can: the survivor is not td-svc's child, so its exit
        // arrives as no event at all.
        let _ = live.child.kill();
        let _ = live.child.wait();
        rt.lookup_mut("greeter").unwrap().next_sweep = Instant::now().checked_sub(SWEPT_AGO);
        rt.enforce_deadlines();
        assert_eq!(
            rt.lookup("greeter").unwrap().phase,
            Phase::Stopped,
            "the containment drained but nothing swept it back to stopped"
        );
    }

    /// Far enough in the past that a scheduled sweep is unambiguously due.
    const SWEPT_AGO: Duration = Duration::from_secs(3600);

    /// The other half of I4: a `Process` scope is empty the moment its leader
    /// is reaped, and must NOT be scanned for.
    ///
    /// That pid is no longer ours once the waiter reaps it, and the kernel can
    /// hand it straight to something unrelated. Scanning anyway finds the
    /// stranger, leaves the unit `stopping` with no event that could ever
    /// finish it, and points the escalation's KILL at whatever now holds the
    /// pid.
    #[test]
    fn a_process_containment_is_empty_once_its_leader_is_reaped() {
        let mut rt = runtime("[a]\ntype=daemon\nexec=/x\n");
        let Some(live) = live_child() else { return };
        {
            let service = rt.lookup_mut("a").unwrap();
            service.stopping = true;
            // Stands in for a recycled pid: a process that is provably ALIVE
            // and provably not this unit's. A scan would still see it.
            service.stop_scope = Some(Containment::Process(live.pid));
        }
        rt.on_exit("a", None);
        assert_eq!(
            rt.lookup("a").unwrap().phase,
            Phase::Stopped,
            "a single-process containment was scanned after its leader was \
             reaped, so a recycled pid kept the unit from ever stopping"
        );
        drop(live);
    }

    /// A `tty=` that names something which is not a device resolves to
    /// nothing, rather than to device 0.
    ///
    /// Device 0 means "no controlling terminal", and `contains_self` refuses a
    /// `Console` scope carrying it outright — so a unit that built one would be
    /// permanently unstoppable, its `timeout=` TERM refused along with
    /// everything else. Dropping the guard left the suite green: the existing
    /// coverage only exercises a path `metadata` cannot stat at all, which
    /// fails one step earlier and never reaches this.
    #[test]
    fn a_tty_that_is_not_a_device_resolves_to_no_containment() {
        let path = format!(
            "{}/td-svc-not-a-device-{}",
            std::env::temp_dir().display(),
            std::process::id()
        );
        if std::fs::write(&path, b"regular file").is_err() {
            return;
        }
        // A regular file opens fine and has rdev 0 — the case the `!= 0` guard
        // is for, and the one a path that cannot be opened never reaches.
        let Ok(file) = std::fs::File::open(&path) else {
            return;
        };
        assert_eq!(
            attached_device(&file),
            None,
            "a non-device tty= resolved to device 0, which makes the unit \
             unstoppable: every signal to that containment is refused"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// A stale exit must not clear a LIVE instance's identity.
    ///
    /// The stale-exit branch exists for `time_out`, which bumps the generation
    /// by one and keeps the pid. If the unit has since RESTARTED, the pid and
    /// `kill_at` it would clear belong to the new instance — so news about a
    /// long-dead process would forget a running one and disarm the escalation
    /// that was going to kill it.
    #[test]
    fn a_stale_exit_does_not_forget_a_newer_instance() {
        let mut rt = runtime("[a]\ntype=daemon\nexec=/x\nrestart=always\n");
        {
            let service = rt.lookup_mut("a").unwrap();
            // Two bumps past the event below: timed out, then restarted.
            service.generation = 7;
            service.phase = Phase::Ready;
            service.pid = Some(4242);
            service.starttime = Some(99);
            service.kill_at = Instant::now().checked_add(Duration::from_secs(30));
        }
        rt.dispatch(Event::Exited {
            name: "a".to_string(),
            generation: 5,
            code: Some(0),
        });
        let service = rt.lookup("a").unwrap();
        assert_eq!(
            service.pid,
            Some(4242),
            "a stale exit forgot the pid of a newer, live instance"
        );
        assert!(
            service.kill_at.is_some(),
            "a stale exit disarmed the KILL of a newer, live instance"
        );
    }


    /// A runtime whose table is a real file, so `reload` has one to re-read.
    fn reloadable(text: &str, tag: &str) -> (Runtime, String) {
        let dir = format!(
            "{}/td-svc-reload-{}-{tag}",
            std::env::temp_dir().display(),
            std::process::id()
        );
        let _ = std::fs::create_dir_all(&dir);
        let path = format!("{dir}/td-svc.conf");
        std::fs::write(&path, text).unwrap();
        let (units, problems) = parse(text);
        assert!(problems.is_empty(), "{problems:?}");
        let mut rt = Runtime::new(units, &path).0;
        rt.table_path = path;
        (rt, dir)
    }

    /// `reload` is transactional: any diagnostic keeps the running table.
    ///
    /// Applying the valid fragments of a table an operator got wrong is how a
    /// reload turns one bad stanza into a machine with no console. The bar is
    /// exactly `td-svc check`'s, because that is the bar the operator was told
    /// about — parse problems AND a plan that does not resolve.
    #[test]
    fn a_reload_that_does_not_check_changes_nothing() {
        let good = "[a]\ntype=daemon\nexec=/x\nrestart=always\n";
        let (mut rt, dir) = reloadable(good, "bad");
        let before: Vec<String> = rt.services.iter().map(|s| s.unit.name.clone()).collect();

        for (why, text) in [
            ("a malformed line", "[a]\ntype=daemon\nexec=/x\nrequires firewall\n"),
            ("an unknown key value", "[a]\ntype=nonsense\nexec=/x\n"),
            (
                "a dependency cycle",
                "[a]\ntype=oneshot\nexec=/x\nafter=b\n[b]\ntype=oneshot\nexec=/x\nafter=a\n",
            ),
            ("no units at all", "# nothing here\n"),
        ] {
            std::fs::write(&rt.table_path, text).unwrap();
            let reply = rt.control("reload");
            assert!(
                reply.starts_with("error:"),
                "{why} was accepted by reload: {reply}"
            );
            assert!(
                reply.contains("kept the running table"),
                "{why}: reload did not say what it kept: {reply}"
            );
            let after: Vec<String> = rt.services.iter().map(|s| s.unit.name.clone()).collect();
            assert_eq!(after, before, "{why} changed the running table anyway");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A good reload applies, without disturbing what is running.
    ///
    /// A unit whose stanza changed keeps its process — the new definition
    /// applies at its next start — because restarting on reload would take the
    /// console down as a side effect of editing an unrelated stanza. A unit
    /// that VANISHED is no longer declared, so it is stopped rather than left
    /// running with nothing supervising it.
    #[test]
    fn a_good_reload_applies_and_leaves_running_processes_alone() {
        let (mut rt, dir) = reloadable(
            "[keep]\ntype=daemon\nexec=/x\nrestart=always\n\
             [gone]\ntype=daemon\nexec=/x\nrestart=always\n\
             [gone-live]\ntype=daemon\nexec=/x\nrestart=always\n",
            "good",
        );
        // `keep` is up; the reload must not touch it. `gone-live` is up too and
        // is about to be dropped from the table — the two removed units differ
        // only in whether anything is RUNNING, which is the whole distinction
        // under test.
        let (Some(live), Some(doomed)) = (live_child(), live_child()) else {
            let _ = std::fs::remove_dir_all(&dir);
            return;
        };
        for (name, child) in [("keep", &live), ("gone-live", &doomed)] {
            let service = rt.lookup_mut(name).unwrap();
            service.phase = Phase::Ready;
            service.pid = Some(child.pid);
            service.starttime = Some(child.starttime);
        }
        std::fs::write(
            &rt.table_path,
            "[keep]\ntype=daemon\nexec=/x\nrestart=always\nstop-timeout=3\n\
             [fresh]\ntype=oneshot\nexec=/bin/true\n",
        )
        .unwrap();
        let reply = rt.control("reload");
        assert!(!reply.starts_with("error:"), "{reply}");
        assert!(reply.contains("reloaded"), "{reply}");

        // The running process is untouched...
        let keep = rt.lookup("keep").unwrap();
        assert_eq!(keep.pid, Some(live.pid), "a reload restarted a live service");
        assert_eq!(keep.phase, Phase::Ready);
        // ...and it picked up its NEW definition for next time.
        assert_eq!(keep.unit.stop_timeout, Duration::from_secs(3));
        // The new unit is present and startable.
        assert!(rt.index_of("fresh").is_some(), "a new unit was not added");
        assert!(
            rt.index_of("fresh").is_some_and(|i| rt.order.contains(&i)),
            "a new unit was added but left out of the plan, so nothing starts it"
        );
        // A removed unit that was NOT running is gone at once. Keeping it would
        // leak a `Service` per removal per reload, and re-declaring the name
        // later would match the corpse and adopt its phase.
        assert!(
            rt.index_of("gone").is_none(),
            "a removed unit with no process was kept anyway"
        );
        // A removed unit that IS running stays visible until it is down, and is
        // marked so the teardown knows to wait for it even though no plan names
        // it any more.
        let doomed_index = rt
            .index_of("gone-live")
            .expect("a removed unit with a live process must stay visible");
        assert!(
            rt.services.get(doomed_index).is_some_and(|s| s.retired),
            "a removed but still-running unit was not marked retired"
        );
        assert!(
            !rt.order.contains(&doomed_index),
            "a unit no longer in the table is still in the start plan"
        );
        // ...and a shutdown starting now must still tear it down, or
        // /etc/shutdown unmounts while it is holding a filesystem open.
        rt.marker_path = format!("{dir}/shutdown");
        let reply = rt.begin_shutdown(Power::Reboot);
        assert!(!reply.starts_with("error:"), "{reply}");
        assert!(
            rt.shutdown
                .as_ref()
                .is_some_and(|s| s.walk.contains(&doomed_index)),
            "the teardown skipped a removed unit that was still running"
        );
        let _ = std::fs::remove_dir_all(&dir);
        drop(live);
        drop(doomed);
    }

    /// A runtime whose I6 marker is somewhere a test may write.
    fn shutdown_runtime(text: &str, tag: &str) -> (Runtime, String) {
        let mut rt = runtime(text);
        let dir = format!(
            "{}/td-svc-shutdown-{}-{tag}",
            std::env::temp_dir().display(),
            std::process::id()
        );
        let _ = std::fs::create_dir_all(&dir);
        let path = format!("{dir}/shutdown");
        rt.marker_path = path.clone();
        (rt, dir)
    }

    /// The word is the verb, the marker's contents and the applet's basename.
    ///
    /// Deliberately ONE string: a resume reads back what the request wrote, so
    /// a mapping that disagreed anywhere would reboot a machine an operator
    /// asked to power off — silently, since nothing compares the two.
    #[test]
    fn a_power_action_survives_the_round_trip_through_the_marker() {
        for power in [Power::Reboot, Power::Off, Power::Halt] {
            assert_eq!(Power::parse(power.as_str()), Some(power));
            assert_eq!(power.applet(), format!("/bin/{}", power.as_str()));
        }
        let dir = format!(
            "{}/td-svc-marker-{}",
            std::env::temp_dir().display(),
            std::process::id()
        );
        let _ = std::fs::create_dir_all(&dir);
        let path = format!("{dir}/shutdown");

        assert_eq!(read_marker(&path), None, "no marker must read as no shutdown");
        for power in [Power::Reboot, Power::Off, Power::Halt] {
            write_marker(&path, power).unwrap();
            assert_eq!(read_marker(&path), Some(power));
        }
        // A marker that exists but says something unusable still means a
        // shutdown BEGAN. Resuming as a reboot is recoverable; parking on a
        // machine whose filesystems are going away is not.
        std::fs::write(&path, b"something else entirely\n").unwrap();
        assert_eq!(
            read_marker(&path),
            Some(Power::Reboot),
            "an unreadable marker must resume, not park"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The marker goes down BEFORE the first service is stopped.
    ///
    /// This ordering is the whole of I6. A supervisor that stopped sshd and
    /// then died before recording why would be respawned by PID 1, see no
    /// marker, and start sshd again — against an `/etc/shutdown` already
    /// unmounting underneath it.
    #[test]
    fn the_marker_is_written_before_anything_is_stopped() {
        let (mut rt, dir) = shutdown_runtime(
            "[a]\ntype=daemon\nexec=/x\nrestart=always\n",
            "before",
        );
        mark_running(&mut rt, "a", Duration::from_secs(5));
        let reply = rt.control("poweroff");
        assert!(reply.contains("poweroff requested"), "{reply}");
        assert_eq!(
            read_marker(&rt.marker_path),
            Some(Power::Off),
            "the transition began without recording it"
        );
        // Nothing has been asked to stop yet — that is `advance_shutdown`'s
        // job, one loop pass later.
        assert!(!rt.lookup("a").unwrap().stopping);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A second request is a no-op, not a second teardown.
    ///
    /// Two presses, or a greeter logout racing the boot-fail park handshake,
    /// are the ordinary way this arrives twice. Restarting the sequence would
    /// rewind the cursor and re-stop services already down.
    #[test]
    fn a_second_shutdown_request_does_not_restart_the_sequence() {
        let (mut rt, dir) = shutdown_runtime("[a]\ntype=daemon\nexec=/x\n", "twice");
        assert!(rt.control("reboot").contains("reboot requested"));
        let again = rt.control("poweroff");
        assert!(again.contains("already in progress"), "{again}");
        assert!(
            !again.starts_with("error:"),
            "a second request is ordinary, not an error: {again}"
        );
        // And it does NOT change where the shutdown ends.
        assert_eq!(read_marker(&rt.marker_path), Some(Power::Reboot));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Once the teardown begins, nothing may start a service again.
    #[test]
    fn a_shutdown_refuses_start_restart_and_reload() {
        let (mut rt, dir) = shutdown_runtime(
            "[a]\ntype=daemon\nexec=/x\nrestart=always\n",
            "refuse",
        );
        assert!(rt.control("reboot").contains("reboot requested"));
        for verb in ["start a", "restart a", "reload"] {
            let reply = rt.control(verb);
            assert!(
                reply.starts_with("error:") && reply.contains("shutdown is in progress"),
                "{verb} was not refused during a shutdown: {reply}"
            );
        }
        // `stop` is still allowed — it is what the teardown itself issues.
        assert!(!rt.control("stop a").starts_with("error: stop: refused"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The restart policy must not fight the teardown.
    ///
    /// `start_eligible` walks every unit each pass; a `restart=always` daemon
    /// the shutdown just stopped is exactly the shape it would bring back, and
    /// then the teardown would walk past a service that is up again.
    #[test]
    fn nothing_starts_once_the_shutdown_has_begun() {
        // `exec=/x`, and the assertions look at PHASE, not just at `pid`. An
        // earlier version used `/bin/true` and asserted only `pid.is_none()`,
        // which passed with the gate DELETED: this host has no `/bin/true`, so
        // the spawn failed and left `pid` empty either way. A unit that was
        // never started is `Down` with no retry; one that was started and could
        // not spawn is `Failed`, and a `restart=always` one has a `retry_at` —
        // states the failure cannot hide.
        let (mut rt, dir) = shutdown_runtime(
            "[a]\ntype=daemon\nexec=/x\nrestart=always\n\
             [b]\ntype=oneshot\nexec=/x\n",
            "nostart",
        );
        assert!(rt.control("reboot").contains("reboot requested"));
        rt.start_eligible();
        for name in ["a", "b"] {
            let service = rt.lookup(name).unwrap();
            assert!(
                service.pid.is_none(),
                "{name} was started after the shutdown began"
            );
            assert_eq!(
                service.phase,
                Phase::Down,
                "{name} was started after the shutdown began (it left Down)"
            );
            assert_eq!(
                service.retry_at, None,
                "{name} was queued for a start after the shutdown began"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Services are stopped in REVERSE plan order.
    ///
    /// That is the only thing that makes the order mean anything: a dependent
    /// has to be down before the thing it depends on. Stopping in plan order
    /// would tear out `netup` while `sshd` is still serving on it.
    ///
    /// Driven through `next_to_stop` rather than real processes: signalling
    /// needs `/bin/kill`, and with the signals failing a forwards walk and a
    /// backwards walk leave exactly the same wreckage, so a state-based test
    /// would pass either way.
    #[test]
    fn the_teardown_walks_the_plan_backwards() {
        // Plan order is [first, second, third]; service indices need not match.
        let order = vec![7, 8, 9];
        let mut live = vec![false; 10];
        for i in [7, 8, 9] {
            live[i] = true;
        }
        let mut seen = Vec::new();
        let mut cursor = 0;
        while let Some((index, next)) = next_to_stop(&order, cursor, &live) {
            seen.push(index);
            cursor = next;
        }
        assert_eq!(
            seen,
            vec![9, 8, 7],
            "the teardown did not walk the plan backwards"
        );

        // Units already down are skipped, and the cursor never revisits one.
        live[8] = false;
        let mut seen = Vec::new();
        let mut cursor = 0;
        while let Some((index, next)) = next_to_stop(&order, cursor, &live) {
            seen.push(index);
            cursor = next;
        }
        assert_eq!(seen, vec![9, 7], "a unit that was already down was stopped");

        // Nothing live: the teardown is finished, which is what tells the loop
        // to run /etc/shutdown and hand off.
        assert_eq!(next_to_stop(&order, 0, &[false; 10]), None);
        // And an exhausted cursor terminates rather than wrapping.
        assert_eq!(next_to_stop(&order, order.len(), &live), None);
    }

    /// The greeter is `restart=always` and its exit IS one of the shutdown's steps.
    ///
    /// `/etc/tty-session` ends by exec'ing `td-svc reboot`, so the process the
    /// supervisor is watching for the greeter becomes the client asking to
    /// reboot — and stopping the greeter is one of the steps that request
    /// triggers. Read that exit as a crash and a `restart=always` unit comes
    /// back mid-teardown, forever: the shutdown would stop it, it would
    /// respawn, and `advance_shutdown` would never reach the handoff. The
    /// short-circuit is `stopping`, checked before the restart policy.
    #[test]
    fn a_restart_always_unit_stopped_by_the_shutdown_does_not_come_back() {
        let (mut rt, dir) = shutdown_runtime(
            "[greeter]\ntype=daemon\nexec=/etc/tty-session\nrestart=always\n",
            "greeter",
        );
        mark_running(&mut rt, "greeter", Duration::from_secs(60));
        // The state the teardown's `control_stop` leaves behind, set directly so
        // the claim under test does not rest on a host having `/bin/kill`: the
        // signal is not what makes the restart wrong, `stopping` is.
        {
            let service = rt.lookup_mut("greeter").unwrap();
            service.pid = Some(i32::MAX);
            service.stopping = true;
            service.phase = Phase::Ready;
        }
        rt.begin_shutdown(Power::Reboot);

        // The greeter exits, as `exit`/Ctrl-D at the console always makes it.
        // `restart=always` must not fire.
        rt.on_exit("greeter", Some(0));
        let greeter = rt.lookup("greeter").unwrap();
        assert_eq!(
            greeter.retry_at, None,
            "a restart=always greeter was queued for restart by its own shutdown"
        );
        assert_eq!(greeter.phase, Phase::Stopped, "the stop did not complete");

        // With nothing left up, the teardown reaches the handoff rather than
        // stopping a greeter that keeps coming back.
        assert_eq!(
            rt.advance_shutdown(),
            Some(Power::Reboot),
            "the shutdown never finished"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// I6's other half: a supervisor that comes back to a marker supervises
    /// NOTHING.
    ///
    /// PID 1 respawns td-svc unconditionally, so this is the boot after a crash
    /// mid-teardown. Starting anything here brings services up against
    /// filesystems `/etc/shutdown` has already released. `run` never returns,
    /// so the decision lives in `resume_if_marked` where a test can reach it.
    #[test]
    fn a_supervisor_that_resumes_a_shutdown_starts_nothing() {
        let (mut rt, dir) = shutdown_runtime(
            "[a]\ntype=daemon\nexec=/x\nrestart=always\n\
             [b]\ntype=oneshot\nexec=/x\n",
            "resume",
        );
        write_marker(&rt.marker_path, Power::Halt).unwrap();

        rt.resume_if_marked();
        assert!(
            rt.shutdown.is_some(),
            "a marker on disk did not resume the teardown"
        );
        // The ACTION survives, or a machine asked to halt comes back rebooting.
        assert_eq!(
            rt.shutdown.as_ref().map(|s| s.power),
            Some(Power::Halt),
            "the resumed shutdown lost the power action"
        );

        rt.start_eligible();
        for name in ["a", "b"] {
            let service = rt.lookup(name).unwrap();
            assert_eq!(
                service.phase,
                Phase::Down,
                "{name} was started by a supervisor resuming a shutdown"
            );
            assert_eq!(service.retry_at, None, "{name} was queued for a start");
        }

        // No marker means an ordinary boot, and nothing is resumed.
        let (mut fresh, dir2) = shutdown_runtime("[a]\ntype=oneshot\nexec=/x\n", "noresume");
        fresh.resume_if_marked();
        assert!(
            fresh.shutdown.is_none(),
            "a boot with no marker resumed a shutdown that never happened"
        );
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&dir2);
    }

    /// Arm against scratch sysctls and hand back the sentinel's pid.
    ///
    /// A REAL `arm_cad`, not a fake: the pid the events below carry has to be
    /// the one the runtime believes in, and pointing the paths somewhere
    /// writable is the only part of arming a test may not do for real — the
    /// true sysctls would disarm the machine running this suite and aim its
    /// kernel at a pid belonging to the test harness.
    ///
    /// The sentinel here is this test binary re-executed with `cad-sentinel`,
    /// which libtest reads as a filter matching no test, so it exits at once.
    /// That is fine and is why the pid is returned: arming is complete the
    /// moment the runtime records it, and every assertion below is about what
    /// the runtime does with a death, not about how long the child lived.
    fn arm_at(rt: &mut Runtime, dir: &str) -> i32 {
        rt.cad_enabled_path = format!("{dir}/enabled");
        rt.cad_pid_path = format!("{dir}/cad_pid");
        rt.arm_cad();
        rt.cad.as_ref().map_or(0, |armed| armed.pid)
    }

    /// Arming writes both sysctls and leaves a sentinel the runtime holds.
    ///
    /// Every other test here starts from a death, which says nothing about
    /// whether anything was ever armed — and an `arm_cad` that silently gave up
    /// on every path would satisfy all of them. This is the one that fails if
    /// Ctrl-Alt-Del is never set up at all.
    #[test]
    fn arming_writes_both_sysctls_and_holds_a_sentinel() {
        let (mut rt, dir) = shutdown_runtime("[a]\ntype=oneshot\nexec=/x\n", "cad-arm");
        let pid = arm_at(&mut rt, &dir);
        assert!(pid > 0, "arming did not record a sentinel");
        assert!(rt.cad.is_some(), "the runtime is not holding the sentinel");
        // The hard reset is off, so a press is a signal rather than a reset...
        assert_eq!(
            std::fs::read_to_string(format!("{dir}/enabled"))
                .unwrap_or_default()
                .trim(),
            "0",
            "the kernel's own hard reset was left enabled"
        );
        // ...and it is aimed at the sentinel this runtime holds, not at PID 1.
        assert_eq!(
            std::fs::read_to_string(format!("{dir}/cad_pid"))
                .unwrap_or_default()
                .trim(),
            pid.to_string(),
            "cad_pid does not name the sentinel that was armed"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A sentinel killed by SIGINT is a shutdown request; anything else is a
    /// broken sentinel.
    ///
    /// The kernel's only way to say "Ctrl-Alt-Del" is to kill `cad_pid` with
    /// SIGINT, so the SIGNAL is the whole message. Reading any death as a press
    /// would turn a sentinel that crashed — or that someone killed while
    /// debugging — into an unannounced reboot of a live machine.
    #[test]
    fn only_a_sigint_death_of_the_sentinel_is_a_shutdown_request() {
        let (mut rt, dir) = shutdown_runtime("[a]\ntype=oneshot\nexec=/x\n", "cad-int");
        let pid = arm_at(&mut rt, &dir);

        rt.on_sentinel_died(pid, Some(crate::sys::SIGINT));
        assert!(
            rt.shutdown.is_some(),
            "a SIGINT death of the sentinel must begin a shutdown"
        );
        assert_eq!(
            rt.shutdown.as_ref().map(|s| s.power),
            Some(Power::Reboot),
            "a press reboots"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_sentinel_that_died_of_anything_else_does_not_shut_the_machine_down() {
        let (mut rt, dir) = shutdown_runtime("[a]\ntype=oneshot\nexec=/x\n", "cad-other");

        for death in [Some(crate::sys::SIGKILL), None] {
            rt.shutdown = None;
            let pid = arm_at(&mut rt, &dir);
            rt.on_sentinel_died(pid, death);
            assert!(
                rt.shutdown.is_none(),
                "a sentinel that died of {death:?} is a bug to re-arm from, not a press"
            );
            assert!(
                rt.cad_retry_at.is_some(),
                "a sentinel that died of {death:?} left nothing to re-arm from"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The death of a sentinel we already replaced is not news.
    ///
    /// Retiring a sentinel is done by closing its pipe, so EVERY re-arm leaves
    /// the previous one's watcher thread about to report a death. Acting on
    /// that report drops the sentinel just armed — whose own watcher then
    /// reports ITS death, which drops the next one. The loop is unbounded, it
    /// forks a process per turn, and every sentinel in it is correctly armed at
    /// the moment it is destroyed.
    #[test]
    fn the_death_of_a_replaced_sentinel_is_ignored() {
        let (mut rt, dir) = shutdown_runtime("[a]\ntype=oneshot\nexec=/x\n", "cad-stale");
        let first = arm_at(&mut rt, &dir);
        let second = arm_at(&mut rt, &dir);
        assert_ne!(first, second, "the re-arm reused the retired sentinel's pid");

        rt.on_sentinel_died(first, None);
        assert_eq!(
            rt.cad.as_ref().map(|armed| armed.pid),
            Some(second),
            "a retired sentinel's death dropped the live one"
        );
        assert!(
            rt.cad_retry_at.is_none(),
            "a retired sentinel's death scheduled a re-arm on top of a live sentinel"
        );
        // And it is a press for a sentinel nobody holds any more, too: SIGINT
        // from a stale watcher must not reboot a machine that is armed and well.
        rt.on_sentinel_died(first, Some(crate::sys::SIGINT));
        assert!(
            rt.shutdown.is_none(),
            "a retired sentinel's SIGINT rebooted the machine"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An arming that fails part way reaps the sentinel and tries again.
    ///
    /// Failing AFTER the hard reset is off is the dangerous shape: the machine
    /// has neither the kernel's reset nor a sentinel, which is worse than
    /// either state this module chooses between, so it cannot be left there.
    /// And the sentinel spawned before the failure has to be reaped — td-svc is
    /// not PID 1, so nothing else will, and a dropped `Child` is never waited
    /// on.
    #[test]
    fn an_arming_that_fails_after_the_sentinel_exists_reaps_it_and_retries() {
        let (mut rt, dir) = shutdown_runtime("[a]\ntype=oneshot\nexec=/x\n", "cad-halfarm");
        rt.cad_enabled_path = format!("{dir}/enabled");
        // A DIRECTORY where cad_pid should be: the write fails (EISDIR) after
        // the sentinel is already running and the hard reset already off.
        let blocked = format!("{dir}/cad_pid");
        let _ = std::fs::create_dir_all(&blocked);
        rt.cad_pid_path = blocked;

        rt.arm_cad();
        assert!(rt.cad.is_none(), "a failed arming recorded a sentinel anyway");
        assert!(
            rt.cad_retry_at.is_some(),
            "a failure that left the hard reset disabled did not schedule a retry"
        );
        // The watcher only reports once its `wait` returned, so this event IS
        // the proof the sentinel was reaped rather than left a zombie.
        assert!(
            matches!(
                rt.next_event(Duration::from_secs(10)),
                Some(Event::SentinelDied { .. })
            ),
            "the abandoned sentinel was never waited on; it is a zombie"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A sysctl that does not exist is not a fault, and not retried forever.
    ///
    /// On td's current `allnoconfig` kernel there is no CONFIG_VT, so
    /// `ctrl_alt_del()` has no caller and the sysctl is simply absent. Retrying
    /// that on a timer until the machine is switched off would be a scheduled
    /// failure; a write that fails for any OTHER reason is worth retrying,
    /// because the file is there and something transient stopped it.
    #[test]
    fn an_absent_sysctl_is_not_retried_but_a_failing_one_is() {
        let (mut rt, dir) = shutdown_runtime("[a]\ntype=oneshot\nexec=/x\n", "cad-absent");
        rt.cad_enabled_path = format!("{dir}/no-such-sysctl/ctrl-alt-del");
        rt.cad_pid_path = format!("{dir}/cad_pid");
        rt.arm_cad();
        assert!(rt.cad.is_none(), "arming succeeded against a missing sysctl");
        assert!(
            rt.cad_retry_at.is_none(),
            "a sysctl that will never appear was put on a retry timer"
        );

        // Present, but unwritable: a directory in its place.
        let present = format!("{dir}/enabled");
        let _ = std::fs::create_dir_all(&present);
        rt.cad_enabled_path = present;
        rt.arm_cad();
        assert!(rt.cad.is_none(), "arming succeeded against an unwritable sysctl");
        assert!(
            rt.cad_retry_at.is_some(),
            "a sysctl that is THERE and failed to take was not retried"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A sentinel that cannot stay alive is retried on a backoff, not a spin.
    ///
    /// The failure this bounds is a persistent one — a binary whose
    /// `cad-sentinel` verb stopped routing, say — where every re-arm dies as
    /// fast as it is spawned. Re-arming inline made that a fork loop that
    /// starved the event loop and filled the console.
    #[test]
    fn a_sentinel_that_keeps_dying_backs_off_instead_of_spinning() {
        let (mut rt, dir) = shutdown_runtime("[a]\ntype=oneshot\nexec=/x\n", "cad-backoff");
        let pid = arm_at(&mut rt, &dir);

        rt.on_sentinel_died(pid, None);
        assert_eq!(rt.cad_failures, 1, "the first failure was not counted");
        let first = rt.cad_retry_at.expect("no re-arm was scheduled");
        assert!(
            rt.cad.is_none(),
            "the re-arm happened inline; nothing was throttled"
        );
        // Not yet due: the loop must not re-arm early, or the backoff is a
        // number nobody reads.
        rt.advance_cad();
        assert!(rt.cad.is_none(), "re-armed before the backoff elapsed");
        assert_eq!(rt.cad_retry_at, Some(first), "the schedule was disturbed");

        // Due now.
        rt.cad_retry_at = Some(Instant::now());
        rt.advance_cad();
        let next = rt.cad.as_ref().map(|armed| armed.pid);
        assert!(next.is_some(), "the scheduled re-arm never armed anything");
        assert!(rt.cad_retry_at.is_none(), "the schedule was not cleared");

        // A second failure waits longer than the first.
        rt.on_sentinel_died(next.unwrap_or(0), None);
        assert_eq!(rt.cad_failures, 2, "consecutive failures are not accumulating");
        assert!(
            crate::backoff::delay(2) > crate::backoff::delay(1),
            "the backoff does not grow, so this test proves nothing"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A runtime whose eviction record is a scratch file, not `/run`.
    fn evict_runtime(text: &str, tag: &str) -> (Runtime, String) {
        let (mut rt, dir) = shutdown_runtime(text, tag);
        rt.started_path = format!("{dir}/started");
        (rt, dir)
    }

    /// Starting a service RECORDS it. Without this the whole mechanism is
    /// inert: a supervisor that records nothing leaves its successor an empty
    /// file and nothing to evict.
    ///
    /// Every other test here hand-writes the record, so this is the one that
    /// exercises the writer, and it needs a unit that really spawns.
    #[test]
    fn starting_a_service_records_it_for_the_next_supervisor() {
        let (mut rt, dir) = evict_runtime(
            "[a]\ntype=daemon\nexec=/bin/sh -c 'sleep 30'\n",
            "evict-writer",
        );
        rt.start_eligible();
        let Some(pid) = rt.lookup("a").and_then(|service| service.pid) else {
            eprintln!("note: the unit did not spawn here; skipping");
            let _ = std::fs::remove_dir_all(&dir);
            return;
        };
        let (entries, problems) = crate::evict::read(&rt.started_path);
        assert!(problems.is_empty(), "{problems:?}");
        assert!(
            entries.iter().any(|e| e.pid == pid && e.name == "a"),
            "the spawn was not recorded, so a successor has nothing to evict: {entries:?}"
        );
        let _ = rt.signal(Containment::Process(pid), crate::sys::SIGKILL);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The recorded terminal — not `/proc` — decides how a console orphan is
    /// addressed.
    ///
    /// This is the module's headline claim and the reason `tty` is in the file
    /// at all: the child never carries the device (td-svc opens consoles
    /// `O_NOCTTY`), so a containment re-derived from `/proc` narrows to the
    /// wrapper and leaves the login tree running.
    #[test]
    fn a_recorded_terminal_decides_how_a_console_orphan_is_addressed() {
        let console = crate::evict::Entry {
            pid: 4242,
            starttime: 7,
            tty: 1032,
            name: "greeter".to_string(),
        };
        assert_eq!(
            containment_of(&console),
            Containment::Console {
                leader: 4242,
                tty: 1032,
            },
            "a recorded terminal was ignored; the login tree would survive"
        );
        // With no terminal recorded there is nothing to prefer, so it is read
        // fresh — and an absent pid leads nothing we can prove.
        let plain = crate::evict::Entry {
            tty: 0,
            ..console.clone()
        };
        assert_eq!(containment_of(&plain), Containment::Process(4242));
    }

    /// I3 at the eviction's decision point: an incomplete scan is not
    /// emptiness. Reading it the other way turns an unreadable `/proc` into
    /// "already evicted" and starts the duplicate.
    #[test]
    fn an_incomplete_scan_is_not_an_evicted_containment() {
        assert!(
            scan_is_empty(procfs::Scan::default()),
            "a clean empty scan is emptiness"
        );
        assert!(
            !scan_is_empty(procfs::Scan {
                pids: Vec::new(),
                errors: vec!["/proc/7: boom".to_string()],
            }),
            "an unreadable scan was read as evicted"
        );
    }

    /// The refusal DECISION, not its spelling: a name is refused because its
    /// containment is still occupied, and is carried forward for the next
    /// supervisor at the same time.
    ///
    /// `refuse_unevicted` signals nothing, which is what lets this point at a
    /// process the test wants to keep.
    #[test]
    fn an_orphan_still_in_its_containment_refuses_its_unit() {
        let (mut rt, dir) = evict_runtime("[a]\ntype=oneshot\nexec=/x\n", "evict-refuse");
        let Some(live) = live_child() else {
            eprintln!("note: cannot spawn a child here; skipping");
            let _ = std::fs::remove_dir_all(&dir);
            return;
        };
        let entry = crate::evict::Entry {
            pid: live.pid,
            starttime: live.starttime,
            tty: 0,
            name: "a".to_string(),
        };
        let refused = rt.refuse_unevicted(vec![(entry, Containment::Process(live.pid))]);
        assert_eq!(
            refused,
            vec!["a".to_string()],
            "a unit whose orphan is still running was not refused"
        );
        assert!(
            rt.unevicted.iter().any(|e| e.pid == live.pid),
            "the refused orphan was not carried forward: {:?}",
            rt.unevicted
        );

        // And one that IS gone is neither refused nor carried.
        let gone = crate::evict::Entry {
            pid: i32::MAX,
            starttime: 1,
            tty: 0,
            name: "b".to_string(),
        };
        let refused = rt.refuse_unevicted(vec![(gone, Containment::Process(i32::MAX))]);
        assert!(
            refused.is_empty(),
            "an empty containment refused its unit anyway: {refused:?}"
        );
        drop(live);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn log_scratch(tag: &str) -> String {
        let dir = format!(
            "{}/td-svc-capture-{}-{tag}",
            std::env::temp_dir().display(),
            std::process::id()
        );
        let _ = std::fs::create_dir_all(&dir);
        dir
    }

    /// Poll a log until it holds what was asked for, or give up.
    fn log_reaches(path: &str, wanted: &[&str]) -> String {
        for _ in 0..500 {
            let text = std::fs::read_to_string(path).unwrap_or_default();
            if wanted.iter().all(|w| text.contains(w)) {
                return text;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        std::fs::read_to_string(path).unwrap_or_default()
    }

    /// A service with `log=` really has its output captured — through the whole
    /// path, from `build` wiring a pipe to a line in the file.
    ///
    /// BOTH streams: stdout and stderr are separate pipes and separate drains,
    /// and a service's failures arrive on the one it is easiest to forget.
    #[test]
    fn a_captured_service_writes_both_its_streams_to_its_log() {
        let dir = log_scratch("both");
        let path = format!("{dir}/a.log");
        let mut rt = runtime(&format!(
            "[a]\ntype=oneshot\nexec=/bin/sh -c 'echo to-stdout; echo to-stderr >&2'\nlog={path}\n"
        ));
        rt.start_eligible();
        if rt.lookup("a").and_then(|service| service.pid).is_none() {
            eprintln!("note: the unit did not spawn here; skipping");
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }
        let text = log_reaches(&path, &["to-stdout", "to-stderr"]);
        assert!(
            text.contains("to-stdout"),
            "stdout was not captured: {text:?}"
        );
        assert!(
            text.contains("to-stderr"),
            "stderr was not captured: {text:?}"
        );
        rt.close_logs();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A restart does NOT get a second writer.
    ///
    /// Two writers on one path race their rotations: both rename, and one then
    /// reopens a file the other has already moved. The capture is per SERVICE
    /// and outlives the instance, so a crash-looping daemon keeps one.
    #[test]
    fn a_restarted_service_keeps_the_one_writer_it_already_had() {
        let dir = log_scratch("restart");
        let path = format!("{dir}/a.log");
        let mut rt = runtime(&format!(
            "[a]\ntype=oneshot\nexec=/bin/sh -c 'echo once'\nlog={path}\n"
        ));
        rt.start_eligible();
        let Some(first) = rt.lookup("a").and_then(|service| service.log.clone()) else {
            eprintln!("note: the unit did not spawn here; skipping");
            let _ = std::fs::remove_dir_all(&dir);
            return;
        };
        // The first instance is reaped before the second starts: leaving it
        // running would put a stray child behind every run of the suite.
        if let Some(pid) = rt.lookup("a").and_then(|service| service.pid) {
            let _ = rt.signal(Containment::Process(pid), crate::sys::SIGKILL);
        }
        rt.start(0);
        let second = rt.lookup("a").and_then(|service| service.log.clone());
        assert!(
            second.is_some_and(|s| Arc::ptr_eq(&s, &first)),
            "the restart created a second writer on the same file"
        );
        rt.close_logs();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The shutdown releases every /var handle, and that is what `umount /var`
    /// needs. Ordering matters as much as the act: this runs BEFORE
    /// `/etc/shutdown`, which is where the unmount happens.
    #[test]
    fn the_shutdown_releases_every_log_handle() {
        let dir = log_scratch("close");
        let path = format!("{dir}/a.log");
        let mut rt = runtime(&format!(
            "[a]\ntype=oneshot\nexec=/bin/sh -c 'echo hello'\nlog={path}\n"
        ));
        rt.start_eligible();
        if rt.lookup("a").and_then(|service| service.log.clone()).is_none() {
            eprintln!("note: the unit did not spawn here; skipping");
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }
        let _ = log_reaches(&path, &["hello"]);
        let stuck = rt.close_logs();
        assert!(stuck.is_empty(), "a writer would not let go: {stuck:?}");
        let held = std::fs::read_dir("/proc/self/fd")
            .map(|entries| {
                entries.flatten().any(|entry| {
                    std::fs::read_link(entry.path())
                        .is_ok_and(|t| t == std::path::Path::new(&path))
                })
            })
            .unwrap_or(false);
        assert!(
            !held,
            "a log descriptor outlived the shutdown close; umount /var would fail EBUSY"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A reload that drops a service stops its writer.
    ///
    /// The `Service` is the last holder of that handle, so a writer left
    /// running waits forever on a queue nothing can reach — still holding the
    /// `/var` descriptor that makes `umount` fail, and no longer reachable
    /// from `captures()` for the shutdown to stop.
    #[test]
    fn a_service_dropped_by_a_reload_takes_its_log_writer_with_it() {
        let dir = log_scratch("reload-drop");
        let path = format!("{dir}/a.log");
        let table = format!("{dir}/table.conf");
        let _ = std::fs::write(
            &table,
            format!("[a]\ntype=oneshot\nexec=/bin/sh -c ':'\nlog={path}\n"),
        );
        let mut rt = runtime(&format!(
            "[a]\ntype=oneshot\nexec=/bin/sh -c ':'\nlog={path}\n"
        ));
        rt.table_path = table.clone();
        rt.start_eligible();
        let Some(capture) = rt.lookup("a").and_then(|service| service.log.clone()) else {
            eprintln!("note: the unit did not spawn here; skipping");
            let _ = std::fs::remove_dir_all(&dir);
            return;
        };
        // The oneshot has run; its exit event is processed by the loop this
        // test does not run, so the pid is cleared here. What is being tested
        // is the reload's disposal of a service that is DOWN and no longer
        // declared, which is the path that drops the `Service` outright.
        if let Some(service) = rt.lookup_mut("a") {
            service.pid = None;
        }
        let _ = std::fs::write(&table, "[b]\ntype=oneshot\nexec=/bin/sh -c ':'\n");
        let reply = rt.control("reload");
        assert!(!reply.starts_with("error:"), "{reply}");
        assert!(
            rt.lookup("a").is_none(),
            "the dropped unit is still in the table"
        );
        // Observed DIRECTLY, not through `close_all` — that stops the writer
        // itself, so asking through it could not tell a writer the reload
        // retired from one the question had just stopped.
        let mut closed = false;
        for _ in 0..500 {
            if capture.is_closed() {
                closed = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            closed,
            "the dropped service's writer is still running and still holding its file; \
             the shutdown can no longer reach it to close"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A reload that MOVES a service's log is honoured on the next start.
    ///
    /// A capture is keyed to the destination it was opened for, so reusing it
    /// would keep writing to the previous file and make the reload silently
    /// not apply.
    #[test]
    fn a_reload_that_changes_where_output_goes_retires_the_old_writer() {
        let dir = log_scratch("reload-move");
        let first = format!("{dir}/first.log");
        let second = format!("{dir}/second.log");
        let table = format!("{dir}/table.conf");
        let unit = |log: &str| format!("[a]\ntype=daemon\nexec=/bin/sh -c 'sleep 30'\nlog={log}\n");
        let _ = std::fs::write(&table, unit(&first));
        let mut rt = runtime(&unit(&first));
        rt.table_path = table.clone();
        rt.start_eligible();
        let Some(before) = rt.lookup("a").and_then(|service| service.log.clone()) else {
            eprintln!("note: the unit did not spawn here; skipping");
            let _ = std::fs::remove_dir_all(&dir);
            return;
        };

        let _ = std::fs::write(&table, unit(&second));
        let reply = rt.control("reload");
        assert!(!reply.starts_with("error:"), "{reply}");
        assert!(
            rt.lookup("a").and_then(|service| service.log.clone()).is_none(),
            "the capture opened on the OLD path survived the reload; output would keep going there"
        );
        let mut closed = false;
        for _ in 0..500 {
            if before.is_closed() {
                closed = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(closed, "the writer on the old path is still running");
        if let Some(pid) = rt.lookup("a").and_then(|service| service.pid) {
            let _ = rt.signal(Containment::Process(pid), crate::sys::SIGKILL);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// No writer means NO PIPE. A pipe td-svc holds and never reads is the
    /// worst outcome this feature has available: the service blocks in
    /// `write(2)` the moment it fills the buffer, and a service wedged by its
    /// own logging is exactly what the bounded queue exists to prevent.
    ///
    /// The unit then runs with td-svc's own stdio, as it did before capture
    /// existed — degraded, but running and not blocked.
    #[test]
    fn a_unit_whose_writer_could_not_start_gets_no_pipe_to_block_on() {
        let unit = crate::table::Unit {
            name: "a".to_string(),
            argv: vec!["/bin/sh".to_string(), "-c".to_string(), ":".to_string()],
            log: Some("/var/log/a.log".to_string()),
            ..Default::default()
        };
        // Spawned, because the pipe ends only exist on a real `Child`.
        // Distinguished from a spawn failure so a missing /bin/sh cannot
        // report itself as a wiring bug.
        let piped = |captured: bool| -> Option<(bool, bool)> {
            let (mut cmd, _) = build(&unit, false, captured).ok()?;
            let mut child = cmd.spawn().ok()?;
            let wired = (child.stdout.is_some(), child.stderr.is_some());
            let _ = child.wait();
            Some(wired)
        };
        let Some(without) = piped(false) else {
            eprintln!("note: cannot spawn a child here; skipping");
            return;
        };
        assert_eq!(
            without,
            (false, false),
            "a pipe was wired with no writer to empty it; the service would wedge"
        );
        // And WITH a writer it is piped, or nothing would be captured at all.
        assert_eq!(
            piped(true),
            Some((true, true)),
            "a captured unit was not given pipes"
        );
    }

    /// A group whose LEADER is already gone is still evicted.
    ///
    /// The dangerous shape: sshd forks its children into its own process group
    /// and then dies. The recorded pid is unfindable, but the group is exactly
    /// what `Containment` exists to reach — and skipping the entry because the
    /// leader is missing leaves those children running and starts a second
    /// sshd beside them.
    #[test]
    fn a_dead_leader_does_not_hide_its_surviving_group() {
        use std::os::unix::process::CommandExt;
        let (mut rt, dir) = evict_runtime("[a]\ntype=oneshot\nexec=/x\n", "evict-orphaned-group");
        // A group leader that backgrounds a child and exits. The child stays in
        // the leader's group, which keeps its id as long as a member lives.
        let Ok(mut leader) = Command::new("/bin/sh")
            .arg("-c")
            .arg("sh -c 'sleep 30' & exit 0")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
        else {
            eprintln!("note: cannot spawn a child here; skipping");
            let _ = std::fs::remove_dir_all(&dir);
            return;
        };
        let Ok(pid) = i32::try_from(leader.id()) else {
            let _ = std::fs::remove_dir_all(&dir);
            return;
        };
        let starttime = procfs::stat_of(pid)
            .ok()
            .flatten()
            .map_or(0, |st| st.starttime);
        // Reaped, so the number is FREE — which is what makes addressing the
        // group by it sound. (If the kernel reissues it to a stranger between
        // here and the scan, `survivors_of` answers None and this test skips
        // rather than misfires.)
        let _ = leader.wait();
        if !matches!(procfs::stat_of(pid), Ok(None)) {
            eprintln!("note: the leader's pid did not free up; skipping");
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }

        crate::evict::write(
            &rt.started_path,
            &[crate::evict::Entry {
                pid,
                starttime,
                tty: 0,
                name: "a".to_string(),
            }],
        )
        .unwrap();
        let refused = rt.evict_orphans();
        assert!(
            refused.is_empty(),
            "the group was reachable, so its unit should not be refused: {refused:?}"
        );
        assert!(
            procfs::members(Containment::Group(pid), rt.self_pid)
                .is_ok_and(|scan| scan.proven_empty()),
            "the leader's children outlived the eviction; a duplicate would follow"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An orphan that could NOT be cleared stays in the record.
    ///
    /// Otherwise the failure compounds: this supervisor refuses to start the
    /// unit, then dies, and its successor reads a clean file and starts the
    /// duplicate the refusal was protecting against.
    #[test]
    fn an_orphan_that_survived_stays_in_the_record() {
        let (mut rt, dir) = evict_runtime("[a]\ntype=oneshot\nexec=/x\n", "evict-carry");
        rt.carry_forward(crate::evict::Entry {
            pid: 4242,
            starttime: 99,
            tty: 1032,
            name: "a".to_string(),
        });
        // Twice with the same pid must not grow the file: `carry_forward` is
        // reached once per boot, but the record it writes outlives the boot.
        rt.carry_forward(crate::evict::Entry {
            pid: 4242,
            starttime: 99,
            tty: 1032,
            name: "a".to_string(),
        });
        rt.persist_started();

        let (entries, problems) = crate::evict::read(&rt.started_path);
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(
            entries,
            vec![crate::evict::Entry {
                pid: 4242,
                starttime: 99,
                tty: 1032,
                name: "a".to_string(),
            }],
            "the unevicted orphan did not survive into the successor's record"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A record anyone can write is not a list of processes root will signal.
    ///
    /// The directory is the barrier: `/run/td-svc` is created 0700 so that the
    /// only writer is td-svc itself. A reader that runs BEFORE the socket is
    /// bound — eviction does — has to check that for itself.
    #[test]
    fn a_record_in_a_directory_others_can_write_is_ignored() {
        use std::os::unix::fs::PermissionsExt;
        let dir = format!(
            "{}/td-svc-untrusted-{}",
            std::env::temp_dir().display(),
            std::process::id()
        );
        let _ = std::fs::create_dir_all(&dir);
        let path = format!("{dir}/started");
        crate::evict::write(
            &path,
            &[crate::evict::Entry {
                pid: 4242,
                starttime: 99,
                tty: 0,
                name: "a".to_string(),
            }],
        )
        .unwrap();
        let (entries, _) = crate::evict::read(&path);
        assert_eq!(entries.len(), 1, "a 0700 directory should be readable");

        // Readable-but-not-writable is fine: only root can put a record in a
        // root-owned 0755 directory, and refusing it would disable eviction on
        // a machine that is not under attack.
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755));
        let (entries, problems) = crate::evict::read(&path);
        assert_eq!(
            entries.len(),
            1,
            "a directory others can only READ was refused: {problems:?}"
        );

        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o777));
        let (entries, problems) = crate::evict::read(&path);
        assert!(
            entries.is_empty(),
            "a record anyone could have written was believed: {entries:?}"
        );
        assert_eq!(problems.len(), 1, "the refusal was silent: {problems:?}");
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An orphan a previous supervisor left running is killed.
    ///
    /// The whole point: PID 1 respawns td-svc, so without this the replacement
    /// starts a second copy of everything the first one was running.
    #[test]
    fn an_orphan_from_a_previous_supervisor_is_evicted() {
        let (mut rt, dir) = evict_runtime("[a]\ntype=oneshot\nexec=/x\n", "evict-live");
        let Some(orphan) = live_group_leader() else {
            eprintln!("note: cannot spawn a child here; skipping");
            let _ = std::fs::remove_dir_all(&dir);
            return;
        };
        let pid = orphan.pid;
        crate::evict::write(
            &rt.started_path,
            &[crate::evict::Entry {
                pid,
                starttime: orphan.starttime,
                tty: 0,
                name: "a".to_string(),
            }],
        )
        .unwrap();

        let refused = rt.evict_orphans();
        assert!(
            refused.is_empty(),
            "a killable orphan should not have refused its unit: {refused:?}"
        );
        assert!(
            gone_within(pid, Duration::from_secs(10)),
            "the orphan is still running; a duplicate is about to be started"
        );
        // And it drops out of the record, so a later read is not re-deciding
        // about a pid that is already gone. The file itself stays: entries that
        // were NOT cleared have to survive in it.
        assert!(
            rt.unevicted.is_empty(),
            "a cleared orphan was carried forward: {:?}",
            rt.unevicted
        );
        let (left, _) = crate::evict::read(&rt.started_path);
        assert!(
            !left.iter().any(|e| e.pid == pid),
            "the evicted pid is still in the record: {left:?}"
        );
        drop(orphan);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A recorded pid whose starttime does NOT match is left alone.
    ///
    /// This is the safety property, and it is the reason the record carries a
    /// starttime at all. Pids are reissued; a record naming one that has been
    /// reused points at a stranger, and this file is a list of things td-svc
    /// is about to KILL. Getting it wrong once means killing an unrelated
    /// process on every boot after a supervisor crash.
    #[test]
    fn a_recorded_pid_whose_starttime_differs_is_not_touched() {
        let (mut rt, dir) = evict_runtime("[a]\ntype=oneshot\nexec=/x\n", "evict-reuse");
        let Some(bystander) = live_group_leader() else {
            eprintln!("note: cannot spawn a child here; skipping");
            let _ = std::fs::remove_dir_all(&dir);
            return;
        };
        let pid = bystander.pid;
        // The same pid, a different process: exactly what pid reuse looks like
        // from a record written before the reuse.
        crate::evict::write(
            &rt.started_path,
            &[crate::evict::Entry {
                pid,
                starttime: bystander.starttime.wrapping_add(1),
                tty: 0,
                name: "a".to_string(),
            }],
        )
        .unwrap();

        let refused = rt.evict_orphans();
        assert!(
            refused.is_empty(),
            "a pid that is not ours must not refuse a unit either: {refused:?}"
        );
        // Give any (wrong) signal time to land before believing it did not.
        std::thread::sleep(Duration::from_millis(300));
        // ALIVE, not merely present. A killed child whose `Child` nothing has
        // reaped is a ZOMBIE, and `/proc/<pid>/stat` still exists for one — so
        // an is-it-there assertion is satisfied by the very corpse it is
        // supposed to rule out, and passes whether or not the identity check
        // is doing anything.
        let stat = procfs::stat_of(pid).ok().flatten();
        assert!(
            stat.as_ref().is_some_and(|st| !st.zombie),
            "td-svc killed a process that was NOT the one recorded (state: {})",
            match stat {
                Some(st) if st.zombie => "zombie",
                Some(_) => "alive",
                None => "reaped",
            }
        );
        drop(bystander);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A unit whose previous copy survived eviction is not started again.
    ///
    /// The residue case, and the one where a duplicate hurts most: something
    /// is still holding the port or the terminal. `Failed` with no `retry_at`
    /// is the state `start_eligible` will not leave on its own.
    #[test]
    fn a_unit_whose_orphan_survived_is_not_started() {
        let (mut rt, dir) = evict_runtime("[a]\ntype=oneshot\nexec=/x\n", "evict-refuse");
        rt.refuse_duplicates(&["a".to_string()]);
        let service = rt.lookup("a").expect("the unit vanished");
        assert_eq!(service.phase, Phase::Failed, "the unit was not refused");
        assert!(
            service.retry_at.is_none(),
            "a retry would start the duplicate anyway, on a timer"
        );
        // And a pass of the loop's own start logic leaves it alone. This is
        // the assertion that matters: the phase above is only how the refusal
        // is spelled, and `start_eligible` is what reads it.
        let _ = rt.start_eligible();
        assert!(
            rt.lookup("a").is_some_and(|s| s.pid.is_none()),
            "the refused unit was started anyway"
        );
        assert_eq!(
            rt.lookup("a").map(|s| s.phase),
            Some(Phase::Failed),
            "the refusal did not survive a pass of start_eligible"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// What is running is recorded, and what cannot be verified is not.
    #[test]
    fn the_record_holds_the_live_set_and_only_the_verifiable_part_of_it() {
        let (mut rt, dir) = evict_runtime(
            "[a]\ntype=oneshot\nexec=/x\n[b]\ntype=oneshot\nexec=/x\n",
            "evict-record",
        );
        if let Some(service) = rt.lookup_mut("a") {
            service.pid = Some(4242);
            service.starttime = Some(99);
            service.tty_dev = Some(1032);
        }
        // No starttime: running, but nothing can prove which process it is.
        if let Some(service) = rt.lookup_mut("b") {
            service.pid = Some(4243);
            service.starttime = None;
        }
        rt.persist_started();

        let (entries, problems) = crate::evict::read(&rt.started_path);
        assert!(problems.is_empty(), "the record did not parse: {problems:?}");
        assert_eq!(
            entries,
            vec![crate::evict::Entry {
                pid: 4242,
                starttime: 99,
                tty: 1032,
                name: "a".to_string(),
            }],
            "an unverifiable pid must not be handed to a successor to kill"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A sentinel that stayed up clears the count; one that did not, does not.
    ///
    /// The count means CONSECUTIVE failures. If it only ever grew, a boot that
    /// hit a dozen transient arming failures would answer one unrelated death
    /// hours later at `backoff::CAP` — five minutes unarmed, with the kernel's
    /// hard reset off, for a fault that had nothing to do with them.
    #[test]
    fn a_sentinel_that_stayed_up_clears_the_rearm_backoff() {
        let (mut rt, dir) = shutdown_runtime("[a]\ntype=oneshot\nexec=/x\n", "cad-reset");
        let pid = arm_at(&mut rt, &dir);
        rt.cad_failures = 12;
        // Armed long enough ago to count as having stayed up.
        if let Some(armed) = rt.cad.as_mut() {
            armed.since = Instant::now()
                .checked_sub(crate::backoff::MIN_UPTIME * 2)
                .unwrap_or_else(Instant::now);
        }
        rt.on_sentinel_died(pid, None);
        assert_eq!(
            rt.cad_failures, 1,
            "a sentinel that stayed up must reset the count, leaving only this failure"
        );

        // And one that died immediately does NOT clear it: that is exactly the
        // case the backoff exists to throttle.
        let pid = arm_at(&mut rt, &dir);
        rt.cad_failures = 4;
        rt.on_sentinel_died(pid, None);
        assert_eq!(
            rt.cad_failures, 5,
            "a sentinel that died at once must keep accumulating"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A re-arm scheduled before a shutdown does not fire during it.
    ///
    /// `schedule_cad_rearm` refuses to make new timers once a teardown is in
    /// flight, but a timer made BEFORE it began outlives that check and comes
    /// due in the middle of the teardown. Firing it spawns a sentinel into a
    /// sequence whose whole job is to stop processes — one more thing to stop,
    /// arriving after the walk that decides what to stop was computed.
    #[test]
    fn a_rearm_scheduled_before_a_shutdown_is_dropped_not_fired() {
        let (mut rt, dir) = shutdown_runtime("[a]\ntype=oneshot\nexec=/x\n", "cad-pending");
        let pid = arm_at(&mut rt, &dir);
        rt.on_sentinel_died(pid, None);
        assert!(rt.cad_retry_at.is_some(), "no re-arm was scheduled to test");

        // The teardown begins while that timer is pending, and it comes due.
        assert!(!rt.begin_shutdown(Power::Reboot).starts_with("error: "));
        rt.cad_retry_at = Some(Instant::now());
        rt.advance_cad();
        assert!(
            rt.cad.is_none(),
            "a stale re-arm spawned a sentinel into the teardown"
        );
        assert!(
            rt.cad_retry_at.is_none(),
            "the stale re-arm is still pending; it will be retried every pass"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A press that could not begin a shutdown must leave the machine armed.
    ///
    /// The arming is spent either way — the sentinel is dead. If the shutdown
    /// then fails, the machine is still up, still running, and now catches no
    /// presses at all while the kernel's own hard reset is disabled. That is
    /// strictly worse than both behaviours this module chooses between: the
    /// keys do nothing whatsoever.
    #[test]
    fn a_press_whose_shutdown_is_refused_re_arms() {
        let (mut rt, dir) = shutdown_runtime("[a]\ntype=oneshot\nexec=/x\n", "cad-refused");
        // An unwritable marker is how `begin_shutdown` fails: it records the
        // shutdown BEFORE stopping anything (I6), so a marker it cannot write
        // means nothing has been stopped and the machine is still live. A FILE
        // in the parent's place, because the writer creates missing directories
        // — a merely absent parent is not a failure at all.
        let blocker = format!("{dir}/blocker");
        let _ = std::fs::write(&blocker, "not a directory\n");
        rt.marker_path = format!("{blocker}/shutdown");
        let pid = arm_at(&mut rt, &dir);

        rt.on_sentinel_died(pid, Some(crate::sys::SIGINT));
        assert!(
            rt.shutdown.is_none(),
            "the shutdown was supposed to fail; this test proves nothing"
        );
        assert!(
            rt.cad_retry_at.is_some(),
            "a refused press left Ctrl-Alt-Del disarmed forever"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Once the teardown has begun, a sentinel death changes nothing.
    ///
    /// Two presses are ordinary — someone holds the keys down, or presses again
    /// because nothing visible happened yet. The second must not restart the
    /// sequence (I6 is monotonic), and must not re-arm: a fresh sentinel would
    /// be one more process for the teardown to stop.
    #[test]
    fn a_press_during_a_shutdown_neither_restarts_it_nor_re_arms() {
        let (mut rt, dir) = shutdown_runtime("[a]\ntype=oneshot\nexec=/x\n", "cad-twice");
        let pid = arm_at(&mut rt, &dir);

        rt.on_sentinel_died(pid, Some(crate::sys::SIGINT));
        let first = rt.shutdown.as_ref().map(|s| (s.power, s.cursor));
        assert!(first.is_some(), "the first press must begin the shutdown");
        assert!(
            rt.cad.is_none() && rt.cad_retry_at.is_none(),
            "a press re-armed instead of letting the teardown proceed"
        );

        // A second press can only exist if something armed one during the
        // teardown; arm it by hand so the assertion is about what a press does
        // then, not about there being nothing to press.
        let again = arm_at(&mut rt, &dir);
        rt.on_sentinel_died(again, Some(crate::sys::SIGINT));
        assert_eq!(
            rt.shutdown.as_ref().map(|s| (s.power, s.cursor)),
            first,
            "a second press restarted the teardown"
        );
        // And a non-press death during a shutdown must not build a new sentinel.
        let again = arm_at(&mut rt, &dir);
        rt.on_sentinel_died(again, None);
        assert!(
            rt.cad.is_none() && rt.cad_retry_at.is_none(),
            "the supervisor re-armed while a shutdown was in flight"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The plan a real table produces is the one the reverse walk consumes.
    ///
    /// `next_to_stop` is pinned on synthetic indices above; this is the other
    /// half — that `order` really does run dependencies-first, so reversing it
    /// really does stop dependents first.
    #[test]
    fn the_plan_this_walks_backwards_is_dependencies_first() {
        let rt = runtime(
            "[netup]\ntype=oneshot\nexec=/bin/true\n\
             [sshd]\ntype=daemon\nexec=/x\nafter=netup\n",
        );
        let names: Vec<&str> = rt
            .order
            .iter()
            .filter_map(|&i| rt.services.get(i))
            .map(|s| s.unit.name.as_str())
            .collect();
        assert_eq!(
            names,
            ["netup", "sshd"],
            "the plan is not dependencies-first, so reversing it is not \
             dependents-first either"
        );
    }

    /// A requested stop keeps its identity through the KILL.
    ///
    /// `escalate` used to release `pid`/`starttime` unconditionally, but the
    /// waiter thread still owns the `Child` and the leader can outlive the KILL
    /// by a moment. With the pid gone the sweep reads the containment as empty
    /// (a `Console` scope cannot see the leader once its half is narrowed
    /// away), declares `Stopped` over a live leader, and the late exit then
    /// runs the restart policy — putting back the service an operator stopped.
    ///
    /// No `/bin/kill` is needed: an empty scope means nothing is signalled, so
    /// this isolates the release from the send.
    #[test]
    fn escalating_a_requested_stop_does_not_release_the_leader() {
        let mut rt = runtime("[a]\ntype=daemon\nexec=/x\nrestart=always\n");
        let index = rt.index_of("a").unwrap();
        {
            let service = rt.lookup_mut("a").unwrap();
            service.phase = Phase::Ready;
            service.pid = Some(4242);
            service.starttime = Some(1);
            service.stopping = true;
            // Nothing to signal, so `escalate` reaches the release directly.
            service.stop_scope = None;
            service.kill_at = Instant::now().checked_sub(SWEPT_AGO);
        }
        rt.escalate(index);
        let service = rt.lookup("a").unwrap();
        assert_eq!(
            service.pid,
            Some(4242),
            "the leader's identity was released while its waiter still owned the \
             child; the sweep can now call a live leader stopped"
        );
        assert!(service.starttime.is_some(), "the start time went with it");
    }

    /// An exit that arrives AFTER a stop completed is not a crash.
    ///
    /// `stopping` is cleared the moment the containment proves empty, so a
    /// leader whose exit lands later hits the ordinary restart policy — and a
    /// `restart=always` daemon comes straight back, with `Stopped` still on the
    /// screen that reported it.
    #[test]
    fn an_exit_after_a_completed_stop_does_not_restart_the_unit() {
        let mut rt = runtime("[a]\ntype=daemon\nexec=/x\nrestart=always\n");
        {
            let service = rt.lookup_mut("a").unwrap();
            service.phase = Phase::Stopped;
            service.pid = Some(4242);
            service.starttime = Some(1);
            service.stopping = false;
        }
        rt.on_exit("a", None);
        let service = rt.lookup("a").unwrap();
        assert_eq!(
            service.phase,
            Phase::Stopped,
            "a late exit reopened a stop that had already completed"
        );
        assert!(
            service.retry_at.is_none(),
            "a restart was scheduled for a service an operator stopped"
        );
    }

    /// The KILL is aimed at the RECORDED scope, through the real call site.
    ///
    /// `kill_target` is pinned as a function, but nothing pinned that
    /// `escalate` passes `stop_scope` into it: replacing that argument with
    /// `None` left every test green while silently restoring the P0. The
    /// `killed` flag is set exactly when a target was chosen, so it stands in
    /// for the send on a host with no `/bin/kill`.
    #[test]
    fn escalate_aims_at_the_recorded_scope() {
        let mut rt = runtime("[a]\ntype=daemon\nexec=/x\nrestart=always\n");
        let index = rt.index_of("a").unwrap();
        {
            let service = rt.lookup_mut("a").unwrap();
            service.phase = Phase::Ready;
            service.pid = None; // leader reaped; survivors hold the group
            service.stopping = true;
            service.stop_scope = Some(Containment::Group(999_998));
            service.kill_at = Instant::now().checked_sub(SWEPT_AGO);
        }
        rt.escalate(index);
        let service = rt.lookup("a").unwrap();
        assert!(
            service.killed,
            "no KILL was aimed at the recorded scope, so survivors of a stop are \
             never killed once the leader is reaped"
        );
        assert!(
            service.next_sweep.is_some(),
            "no re-scan was scheduled after the KILL, so nothing confirms it worked"
        );
    }

    /// A pending sweep must WAKE the loop, or it is not a timer.
    ///
    /// Nothing else wakes td-svc when a containment drains, so a sweep left out
    /// of `next_wake` waits on whatever unrelated deadline happens to be
    /// nearest — a crash-looper parked at the five-minute backoff cap, with the
    /// console down for all of it.
    #[test]
    fn a_pending_sweep_is_a_wake_source() {
        let mut rt = runtime("[a]\ntype=daemon\nexec=/x\n");
        let soon = Instant::now().checked_add(Duration::from_secs(1)).unwrap();
        {
            let service = rt.lookup_mut("a").unwrap();
            service.phase = Phase::Ready;
            service.stopping = true;
            service.pid = None;
            service.next_sweep = Some(soon);
        }
        let wake = rt.start_eligible();
        assert!(
            wake.is_some_and(|at| at <= soon),
            "a pending sweep did not enter the wake calculation: {wake:?}"
        );
    }

    /// A stop in flight is neither running nor stopped, and both readers of
    /// that state used to see "running".
    ///
    /// The phase cannot move until the stop completes — saying `Stopped` before
    /// the containment is empty is the fail-open I4 exists to refuse — so a
    /// unit whose leader is reaped but whose survivors remain sits at `Ready`.
    /// That is right for the invariant and wrong for everyone reading it: the
    /// operator sees a healthy service, and a strict dependent starts against
    /// one that is gone.
    #[test]
    fn a_stop_in_flight_is_reported_and_does_not_satisfy_a_strict_requires() {
        let mut rt = runtime(
            "[a]\ntype=daemon\nexec=/x\nrestart=always\n\
             [b]\ntype=oneshot\nexec=/bin/true\nrequires=a\n",
        );
        {
            let service = rt.lookup_mut("a").unwrap();
            // The state a leader-reaped stop leaves behind.
            service.phase = Phase::Ready;
            service.pid = None;
            service.stopping = true;
        }
        let index = rt.index_of("a").unwrap();
        let line = rt.status_line(index);
        assert!(
            line.contains("a stopping"),
            "a unit mid-stop reported its pre-stop phase: {line}"
        );
        let b = rt.lookup("b").unwrap().unit.clone();
        assert_eq!(
            rt.requires_failed(&b).as_deref(),
            Some("a"),
            "a strict dependent started against a service that was being torn down"
        );
    }

    /// The KILL half of a requested stop, which a reaped leader used to erase.
    ///
    /// `escalate` re-derived its target, and deriving one needs a live pid — so
    /// the moment the leader was reaped the KILL addressed `None` and did
    /// nothing at all. The survivors it exists to reach are precisely that
    /// case: the stop is unfinished BECAUSE something outlived the leader. On
    /// the shipped greeter that is an auto-logged-in interactive shell, which
    /// ignores TERM, so `stop greeter` left a live root shell on the console
    /// and no KILL ever came.
    #[test]
    fn a_requested_stop_kills_the_scope_it_termed_even_once_the_leader_is_reaped() {
        // The leader is gone (`same` false) but the group it led still has
        // members. A group id cannot be recycled while the group is non-empty,
        // so it stays a legitimate target.
        assert_eq!(
            kill_target(true, false, Some(Containment::Group(4242)), None),
            Some(Containment::Group(4242)),
            "the KILL was skipped once the leader was reaped, so a survivor \
             that ignores TERM is never killed at all"
        );
        // Same for a console: the terminal half is keyed on a DEVICE, so it
        // survives the leader and still names the login tree.
        assert_eq!(
            kill_target(
                true,
                false,
                Some(Containment::Console {
                    leader: 77,
                    tty: 1088
                }),
                None
            ),
            Some(Containment::Console {
                leader: 0,
                tty: 1088
            }),
            "the console's terminal half was dropped with its leader"
        );
        // But the pid-keyed half must go: it may already be someone else.
        assert_eq!(
            kill_target(true, false, Some(Containment::Process(77)), None),
            None,
            "a single-process scope was KILLed after its leader was reaped, so \
             the signal lands on whatever recycled that pid"
        );
    }

    /// The KILL goes to the RECORDED scope, never a freshly derived one.
    ///
    /// Recording `stop_scope` is pointless if the escalation re-derives. A
    /// `tty=` unit whose device stops resolving between the TERM and the KILL
    /// derives down to its wrapper alone — so the TERM would go to the whole
    /// terminal and the KILL to one process of it.
    #[test]
    fn the_kill_addresses_the_same_set_the_term_did() {
        let recorded = Containment::Console {
            leader: 5,
            tty: 1088,
        };
        let derived = Containment::Process(5);
        assert_eq!(
            kill_target(true, true, Some(recorded), Some(derived)),
            Some(recorded),
            "the KILL re-derived its target instead of using the one the TERM \
             went to"
        );
        // With no stop in flight there is nothing recorded to honour, and a
        // leader that is no longer ours leaves nothing safe to address.
        assert_eq!(
            kill_target(false, true, None, Some(derived)),
            Some(derived)
        );
        assert_eq!(kill_target(false, false, Some(recorded), Some(derived)), None);
    }

    /// `start` during a stop whose leader is already reaped.
    ///
    /// The leader exiting is not the stop finishing (I4), and that state has
    /// `pid == None`. Reading it as "not running" cleared `stopping` and
    /// queued a SECOND instance alongside processes the operator had just
    /// asked to end — with `kill_at` dropped, so nothing swept them either.
    #[test]
    fn start_during_a_stop_whose_leader_is_reaped_does_not_double_start() {
        let mut rt = runtime("[a]\ntype=daemon\nexec=/x\nrestart=always\n");
        {
            let service = rt.lookup_mut("a").unwrap();
            service.phase = Phase::Ready;
            service.pid = None;
            service.stopping = true;
            service.stop_scope = Some(Containment::Group(999_999));
        }
        let reply = rt.control("start a");
        assert!(reply.contains("stop in progress"), "{reply}");
        let service = rt.lookup("a").unwrap();
        assert!(
            service.stopping,
            "the in-flight stop was cleared; nothing would sweep its survivors"
        );
        assert!(
            service.start_after_stop,
            "the start was neither honoured now nor recorded for later"
        );
        assert_ne!(
            service.phase,
            Phase::Down,
            "the unit was queued to start beside processes still being torn down"
        );
    }

    /// An UNREADABLE scan is not an empty one (I3). A stop that cannot confirm
    /// emptiness must stay a stop, or the teardown proceeds over a live service.
    #[test]
    fn a_scan_that_cannot_be_read_does_not_count_as_stopped() {
        // The arm this is named for. An unreadable `/proc` cannot be arranged
        // from a unit test, which is why the decision is a function: without
        // this the I3 reading had nothing pinning it, and returning 0 here —
        // proceeding with a teardown over a service that may still be running
        // — left the suite green.
        let oops = io::Error::other("proc went away");
        assert_eq!(
            occupancy(Err(&oops)),
            1,
            "an unreadable scan was counted as an empty containment"
        );

        // A scan that found nothing but could not read everything is not
        // empty either.
        let partial = procfs::Scan {
            pids: Vec::new(),
            errors: vec!["/proc/9: EACCES".to_string()],
        };
        assert_eq!(
            occupancy(Ok(&partial)),
            1,
            "a scan with unread entries was treated as proof of emptiness"
        );

        // And the control case, or the two above could pass by always
        // answering 1.
        let empty = procfs::Scan {
            pids: Vec::new(),
            errors: Vec::new(),
        };
        assert_eq!(occupancy(Ok(&empty)), 0);

        // End to end: a scope that is provably empty completes the stop.
        let mut rt = runtime("[a]\ntype=daemon\nexec=/x\n");
        {
            let service = rt.lookup_mut("a").unwrap();
            service.stopping = true;
            service.stop_scope = None;
        }
        rt.on_exit("a", None);
        assert_eq!(rt.lookup("a").unwrap().phase, Phase::Stopped);
    }

    /// The TERM gets the same identity check the KILL does, one step earlier.
    ///
    /// A waiter can already have reaped the child while its `Exited` event is
    /// still in the channel, and the kernel can have handed the pid on. For a
    /// `tty=` unit that would derive a containment from a STRANGER and TERM
    /// everything on its terminal, so this fails closed and signals nothing.
    #[test]
    fn a_stop_refuses_a_pid_it_cannot_prove_is_still_ours() {
        let mut rt = runtime("[a]\ntype=daemon\nexec=/x\nrestart=always\n");
        let Some(live) = live_child() else { return };
        {
            let service = rt.lookup_mut("a").unwrap();
            service.phase = Phase::Ready;
            service.pid = Some(live.pid);
            // A start time that is not this process's: the shape a recycled pid
            // presents.
            service.starttime = Some(live.starttime.wrapping_add(999_999));
            service.started = Some(Instant::now());
        }
        let reply = rt.control("stop a");
        assert!(
            reply.starts_with("error:") && reply.contains("no longer the process"),
            "a recycled pid was signalled anyway: {reply:?}"
        );
        let service = rt.lookup("a").unwrap();
        assert!(!service.stopping, "a refused stop was recorded as in flight");
        assert!(service.kill_at.is_none(), "a refused stop armed a KILL");
    }

    /// `stop` during an in-flight stop must not leave a stale restart intent,
    /// and `start` during one must not silently do nothing.
    #[test]
    fn a_second_request_while_a_stop_is_in_flight_does_not_strand_the_unit() {
        let mut rt = runtime("[a]\ntype=daemon\nexec=/x\nrestart=always\n");
        let Some(live) = live_child() else { return };
        {
            let service = rt.lookup_mut("a").unwrap();
            service.phase = Phase::Ready;
            service.pid = Some(live.pid);
            service.starttime = Some(live.starttime);
            service.started = Some(Instant::now());
        }
        // restart arms the intent...
        assert!(rt.control("restart a").contains("restart requested"));
        assert!(rt.lookup("a").unwrap().start_after_stop);

        // ...and `start` during the stop keeps it rather than replying
        // "already running" and doing nothing.
        let reply = rt.control("start a");
        assert!(reply.contains("stop in progress"), "{reply}");
        assert!(rt.lookup("a").unwrap().start_after_stop);

        // A `stop` after the LEADER is gone must clear the restart intent, or
        // the stop would complete into a start nobody asked for any more...
        rt.lookup_mut("a").unwrap().pid = None;
        let reply = rt.control("stop a");
        assert!(reply.contains("already in progress"), "{reply}");
        let service = rt.lookup("a").unwrap();
        assert!(
            !service.start_after_stop,
            "a stale restart intent survived a stop"
        );
        // ...but it must NOT declare the unit stopped or drop the teardown. A
        // reaped leader is not an empty containment (I4), and clearing
        // `stopping` here strands whatever is still in it with nothing left to
        // sweep it — the fail-open this whole path exists to refuse.
        assert!(
            service.stopping,
            "a second stop abandoned the teardown already in flight"
        );
        assert_ne!(
            service.phase,
            Phase::Stopped,
            "the unit reported stopped without its containment being checked"
        );
    }

    /// `requires=` asks whether the dependency is THERE. A service an operator
    /// stopped is exactly as absent as one that failed, so it must not satisfy
    /// a strict dependency — even though it settles for `after=`, which asks
    /// only whether a decision has been reached.
    #[test]
    fn a_stopped_unit_does_not_satisfy_a_strict_requires() {
        let mut rt = runtime(
            "[a]\ntype=daemon\nexec=/x\nrestart=always\n\
             [b]\ntype=oneshot\nexec=/bin/true\nrequires=a\n",
        );
        rt.lookup_mut("a").unwrap().phase = Phase::Stopped;
        let b = rt.lookup("b").unwrap().unit.clone();
        assert_eq!(
            rt.requires_failed(&b).as_deref(),
            Some("a"),
            "a strict dependent started while what it requires was stopped"
        );
    }

    /// The containment that actually reaches a login tree — keyed on the
    /// device the UNIT named, because the child never has one.
    ///
    /// An earlier draft read field 7 off the child and was inert: td-svc opens
    /// the tty `O_NOCTTY`, so the wrapper never acquires a controlling terminal
    /// and its field 7 stays 0 for its whole life. Confirmed by running the
    /// shipped binary against a real pty. Its test fabricated a wrapper that
    /// already held the terminal — a state that cannot occur — so it passed
    /// while the real login tree escaped. This one asks `containment()`, which
    /// resolves the configured device, rather than asserting on `classify`.
    #[test]
    fn a_console_unit_is_contained_by_the_device_it_named_plus_its_leader() {
        // A device that exists on any Linux host, so this resolves for real.
        let (unit_tty, path) = ("tty", "/dev/tty");
        let Ok(meta) = std::fs::metadata(path) else {
            return; // no /dev/tty in this sandbox; the recipe leg covers it
        };
        let expect = i32::try_from(std::os::unix::fs::MetadataExt::rdev(&meta)).unwrap();
        let mut rt = runtime(&format!(
            "[greeter]\ntype=daemon\nexec=/x\ntty={unit_tty}\n"
        ));
        let child = rt.self_pid.wrapping_add(1);
        {
            let service = rt.lookup_mut("greeter").unwrap();
            service.pid = Some(child);
            // What `attach_tty` records at spawn, read off the descriptor it
            // actually opened.
            service.tty_dev = Some(expect);
        }
        let mode = rt.lookup("greeter").unwrap().containment().unwrap().unwrap();
        assert_eq!(
            mode,
            Containment::Console {
                leader: child,
                tty: expect
            },
            "a console unit must be contained by the device it holds; keying on the \
             child's own field 7 gives 0, which matches nothing"
        );
        assert!(
            !rt.contains_self(mode),
            "containment {mode:?} would have signalled td-svc itself"
        );

        // The ATTACHED device wins over the one the unit named. They differ
        // whenever `attach_tty` fell back to `/dev/console`, and a containment
        // built from the name would then signal a terminal this service is not
        // on — someone else's session.
        let elsewhere = expect.wrapping_add(1);
        rt.lookup_mut("greeter").unwrap().tty_dev = Some(elsewhere);
        assert_eq!(
            rt.lookup("greeter").unwrap().containment().unwrap().unwrap(),
            Containment::Console {
                leader: child,
                tty: elsewhere
            },
            "containment re-resolved the unit's tty= instead of using the device \
             the running instance actually got"
        );

        // And with nothing attached there is no console containment at all:
        // that state means `attach_tty` opened NOTHING, so the child has no
        // terminal and the device it merely named belongs to someone else.
        rt.lookup_mut("greeter").unwrap().tty_dev = None;
        let fallen_back = rt.lookup("greeter").unwrap().containment().unwrap();
        assert!(
            !matches!(fallen_back, Some(Containment::Console { .. })),
            "a unit that got no terminal was contained by the device it named: \
             {fallen_back:?}"
        );
    }

    /// Both halves are needed, and a test that drops either would still pass on
    /// the other. The leader alone misses everything getty exec'd; the terminal
    /// alone misses the one process td-svc actually waits on.
    #[test]
    fn a_console_containment_reaches_the_leader_and_the_terminal_separately() {
        let leader = 424_242;
        let tty = 1088; // /dev/ttyS0
        let mode = Containment::Console { leader, tty };
        let on_tty = procfs::Stat {
            pgrp: 9,
            session: 9,
            tty_nr: tty,
            starttime: 1,
            zombie: false,
        };
        let elsewhere = procfs::Stat {
            pgrp: 9,
            session: 9,
            tty_nr: 0,
            starttime: 1,
            zombie: false,
        };
        // getty and its descendants: on the terminal, unrelated pid.
        assert!(procfs::matches(mode, 999, &on_tty));
        // the wrapper: the leader pid, with NO controlling terminal.
        assert!(procfs::matches(mode, leader, &elsewhere));
        // a daemon with neither: must not be selected. This is the one that
        // matters — every process on the machine has tty_nr 0.
        assert!(!procfs::matches(mode, 1000, &elsewhere));
    }

    /// tty 0 is "no controlling terminal", which every daemon on the machine
    /// has — td-svc and PID 1 included. It must never MATCH: a console
    /// containment naming device 0 would otherwise select the entire system.
    #[test]
    fn no_controlling_terminal_is_never_a_containment() {
        let rt = runtime("[greeter]\ntype=daemon\nexec=/x\ntty=ttyS0\n");
        let child = rt.self_pid.wrapping_add(1);
        let own = rt.self_ids.unwrap_or(procfs::Stat {
            pgrp: rt.self_pid,
            session: rt.self_pid,
            tty_nr: 0,
            starttime: 0,
            zombie: false,
        });
        let (own_pgrp, own_session) = (own.pgrp, own.session);
        let no_tty = procfs::Stat {
            pgrp: own_pgrp,
            session: own_session,
            tty_nr: 0,
            starttime: 1,
            zombie: false,
        };
        assert_eq!(classify(child, &no_tty), Containment::Process(child));
        // `attached_device` refuses to build a 0, and the scan refuses to match
        // one that arrived by any other route: an unrelated daemon (tty_nr 0,
        // not the leader) must not be selected by a Console naming device 0.
        assert_eq!(packed_device(0), None);
        let elsewhere = procfs::Stat {
            pgrp: 9,
            session: 9,
            tty_nr: 0,
            starttime: 1,
            zombie: false,
        };
        assert!(
            !procfs::matches(
                Containment::Console {
                    leader: child,
                    tty: 0
                },
                child.wrapping_add(5),
                &elsewhere
            ),
            "device 0 matched an unrelated daemon; 0 means NO controlling terminal, \
             so that set is every process on the machine"
        );
    }

    /// ...and the send-side backstop refuses it even if the two ever disagree.
    #[test]
    fn signalling_a_containment_td_svc_is_inside_is_refused() {
        let mut rt = runtime("[a]\ntype=daemon\nexec=/x\n");
        let own = rt.self_ids.unwrap_or(procfs::Stat {
            pgrp: rt.self_pid,
            session: rt.self_pid,
            tty_nr: 0,
            starttime: 0,
            zombie: false,
        });
        let (own_pgrp, own_session) = (own.pgrp, own.session);
        assert!(rt.contains_self(Containment::Process(rt.self_pid)));
        assert!(rt.contains_self(Containment::Group(own_pgrp)));
        assert!(rt.contains_self(Containment::Session(own_session)));
        // td-svc holds no controlling terminal, so its own tty_nr is 0 — which
        // makes a Console naming device 0, the set that would be every daemon,
        // self-refusing before the scan is ever reached.
        assert!(rt.contains_self(Containment::Console {
            leader: rt.self_pid.wrapping_add(1),
            tty: own.tty_nr
        }));
        // A group led by some other pid is not ours.
        assert!(!rt.contains_self(Containment::Group(rt.self_pid.wrapping_add(1))));

        // Not knowing its own ids means refusing every group and session. The
        // earlier draft substituted self_pid and let them ALL through, because
        // a pid never equals a real pgid held by something else.
        rt.self_ids = None;
        assert!(rt.contains_self(Containment::Group(rt.self_pid.wrapping_add(1))));
        assert!(rt.contains_self(Containment::Session(rt.self_pid.wrapping_add(1))));
        // A single pid is still identifiable without them.
        assert!(!rt.contains_self(Containment::Process(rt.self_pid.wrapping_add(1))));
    }

    /// A strict dependency crash-looping at the cap is worse news than one
    /// that failed once, so `Held` must gate `requires=` too.
    #[test]
    fn a_held_strict_dependency_skips_its_dependent() {
        let mut rt = runtime(
            "[dep]\ntype=daemon\nexec=/x\nrestart=always\n\
             [svc]\ntype=daemon\nexec=/y\nrequires=dep\n",
        );
        rt.lookup_mut("dep").unwrap().phase = Phase::Held;
        let svc = rt.lookup("svc").unwrap().unit.clone();
        assert_eq!(rt.requires_failed(&svc), Some("dep".into()));
    }

    /// A TERM the unit ignores must escalate. Without the KILL the process runs
    /// on for good and holds its waiter thread with it.
    #[test]
    fn a_unit_that_ignores_the_term_is_killed_after_its_stop_timeout() {
        let mut rt = runtime("[slow]\ntype=oneshot\nexec=/x\ntimeout=1\nstop-timeout=5\n");
        {
            let service = rt.lookup_mut("slow").unwrap();
            service.phase = Phase::Starting;
            service.pid = Some(4242);
            service.deadline = Instant::now().checked_sub(Duration::from_secs(1));
        }
        rt.enforce_deadlines();
        let slow = rt.lookup("slow").unwrap();
        assert_eq!(slow.phase, Phase::Failed);
        assert!(slow.kill_at.is_some(), "the KILL must be scheduled, not skipped");
        assert_eq!(slow.pid, Some(4242), "the pid is the only handle on it");

        // ...and once that elapses, the escalation runs and lets the pid go.
        rt.lookup_mut("slow").unwrap().kill_at = Instant::now().checked_sub(Duration::from_secs(1));
        rt.enforce_deadlines();
        let slow = rt.lookup("slow").unwrap();
        assert!(slow.kill_at.is_none());
        assert_eq!(slow.pid, None);
    }

    #[test]
    fn a_service_with_no_pid_has_nothing_to_contain() {
        let rt = runtime("[a]\ntype=daemon\nexec=/x\n");
        assert_eq!(rt.lookup("a").unwrap().containment().unwrap(), None);
    }

    /// One derivation, two callers — `build` reports diagnostics against it and
    /// `attach_tty` opens it, and a bare name means `/dev/<name>` exactly as
    /// td-init resolves an inittab id field.
    #[test]
    fn a_bare_tty_name_resolves_under_dev_and_an_absolute_one_is_left_alone() {
        assert_eq!(tty_path("ttyS0"), "/dev/ttyS0");
        assert_eq!(tty_path("/dev/ttyS0"), "/dev/ttyS0");
        assert_eq!(tty_path("/dev/pts/3"), "/dev/pts/3");
    }
}

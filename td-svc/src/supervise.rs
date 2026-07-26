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
}

impl Phase {
    /// Has this unit reached a decision? `after=` waits for one; it does NOT
    /// require success. A unit stuck at `Down` with a pending retry is NOT
    /// settled on its first attempt but IS once it has failed one, or a missing
    /// binary would block every dependent — the console included — for the ~7
    /// minutes it takes backoff to reach the hold.
    fn settled(self) -> bool {
        matches!(self, Phase::Ready | Phase::Failed | Phase::Held)
    }
}

pub struct Service {
    pub unit: Unit,
    pub phase: Phase,
    pub pid: Option<i32>,
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
    /// Set when the instance a probe was launched for is gone, so the probe
    /// thread stops forking attempts instead of running out its full timeout.
    cancel: Option<Arc<AtomicBool>>,
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
            cancel: None,
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
        let Some(stat) = procfs::stat_of(pid)? else {
            return Ok(None);
        };
        Ok(Some(classify(pid, &stat)))
    }
}

/// Which containment a `tty=` child's `/proc` entry describes. Split out from
/// the read so the classification is testable without fabricating a `/proc`.
fn classify(pid: i32, stat: &procfs::Stat) -> Containment {
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
    Exited {
        name: String,
        generation: u64,
        code: Option<i32>,
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
    fn name(&self) -> &str {
        match self {
            Event::Exited { name, .. }
            | Event::WaitFailed { name, .. }
            | Event::Ready { name, .. }
            | Event::ProbeFailed { name, .. } => name,
        }
    }

    fn generation(&self) -> u64 {
        match self {
            Event::Exited { generation, .. }
            | Event::WaitFailed { generation, .. }
            | Event::Ready { generation, .. }
            | Event::ProbeFailed { generation, .. } => *generation,
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

fn log(msg: &str) {
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
fn build(unit: &Unit) -> Result<Command, String> {
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
                Err(e) => log(&format!("{}: {CONSOLE}: {e}", unit.name)),
            }
        }
        None => {
            use std::os::unix::process::CommandExt;
            cmd.process_group(0);
        }
    }
    Ok(cmd)
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
fn attach_tty(cmd: &mut Command, unit: &Unit) {
    let Some(tty) = &unit.tty else { return };
    let path = tty_path(tty);
    let opened = match open_tty(&path) {
        Ok(file) => Some(file),
        Err(e) => {
            log(&format!("{}: {path}: {e}; falling back to {CONSOLE}", unit.name));
            open_tty(CONSOLE)
                .map_err(|e| log(&format!("{}: {CONSOLE}: {e}; it will have no terminal", unit.name)))
                .ok()
        }
    };
    let Some(file) = opened else { return };
    match file.try_clone() {
        Ok(out) => {
            cmd.stdin(Stdio::from(file)).stdout(Stdio::from(out));
        }
        Err(e) => log(&format!("{}: {path}: {e}", unit.name)),
    }
}

/// The uutils multicall's `kill`. td-svc takes no `kill(2)` surface at all
/// (DESIGN.md §4), so signalling is an exec — and a negative operand is how a
/// whole process group is addressed. That the pinned uutils reads `-<pgid>` as a
/// group and not as a flag is the crate's one external assumption; td-svc-test
/// proves it against the exact argv composed here.
const KILL: &str = "/bin/kill";

/// Send one signal to one target. `target` is a pid, or `-<pgid>` for a group.
///
/// The exit status is discarded on purpose: it reports whether the SEND
/// succeeded, and I3 forbids reading liveness out of it.
fn send_signal(target: &str, signal: &str) {
    let sent = Command::new(KILL)
        .arg(format!("-{signal}"))
        .arg(target)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    // The exit STATUS is discarded per I3 — it reports whether the send landed,
    // and inferring liveness from it is exactly what I3 forbids. A failure to
    // RUN `kill` at all is a different thing and must be said out loud: it
    // means td-svc cannot signal anything, and every "killing it" line above
    // would describe an action that never happened.
    if let Err(e) = sent {
        log(&format!("cannot run {KILL}: {e}; nothing was signalled"));
    }
}

/// Spawn a helper thread, reporting rather than panicking if the OS refuses.
/// `std::thread::spawn` PANICS on failure, and `panic=abort` would make that
/// the end of the supervisor.
///
/// The caller must handle `false`. Every helper here is the only thing that
/// will ever report on a service, so a thread that did not start means an
/// event that never arrives — and the unit would sit at `Starting` forever,
/// blocking its dependents with no diagnostic beyond this one line.
#[must_use]
fn spawn_thread<F: FnOnce() + Send + 'static>(what: &str, body: F) -> bool {
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

pub struct Runtime {
    services: Vec<Service>,
    /// Indices into `services`, in start order. Indices rather than names so
    /// the loop does not clone a `Vec<String>` on every wake.
    order: Vec<usize>,
    tx: Sender<Event>,
    rx: Receiver<Event>,
    self_pid: i32,
    /// td-svc's own group and session, read once at startup — nothing moves it
    /// between them, since it calls neither `setsid` nor `setpgid`.
    ///
    /// `None` means `/proc/self/stat` was unreadable, and then NO group or
    /// session may be signalled at all: without knowing its own ids td-svc
    /// cannot prove a target is not itself, and the guard's whole job is to be
    /// sure. (An earlier draft substituted `self_pid` here and called that
    /// stricter. It is the reverse — a pid never equals a real pgid held by
    /// something else, so every comparison passed and the guard was off exactly
    /// when `/proc` was least trustworthy.)
    self_ids: Option<(i32, i32)>,
}

impl Runtime {
    pub fn new(units: Vec<Unit>) -> (Runtime, Vec<String>) {
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
                self_ids: own.map(|s| (s.pgrp, s.session)),
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
            let failed = matches!(
                self.lookup(dep).map(|s| s.phase),
                Some(Phase::Failed | Phase::Held)
            );
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

        let mut cmd = match build(&unit) {
            Ok(cmd) => cmd,
            Err(e) => {
                self.record_start_failure(index, &format!("{}: {e}", unit.name));
                return;
            }
        };
        attach_tty(&mut cmd, &unit);

        match cmd.spawn() {
            Ok(child) => {
                let pid = child.id() as i32;
                let cancel = Arc::new(AtomicBool::new(false));
                // Read before anything else: field 22 is set at fork, so it is
                // valid the instant `spawn` returns — unlike pgrp/session,
                // which the child has not had a chance to change yet.
                let starttime = procfs::stat_of(pid).ok().flatten().map(|s| s.starttime);
                if let Some(service) = self.services.get_mut(index) {
                    service.pid = Some(pid);
                    service.starttime = starttime;
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
        self.signal(mode, "KILL");
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
        for position in 0..self.order.len() {
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
                Phase::Starting | Phase::Ready => false,
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
        // A oneshot's timeout, and the KILL that follows it, are the other
        // things that can need a wake-up.
        for service in &self.services {
            for at in [service.deadline, service.kill_at].into_iter().flatten() {
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
            Ok(Some(mode)) => self.signal(mode, "TERM"),
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
        let containment = service.containment();
        let same = match (service.pid, service.starttime) {
            (Some(pid), Some(starttime)) => procfs::is_same_process(pid, starttime),
            // No recorded identity means no proof it is still the same process,
            // so nothing is signalled. Failing closed here costs at most a
            // leaked process; failing open costs an unrelated one.
            _ => Ok(false),
        };
        service.kill_at = None;
        match (same, containment) {
            (Ok(true), Ok(Some(mode))) => {
                log(&format!("{name}: did not exit on TERM; killing it"));
                self.signal(mode, "KILL");
            }
            // Already gone — the TERM worked and the waiter reaped it.
            (Ok(false), _) | (_, Ok(None)) => {}
            (Err(e), _) | (_, Err(e)) => {
                log(&format!("{name}: cannot read /proc to kill it: {e}"))
            }
        }
        if let Some(service) = self.services.get_mut(index) {
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
    fn signal(&self, mode: Containment, signal: &str) {
        if self.contains_self(mode) {
            log(&format!(
                "refusing to send {signal} to {mode:?}: td-svc is inside it"
            ));
            return;
        }
        match mode {
            Containment::Process(pid) => send_signal(&pid.to_string(), signal),
            Containment::Group(pgid) => send_signal(&format!("-{pgid}"), signal),
            Containment::Session(_) => {
                // A session has no kill(2) target; the members are enumerated
                // and signalled individually — the `killall5` shape.
                match procfs::members(mode, self.self_pid) {
                    Ok(scan) => {
                        // Best effort, deliberately: one unreadable stranger
                        // must not stop td-svc signalling the ones it read.
                        // (Liveness is the opposite — see procfs::Scan.)
                        for e in &scan.errors {
                            log(&format!("scanning to signal a session: {e}"));
                        }
                        for pid in scan.pids {
                            send_signal(&pid.to_string(), signal);
                        }
                    }
                    Err(e) => log(&format!("cannot enumerate a session to signal it: {e}")),
                }
            }
        }
    }

    /// Would signalling this containment reach td-svc itself? Answers TRUE
    /// when it cannot tell, which is the only safe direction: the cost of a
    /// false yes is one service that does not stop, and of a false no the
    /// supervisor and everything under it.
    fn contains_self(&self, mode: Containment) -> bool {
        match mode {
            Containment::Process(pid) => pid == self.self_pid,
            Containment::Group(pgid) => match self.self_ids {
                Some((own_pgrp, _)) => pgid == own_pgrp || pgid == self.self_pid,
                None => true,
            },
            Containment::Session(sid) => match self.self_ids {
                Some((_, own_session)) => sid == own_session || sid == self.self_pid,
                None => true,
            },
        }
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
        loop {
            self.enforce_deadlines();
            let next_wake = self.start_eligible();
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

    /// Apply one event, ignoring any that describes an instance we have since
    /// replaced. Without the generation check, a probe launched for a dead
    /// instance can mark its replacement ready.
    fn dispatch(&mut self, event: Event) {
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
            if matches!(event, Event::Exited { .. }) {
                if let Some(service) = self.lookup_mut(&name) {
                    if service.kill_at.is_some() {
                        service.kill_at = None;
                        service.pid = None;
                        service.starttime = None;
                    }
                }
            }
            return;
        }
        match event {
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

    /// Every pid belonging to a service, for the stop path.
    #[allow(dead_code)]
    pub fn members_of(&self, name: &str) -> io::Result<procfs::Scan> {
        let Some(service) = self.lookup(name) else {
            return Ok(procfs::Scan::default());
        };
        match service.containment()? {
            Some(mode) => procfs::members(mode, self.self_pid),
            None => Ok(procfs::Scan::default()),
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
            send_signal(&format!("-{}", child.id()), "KILL");
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
        Runtime::new(units).0
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
        let (rt, complaints) = Runtime::new(units);
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
        let (rt, _) = Runtime::new(units);
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
        let (rt, complaints) = Runtime::new(units);
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
        let (own_pgrp, own_session) = rt.self_ids.unwrap_or((rt.self_pid, rt.self_pid));
        let inherited = procfs::Stat {
            pgrp: own_pgrp,
            session: own_session,
            starttime: 1,
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
            starttime: 1,
        };
        assert_eq!(classify(child, &grouped), Containment::Group(child));

        // After setsid: leads both, and the session is what holds the login
        // tree together once the shell starts making groups inside it.
        let sessioned = procfs::Stat {
            pgrp: child,
            session: child,
            starttime: 1,
        };
        assert_eq!(classify(child, &sessioned), Containment::Session(child));
    }

    /// ...and the send-side backstop refuses it even if the two ever disagree.
    #[test]
    fn signalling_a_containment_td_svc_is_inside_is_refused() {
        let mut rt = runtime("[a]\ntype=daemon\nexec=/x\n");
        let (own_pgrp, own_session) = rt.self_ids.unwrap_or((rt.self_pid, rt.self_pid));
        assert!(rt.contains_self(Containment::Process(rt.self_pid)));
        assert!(rt.contains_self(Containment::Group(own_pgrp)));
        assert!(rt.contains_self(Containment::Session(own_session)));
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

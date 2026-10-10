#![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
use super::*;
use std::fs;
use std::os::fd::AsRawFd;
use std::os::unix::fs::DirBuilderExt;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

pub(crate) const ID: &str = "0f1e2d3c-4b5a-6978-8796-a5b4c3d2e1f0";
pub(crate) const OTHER: &str = "11111111-2222-3333-4444-555555555555";
/// Each unit's first leader; each restart's leader is the next pid.
const LEADERS: &[(&str, u32)] = &[("sshd", 100), ("greeter", 200)];
/// `uptime`'s ticks, and a start time before and after it.
const UPTIME: &str = "500.00 12.34\n";
const BEFORE: u64 = 40_000;
const AFTER: u64 = 50_000;

/// How a scripted td-svc answers, through its client.
#[derive(Clone, Copy, Default)]
pub(crate) struct Behavior {
    /// The first exchange is answered busy.
    pub(crate) busy_first: bool,
    /// The first client exits with no reply: its own timeout.
    pub(crate) silent_first: bool,
    /// The first client outlives the exchange's bound.
    pub(crate) hang_first: bool,
    /// Every exchange is answered busy.
    pub(crate) busy_always: bool,
    /// Every client exits with no reply: no supervisor answers.
    pub(crate) absent: bool,
    /// The greeter's restart is refused.
    pub(crate) refuse_greeter: bool,
    /// Statuses that answer `stopping` after each restart.
    pub(crate) stopping_polls: usize,
    /// The greeter's containment never empties.
    pub(crate) never_empties: bool,
    /// The greeter's new leader started before the request.
    pub(crate) stale_leader: bool,
    /// The greeter reads this state for this many polls after its
    /// restart, then runs its new leader.
    pub(crate) greeter_settles: Option<(&'static str, usize)>,
    /// The greeter's statuses after its restart are this late.
    pub(crate) late: Option<Duration>,
}

/// What one scripted client does.
enum Answer {
    Reply(String),
    /// Exits 1 having printed nothing.
    Silent,
    /// Never exits within the exchange's bound.
    Hang,
}

#[derive(Default)]
struct Script {
    behavior: Behavior,
    requests: Vec<String>,
    /// Each unit's restarts so far and its statuses since the last.
    restarts: Vec<(String, u32, usize)>,
}

impl Script {
    fn answer(&mut self, request: &str) -> Answer {
        let first = self.requests.is_empty();
        self.requests.push(request.to_owned());
        if self.behavior.busy_always || (first && self.behavior.busy_first) {
            return Answer::Reply("error: supervisor did not answer in time\n".into());
        }
        if self.behavior.absent || (first && self.behavior.silent_first) {
            return Answer::Silent;
        }
        if first && self.behavior.hang_first {
            return Answer::Hang;
        }
        match self.reply(request) {
            Some(reply) => Answer::Reply(reply),
            None => Answer::Silent,
        }
    }

    fn reply(&mut self, request: &str) -> Option<String> {
        let (verb, unit) = request.split_once(' ')?;
        let &(_, first_leader) = LEADERS.iter().find(|(name, _)| *name == unit)?;
        if !self.restarts.iter().any(|(name, _, _)| name == unit) {
            self.restarts.push((unit.to_owned(), 0, 0));
        }
        let (_, restarts, polls) = self.restarts.iter_mut().find(|(name, _, _)| name == unit)?;
        let leader = first_leader + *restarts;
        match verb {
            "restart" => {
                if unit == "greeter" && self.behavior.refuse_greeter {
                    return Some(format!(
                        "error: greeter: pid {leader} is no longer the process we started; \
                         nothing signalled\n"
                    ));
                }
                *restarts += 1;
                *polls = 0;
                Some(format!(
                    "{unit}: restart requested; TERM sent to Group({leader})\n"
                ))
            }
            "status" if *restarts == 0 => Some(format!("{unit} ready pid={leader} failures=0\n")),
            "status" => {
                *polls += 1;
                let stopping = *polls <= self.behavior.stopping_polls
                    || (unit == "greeter" && self.behavior.never_empties);
                let polled = *polls;
                let settled = self
                    .behavior
                    .greeter_settles
                    .filter(|(_, count)| unit == "greeter" && polled <= *count)
                    .map(|(state, _)| state);
                Some(if let Some(state) = settled.filter(|_| !stopping) {
                    format!("greeter {state} pid=- failures=1\n")
                } else if stopping && unit == "sshd" {
                    "sshd stopping pid=- failures=0 leaf=unproven\n".into()
                } else if stopping {
                    format!("greeter stopping pid={} failures=0\n", leader - 1)
                } else {
                    format!("{unit} ready pid={leader} failures=0\n")
                })
            }
            _ => Some(format!("error: unknown request {verb:?}\n")),
        }
    }
}

/// The scripted td-svc: each control exchange's client child prints what
/// `Behavior` answers, decided as the request is made, as td-svc's loop
/// acts on a request when it reads it.
#[derive(Clone)]
pub(crate) struct Svc {
    script: Arc<Mutex<Script>>,
}

impl Svc {
    pub(crate) fn requests(&self) -> Vec<String> {
        self.script.lock().unwrap().requests.clone()
    }

    /// The host's control launch, its client children bounded by `lifetime`.
    fn control(&self) -> Box<dyn Fn(Verb, &'static str) -> Result<Inspection, String>> {
        let script = Arc::clone(&self.script);
        Box::new(move |verb, unit| {
            let request = format!("{} {unit}", verb.word());
            let (answer, late) = {
                let mut script = script.lock().unwrap();
                let restarted = script.requests.iter().any(|r| r == "restart greeter");
                let late = script
                    .behavior
                    .late
                    .filter(|_| restarted && request == "status greeter");
                (script.answer(&request), late)
            };
            // The reply td-svc gave becomes readable only this late.
            if let Some(late) = late {
                thread::sleep(late);
            }
            let (bytes, code, hang) = match answer {
                Answer::Reply(reply) => {
                    let code = u8::from(reply.starts_with("error:"));
                    (reply.into_bytes(), code, false)
                }
                Answer::Silent => (Vec::new(), 1, false),
                Answer::Hang => (Vec::new(), 0, true),
            };
            crate::inspection::tests::printed(&bytes, code, hang, REPLY_OUTPUT, EXCHANGE)
        })
    }
}

/// A scratch machine: `run`, the login directory, `proc` with `uptime`
/// and each new leader's `stat`, the boot ID, and the control socket's
/// path, all the test's own.
pub(crate) struct Machine {
    pub(crate) root: PathBuf,
    uid: u32,
    gid: u32,
    /// The renders and reboot requests started.
    renders: Arc<AtomicUsize>,
    reboots: Arc<AtomicUsize>,
    svc: Svc,
}

/// A helper's scripted run: what it prints, its exit code, and whether it
/// then outlives every deadline.
#[derive(Clone)]
pub(crate) struct Run {
    bytes: Vec<u8>,
    code: u8,
    hang: bool,
}

/// A helper that prints `text` and exits 0.
pub(crate) fn prints(text: &str) -> Run {
    Run {
        bytes: text.as_bytes().to_vec(),
        code: 0,
        hang: false,
    }
}

impl Run {
    pub(crate) fn exits(self, code: u8) -> Self {
        Self { code, ..self }
    }

    pub(crate) fn hangs(self) -> Self {
        Self { hang: true, ..self }
    }
}

/// td-svc's acceptance of the reboot request.
pub(crate) fn accepted() -> Run {
    prints("reboot requested\n")
}

impl Machine {
    pub(crate) fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "td-authd-revocation-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&root);
        for directory in ["run", "var/lib/td", "proc", "dev"] {
            fs::DirBuilder::new()
                .mode(0o755)
                .recursive(true)
                .create(root.join(directory))
                .unwrap();
        }
        fs::DirBuilder::new()
            .mode(0o700)
            .create(root.join("var/lib/td/login"))
            .unwrap();
        let meta = fs::metadata(&root).unwrap();
        let machine = Self {
            root,
            uid: meta.uid(),
            gid: meta.gid(),
            renders: Arc::default(),
            reboots: Arc::default(),
            svc: Svc {
                script: Arc::default(),
            },
        };
        machine.boot(ID);
        fs::write(machine.root.join("proc/uptime"), UPTIME).unwrap();
        for (_, first) in LEADERS {
            for restart in 1..=32 {
                machine.leader(first + restart, AFTER);
            }
        }
        machine
    }

    /// `pid`'s `stat`, starting at `ticks`, its command name hostile.
    fn leader(&self, pid: u32, ticks: u64) {
        let directory = self.root.join(format!("proc/{pid}"));
        fs::DirBuilder::new()
            .recursive(true)
            .create(&directory)
            .unwrap();
        let fields: Vec<String> = (3..=21)
            .map(|field| if field == 3 { "S".into() } else { "0".into() })
            .collect();
        fs::write(
            directory.join("stat"),
            format!("{pid} (a) b (c) {} {ticks} 0 0\n", fields.join(" ")),
        )
        .unwrap();
    }

    pub(crate) fn boot(&self, id: &str) {
        fs::write(self.root.join("boot_id"), format!("{id}\n")).unwrap();
    }

    pub(crate) fn record_path(&self) -> PathBuf {
        self.root.join("run/td-login-cutover")
    }

    pub(crate) fn guard(&self) -> PathBuf {
        self.root.join("var/lib/td/login/cutover-reboot")
    }

    /// The record for `boot` naming `state`, as firstboot publishes it.
    pub(crate) fn record(&self, boot: &str, state: Reduced) {
        let boot = BootId::read(&{
            let path = self.root.join("record-boot");
            fs::write(&path, format!("{boot}\n")).unwrap();
            path
        })
        .unwrap();
        cutover::publish(
            &self.record_path(),
            cutover::record(&boot, state).as_bytes(),
            self.uid,
            self.gid,
        )
        .unwrap();
    }

    pub(crate) fn recorded(&self) -> Option<String> {
        fs::read_to_string(self.record_path()).ok()
    }

    /// How many reboots were requested.
    pub(crate) fn reboots(&self) -> usize {
        self.reboots.load(Ordering::Relaxed)
    }

    /// How many renders started.
    pub(crate) fn renders(&self) -> usize {
        self.renders.load(Ordering::Relaxed)
    }

    /// The scripted td-svc every host of this machine asks, answering by
    /// `behavior` from now on.
    pub(crate) fn svc(&self, behavior: Behavior) -> Svc {
        self.svc.script.lock().unwrap().behavior = behavior;
        self.svc.clone()
    }

    /// A helper playing a fixed one: counted in `counter` as it starts,
    /// under `limit` and `lifetime` as production's, each start running
    /// the next of `runs` and then the last again.
    fn helper(
        counter: &Arc<AtomicUsize>,
        runs: Vec<Run>,
        limit: usize,
        lifetime: Duration,
    ) -> Box<dyn Fn() -> Result<Inspection, String>> {
        let counter = Arc::clone(counter);
        Box::new(move || {
            let started = counter.fetch_add(1, Ordering::Relaxed);
            let run = runs
                .get(started.min(runs.len().saturating_sub(1)))
                .ok_or("no run")?;
            crate::inspection::tests::printed(&run.bytes, run.code, run.hang, limit, lifetime)
        })
    }

    /// The machine's host, with `render` and `reboot` as its helpers' runs
    /// and short deadlines.
    pub(crate) fn host(&self, render: Run, reboot: Run) -> Host {
        self.host_with(render, vec![reboot])
    }

    /// The machine's host, its reboot requests running `reboots` in turn.
    pub(crate) fn host_with(&self, render: Run, reboots: Vec<Run>) -> Host {
        Host {
            record: self.record_path(),
            boot_id: self.root.join("boot_id"),
            guard: self.guard(),
            line: self.root.join("dev/ttyS0"),
            proc: self.root.join("proc"),
            uid: self.uid,
            gid: self.gid,
            render: Self::helper(&self.renders, vec![render], RENDER_OUTPUT, RENDER),
            reboot: Self::helper(&self.reboots, reboots, REPLY_OUTPUT, REBOOT),
            control: self.svc.control(),
            unit_time: UNIT,
        }
    }

    /// A revocation whose render prints `form` and whose reboot request
    /// td-svc accepts.
    pub(crate) fn revocation(&self, form: &str) -> Revocation {
        self.revocation_with(prints(&format!("{form}\n")), accepted())
    }

    /// A revocation whose render and reboot request run so.
    pub(crate) fn revocation_with(&self, render: Run, reboot: Run) -> Revocation {
        Revocation::with_host(self.host(render, reboot))
    }
}

impl Drop for Machine {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

const RENDER: Duration = Duration::from_millis(800);
const EXCHANGE: Duration = Duration::from_millis(800);
const UNIT: Duration = Duration::from_millis(2500);
const REBOOT: Duration = Duration::from_secs(2);

/// Ticks `revocation` with the slot `free` until its status is no longer
/// pending, or `limit` passes.
pub(crate) fn settle(revocation: &mut Revocation, free: bool, state: Reduced) -> Status {
    let until = Instant::now() + Duration::from_secs(10);
    loop {
        revocation.tick(free, || state).unwrap();
        if revocation.status() != Status::Pending {
            return revocation.status();
        }
        assert!(Instant::now() < until, "the cutover never settled");
        thread::sleep(Duration::from_millis(2));
    }
}

fn record_bytes(boot: &str, state: &str) -> String {
    format!("td-login-cutover-v1\n{boot}\n{state}\n")
}

/// A pseudo-terminal's peer node, the test's own: a character device to
/// hand back.
fn pty() -> (fs::File, PathBuf) {
    let (master, slave) = crate::terminal::terminal_pair().unwrap();
    let path = fs::read_link(format!("/proc/self/fd/{}", slave.as_raw_fd())).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o620)).unwrap();
    drop(slave);
    (master, path)
}

#[test]
fn production_reads_the_fixed_paths_and_deadlines() {
    let host = Host::production();
    assert_eq!(host.record, Path::new("/run/td-login-cutover"));
    assert_eq!(host.boot_id, Path::new("/proc/sys/kernel/random/boot_id"));
    assert_eq!(host.guard, Path::new("/var/lib/td/login/cutover-reboot"));
    assert_eq!(host.line, Path::new("/dev/ttyS0"));
    assert_eq!((host.uid, host.gid), (0, 0));
    // The helpers' argv, bounds and deadlines are pinned in confinement,
    // since starting one here would run it.
    assert_eq!(
        (EXCHANGE_TIME, host.unit_time, POLL_STEP),
        (
            Duration::from_secs(12),
            Duration::from_secs(30),
            Duration::from_millis(250)
        )
    );
    // An exchange outlasts td-svc's own 10-second reply wait, and a unit's
    // deadline its restart exchange and that exchange's retry.
    assert!(EXCHANGE_TIME > Duration::from_secs(10));
    assert!(UNIT_TIME > EXCHANGE_TIME * 2);
    assert_eq!(
        [Verb::Status, Verb::Restart].map(Verb::word),
        ["status", "restart"]
    );
    assert_eq!((RENDER_TIME, RENDER_OUTPUT), (Duration::from_secs(10), 16));
    // The reboot request's acceptances and every busy reply fit its bound.
    for reply in REBOOT_REPLIES.iter().chain(BUSY) {
        assert!(reply.len() <= REPLY_OUTPUT, "{reply}");
    }
    assert!(RENDER_OUTPUT >= "unenrolled\n".len());
    // td-svc's longest acknowledgement and refusal for these units fit.
    for reply in [
        "greeter: restart requested; a stop is already in progress and its containment \
         is not empty yet\n",
        "error: greeter: the plan could not order it (a dependency cycle, or a unit \
         downstream of one), so nothing would start it; fix the table and restart the \
         supervisor\n",
    ] {
        assert!(reply.len() <= REPLY_OUTPUT, "{}", reply.len());
    }
    assert_eq!(UNITS, ["sshd", "greeter"]);
    assert_eq!(
        [
            Status::Settled,
            Status::Pending,
            Status::Restarting,
            Status::Held
        ]
        .map(Status::byte),
        [0, 1, 2, 3]
    );
}

/// A record naming the reduced state for this boot changes nothing: no
/// render, no td-svc request, and a guard an earlier boot wrote is
/// removed, while one naming this boot is kept.
#[test]
fn a_record_naming_the_state_for_this_boot_changes_nothing() {
    let machine = Machine::new();
    let svc = machine.svc(Behavior::default());
    for state in [Reduced::Unenrolled, Reduced::Enforced] {
        machine.record(ID, state);
        let mut revocation = machine.revocation("enforced");
        fs::write(machine.guard(), format!("{ID}\n")).unwrap();
        revocation.check(state);
        assert_eq!(revocation.status(), Status::Settled);
        revocation.tick(true, || state).unwrap();
        assert!(machine.guard().exists(), "this boot's guard");
        fs::write(machine.guard(), format!("{OTHER}\n")).unwrap();
        revocation.check(state);
        assert!(!machine.guard().exists(), "an earlier boot's guard");
        fs::write(machine.guard(), b"torn").unwrap();
        revocation.check(state);
        assert!(!machine.guard().exists(), "a torn guard");
    }
    assert_eq!(machine.renders(), 0);
    assert!(svc.requests().is_empty());
}

/// The whole cutover: the render, the line handed back, both leaders read,
/// both restarts asked before either is polled, each polled until it has
/// left `stopping` and runs a new leader, then the record naming the form
/// the render printed.
#[test]
fn a_cutover_renders_hands_back_restarts_both_and_records_the_form() {
    for (state, form) in [
        (Reduced::Enforced, "enforced"),
        (Reduced::Unenrolled, "unenrolled"),
    ] {
        let machine = Machine::new();
        let other = if state == Reduced::Enforced {
            Reduced::Unenrolled
        } else {
            Reduced::Enforced
        };
        machine.record(ID, other);
        let (_master, line) = pty();
        let mut host = machine.host(prints(&format!("{form}\n")), accepted());
        host.line = line.clone();
        let mut revocation = Revocation::with_host(host);
        let svc = machine.svc(Behavior {
            stopping_polls: 2,
            ..Behavior::default()
        });
        revocation.check(state);
        assert_eq!(revocation.status(), Status::Pending);
        assert_eq!(settle(&mut revocation, true, state), Status::Settled);
        assert_eq!(machine.recorded().unwrap(), record_bytes(ID, form));
        assert_eq!(machine.renders(), 1);
        let meta = fs::symlink_metadata(&line).unwrap();
        assert_eq!(meta.mode() & 0o7777, 0o600);
        assert_eq!((meta.uid(), meta.gid()), (machine.uid, machine.gid));
        let requests = svc.requests();
        assert_eq!(
            requests[..4],
            [
                "status sshd",
                "status greeter",
                "restart sshd",
                "restart greeter"
            ]
        );
        // Each was polled through its two `stopping` answers.
        for unit in ["sshd", "greeter"] {
            let polls = requests[4..]
                .iter()
                .filter(|request| **request == format!("status {unit}"))
                .count();
            assert_eq!(polls, 3, "{unit}: {requests:?}");
        }
        assert_eq!(machine.reboots(), 0);
        // A second check now finds the record and changes nothing.
        revocation.check(state);
        assert_eq!(revocation.status(), Status::Settled);
        assert_eq!(machine.renders(), 1);
    }
}

/// An absent record, one naming another boot, and every record outside
/// the exact grammar or the file rules name no state: each cuts over to
/// the current state.
#[test]
fn a_record_that_names_no_state_for_this_boot_cuts_over() {
    let machine = Machine::new();
    let _svc = machine.svc(Behavior::default());
    let current = |machine: &Machine| {
        let mut revocation = machine.revocation("enforced");
        revocation.check(Reduced::Enforced);
        let status = revocation.status();
        if status == Status::Pending {
            assert_eq!(
                settle(&mut revocation, true, Reduced::Enforced),
                Status::Settled
            );
        }
        status
    };
    assert_eq!(machine.recorded(), None);
    assert_eq!(current(&machine), Status::Pending);
    assert_eq!(machine.recorded().unwrap(), record_bytes(ID, "enforced"));
    assert_eq!(current(&machine), Status::Settled);
    let path = machine.record_path();
    let exact = record_bytes(ID, "enforced");
    let damaged: &[&dyn Fn()] = &[
        &|| machine.record(OTHER, Reduced::Enforced),
        &|| fs::write(&path, exact.trim_end()).unwrap(),
        &|| fs::write(&path, format!("{exact}\n")).unwrap(),
        &|| fs::write(&path, exact.replace("v1", "v2")).unwrap(),
        &|| fs::write(&path, exact.to_uppercase()).unwrap(),
        &|| fs::write(&path, format!("{exact}{}", " ".repeat(100))).unwrap(),
        &|| fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap(),
        &|| fs::set_permissions(&path, fs::Permissions::from_mode(0o400)).unwrap(),
        &|| {
            fs::hard_link(&path, machine.root.join("second")).unwrap();
        },
        &|| {
            let target = machine.root.join("target");
            fs::write(&target, &exact).unwrap();
            fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).unwrap();
            fs::remove_file(&path).unwrap();
            std::os::unix::fs::symlink(&target, &path).unwrap();
        },
        &|| {
            fs::remove_file(&path).unwrap();
            fs::create_dir(&path).unwrap();
        },
    ];
    for (index, damage) in damaged.iter().enumerate() {
        let _ = fs::remove_file(machine.root.join("second"));
        if path.is_dir() {
            fs::remove_dir(&path).unwrap();
        }
        machine.record(ID, Reduced::Enforced);
        assert_eq!(current(&machine), Status::Settled, "{index}");
        damage();
        if path.is_dir() {
            // A directory at the name cannot be removed: the cutover fails
            // before its render, then holds behind the guard.
            assert_eq!(current(&machine), Status::Restarting, "{index}");
            assert_eq!(current(&machine), Status::Held, "{index}");
            fs::remove_file(machine.guard()).unwrap();
            continue;
        }
        assert_eq!(current(&machine), Status::Pending, "{index}");
        assert_eq!(machine.recorded().unwrap(), exact, "{index}");
        assert_eq!(current(&machine), Status::Settled, "{index}");
    }
    // A FIFO at the name is not waited on.
    fs::remove_dir(&path).unwrap();
    let fifo = Command::new("/usr/bin/mkfifo").arg(&path).status();
    if fifo.is_ok_and(|status| status.success()) {
        assert_eq!(current(&machine), Status::Pending);
    }
    // Another boot's ID in the record: this boot's is read afresh.
    machine.record(ID, Reduced::Enforced);
    machine.boot(OTHER);
    assert_eq!(current(&machine), Status::Pending);
    assert_eq!(machine.recorded().unwrap(), record_bytes(OTHER, "enforced"));
}

/// td-svc answering busy once, or its client exiting once with no reply
/// (its own timeout), is asked once more and the cutover completes; busy
/// twice, or no reply twice, fails it, as does a client that outlives the
/// exchange's bound.
#[test]
fn one_transient_td_svc_failure_is_asked_once_more() {
    for behavior in [
        Behavior {
            busy_first: true,
            ..Behavior::default()
        },
        Behavior {
            silent_first: true,
            ..Behavior::default()
        },
    ] {
        let machine = Machine::new();
        let svc = machine.svc(behavior);
        let mut revocation = machine.revocation("enforced");
        revocation.check(Reduced::Enforced);
        assert_eq!(
            settle(&mut revocation, true, Reduced::Enforced),
            Status::Settled
        );
        let requests = svc.requests();
        assert_eq!(requests[..2], ["status sshd", "status sshd"]);
        assert_eq!(machine.recorded().unwrap(), record_bytes(ID, "enforced"));
        assert!(!machine.guard().exists());
    }
    for behavior in [
        Behavior {
            busy_always: true,
            ..Behavior::default()
        },
        Behavior {
            absent: true,
            ..Behavior::default()
        },
    ] {
        let machine = Machine::new();
        let svc = machine.svc(behavior);
        let mut revocation = machine.revocation("enforced");
        revocation.check(Reduced::Enforced);
        assert_eq!(
            settle(&mut revocation, true, Reduced::Enforced),
            Status::Restarting
        );
        assert_eq!(svc.requests(), ["status sshd", "status sshd"]);
        assert_eq!(machine.recorded(), None);
    }
    // A client the exchange's bound killed is not asked again.
    let machine = Machine::new();
    let svc = machine.svc(Behavior {
        hang_first: true,
        ..Behavior::default()
    });
    let mut revocation = machine.revocation("enforced");
    revocation.check(Reduced::Enforced);
    let started = Instant::now();
    assert_eq!(
        settle(&mut revocation, true, Reduced::Enforced),
        Status::Restarting
    );
    assert!(started.elapsed() >= EXCHANGE);
    assert_eq!(svc.requests(), ["status sshd"]);
}

/// A unit `stopped` after its restart fails the revocation at once, long
/// before its deadline; `failed` and `held` are td-svc's restart backoff
/// for these restart-always units, polled until the new leader runs.
#[test]
fn only_a_stopped_unit_fails_before_its_deadline() {
    let machine = Machine::new();
    let svc = machine.svc(Behavior {
        greeter_settles: Some(("stopped", usize::MAX)),
        ..Behavior::default()
    });
    let mut revocation = machine.revocation("enforced");
    revocation.check(Reduced::Enforced);
    let started = Instant::now();
    assert_eq!(
        settle(&mut revocation, false, Reduced::Enforced),
        Status::Restarting
    );
    assert!(started.elapsed() < UNIT, "{:?}", started.elapsed());
    assert_eq!(svc.requests().last().unwrap(), "status greeter");
    assert_eq!(machine.recorded(), None);
    for state in ["failed", "held", "down"] {
        let machine = Machine::new();
        let svc = machine.svc(Behavior {
            greeter_settles: Some((state, 3)),
            ..Behavior::default()
        });
        let mut revocation = machine.revocation("enforced");
        revocation.check(Reduced::Enforced);
        assert_eq!(
            settle(&mut revocation, true, Reduced::Enforced),
            Status::Settled,
            "{state}"
        );
        let polls = svc
            .requests()
            .iter()
            .filter(|r| *r == "status greeter")
            .count();
        // Its leader, three backoff polls, then its new leader.
        assert_eq!(polls, 5, "{state}");
        assert_eq!(machine.recorded().unwrap(), record_bytes(ID, "enforced"));
    }
}

/// A reply that arrives after its unit's deadline is not accepted: the
/// deadline fails the revocation first.
#[test]
fn a_reply_after_the_units_deadline_is_not_accepted() {
    let machine = Machine::new();
    // The greeter's first status after its restart names its new leader,
    // but becomes readable only past the greeter's deadline.
    let svc = machine.svc(Behavior {
        late: Some(UNIT + Duration::from_millis(100)),
        ..Behavior::default()
    });
    let mut revocation = machine.revocation("enforced");
    revocation.check(Reduced::Enforced);
    assert_eq!(
        settle(&mut revocation, false, Reduced::Enforced),
        Status::Restarting
    );
    assert_eq!(machine.recorded(), None);
    assert!(svc.requests().iter().any(|r| r == "restart greeter"));
}

/// A refusal, a containment that never empties, and a new leader that
/// started before the request each fail the revocation: the guard names
/// this boot before the reboot request, which waits for the slot, is made
/// once, and leaves the status failed.
#[test]
fn a_failed_revocation_writes_the_guard_then_requests_one_reboot_once_the_slot_is_free() {
    for behavior in [
        Behavior {
            refuse_greeter: true,
            ..Behavior::default()
        },
        Behavior {
            never_empties: true,
            ..Behavior::default()
        },
        Behavior {
            stale_leader: true,
            ..Behavior::default()
        },
    ] {
        let machine = Machine::new();
        if behavior.stale_leader {
            machine.leader(201, BEFORE);
        }
        let svc = machine.svc(behavior);
        let mut revocation = machine.revocation("enforced");
        revocation.check(Reduced::Enforced);
        let started = Instant::now();
        assert_eq!(
            settle(&mut revocation, false, Reduced::Enforced),
            Status::Restarting
        );
        if behavior.never_empties || behavior.stale_leader {
            assert!(started.elapsed() >= UNIT, "{:?}", started.elapsed());
        }
        assert!(revocation.restarting());
        assert_eq!(
            fs::read_to_string(machine.guard()).unwrap(),
            format!("{ID}\n")
        );
        let meta = fs::symlink_metadata(machine.guard()).unwrap();
        assert_eq!(meta.mode() & 0o7777, 0o600);
        assert_eq!(machine.recorded(), None);
        if behavior.refuse_greeter {
            assert_eq!(svc.requests().last().unwrap(), "restart greeter");
        }
        // The slot is held: no reboot yet, and a check changes nothing.
        for _ in 0..20 {
            revocation.tick(false, || Reduced::Enforced).unwrap();
            revocation.check(Reduced::Enforced);
        }
        assert_eq!(machine.reboots(), 0);
        let until = Instant::now() + Duration::from_secs(5);
        while !matches!(revocation.phase, Phase::Failed(Reboot::Accepted)) {
            revocation.tick(true, || Reduced::Enforced).unwrap();
            assert!(Instant::now() < until);
            thread::sleep(Duration::from_millis(2));
        }
        for _ in 0..20 {
            revocation.tick(true, || Reduced::Enforced).unwrap();
        }
        assert_eq!(machine.reboots(), 1);
        assert_eq!(revocation.status(), Status::Restarting);
    }
}

/// A reboot request td-svc does not accept, or that cannot start, ends
/// the generation.
#[test]
fn a_reboot_request_that_fails_ends_the_generation() {
    for reboot in [
        prints("error: refused\n").exits(1),
        prints("poweroff requested\n"),
        accepted().exits(1),
        prints("").hangs(),
    ] {
        let machine = Machine::new();
        let _svc = machine.svc(Behavior {
            refuse_greeter: true,
            ..Behavior::default()
        });
        let shown = String::from_utf8_lossy(&reboot.bytes).into_owned();
        let mut revocation = machine.revocation_with(prints("enforced\n"), reboot);
        revocation.check(Reduced::Enforced);
        let until = Instant::now() + Duration::from_secs(10);
        let error = loop {
            match revocation.tick(true, || Reduced::Enforced) {
                Ok(()) => {}
                Err(error) => break error,
            }
            assert!(Instant::now() < until, "{shown}");
            thread::sleep(Duration::from_millis(2));
        };
        assert!(error.contains("reboot"), "{error}");
        assert_eq!(machine.reboots(), 1);
    }
}

/// A busy reboot request, or one whose client exits with no reply, is
/// asked once more, as a control exchange is; twice fails the generation.
#[test]
fn one_transient_reboot_failure_is_asked_once_more() {
    let busy = || prints("error: supervisor did not answer in time\n").exits(1);
    let silent = || prints("").exits(1);
    for first in [busy(), silent()] {
        let machine = Machine::new();
        let _svc = machine.svc(Behavior {
            refuse_greeter: true,
            ..Behavior::default()
        });
        let mut revocation =
            Revocation::with_host(machine.host_with(prints("enforced\n"), vec![first, accepted()]));
        revocation.check(Reduced::Enforced);
        let until = Instant::now() + Duration::from_secs(10);
        while !matches!(revocation.phase, Phase::Failed(Reboot::Accepted)) {
            revocation.tick(true, || Reduced::Enforced).unwrap();
            assert!(Instant::now() < until);
            thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(machine.reboots(), 2);
    }
    for runs in [vec![busy()], vec![silent()], vec![busy(), silent()]] {
        let machine = Machine::new();
        let _svc = machine.svc(Behavior {
            refuse_greeter: true,
            ..Behavior::default()
        });
        let mut revocation = Revocation::with_host(machine.host_with(prints("enforced\n"), runs));
        revocation.check(Reduced::Enforced);
        let until = Instant::now() + Duration::from_secs(10);
        let error = loop {
            if let Err(error) = revocation.tick(true, || Reduced::Enforced) {
                break error;
            }
            assert!(Instant::now() < until);
            thread::sleep(Duration::from_millis(2));
        };
        assert!(error.contains("reboot"), "{error}");
        assert_eq!(machine.reboots(), 2);
    }
    // Teardown's request retries alike.
    let machine = Machine::new();
    let _svc = machine.svc(Behavior {
        refuse_greeter: true,
        ..Behavior::default()
    });
    let mut revocation =
        Revocation::with_host(machine.host_with(prints("enforced\n"), vec![busy(), accepted()]));
    revocation.check(Reduced::Enforced);
    settle(&mut revocation, false, Reduced::Enforced);
    revocation.abandon(true);
    assert_eq!(machine.reboots(), 2);
}

/// While the guard exists, as after the automatic reboot a failure
/// requested, a failure holds without a reboot; so does a guard that
/// cannot be written. A later boot's successful check removes the guard.
#[test]
fn a_failure_with_the_guard_present_or_unwritable_holds() {
    let machine = Machine::new();
    let _svc = machine.svc(Behavior {
        refuse_greeter: true,
        ..Behavior::default()
    });
    // The boot after the automatic reboot: the guard names the last one.
    machine.boot(OTHER);
    let mut revocation = machine.revocation("enforced");
    revocation.check(Reduced::Enforced);
    assert_eq!(
        settle(&mut revocation, false, Reduced::Enforced),
        Status::Restarting
    );
    machine.boot(ID);
    let mut revocation = machine.revocation("enforced");
    revocation.check(Reduced::Enforced);
    assert_eq!(
        settle(&mut revocation, true, Reduced::Enforced),
        Status::Held
    );
    assert!(!revocation.restarting());
    for _ in 0..20 {
        revocation.tick(true, || Reduced::Enforced).unwrap();
    }
    assert_eq!(machine.reboots(), 0);
    assert_eq!(
        fs::read_to_string(machine.guard()).unwrap(),
        format!("{OTHER}\n")
    );
    // An unwritable guard: its directory is gone.
    let unwritable = Machine::new();
    let _svc = unwritable.svc(Behavior {
        refuse_greeter: true,
        ..Behavior::default()
    });
    fs::remove_dir(unwritable.root.join("var/lib/td/login")).unwrap();
    let mut revocation = unwritable.revocation("enforced");
    revocation.check(Reduced::Enforced);
    assert_eq!(
        settle(&mut revocation, true, Reduced::Enforced),
        Status::Held
    );
    // A guard directory the shared predicate refuses: group-writable, or
    // a link to a valid one, holds without a guard.
    for damage in ["mode", "link"] {
        let damaged = Machine::new();
        let _svc = damaged.svc(Behavior {
            refuse_greeter: true,
            ..Behavior::default()
        });
        let directory = damaged.root.join("var/lib/td/login");
        if damage == "mode" {
            fs::set_permissions(&directory, fs::Permissions::from_mode(0o770)).unwrap();
        } else {
            let real = damaged.root.join("var/lib/td/real");
            fs::rename(&directory, &real).unwrap();
            std::os::unix::fs::symlink(&real, &directory).unwrap();
        }
        let mut revocation = damaged.revocation("enforced");
        revocation.check(Reduced::Enforced);
        assert_eq!(
            settle(&mut revocation, true, Reduced::Enforced),
            Status::Held,
            "{damage}"
        );
        assert!(fs::symlink_metadata(damaged.guard()).is_err(), "{damage}");
        assert!(
            !damaged.root.join("var/lib/td/real/cutover-reboot").exists(),
            "{damage}"
        );
        assert_eq!(damaged.reboots(), 0, "{damage}");
    }
    // And a boot ID that cannot be read cannot be guarded either.
    let unreadable = Machine::new();
    let _svc = unreadable.svc(Behavior {
        refuse_greeter: true,
        ..Behavior::default()
    });
    let mut revocation = unreadable.revocation("enforced");
    revocation.check(Reduced::Enforced);
    fs::write(unreadable.root.join("boot_id"), b"not an id\n").unwrap();
    assert_eq!(
        settle(&mut revocation, true, Reduced::Enforced),
        Status::Held
    );
    assert!(!unreadable.guard().exists());
    for _ in 0..20 {
        revocation.tick(true, || Reduced::Enforced).unwrap();
    }
    assert_eq!(unwritable.reboots() + unreadable.reboots(), 0);
    // A successful check of a later boot removes the guard, and a later
    // failure may reboot again.
    let machine = Machine::new();
    let _svc = machine.svc(Behavior::default());
    fs::write(machine.guard(), format!("{OTHER}\n")).unwrap();
    let mut revocation = machine.revocation("enforced");
    revocation.check(Reduced::Enforced);
    assert_eq!(
        settle(&mut revocation, true, Reduced::Enforced),
        Status::Settled
    );
    assert!(!machine.guard().exists());
}

/// A render that prints anything but one word and a newline, fails, or
/// outlives its deadline fails the revocation; the late one is killed and
/// reaped at the deadline, and none publishes a record.
#[test]
fn a_render_that_fails_or_outlives_its_deadline_fails_the_revocation() {
    for (render, late) in [
        (prints("enforced"), false),
        (prints("ENFORCED\n"), false),
        (prints("enforced\nenforced\n"), false),
        (prints("enforced \n"), false),
        (prints("unenrolled\nxxxxxx"), false),
        (prints(""), false),
        (prints("enforced\n").exits(1), false),
        (prints("").hangs(), true),
        (prints("enforced\n").hangs(), true),
    ] {
        let render_shown = format!("{:?}", String::from_utf8_lossy(&render.bytes));
        let machine = Machine::new();
        let svc = machine.svc(Behavior::default());
        let mut revocation = machine.revocation_with(render, accepted());
        let started = Instant::now();
        revocation.check(Reduced::Enforced);
        assert_eq!(
            settle(&mut revocation, true, Reduced::Enforced),
            Status::Restarting,
            "{render_shown}"
        );
        assert_eq!(started.elapsed() >= RENDER, late, "{render_shown}");
        assert!(started.elapsed() < RENDER * 4, "{render_shown}");
        assert!(svc.requests().is_empty(), "{render_shown}");
        assert_eq!(machine.recorded(), None);
        assert!(machine.guard().exists());
    }
    // A render that cannot start fails at once.
    let machine = Machine::new();
    let mut host = machine.host(prints(""), accepted());
    host.render =
        Box::new(|| Inspection::printing(Command::new("/nonexistent/td-firstboot"), 16, RENDER));
    let mut revocation = Revocation::with_host(host);
    revocation.check(Reduced::Enforced);
    assert_eq!(revocation.status(), Status::Restarting);
}

/// The line is taken only as a character device: absent, it is skipped;
/// any other node, or a change that fails, fails the revocation before
/// td-svc is asked anything.
#[test]
fn the_line_is_handed_back_only_as_a_character_device() {
    let machine = Machine::new();
    let (_master, line) = pty();
    hand_back(&line, machine.uid, machine.gid).unwrap();
    let meta = fs::symlink_metadata(&line).unwrap();
    assert_eq!(meta.mode() & 0o7777, 0o600);
    assert_eq!((meta.uid(), meta.gid()), (machine.uid, machine.gid));
    assert_eq!(hand_back(&machine.root.join("absent"), 0, 0), Ok(()));
    let file = machine.root.join("file");
    fs::write(&file, b"").unwrap();
    let link = machine.root.join("link");
    std::os::unix::fs::symlink(&line, &link).unwrap();
    let directory = machine.root.join("directory");
    fs::create_dir(&directory).unwrap();
    for node in [&file, &link, &directory] {
        assert!(
            hand_back(node, machine.uid, machine.gid).is_err(),
            "{node:?}"
        );
    }
    // A link is never followed: the device behind it keeps its mode.
    fs::set_permissions(&line, fs::Permissions::from_mode(0o620)).unwrap();
    assert!(hand_back(&link, machine.uid, machine.gid).is_err());
    assert_eq!(fs::symlink_metadata(&line).unwrap().mode() & 0o7777, 0o620);
    // An owner no process may set.
    assert!(hand_back(&line, u32::MAX - 1, u32::MAX - 1).is_err());
    // In a cutover: a regular file at the line fails it before td-svc.
    let svc = machine.svc(Behavior::default());
    fs::write(machine.root.join("dev/ttyS0"), b"").unwrap();
    let mut revocation = machine.revocation("enforced");
    revocation.check(Reduced::Enforced);
    assert_eq!(
        settle(&mut revocation, true, Reduced::Enforced),
        Status::Restarting
    );
    assert!(svc.requests().is_empty());
}

/// A check that falls due while a cutover runs is taken in the tick that
/// settles it, so `00` is never reported between the two, and the second
/// converges to the newer state.
#[test]
fn a_check_due_during_a_cutover_is_taken_when_it_settles() {
    let machine = Machine::new();
    let svc = machine.svc(Behavior::default());
    let mut revocation = machine.revocation("unenrolled");
    revocation.check(Reduced::Unenrolled);
    revocation.check(Reduced::Unenrolled);
    assert_eq!(machine.renders(), 1, "a second check while one runs");
    // The state turns enforced while the first cutover runs: the tick
    // that settles it publishes its record and begins the second, which
    // removes that record again, so every tick reads pending.
    let until = Instant::now() + Duration::from_secs(10);
    while machine.renders() < 2 {
        revocation.tick(true, || Reduced::Enforced).unwrap();
        assert_eq!(revocation.status(), Status::Pending, "a 00 between the two");
        assert!(Instant::now() < until);
        thread::sleep(Duration::from_millis(2));
    }
    assert_eq!(machine.recorded(), None);
    // This render prints `unenrolled`, so the second cutover records what it
    // rendered, which a check against `enforced` repeats.
    assert_eq!(
        settle(&mut revocation, true, Reduced::Enforced),
        Status::Settled
    );
    assert_eq!(machine.renders(), 2);
    assert_eq!(machine.recorded().unwrap(), record_bytes(ID, "unenrolled"));
    assert_eq!(
        svc.requests()
            .iter()
            .filter(|r| r.starts_with("restart"))
            .count(),
        4
    );
}

/// Teardown abandons a cutover in flight: the render is killed and
/// reaped, and no record is written, so the next generation repeats it.
#[test]
fn teardown_abandons_a_cutover_without_its_record() {
    let machine = Machine::new();
    let svc = machine.svc(Behavior::default());
    let mut revocation = machine.revocation_with(prints("").hangs(), accepted());
    revocation.check(Reduced::Enforced);
    let Phase::Render(render) = &revocation.phase else {
        panic!("no render");
    };
    let pid = crate::inspection::tests::pid(render).unwrap();
    let started = Instant::now();
    revocation.abandon(true);
    assert!(started.elapsed() < Duration::from_millis(500));
    assert!(!Path::new(&format!("/proc/{pid}")).exists());
    assert_eq!(revocation.status(), Status::Settled);
    revocation.tick(true, || Reduced::Enforced).unwrap();
    assert_eq!(machine.recorded(), None);
    assert!(svc.requests().is_empty());
    // A teardown during the restarts writes nothing either.
    let mut revocation = machine.revocation("enforced");
    revocation.check(Reduced::Enforced);
    while !matches!(revocation.phase, Phase::Restart(_)) {
        revocation.tick(true, || Reduced::Enforced).unwrap();
        thread::sleep(Duration::from_millis(1));
    }
    revocation.abandon(true);
    revocation.tick(true, || Reduced::Enforced).unwrap();
    assert_eq!(machine.recorded(), None);
    let mut next = machine.revocation("enforced");
    next.check(Reduced::Enforced);
    assert_eq!(next.status(), Status::Pending);
}

/// td-svc's status grammar: one line for the unit asked, its state, its
/// pid, and `leaf=unproven` only while stopping.
#[test]
fn status_lines_have_exactly_td_svcs_shape() {
    let line = Line::parse("sshd", "sshd ready pid=12 failures=0\n").unwrap();
    assert_eq!((line.state.as_str(), line.pid), ("ready", Some(12)));
    let line = Line::parse("sshd", "sshd stopping pid=- failures=3 leaf=unproven\n").unwrap();
    assert_eq!((line.state.as_str(), line.pid), ("stopping", None));
    for bad in [
        "sshd ready pid=12 failures=0",
        "greeter ready pid=12 failures=0\n",
        "sshd ready pid=12 failures=0\nsshd ready pid=12 failures=0\n",
        "sshd ready pid=12 failures=0 leaf=unproven\n",
        "sshd stopping pid=12 failures=0 leaf=proven\n",
        "sshd ready pid=+12 failures=0\n",
        "sshd ready pid= failures=0\n",
        "sshd ready pid=12 failures=\n",
        "sshd ready pid=12\n",
        "sshd running pid=12 failures=0\n",
        "sshd  ready pid=12 failures=0\n",
        "error: no such service \"sshd\"\n",
    ] {
        assert!(Line::parse("sshd", bad).is_none(), "{bad:?}");
    }
    assert_eq!(rendered(b"enforced\n"), Some(Reduced::Enforced));
    assert_eq!(rendered(b"unenrolled\n"), Some(Reduced::Unenrolled));
    assert_eq!(rendered(b"enforced"), None);
}

/// A cutover removes the record before its render, so one the state
/// changed back under, or whose record cannot be written, leaves no record
/// a later check finds matching.
#[test]
fn a_cutover_begins_by_removing_the_record() {
    // An enforced machine: the last key's removal begins a cutover to
    // unenrolled, the state turns back, and the generation ends before
    // the record is published.
    let machine = Machine::new();
    let svc = machine.svc(Behavior::default());
    machine.record(ID, Reduced::Enforced);
    let mut revocation = machine.revocation("unenrolled");
    revocation.check(Reduced::Unenrolled);
    assert_eq!(revocation.status(), Status::Pending);
    assert_eq!(machine.recorded(), None, "removed before the render");
    while !svc.requests().iter().any(|r| r == "restart greeter") {
        revocation.tick(true, || Reduced::Enforced).unwrap();
        thread::sleep(Duration::from_millis(2));
    }
    revocation.abandon(true);
    assert_eq!(machine.recorded(), None);
    let mut next = machine.revocation("enforced");
    next.check(Reduced::Enforced);
    assert_eq!(next.status(), Status::Pending, "the old record matched");
    assert_eq!(settle(&mut next, true, Reduced::Enforced), Status::Settled);
    assert_eq!(machine.recorded().unwrap(), record_bytes(ID, "enforced"));

    // A completed cutover whose record cannot be written: the old record
    // is gone, so a check against the old state cuts over again.
    let machine = Machine::new();
    let _svc = machine.svc(Behavior::default());
    machine.record(ID, Reduced::Enforced);
    let mut revocation = machine.revocation("unenrolled");
    revocation.check(Reduced::Unenrolled);
    let temporary = cutover::temporary(&machine.record_path());
    fs::create_dir(&temporary).unwrap();
    fs::write(temporary.join("keep"), b"").unwrap();
    assert_eq!(
        settle(&mut revocation, true, Reduced::Unenrolled),
        Status::Settled
    );
    assert_eq!(machine.recorded(), None, "the write failed");
    let renders = machine.renders();
    revocation.check(Reduced::Enforced);
    assert_eq!(revocation.status(), Status::Pending);
    assert_eq!(machine.renders(), renders + 1);

    // A record that cannot be removed fails the cutover before its render.
    let machine = Machine::new();
    let _svc = machine.svc(Behavior::default());
    machine.record(ID, Reduced::Enforced);
    let lock = cutover::beside(&machine.record_path(), ".lock");
    fs::set_permissions(&lock, fs::Permissions::from_mode(0o644)).unwrap();
    let mut revocation = machine.revocation("unenrolled");
    revocation.check(Reduced::Unenrolled);
    assert_eq!(revocation.status(), Status::Restarting);
    assert_eq!(machine.renders(), 0);
    assert_eq!(machine.recorded().unwrap(), record_bytes(ID, "enforced"));
}

/// Generation teardown while a failure's reboot is owed requests it,
/// waiting no longer than the request's bound; one already asked is
/// waited for, not killed; an accepted one is not asked again.
#[test]
fn teardown_requests_a_failures_owed_reboot() {
    // Waiting for the slot when the generation ends.
    let machine = Machine::new();
    let _svc = machine.svc(Behavior {
        refuse_greeter: true,
        ..Behavior::default()
    });
    let mut revocation = machine.revocation("enforced");
    revocation.check(Reduced::Enforced);
    assert_eq!(
        settle(&mut revocation, false, Reduced::Enforced),
        Status::Restarting
    );
    assert_eq!(machine.reboots(), 0);
    revocation.abandon(true);
    assert_eq!(machine.reboots(), 1);
    assert_eq!(revocation.status(), Status::Settled);
    assert!(machine.guard().exists());
    // A request that never answers is bounded by its own deadline.
    let machine = Machine::new();
    let _svc = machine.svc(Behavior {
        refuse_greeter: true,
        ..Behavior::default()
    });
    let mut revocation = machine.revocation_with(prints("enforced\n"), prints("").hangs());
    revocation.check(Reduced::Enforced);
    settle(&mut revocation, false, Reduced::Enforced);
    let started = Instant::now();
    revocation.abandon(true);
    assert!(started.elapsed() >= REBOOT);
    assert!(started.elapsed() < REBOOT + TEARDOWN_REAP + Duration::from_secs(1));
    assert_eq!(machine.reboots(), 1);
    // Already asked: waited for, not asked again.
    let machine = Machine::new();
    let _svc = machine.svc(Behavior {
        refuse_greeter: true,
        ..Behavior::default()
    });
    let mut revocation = machine.revocation("enforced");
    revocation.check(Reduced::Enforced);
    settle(&mut revocation, false, Reduced::Enforced);
    revocation.tick(true, || Reduced::Enforced).unwrap();
    assert!(matches!(revocation.phase, Phase::Failed(Reboot::Asked(..))));
    revocation.abandon(true);
    assert_eq!(machine.reboots(), 1);
    // Accepted: nothing more.
    let mut revocation = machine.revocation("enforced");
    revocation.phase = Phase::Failed(Reboot::Accepted);
    revocation.abandon(true);
    assert_eq!(machine.reboots(), 1);
    // A slot that could not be reaped: no reboot, and the guard stands.
    let machine = Machine::new();
    let _svc = machine.svc(Behavior {
        refuse_greeter: true,
        ..Behavior::default()
    });
    let mut revocation = machine.revocation("enforced");
    revocation.check(Reduced::Enforced);
    settle(&mut revocation, false, Reduced::Enforced);
    revocation.abandon(false);
    assert_eq!(machine.reboots(), 0);
    assert_eq!(revocation.status(), Status::Settled);
    assert!(machine.guard().exists());
}

/// Teardown during the restarts kills and reaps the td-svc client in
/// flight without waiting for it.
#[test]
fn teardown_reaps_a_control_client_without_blocking() {
    let machine = Machine::new();
    let _svc = machine.svc(Behavior {
        hang_first: true,
        ..Behavior::default()
    });
    let mut revocation = machine.revocation("enforced");
    revocation.check(Reduced::Enforced);
    let pid = loop {
        revocation.tick(true, || Reduced::Enforced).unwrap();
        if let Phase::Restart(restart) = &revocation.phase {
            if let Some(exchange) = &restart.exchange {
                break crate::inspection::tests::pid(&exchange.child).unwrap();
            }
        }
        thread::sleep(Duration::from_millis(1));
    };
    let started = Instant::now();
    revocation.abandon(true);
    assert!(started.elapsed() < Duration::from_millis(500));
    assert!(!Path::new(&format!("/proc/{pid}")).exists());
    assert_eq!(machine.recorded(), None);
}

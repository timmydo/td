//! td-authd/DESIGN.md amendment 7: the revocation that brings the console
//! and SSH to the reduced login state whenever the volatile record does
//! not name that state for this boot (td-login/TOKEN-LOGIN.md, "Cutover").
//!
//! A nonblocking state machine beside the operation slot, which `tick`
//! advances: the record's removal, `render-ssh-policy`, the line's
//! hand-back, td-svc's restart of `sshd` and `greeter` and the observation
//! of each new leader, then the record. Every td-svc exchange is a bounded
//! `/bin/td-svc` client child. A failure requests one guarded reboot once
//! the slot is free.

use crate::cutover::{self, BootId, Reduced};
use crate::inspection::Inspection;
use crate::login_status::login_state::{Directory, Owner};
use std::io::{self, Read, Write};
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

const O_NOFOLLOW: i32 = 0x20000;
const O_NONBLOCK: i32 = 0x800;

/// The greeter unit's `tty=`, which a recipe test pins.
const LINE: &str = "/dev/ttyS0";
const GUARD: &str = "/var/lib/td/login/cutover-reboot";
/// The units a cutover restarts, in request order, and no other.
const UNITS: &[&str] = &["sshd", "greeter"];
const RENDER_TIME: Duration = Duration::from_secs(10);
/// One `/bin/td-svc` client's bound, a control exchange's or the reboot
/// request's: td-authd/DESIGN.md amendment 7 says why it is 12 seconds.
const EXCHANGE_TIME: Duration = Duration::from_secs(12);
/// A unit's bound from its restart request: one exchange and its retry,
/// 24 seconds, with time left to observe the new leader.
const UNIT_TIME: Duration = Duration::from_secs(30);
/// How long a unit's status waits before it is asked again.
const POLL_STEP: Duration = Duration::from_millis(250);
/// How long teardown waits for a killed revocation child to be reaped:
/// a killed child is normally collected at once, and one the kernel
/// cannot kill must not wedge teardown (as `Inspection`'s `Drop` says).
const TEARDOWN_REAP: Duration = Duration::from_secs(1);
/// How often teardown looks at its reboot request again.
const TEARDOWN_STEP: Duration = Duration::from_millis(2);
/// `render-ssh-policy`'s output bound.
const RENDER_OUTPUT: usize = 16;
/// td-svc's two acceptances of `reboot`.
const REBOOT_REPLIES: &[&str] = &[
    "reboot requested\n",
    "shutdown already in progress (reboot)\n",
];
/// td-svc's replies when its loop cannot answer, which are transient.
const BUSY: &[&str] = &[
    "error: supervisor did not answer in time\n",
    "error: supervisor is not accepting requests\n",
];
/// One control reply's bound: a status line or an acknowledgement, the
/// longest of which is under 200 bytes.
const REPLY_OUTPUT: usize = 256;
const RECORD_LIMIT: u64 = 128;
/// The clock ticks `/proc/PID/stat`'s start time counts: USER_HZ, which
/// is 100 on x86-64, as `/proc/uptime`'s centiseconds are.
const USER_HZ: u64 = 100;
const STAT_LIMIT: u64 = 4096;

/// The `9a` revocation byte.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Status {
    Settled,
    Pending,
    /// Failed, and a reboot is requested or about to be.
    Restarting,
    /// Failed with the reboot guard present or unwritable: no reboot.
    Held,
}

impl Status {
    pub(crate) fn byte(self) -> u8 {
        match self {
            Self::Settled => 0,
            Self::Pending => 1,
            Self::Restarting => 2,
            Self::Held => 3,
        }
    }
}

/// What a cutover touches: production's fixed paths and helpers, or a
/// test's.
pub(crate) struct Host {
    record: PathBuf,
    boot_id: PathBuf,
    guard: PathBuf,
    line: PathBuf,
    /// Where `uptime` and `PID/stat` are read.
    proc: PathBuf,
    /// The owner the record, the guard and the line must have.
    uid: u32,
    gid: u32,
    /// Starts `render-ssh-policy`, bounded by its output and deadline.
    render: Box<dyn Fn() -> Result<Inspection, String>>,
    /// Starts the reboot request, bounded likewise.
    reboot: Box<dyn Fn() -> Result<Inspection, String>>,
    /// Starts one control exchange about a unit, bounded likewise.
    control: Box<dyn Fn(Verb, &'static str) -> Result<Inspection, String>>,
    unit_time: Duration,
}

impl Host {
    fn production() -> Self {
        Self {
            record: Path::new("/").join(cutover::RECORD),
            boot_id: PathBuf::from(cutover::BOOT_ID),
            guard: PathBuf::from(GUARD),
            line: PathBuf::from(LINE),
            proc: PathBuf::from("/proc"),
            uid: 0,
            gid: 0,
            render: Box::new(|| {
                let mut command = Command::new("/bin/td-firstboot");
                command.arg("render-ssh-policy");
                Inspection::printing(command, RENDER_OUTPUT, RENDER_TIME)
            }),
            reboot: Box::new(|| {
                let mut command = Command::new("/bin/td-svc");
                command.arg("reboot");
                Inspection::printing(command, REPLY_OUTPUT, EXCHANGE_TIME)
            }),
            control: Box::new(|verb, unit| {
                let mut command = Command::new("/bin/td-svc");
                command.args([verb.word(), unit]);
                Inspection::printing(command, REPLY_OUTPUT, EXCHANGE_TIME)
            }),
            unit_time: UNIT_TIME,
        }
    }

    /// The state the record names for `boot`: read without following a
    /// link or blocking, and only a single-link regular file of the
    /// owner, mode 0600, at most 128 bytes, in exactly the record's
    /// grammar. Anything else names no state.
    fn named(&self, boot: Option<&BootId>) -> Option<Reduced> {
        let boot = boot?;
        let file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(O_NOFOLLOW | O_NONBLOCK)
            .open(&self.record)
            .ok()?;
        let meta = file.metadata().ok()?;
        if !meta.file_type().is_file()
            || meta.nlink() != 1
            || (meta.uid(), meta.gid()) != (self.uid, self.gid)
            || meta.mode() & 0o7777 != 0o600
            || meta.len() > RECORD_LIMIT
        {
            return None;
        }
        let mut bytes = Vec::with_capacity(RECORD_LIMIT as usize + 1);
        file.take(RECORD_LIMIT + 1).read_to_end(&mut bytes).ok()?;
        [Reduced::Unenrolled, Reduced::Enforced]
            .into_iter()
            .find(|state| bytes == cutover::record(boot, *state).as_bytes())
    }

    fn write_record(&self, form: Reduced) -> Result<(), String> {
        let boot = BootId::read(&self.boot_id)?;
        cutover::publish(
            &self.record,
            cutover::record(&boot, form).as_bytes(),
            self.uid,
            self.gid,
        )
    }

    /// Whether the reboot guard's name exists, as anything at all.
    fn guarded(&self) -> bool {
        !matches!(
            std::fs::symlink_metadata(&self.guard),
            Err(error) if error.kind() == io::ErrorKind::NotFound
        )
    }

    /// The guard naming this boot, created exclusively without following a
    /// link, owner-only, and synced with its directory before any reboot
    /// is requested. Its directory is walked without following a link and
    /// held, and must pass the shared predicate's directory check, the
    /// owner's and mode 0700, before the guard is created through it.
    fn write_guard(&self) -> Result<(), String> {
        let boot = BootId::read(&self.boot_id)?;
        let (Some(directory), Some(name)) = (self.guard.parent(), self.guard.file_name()) else {
            return Err(format!("{} has no directory", self.guard.display()));
        };
        let owner = Owner {
            uid: self.uid,
            gid: self.gid,
        };
        let held = Directory::open(directory, owner)
            .map_err(|refusal| format!("{}: {}", directory.display(), refusal.reason))?;
        let write = || -> io::Result<()> {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .custom_flags(O_NOFOLLOW)
                .open(held.at(name))?;
            std::os::unix::fs::fchown(&file, Some(self.uid), Some(self.gid))?;
            file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
            file.write_all(format!("{boot}\n").as_bytes())?;
            file.sync_all()?;
            held.file().sync_all()
        };
        write().map_err(|error| format!("write {}: {error}", self.guard.display()))
    }

    /// A successful check: a guard an earlier boot wrote is removed, and
    /// one naming this boot, or any guard while this boot's ID cannot be
    /// read, is kept.
    fn clear_guard(&self) {
        let Ok(boot) = BootId::read(&self.boot_id) else {
            return;
        };
        let mut bytes = Vec::with_capacity(40);
        let read = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(O_NOFOLLOW | O_NONBLOCK)
            .open(&self.guard)
            .and_then(|file| file.take(40).read_to_end(&mut bytes));
        match read {
            Err(error) if error.kind() == io::ErrorKind::NotFound => return,
            Ok(_) if bytes == format!("{boot}\n").as_bytes() => return,
            _ => {}
        }
        if let Err(error) = std::fs::remove_file(&self.guard) {
            log(&format!(
                "cannot remove {} after a successful check: {error}",
                self.guard.display()
            ));
        }
    }

    /// The boot clock's tick now, in `stat`'s start-time units.
    fn ticks(&self) -> Result<u64, String> {
        let text = read_bounded(&self.proc.join("uptime"), 128)?;
        let (seconds, rest) = text
            .split_whitespace()
            .next()
            .and_then(|uptime| uptime.split_once('.'))
            .ok_or("malformed uptime")?;
        let fraction = rest.get(..2).filter(|digits| digits.len() == 2);
        let (Ok(seconds), Some(Ok(centis))) =
            (seconds.parse::<u64>(), fraction.map(str::parse::<u64>))
        else {
            return Err("malformed uptime".into());
        };
        seconds
            .checked_mul(100)
            .and_then(|centis_whole| centis_whole.checked_add(centis))
            .and_then(|centis| centis.checked_mul(USER_HZ))
            .map(|scaled| scaled / 100)
            .ok_or_else(|| "uptime overflow".into())
    }

    /// `pid`'s start time from its `stat`, or none.
    fn started(&self, pid: u32) -> Option<u64> {
        let text = read_bounded(&self.proc.join(pid.to_string()).join("stat"), STAT_LIMIT).ok()?;
        let (_, fields) = text.rsplit_once(')')?;
        fields.split_whitespace().nth(19)?.parse().ok()
    }
}

fn read_bounded(path: &Path, limit: u64) -> Result<String, String> {
    let mut text = String::new();
    std::fs::File::open(path)
        .and_then(|file| file.take(limit).read_to_string(&mut text))
        .map_err(|error| format!("read {}: {error}", path.display()))?;
    Ok(text)
}

fn log(message: &str) {
    let _ = writeln!(io::stderr().lock(), "td-authd: revocation: {message}");
}

/// Returns the greeter's line to the owner with mode 0600 before the
/// greeter restarts: only once `symlink_metadata` shows a character
/// device, owner first, then mode, then both read back. An absent node is
/// skipped; any other node or a failed step fails the revocation.
fn hand_back(line: &Path, uid: u32, gid: u32) -> Result<(), String> {
    let named = |error: io::Error| format!("{}: {error}", line.display());
    match std::fs::symlink_metadata(line) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(named(error)),
        Ok(meta) if !meta.file_type().is_char_device() => {
            return Err(format!("{} is not a character device", line.display()))
        }
        Ok(_) => {}
    }
    std::os::unix::fs::lchown(line, Some(uid), Some(gid)).map_err(named)?;
    std::fs::set_permissions(line, std::fs::Permissions::from_mode(0o600)).map_err(named)?;
    let meta = std::fs::symlink_metadata(line).map_err(named)?;
    if !meta.file_type().is_char_device()
        || (meta.uid(), meta.gid(), meta.mode() & 0o7777) != (uid, gid, 0o600)
    {
        return Err(format!(
            "{} does not read back as a {uid}:{gid} mode-0600 character device",
            line.display()
        ));
    }
    Ok(())
}

/// The form `render-ssh-policy` printed: exactly one word and a newline.
fn rendered(output: &[u8]) -> Option<Reduced> {
    [Reduced::Unenrolled, Reduced::Enforced]
        .into_iter()
        .find(|form| output == format!("{}\n", form.word()).as_bytes())
}

/// What one `status NAME` line says (td-svc/DESIGN.md §8).
struct Line {
    state: String,
    pid: Option<u32>,
}

impl Line {
    /// Exactly `NAME STATE pid=PID failures=N`, with ` leaf=unproven` only
    /// while stopping, for `unit` alone.
    fn parse(unit: &str, reply: &str) -> Option<Self> {
        let line = reply.strip_suffix('\n')?;
        let mut words = line.split(' ');
        if words.next()? != unit {
            return None;
        }
        let state = words.next()?;
        const STATES: &[&str] = &[
            "down", "starting", "ready", "failed", "held", "stopped", "stopping", "skipped",
        ];
        if !STATES.contains(&state) {
            return None;
        }
        let pid = match words.next()?.strip_prefix("pid=")? {
            "-" => None,
            digits if digits.bytes().all(|byte| byte.is_ascii_digit()) => {
                Some(digits.parse().ok()?)
            }
            _ => return None,
        };
        let failures = words.next()?.strip_prefix("failures=")?;
        if failures.is_empty() || !failures.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        match (words.next(), words.next()) {
            (None, _) => {}
            (Some("leaf=unproven"), None) if state == "stopping" => {}
            _ => return None,
        }
        Some(Self {
            state: state.to_owned(),
            pid,
        })
    }
}

enum Fault {
    /// td-svc's busy reply, or its client's exit with no reply at all: its
    /// own timeout, or no connection.
    Transient(String),
    Refused(String),
}

/// A control request's verb.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Verb {
    Status,
    Restart,
}

impl Verb {
    pub(crate) fn word(self) -> &'static str {
        match self {
            Self::Status => "status",
            Self::Restart => "restart",
        }
    }
}

/// One control exchange: a `/bin/td-svc VERB UNIT` client child, launched
/// as the render is and read without blocking within its bound.
struct Exchange {
    unit: usize,
    verb: Verb,
    tries: u8,
    child: Box<Inspection>,
}

enum Reply {
    Pending,
    Text(String),
    Fault(Fault),
}

impl Exchange {
    fn open(
        host: &Host,
        unit: usize,
        name: &'static str,
        verb: Verb,
        tries: u8,
    ) -> Result<Self, String> {
        let child = (host.control)(verb, name)
            .map_err(|why| format!("td-svc {} {name}: {why}", verb.word()))?;
        Ok(Self {
            unit,
            verb,
            tries,
            child: Box::new(child),
        })
    }

    fn poll(&mut self, name: &str) -> Reply {
        answer(
            &mut self.child,
            &format!("td-svc {} {name}", self.verb.word()),
        )
    }
}

/// What a `/bin/td-svc` client's run says, never blocking: a control
/// exchange's or the reboot request's.
fn answer(child: &mut Inspection, request: &str) -> Reply {
    let (bytes, success) = match child.answered() {
        Err(why) => return Reply::Fault(Fault::Refused(format!("{request}: {why}"))),
        Ok(None) => return Reply::Pending,
        Ok(Some(Some(answer))) => answer,
        Ok(Some(None)) if child.missed() => {
            return Reply::Fault(Fault::Refused(format!(
                "{request}: no exit within its bound"
            )))
        }
        Ok(Some(None)) => {
            return Reply::Fault(Fault::Refused(format!(
                "{request}: an unreadable or oversized reply"
            )))
        }
    };
    let Ok(text) = String::from_utf8(bytes) else {
        return Reply::Fault(Fault::Refused(format!("{request}: invalid reply")));
    };
    if BUSY.contains(&text.as_str()) {
        return Reply::Fault(Fault::Transient(format!("{request}: {}", text.trim_end())));
    }
    match (success, text.is_empty()) {
        (true, _) => Reply::Text(text),
        (false, true) => Reply::Fault(Fault::Transient(format!(
            "{request}: the client failed with no reply"
        ))),
        (false, false) => Reply::Fault(Fault::Refused(format!("{request}: {}", text.trim_end()))),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Step {
    /// Its current leader is read, before any request.
    Leader,
    /// Its restart is to be requested.
    Request,
    /// It is polled until a new leader runs.
    Await,
    Done,
}

struct Unit {
    name: &'static str,
    step: Step,
    /// The leader before the request.
    old: Option<u32>,
    /// The boot clock's tick just before the request, and its deadline.
    asked: Option<(u64, Instant)>,
    /// When its status is due again.
    next: Instant,
}

enum Progress {
    Waiting,
    Done(Reduced),
    Failed(String),
}

/// The restarts of a cutover whose render published `form`.
struct Restart {
    form: Reduced,
    units: Vec<Unit>,
    exchange: Option<Exchange>,
}

impl Restart {
    fn new(form: Reduced) -> Self {
        let now = Instant::now();
        Self {
            form,
            units: UNITS
                .iter()
                .map(|name| Unit {
                    name,
                    step: Step::Leader,
                    old: None,
                    asked: None,
                    next: now,
                })
                .collect(),
            exchange: None,
        }
    }

    /// Advances as far as it can without blocking: every leader is read,
    /// then both restarts are requested, then each unit is polled until it
    /// has left `stopping` and runs a new leader, within its deadline. A
    /// unit past its deadline fails before any reply is accepted.
    fn advance(&mut self, host: &Host) -> Progress {
        loop {
            let now = Instant::now();
            let expired = self.units.iter().find(|unit| {
                unit.step != Step::Done && unit.asked.is_some_and(|(_, deadline)| now >= deadline)
            });
            if let Some(unit) = expired {
                return Progress::Failed(format!(
                    "{} has no new leader at its deadline",
                    unit.name
                ));
            }
            if let Some(exchange) = &mut self.exchange {
                let Some(name) = self.units.get(exchange.unit).map(|unit| unit.name) else {
                    return Progress::Failed("unit index".into());
                };
                match exchange.poll(name) {
                    Reply::Pending => return Progress::Waiting,
                    Reply::Fault(Fault::Transient(why)) if exchange.tries < 2 => {
                        log(&format!("{why}; asking once more"));
                        let (unit, verb) = (exchange.unit, exchange.verb);
                        match Exchange::open(host, unit, name, verb, 2) {
                            Ok(again) => self.exchange = Some(again),
                            Err(why) => return Progress::Failed(why),
                        }
                        continue;
                    }
                    Reply::Fault(Fault::Transient(why) | Fault::Refused(why)) => {
                        return Progress::Failed(why)
                    }
                    Reply::Text(text) => {
                        let unit = exchange.unit;
                        self.exchange = None;
                        if let Err(why) = self.receive(host, unit, &text) {
                            return Progress::Failed(why);
                        }
                    }
                }
            }
            let leader = self.units.iter().position(|unit| unit.step == Step::Leader);
            let request = self
                .units
                .iter()
                .position(|unit| unit.step == Step::Request);
            let due = self
                .units
                .iter()
                .position(|unit| unit.step == Step::Await && unit.next <= now);
            let (index, verb) = match (leader, request, due) {
                (Some(index), _, _) => (index, Verb::Status),
                (None, Some(index), _) => (index, Verb::Restart),
                (None, None, Some(index)) => (index, Verb::Status),
                (None, None, None) => {
                    if self.units.iter().all(|unit| unit.step == Step::Done) {
                        return Progress::Done(self.form);
                    }
                    return Progress::Waiting;
                }
            };
            let Some(unit) = self.units.get_mut(index) else {
                return Progress::Failed("unit index".into());
            };
            if verb == Verb::Restart {
                let ticks = match host.ticks() {
                    Ok(ticks) => ticks,
                    Err(why) => return Progress::Failed(why),
                };
                let Some(deadline) = now.checked_add(host.unit_time) else {
                    return Progress::Failed("unit deadline overflow".into());
                };
                unit.asked = Some((ticks, deadline));
            }
            match Exchange::open(host, index, unit.name, verb, 1) {
                Ok(exchange) => self.exchange = Some(exchange),
                Err(why) => return Progress::Failed(why),
            }
        }
    }

    fn receive(&mut self, host: &Host, index: usize, text: &str) -> Result<(), String> {
        let unit = self.units.get_mut(index).ok_or("unit index")?;
        let name = unit.name;
        let refused = || format!("td-svc refused {name}: {}", text.trim_end());
        match unit.step {
            Step::Leader => {
                let line = Line::parse(name, text).ok_or_else(refused)?;
                unit.old = line.pid;
                unit.step = Step::Request;
            }
            Step::Request => {
                let acknowledged = text
                    .strip_prefix(name)
                    .and_then(|rest| rest.strip_prefix(": "))
                    .and_then(|rest| rest.strip_suffix('\n'))
                    .is_some_and(|rest| !rest.contains('\n'));
                if !acknowledged {
                    return Err(refused());
                }
                unit.step = Step::Await;
                unit.next = Instant::now();
            }
            Step::Await => {
                let line = Line::parse(name, text).ok_or_else(refused)?;
                // A unit stopped after its restart was asked fails at once.
                // Both units restart always, so `failed` and `held` are
                // td-svc's restart backoff and `down` a start to come: each
                // is polled until a new leader or the deadline.
                if line.state == "stopped" {
                    return Err(format!("{name} is {} after its restart", line.state));
                }
                let asked = unit.asked.map(|(ticks, _)| ticks);
                let new = matches!(line.state.as_str(), "starting" | "ready")
                    && line.pid.is_some()
                    && line.pid != unit.old
                    && line
                        .pid
                        .and_then(|pid| host.started(pid))
                        .zip(asked)
                        .is_some_and(|(started, asked)| started >= asked);
                if new {
                    unit.step = Step::Done;
                } else {
                    unit.next = Instant::now()
                        .checked_add(POLL_STEP)
                        .ok_or("poll overflow")?;
                }
            }
            Step::Done => return Err("a reply for a finished unit".into()),
        }
        Ok(())
    }
}

enum Reboot {
    /// Asked for once no operation holds the slot.
    Waiting,
    /// The request's client, and how many have been started.
    Asked(Box<Inspection>, u8),
    /// td-svc accepted it; the generation waits to be stopped.
    Accepted,
}

enum Phase {
    /// No cutover runs: the last check's outcome, settled or held.
    Idle(Status),
    Render(Box<Inspection>),
    Restart(Box<Restart>),
    Failed(Reboot),
}

pub(crate) struct Revocation {
    host: Host,
    phase: Phase,
    /// A check fell due while a cutover ran.
    due: bool,
}

impl Revocation {
    pub fn new() -> Self {
        Self::with_host(Host::production())
    }

    fn with_host(host: Host) -> Self {
        Self {
            host,
            phase: Phase::Idle(Status::Settled),
            due: false,
        }
    }

    /// The `9a` revocation byte's meaning.
    pub fn status(&self) -> Status {
        match &self.phase {
            Phase::Idle(status) => *status,
            Phase::Render(_) | Phase::Restart(_) => Status::Pending,
            Phase::Failed(_) => Status::Restarting,
        }
    }

    /// Begins a check: nothing changes when the record names `reduced` for
    /// this boot, and otherwise a cutover begins by removing the record,
    /// so one interrupted, or whose record cannot be written, leaves none
    /// that a later check could find matching. A check that falls due
    /// while a cutover runs is taken in the tick that settles or holds it;
    /// while a failure's reboot is pending none is needed.
    pub fn check(&mut self, reduced: Reduced) {
        match self.phase {
            Phase::Render(_) | Phase::Restart(_) => {
                self.due = true;
                return;
            }
            Phase::Failed(_) => return,
            Phase::Idle(_) => {}
        }
        self.due = false;
        let boot = BootId::read(&self.host.boot_id).ok();
        if self.host.named(boot.as_ref()) == Some(reduced) {
            self.host.clear_guard();
            self.phase = Phase::Idle(Status::Settled);
            return;
        }
        log(&format!(
            "the record does not name {} for this boot; cutting over",
            reduced.word()
        ));
        if let Err(why) = cutover::remove(&self.host.record, self.host.uid, self.host.gid) {
            self.fail(&why);
            return;
        }
        match (self.host.render)() {
            Ok(render) => self.phase = Phase::Render(Box::new(render)),
            Err(why) => self.fail(&format!("render-ssh-policy: {why}")),
        }
    }

    /// Advances the cutover without blocking. `slot_free` says no operation
    /// holds the slot, which a failure's reboot request waits for; a check
    /// that fell due is taken against `reduced` in the tick that settles or
    /// holds the cutover. An error ends the generation: the reboot request
    /// failed.
    pub fn tick(&mut self, slot_free: bool, reduced: impl Fn() -> Reduced) -> Result<(), String> {
        let render = match &mut self.phase {
            Phase::Render(render) => Some(render.finished()),
            _ => None,
        };
        match render {
            None | Some(Ok(None)) => {}
            Some(Ok(Some(Some(output)))) => match rendered(&output) {
                Some(form) => match hand_back(&self.host.line, self.host.uid, self.host.gid) {
                    Ok(()) => self.phase = Phase::Restart(Box::new(Restart::new(form))),
                    Err(why) => self.fail(&format!("hand-back: {why}")),
                },
                None => self.fail("render-ssh-policy printed no form"),
            },
            Some(Ok(Some(None))) => self.fail("render-ssh-policy failed"),
            Some(Err(why)) => self.fail(&format!("render-ssh-policy: {why}")),
        }
        let progress = match &mut self.phase {
            Phase::Restart(restart) => restart.advance(&self.host),
            _ => Progress::Waiting,
        };
        match progress {
            Progress::Waiting => {}
            Progress::Done(form) => {
                if let Err(why) = self.host.write_record(form) {
                    log(&format!(
                        "{why}; the next check repeats the completed cutover"
                    ));
                }
                self.host.clear_guard();
                self.phase = Phase::Idle(Status::Settled);
            }
            Progress::Failed(why) => self.fail(&why),
        }
        // A check that fell due while the cutover ran, in the tick that
        // settled or held it, so no `00` or `03` is reported between the
        // two; a failure that owes a reboot dropped it.
        if self.due && matches!(self.phase, Phase::Idle(_)) {
            self.due = false;
            self.check(reduced());
        }
        if let Phase::Failed(reboot) = &mut self.phase {
            match reboot {
                Reboot::Waiting if slot_free => {
                    let request = (self.host.reboot)()
                        .map_err(|why| format!("revocation failed; reboot request: {why}"))?;
                    *reboot = Reboot::Asked(Box::new(request), 1);
                }
                Reboot::Waiting | Reboot::Accepted => {}
                Reboot::Asked(request, tries) => {
                    if rebooting(&self.host, request, tries)? {
                        *reboot = Reboot::Accepted;
                    }
                }
            }
        }
        Ok(())
    }

    /// A failed revocation: held while the reboot guard exists or cannot
    /// be written, and otherwise the guard written and a reboot owed.
    fn fail(&mut self, why: &str) {
        log(&format!("failed: {why}"));
        if self.host.guarded() {
            log("held: an automatic reboot already followed a failure");
            self.phase = Phase::Idle(Status::Held);
            return;
        }
        match self.host.write_guard() {
            Ok(()) => {
                // The reboot enforces whatever state a due check would find.
                self.due = false;
                self.phase = Phase::Failed(Reboot::Waiting);
            }
            Err(why) => {
                log(&format!("held: {why}"));
                self.phase = Phase::Idle(Status::Held);
            }
        }
    }

    /// Whether a failure's reboot is pending, while a request that takes
    /// the slot is a protocol violation.
    pub fn restarting(&self) -> bool {
        matches!(self.phase, Phase::Failed(_))
    }

    /// Generation teardown: a cutover in flight is abandoned without its
    /// record, its render or td-svc child killed and reaped within
    /// `TEARDOWN_REAP`, so the next generation's check repeats it. With
    /// `reboot`, which says the slot is reaped, a failure whose reboot
    /// td-svc has not accepted requests it now, or waits for the request
    /// already made, no longer than its bound and one retry's; without it
    /// no reboot is requested and the guard stands.
    pub fn abandon(&mut self, reboot: bool) {
        let request = match std::mem::replace(&mut self.phase, Phase::Idle(Status::Settled)) {
            Phase::Render(render) => {
                render.reap_within(TEARDOWN_REAP);
                None
            }
            Phase::Restart(restart) => {
                if let Some(exchange) = restart.exchange {
                    exchange.child.reap_within(TEARDOWN_REAP);
                }
                None
            }
            Phase::Failed(Reboot::Asked(request, _)) if !reboot => {
                request.reap_within(TEARDOWN_REAP);
                None
            }
            Phase::Failed(Reboot::Waiting | Reboot::Asked(..)) if !reboot => {
                log("the slot could not be reaped: no reboot is requested");
                None
            }
            Phase::Failed(Reboot::Waiting) => match (self.host.reboot)() {
                Ok(request) => Some((Box::new(request), 1)),
                Err(why) => {
                    log(&format!("teardown's reboot request: {why}"));
                    None
                }
            },
            Phase::Failed(Reboot::Asked(request, tries)) => Some((request, tries)),
            Phase::Idle(_) | Phase::Failed(Reboot::Accepted) => None,
        };
        self.due = false;
        let Some((mut request, mut tries)) = request else {
            return;
        };
        // Each client is killed at its own bound, so this ends.
        loop {
            match rebooting(&self.host, &mut request, &mut tries) {
                Ok(true) => break,
                Ok(false) => std::thread::sleep(TEARDOWN_STEP),
                Err(why) => {
                    log(&format!("{why}; the next check repeats the cutover"));
                    break;
                }
            }
        }
        request.reap_within(TEARDOWN_REAP);
    }
}

/// Advances a reboot request without blocking: whether td-svc accepted
/// it, after one transient failure asked once more, as a control
/// exchange is; an error once it is refused.
fn rebooting(host: &Host, request: &mut Box<Inspection>, tries: &mut u8) -> Result<bool, String> {
    match answer(request, "td-svc reboot") {
        Reply::Pending => Ok(false),
        Reply::Text(text) if REBOOT_REPLIES.contains(&text.as_str()) => Ok(true),
        Reply::Fault(Fault::Transient(why)) if *tries < 2 => {
            log(&format!("{why}; asking once more"));
            let again = (host.reboot)()
                .map_err(|why| format!("revocation failed; reboot request: {why}"))?;
            **request = again;
            *tries = 2;
            Ok(false)
        }
        Reply::Text(_) | Reply::Fault(_) => {
            Err("revocation failed and td-svc refused the reboot".into())
        }
    }
}

#[cfg(test)]
#[path = "../tests/revocation.rs"]
pub(crate) mod tests;

//! `oom-watch` — the desktop's early OOM killer (DESIGN.md §12).
//!
//! The kernel's own OOM killer acts only once an allocation cannot be
//! met, and with swap on that can be long after the desktop has stopped
//! responding: pages go out to zram and come straight back, and every
//! task stalls on memory. This acts first, on either of two signs:
//!
//! - **Pressure**: the kernel's `full avg10` in `/proc/pressure/memory`,
//!   a moving average over about ten seconds of the time every non-idle
//!   task was stalled on memory at once, stays at or above
//!   `PRESSURE_PERCENT` for `PRESSURE_FOR` while available memory is
//!   under `SCARCE_PERCENT` of RAM. The system-wide figure also counts an
//!   application stalled at its own `memory.high`, which is the throttle
//!   working, not the machine thrashing; scarcity is what tells the two
//!   apart.
//! - **Floor**: available memory and free swap are both under
//!   `FLOOR_PERCENT` of their totals, the rule earlyoom applies, for a
//!   spike pressure has not caught up with.
//!
//! Either way it ranks what one kill would end by the anonymous and
//! shared memory plus swap that kill frees, and kills from the top. An
//! instance leaf under a workload delegation dies whole through its
//! `cgroup.kill`. A `session` leaf, which holds an account's own
//! services and td-jail's launchers, never does: its largest process
//! competes alone and is killed alone, through td-svc's one `kill(2)`,
//! once its start time and leaf are read back. Nothing smaller than
//! `MIN_VICTIM_PERCENT` of RAM is killed.
//! `/system`, where td-svc's own services run (the compositor, td-authd
//! and this watcher among them), and the hierarchy root, where PID 1 and
//! td-svc are, are never scanned.

use std::fs;
use std::io::{self, BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use crate::cgroup;
use crate::procfs;

pub(crate) const VERB: &str = "oom-watch";

const POLL: Duration = Duration::from_secs(1);
const SETTLE: Duration = Duration::from_secs(15);
/// After this many actions with no calm reading between them, each settle
/// doubles, up to `SETTLE_MAX`: what it kills is not what is filling
/// memory, and killing faster would only take more.
const FUTILE: u32 = 3;
const SETTLE_MAX: Duration = Duration::from_secs(240);
const FLOOR_PERCENT: u64 = 5;
const PRESSURE_PERCENT: f64 = 50.0;
const PRESSURE_FOR: Duration = Duration::from_secs(20);
/// Pressure counts only while `MemAvailable` is under this share of RAM.
const SCARCE_PERCENT: u64 = 20;
/// No victim holding less than this share of `MemTotal` is killed.
const MIN_VICTIM_PERCENT: u64 = 1;

const MEMINFO: &str = "/proc/meminfo";
const PRESSURE: &str = "/proc/pressure/memory";
/// The roots of the delegations whose leaves are workloads.
const WORKLOADS: &[&str] = &["td-user-", "td-app-", "td-workload-", "td-runtimes"];
/// The leaf of a delegation that holds its account's own processes.
const SESSION_LEAF: &str = "session";
/// How deep under a delegation a leaf is looked for.
const DEPTH: usize = 4;
const CONTROL_BYTES: u64 = 4096;
const STAT_BYTES: u64 = 64 * 1024;
const PROC_BYTES: u64 = 64 * 1024;

/// `/proc/meminfo`'s four lines this reads, in KiB.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Memory {
    total: u64,
    available: u64,
    swap_total: u64,
    swap_free: u64,
}

fn parse_meminfo(text: &str) -> Option<Memory> {
    let field = |name: &str| {
        text.lines()
            .find_map(|line| line.strip_prefix(name))
            .and_then(|rest| rest.trim().strip_suffix("kB"))
            .and_then(|value| value.trim().parse().ok())
    };
    Some(Memory {
        total: field("MemTotal:")?,
        available: field("MemAvailable:")?,
        swap_total: field("SwapTotal:")?,
        swap_free: field("SwapFree:")?,
    })
}

fn under(part: u64, whole: u64, percent: u64) -> bool {
    part.saturating_mul(100) < whole.saturating_mul(percent)
}

/// Both under the floor; a machine with no swap counts as out of it.
fn below_floor(memory: &Memory) -> bool {
    under(memory.available, memory.total, FLOOR_PERCENT)
        && (memory.swap_total == 0 || under(memory.swap_free, memory.swap_total, FLOOR_PERCENT))
}

/// Little enough available that pressure is the machine's, not one
/// throttled cgroup's.
fn scarce(memory: &Memory) -> bool {
    under(memory.available, memory.total, SCARCE_PERCENT)
}

/// The smallest victim worth a kill, in bytes.
fn min_victim(memory: &Memory) -> u64 {
    memory
        .total
        .saturating_mul(1024)
        .saturating_mul(MIN_VICTIM_PERCENT)
        / 100
}

/// `full avg10` of a PSI file, in percent.
fn full_avg10(text: &str) -> Option<f64> {
    text.lines()
        .find_map(|line| line.strip_prefix("full "))?
        .split_whitespace()
        .find_map(|field| field.strip_prefix("avg10="))?
        .parse()
        .ok()
}

#[derive(Debug, PartialEq)]
enum Reason {
    Floor,
    Pressure(f64),
}

/// What one reading says.
#[derive(Debug, PartialEq)]
enum Seen {
    /// Memory is no longer scarce: it has recovered.
    Calm,
    /// Scarce, but without lasting pressure: the pressure clock restarts,
    /// and nothing is forgiven.
    Scarce,
    /// Pressure while scarce, not yet for long enough.
    Building,
    Act(Reason),
}

/// Pressure has to last, timed by the clock rather than by polls, which
/// stretch under thrash; the floor acts at once.
#[derive(Default)]
struct Trigger {
    since: Option<Instant>,
}

impl Trigger {
    fn observe(&mut self, now: Instant, memory: &Memory, full: Option<f64>) -> Seen {
        if below_floor(memory) {
            self.since = None;
            return Seen::Act(Reason::Floor);
        }
        match full {
            Some(full) if full >= PRESSURE_PERCENT && scarce(memory) => {
                let since = *self.since.get_or_insert(now);
                if now.saturating_duration_since(since) >= PRESSURE_FOR {
                    self.since = None;
                    return Seen::Act(Reason::Pressure(full));
                }
                Seen::Building
            }
            _ => {
                self.since = None;
                if scarce(memory) {
                    Seen::Scarce
                } else {
                    Seen::Calm
                }
            }
        }
    }
}

/// Actions in a row with no calm reading between them: memory still
/// scarce after a kill, pressured or not, is not recovery.
fn futile_after(futile: u32, seen: &Seen) -> u32 {
    match seen {
        Seen::Calm => 0,
        Seen::Scarce | Seen::Building => futile,
        Seen::Act(_) => futile.saturating_add(1),
    }
}

/// How long to wait after the `futile`th action in a row with no calm
/// reading between.
fn settle(futile: u32) -> Duration {
    let doublings = futile.saturating_sub(FUTILE - 1).min(4);
    SETTLE
        .saturating_mul(1 << doublings)
        .min(SETTLE_MAX)
        .max(SETTLE)
}

/// A populated leaf and what it holds.
#[derive(Debug, PartialEq)]
struct Leaf {
    path: PathBuf,
    bytes: u64,
}

impl Leaf {
    fn is_session(&self) -> bool {
        self.path.file_name().and_then(|name| name.to_str()) == Some(SESSION_LEAF)
    }
}

/// One process of a session leaf.
#[derive(Debug, PartialEq)]
struct Process {
    pid: i32,
    starttime: u64,
    name: String,
    bytes: u64,
}

/// What one kill would end: an instance leaf whole, or a session leaf's
/// largest process, ranked by that process alone, since that is all the
/// kill frees.
#[derive(Debug, PartialEq)]
enum Victim {
    Leaf(Leaf),
    Process { leaf: PathBuf, process: Process },
}

impl Victim {
    fn bytes(&self) -> u64 {
        match self {
            Victim::Leaf(leaf) => leaf.bytes,
            Victim::Process { process, .. } => process.bytes,
        }
    }
}

/// Largest first; equal holdings keep the order found.
fn rank(victims: &mut [Victim]) {
    victims.sort_by_key(|victim| std::cmp::Reverse(victim.bytes()));
}

/// Anonymous and shared memory from a leaf's `memory.stat`, in bytes.
/// Page cache it can drop by itself is left out; tmpfs charged to it is
/// counted, though a file another cgroup keeps open outlives the kill.
fn stat_held(text: &str) -> Option<u64> {
    let field = |name: &str| {
        text.lines()
            .find_map(|line| line.strip_prefix(name)?.strip_prefix(' '))
            .and_then(|value| value.trim().parse::<u64>().ok())
    };
    Some(field("anon")?.saturating_add(field("shmem").unwrap_or(0)))
}

/// Anonymous and shared resident memory plus swap, in bytes, and the
/// name, from `/proc/PID/status`. A kernel thread has none of them.
fn parse_status(text: &str) -> Option<(u64, String)> {
    let kib = |name: &str| {
        text.lines()
            .find_map(|line| line.strip_prefix(name))
            .and_then(|rest| rest.trim().strip_suffix("kB"))
            .and_then(|value| value.trim().parse::<u64>().ok())
    };
    let name = text
        .lines()
        .find_map(|line| line.strip_prefix("Name:"))?
        .trim()
        .to_string();
    let anon = kib("RssAnon:")?;
    let shmem = kib("RssShmem:").unwrap_or(0);
    let swapped = kib("VmSwap:").unwrap_or(0);
    Some((
        anon.saturating_add(shmem)
            .saturating_add(swapped)
            .saturating_mul(1024),
        name,
    ))
}

/// What the watcher has said and must not repeat every second.
#[derive(Default)]
struct Said {
    meminfo: bool,
    pressure: bool,
    futile: bool,
    nothing: bool,
}

pub(crate) fn run() -> Result<(), String> {
    say(&format!(
        "watching; kills at {FLOOR_PERCENT}% available memory and free swap, or \
         {PRESSURE_PERCENT}% full memory pressure for {}s under {SCARCE_PERCENT}% available",
        PRESSURE_FOR.as_secs()
    ));
    // The kernel's own OOM killer must not take the watcher first.
    if let Err(e) = fs::write("/proc/self/oom_score_adj", "-1000") {
        say(&format!(
            "/proc/self/oom_score_adj: {e}; the kernel may kill this watcher"
        ));
    }
    let mut trigger = Trigger::default();
    let mut said = Said::default();
    let mut futile = 0u32;
    // Both files are read once a second for the watcher's life, into these.
    let mut meminfo = String::with_capacity(4096);
    let mut pressure = String::with_capacity(256);
    loop {
        // A read that fails under pressure is retried, never fatal: this
        // is the moment the watcher exists for.
        let memory = match read_into(MEMINFO, &mut meminfo) {
            Ok(()) => parse_meminfo(&meminfo).ok_or(None),
            Err(e) => Err(Some(e)),
        };
        let memory = match memory {
            Ok(memory) => Some(memory),
            Err(why) => {
                if !said.meminfo {
                    match why {
                        Some(e) => say(&format!("{MEMINFO}: {e}; retrying")),
                        None => say(&format!("{MEMINFO} is unparseable; retrying")),
                    }
                    said.meminfo = true;
                }
                None
            }
        };
        let Some(memory) = memory else {
            thread::sleep(POLL);
            continue;
        };
        said.meminfo = false;
        let full = match read_into(PRESSURE, &mut pressure) {
            Ok(()) => {
                if said.pressure {
                    say(&format!("{PRESSURE} reads again"));
                    said.pressure = false;
                }
                full_avg10(&pressure)
            }
            Err(e) => {
                if !said.pressure {
                    say(&format!("{PRESSURE}: {e}; acting on the floor alone"));
                    said.pressure = true;
                }
                None
            }
        };
        let seen = trigger.observe(Instant::now(), &memory, full);
        futile = futile_after(futile, &seen);
        match seen {
            Seen::Act(reason) => {
                if futile >= FUTILE && !said.futile {
                    say(&format!(
                        "{futile} actions without memory recovering; slowing down"
                    ));
                    said.futile = true;
                }
                act(&reason, &memory, &mut said);
                thread::sleep(settle(futile));
            }
            Seen::Scarce | Seen::Building => thread::sleep(POLL),
            Seen::Calm => {
                said.futile = false;
                said.nothing = false;
                thread::sleep(POLL);
            }
        }
    }
}

fn say(text: &str) {
    crate::emit_err(&format!("td-svc {VERB}: {text}\n"));
}

/// `path` into `buffer`, replacing what it held, bounded.
fn read_into(path: &str, buffer: &mut String) -> io::Result<()> {
    buffer.clear();
    fs::File::open(path)?
        .take(PROC_BYTES)
        .read_to_string(buffer)
        .map(|_| ())
}

fn act(reason: &Reason, memory: &Memory, said: &mut Said) {
    let why = match reason {
        Reason::Floor => format!("available memory and free swap under {FLOOR_PERCENT}%"),
        Reason::Pressure(full) => format!(
            "full memory pressure {full}% for {}s with under {SCARCE_PERCENT}% available",
            PRESSURE_FOR.as_secs()
        ),
    };
    let mut leaves = Vec::new();
    let mut skipped = 0;
    if let Err(e) = workload_leaves(Path::new(cgroup::ROOT), &mut leaves, &mut skipped) {
        say(&format!("reading workloads: {e}"));
    }
    if skipped > 0 {
        say(&format!(
            "{skipped} workload cgroups could not be read and were passed over"
        ));
    }
    let mut victims = Vec::with_capacity(leaves.len());
    for leaf in leaves {
        if !leaf.is_session() {
            victims.push(Victim::Leaf(leaf));
            continue;
        }
        let mut unread = 0;
        match largest_process(&leaf.path, &mut unread) {
            Ok(Some(process)) => victims.push(Victim::Process {
                leaf: leaf.path,
                process,
            }),
            Ok(None) => {}
            Err(e) => say(&format!("reading {}: {e}", leaf.path.display())),
        }
        if unread > 0 {
            say(&format!("{unread} session processes could not be read"));
        }
    }
    rank(&mut victims);
    let floor = min_victim(memory);
    for victim in victims.iter().take_while(|victim| victim.bytes() >= floor) {
        let (done, what) = match victim {
            Victim::Leaf(leaf) => (
                cgroup::write_control(&leaf.path.join("cgroup.kill"), "1")
                    .map_err(|e| e.to_string()),
                format!(
                    "the cgroup {} ({} MiB)",
                    leaf.path.display(),
                    leaf.bytes >> 20
                ),
            ),
            Victim::Process { leaf, process } => (
                kill(process),
                format!(
                    "{} [{}] ({} MiB) of {}",
                    process.name,
                    process.pid,
                    process.bytes >> 20,
                    leaf.display()
                ),
            ),
        };
        match done {
            Ok(()) => {
                let verb = match victim {
                    Victim::Leaf(_) => "killed",
                    Victim::Process { .. } => "sent SIGKILL to",
                };
                say(&format!("{why}; {verb} {what}"));
                said.nothing = false;
                return;
            }
            // Its memory is already freed: settle rather than kill another.
            Err(e) if e == EXITED => {
                say(&format!("{why}; {what} exited before the signal"));
                said.nothing = false;
                return;
            }
            Err(e) => say(&format!("could not kill {what}: {e}")),
        }
    }
    if !said.nothing {
        say(&format!(
            "{why}; nothing it may kill holds {} MiB or more",
            floor >> 20
        ));
        said.nothing = true;
    }
}

/// Every populated leaf under a workload delegation.
///
/// A cgroup that cannot be read, most often one that went away while the
/// scan ran, and a subtree deeper than `DEPTH`, are counted in `skipped`
/// and passed over: under memory pressure workloads come and go, and one
/// of them must not stop the rest from being judged.
fn workload_leaves(root: &Path, out: &mut Vec<Leaf>, skipped: &mut u32) -> io::Result<()> {
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if entry.file_type()?.is_dir() && WORKLOADS.iter().any(|w| name.starts_with(w)) {
            leaves(&entry.path(), DEPTH, out, skipped);
        }
    }
    Ok(())
}

fn leaves(dir: &Path, depth: usize, out: &mut Vec<Leaf>, skipped: &mut u32) {
    if let Err(_unreadable) = leaf(dir, depth, out, skipped) {
        *skipped = skipped.saturating_add(1);
    }
}

fn leaf(dir: &Path, depth: usize, out: &mut Vec<Leaf>, skipped: &mut u32) -> io::Result<()> {
    let mut children = false;
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            children = true;
            match depth.checked_sub(1) {
                Some(below) => leaves(&entry.path(), below, out, skipped),
                None => *skipped = skipped.saturating_add(1),
            }
        }
    }
    if children {
        return Ok(());
    }
    if !populated(dir)? {
        return Ok(());
    }
    let stat = cgroup::read_bounded(&dir.join("memory.stat"), STAT_BYTES)?;
    let held = stat_held(&stat).ok_or_else(|| io::Error::other("memory.stat has no anon"))?;
    // A kernel without swap has no such file, and nothing swapped.
    let swapped = match cgroup::read_bounded(&dir.join("memory.swap.current"), CONTROL_BYTES) {
        Ok(text) => text
            .trim()
            .parse::<u64>()
            .map_err(|e| io::Error::other(format!("memory.swap.current: {e}")))?,
        Err(e) if e.kind() == io::ErrorKind::NotFound => 0,
        Err(e) => return Err(e),
    };
    out.push(Leaf {
        path: dir.to_path_buf(),
        bytes: held.saturating_add(swapped),
    });
    Ok(())
}

/// Whether the leaf holds a process, from `cgroup.events`, which no
/// number of processes makes long.
fn populated(dir: &Path) -> io::Result<bool> {
    let events = cgroup::read_bounded(&dir.join("cgroup.events"), CONTROL_BYTES)?;
    Ok(events.lines().any(|line| line.trim() == "populated 1"))
}

/// The largest process of a session leaf, streaming its pid list so no
/// length makes the leaf unreadable. A process that is gone, unreadable,
/// pid 1, or exempt by an `oom_score_adj` of -1000 is not a candidate;
/// one that could not be read is counted in `unread`.
fn largest_process(leaf: &Path, unread: &mut u32) -> io::Result<Option<Process>> {
    let relative = leaf
        .strip_prefix(cgroup::ROOT)
        .map_err(|_| io::Error::other("a leaf outside the cgroup root"))?;
    let membership = format!("0::/{}", relative.display());
    let mut largest: Option<Process> = None;
    let mut reader = BufReader::new(fs::File::open(leaf.join("cgroup.procs"))?);
    let mut line = String::with_capacity(16);
    loop {
        line.clear();
        if reader.by_ref().take(32).read_line(&mut line)? == 0 {
            break;
        }
        let Ok(pid) = line.trim().parse::<i32>() else {
            continue;
        };
        if pid <= 1 {
            continue;
        }
        match process(pid, &membership) {
            Ok(Some(found)) => {
                if largest.as_ref().is_none_or(|held| found.bytes > held.bytes) {
                    largest = Some(found);
                }
            }
            Ok(None) => {}
            Err(_unreadable) => *unread = unread.saturating_add(1),
        }
    }
    Ok(largest)
}

/// One process's holding, read between two reads of its start time so
/// every figure, its leaf among them, is that process's: `None` when it is
/// gone, exempt, no longer in the leaf, or a different process by the end.
/// The leaf is read rather than trusted from the list: a listed pid that
/// exited and was reused elsewhere must not be judged as the session's.
fn process(pid: i32, membership: &str) -> io::Result<Option<Process>> {
    let Some(before) = procfs::stat_of(pid)? else {
        return Ok(None);
    };
    let read = |file| match read_proc(pid, file) {
        Ok(text) => Ok(Some(text)),
        Err(e) if gone(&e) => Ok(None),
        Err(e) => Err(e),
    };
    let Some(cgroup) = read("cgroup")? else {
        return Ok(None);
    };
    if !in_leaf(&cgroup, membership) {
        return Ok(None);
    }
    let (Some(adj), Some(status)) = (read("oom_score_adj")?, read("status")?) else {
        return Ok(None);
    };
    if !procfs::is_same_process(pid, before.starttime)? {
        return Ok(None);
    }
    if adj.trim() == "-1000" {
        return Ok(None);
    }
    let Some((bytes, name)) = parse_status(&status) else {
        return Ok(None);
    };
    Ok(Some(Process {
        pid,
        starttime: before.starttime,
        name,
        bytes,
    }))
}

/// ENOENT from the open or ESRCH from the read: the process exited.
fn gone(e: &io::Error) -> bool {
    e.kind() == io::ErrorKind::NotFound || e.raw_os_error() == Some(3)
}

/// `/proc/PID/cgroup` names exactly this cgroup-v2 leaf.
fn in_leaf(cgroup: &str, membership: &str) -> bool {
    cgroup.lines().any(|line| line == membership)
}

/// A `/proc/PID` file as text; a name the kernel gives in other bytes than
/// UTF-8 is shown lossily rather than lost.
fn read_proc(pid: i32, file: &str) -> io::Result<String> {
    let mut bytes = Vec::new();
    fs::File::open(format!("/proc/{pid}/{file}"))?
        .take(PROC_BYTES)
        .read_to_end(&mut bytes)?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

const EXITED: &str = "it exited before the signal";

fn kill(process: &Process) -> Result<(), String> {
    if process.pid <= 1 {
        return Err("refusing pid 1 or below".into());
    }
    // The pid was read a moment ago; it must still be that process.
    match procfs::is_same_process(process.pid, process.starttime) {
        Ok(true) => crate::supervise::send_signal(process.pid, crate::sys::SIGKILL),
        Ok(false) => Err(EXITED.into()),
        Err(e) => Err(e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]
    use super::*;

    fn memory(total: u64, available: u64, swap_total: u64, swap_free: u64) -> Memory {
        Memory {
            total,
            available,
            swap_total,
            swap_free,
        }
    }

    #[test]
    fn meminfo_gives_its_four_lines() {
        let text = "MemTotal:       65012345 kB\nMemFree: 1 kB\nMemAvailable:   38000000 kB\n\
                    SwapCached: 0 kB\nSwapTotal:      16777212 kB\nSwapFree:       10000000 kB\n";
        assert_eq!(
            parse_meminfo(text),
            Some(memory(65012345, 38000000, 16777212, 10000000))
        );
        assert_eq!(parse_meminfo("MemTotal: 1 kB\n"), None);
        assert_eq!(
            parse_meminfo("MemTotal: 1 MB\nMemAvailable: 1 kB\nSwapTotal: 0 kB\nSwapFree: 0 kB\n"),
            None
        );
    }

    #[test]
    fn the_floor_needs_both_memory_and_swap_low() {
        assert!(below_floor(&memory(1000, 49, 1000, 49)));
        assert!(!below_floor(&memory(1000, 50, 1000, 49)));
        assert!(!below_floor(&memory(1000, 49, 1000, 50)));
        // No swap at all: memory alone decides.
        assert!(below_floor(&memory(1000, 49, 0, 0)));
        assert!(!below_floor(&memory(1000, 500, 0, 0)));
    }

    #[test]
    fn the_smallest_victim_is_a_hundredth_of_ram() {
        // 16 GiB in KiB.
        assert_eq!(
            min_victim(&memory(16 * 1024 * 1024, 0, 0, 0)),
            16 * 1024 * 1024 * 1024 / 100
        );
    }

    #[test]
    fn full_avg10_is_read_from_the_full_line() {
        let text = "some avg10=80.00 avg60=1.00 avg300=0.00 total=1\n\
                    full avg10=52.25 avg60=0.50 avg300=0.00 total=2\n";
        assert_eq!(full_avg10(text), Some(52.25));
        assert_eq!(
            full_avg10("some avg10=80.00 avg60=1.00 avg300=0.00 total=1\n"),
            None
        );
        assert_eq!(full_avg10("full avg10=x total=2\n"), None);
    }

    #[test]
    fn pressure_must_last_while_memory_is_scarce_and_the_floor_acts_at_once() {
        let start = Instant::now();
        let at = |s: u64| start + Duration::from_secs(s);
        let scarce = memory(1000, 100, 1000, 900);
        let mut trigger = Trigger::default();
        for s in 0..20 {
            assert_eq!(
                trigger.observe(at(s), &scarce, Some(PRESSURE_PERCENT)),
                Seen::Building
            );
        }
        assert_eq!(
            trigger.observe(at(20), &scarce, Some(PRESSURE_PERCENT)),
            Seen::Act(Reason::Pressure(PRESSURE_PERCENT))
        );
        // One calm reading starts the clock again.
        assert_eq!(trigger.observe(at(21), &scarce, Some(99.0)), Seen::Building);
        assert_eq!(trigger.observe(at(30), &scarce, Some(10.0)), Seen::Scarce);
        assert_eq!(trigger.observe(at(31), &scarce, Some(99.0)), Seen::Building);
        assert_eq!(trigger.observe(at(50), &scarce, Some(99.0)), Seen::Building);
        assert_eq!(
            trigger.observe(at(51), &scarce, Some(99.0)),
            Seen::Act(Reason::Pressure(99.0))
        );
        // A cgroup stalled at its own memory.high on a machine with memory
        // to spare is not thrash, however long it lasts.
        let plenty = memory(1000, 900, 1000, 900);
        for s in 100..200 {
            assert_eq!(trigger.observe(at(s), &plenty, Some(99.0)), Seen::Calm);
        }
        assert_eq!(trigger.observe(at(201), &scarce, None), Seen::Scarce);
        assert_eq!(
            trigger.observe(at(202), &memory(1000, 1, 1000, 1), None),
            Seen::Act(Reason::Floor)
        );
    }

    /// Pressure building again after a pressure kill is not recovery: the
    /// count carries across cycles, so the settle grows.
    #[test]
    fn only_a_calm_reading_forgives_futile_actions() {
        let start = Instant::now();
        let at = |s: u64| start + Duration::from_secs(s);
        let scarce = memory(1000, 100, 1000, 900);
        let mut trigger = Trigger::default();
        let mut futile = 0;
        let mut now = 0;
        for _cycle in 0..FUTILE {
            loop {
                let seen = trigger.observe(at(now), &scarce, Some(99.0));
                futile = futile_after(futile, &seen);
                now += 1;
                if matches!(seen, Seen::Act(_)) {
                    break;
                }
            }
        }
        assert_eq!(futile, FUTILE);
        assert!(settle(futile) > SETTLE);
        // Pressure dipping while memory stays scarce forgives nothing.
        let dip = trigger.observe(at(now), &scarce, Some(10.0));
        assert_eq!(dip, Seen::Scarce);
        assert_eq!(futile_after(futile, &dip), FUTILE);
        let calm = trigger.observe(at(now), &memory(1000, 900, 1000, 900), None);
        assert_eq!(futile_after(futile, &calm), 0);
    }

    #[test]
    fn membership_is_the_cgroup_v2_line_exactly() {
        let leaf = "0::/td-user-1000/session";
        assert!(in_leaf("0::/td-user-1000/session\n", leaf));
        assert!(!in_leaf("0::/system/wayland\n", leaf));
        assert!(!in_leaf("0::/td-user-1000/session/x\n", leaf));
        assert!(!in_leaf("", leaf));
    }

    #[test]
    fn futile_actions_slow_down_to_a_bound() {
        assert_eq!(settle(1), SETTLE);
        assert_eq!(settle(FUTILE - 1), SETTLE);
        assert_eq!(settle(FUTILE), SETTLE * 2);
        assert_eq!(settle(FUTILE + 1), SETTLE * 4);
        assert_eq!(settle(u32::MAX), SETTLE_MAX);
    }

    #[test]
    fn victims_rank_by_what_their_kill_frees() {
        let leaf = |path: &str, bytes| Leaf {
            path: PathBuf::from(path),
            bytes,
        };
        // A session's largest process ranks by itself, not by its leaf.
        let mut victims = vec![
            Victim::Leaf(leaf("/c/td-app-1/7", 10)),
            Victim::Process {
                leaf: PathBuf::from("/c/td-user-1000/session"),
                process: Process {
                    pid: 7,
                    starttime: 1,
                    name: "rustc".into(),
                    bytes: 20,
                },
            },
            Victim::Leaf(leaf("/c/td-app-2/9", 30)),
        ];
        rank(&mut victims);
        let order: Vec<u64> = victims.iter().map(Victim::bytes).collect();
        assert_eq!(order, [30, 20, 10]);
        assert!(leaf("/c/td-user-1000/session", 1).is_session());
        assert!(leaf("/c/td-app-65536/session", 1).is_session());
        assert!(!leaf("/c/td-app-65536/sessions", 1).is_session());
    }

    #[test]
    fn a_leaf_holds_its_anonymous_and_shared_memory() {
        let stat = "anon 1000\nfile 99999\nkernel 5\nshmem 20\nfile_mapped 3\n";
        assert_eq!(stat_held(stat), Some(1020));
        assert_eq!(stat_held("anon 7\n"), Some(7));
        // `anon_thp` is not `anon`.
        assert_eq!(stat_held("anon_thp 5\nfile 1\n"), None);
    }

    #[test]
    fn status_gives_anonymous_shared_and_swapped() {
        let text = "Name:\tcargo test\nVmRSS:\t   9999 kB\nRssAnon:\t 1024 kB\n\
                    RssFile:\t 5000 kB\nRssShmem:\t 1024 kB\nVmSwap:\t  2048 kB\n";
        assert_eq!(
            parse_status(text),
            Some((4 * 1024 * 1024, "cargo test".to_string()))
        );
        assert_eq!(
            parse_status("Name:\tsh\nRssAnon:\t4 kB\n"),
            Some((4096, "sh".to_string()))
        );
        assert_eq!(parse_status("Name:\tkthreadd\n"), None);
    }

    #[test]
    fn leaves_are_populated_workloads_and_never_system() {
        let root = td_fs::TempDir::new("td-svc-oom").unwrap();
        let base = root.path();
        let make = |rel: &str, procs: &str, anon: u64, swapped: u64| {
            let dir = base.join(rel);
            fs::create_dir_all(&dir).unwrap();
            let populated = u8::from(!procs.is_empty());
            fs::write(
                dir.join("cgroup.events"),
                format!("populated {populated}\nfrozen 0\n"),
            )
            .unwrap();
            fs::write(dir.join("memory.stat"), format!("anon {anon}\nshmem 0\n")).unwrap();
            fs::write(dir.join("memory.swap.current"), format!("{swapped}\n")).unwrap();
        };
        make("system/wayland", "10\n", 999_999_999, 0);
        make("td-user-1000/session", "11\n", 900, 99);
        make("td-user-1000/jail-3", "12\n", 300, 0);
        make("td-app-5/session", "13\n", 500, 0);
        make("td-app-5/empty", "", 0, 0);
        make("td-workload-98304/7", "14\n", 100, 50);
        // No swap file, as on a kernel without swap: nothing swapped.
        make("td-app-8/9", "17\n", 40, 0);
        fs::remove_file(base.join("td-app-8/9/memory.swap.current")).unwrap();
        // A leaf without the memory controller cannot be measured; it is
        // passed over and the rest are still judged.
        let unmeasured = base.join("td-app-6/session");
        fs::create_dir_all(&unmeasured).unwrap();
        fs::write(unmeasured.join("cgroup.events"), "populated 1\n").unwrap();
        // Deeper than the walk goes: counted, not judged.
        make("td-app-7/a/b/c/d/e", "16\n", 1, 0);
        let mut out = Vec::new();
        let mut skipped = 0;
        workload_leaves(base, &mut out, &mut skipped).unwrap();
        assert_eq!(skipped, 2);
        out.sort_by_key(|leaf| std::cmp::Reverse(leaf.bytes));
        let found: Vec<(String, u64)> = out
            .iter()
            .map(|leaf| {
                (
                    leaf.path.strip_prefix(base).unwrap().display().to_string(),
                    leaf.bytes,
                )
            })
            .collect();
        assert_eq!(
            found,
            [
                ("td-user-1000/session".to_string(), 999),
                ("td-app-5/session".to_string(), 500),
                ("td-user-1000/jail-3".to_string(), 300),
                ("td-workload-98304/7".to_string(), 150),
                ("td-app-8/9".to_string(), 40),
            ]
        );
    }

    #[test]
    fn pid_one_is_never_signalled() {
        let init = Process {
            pid: 1,
            starttime: 1,
            name: "init".into(),
            bytes: u64::MAX,
        };
        assert!(kill(&init).is_err());
    }
}

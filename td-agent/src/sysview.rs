//! `system_status` (DESIGN.md §8, System view; §12): what the machine is
//! doing, for a conversation whose template turns system view on. The
//! conversation process reads it outside the jail, from procfs and sysfs
//! alone: load, CPU and memory, pressure, temperatures, batteries and
//! the processes busiest by CPU or memory, with their command lines. It
//! starts no program, writes nothing and never opens a process's
//! environ, mem or descriptors; what it cannot read is left out. A
//! command line is the kernel's copy of the process's argument memory,
//! which a process that rewrote it may have run into its environment.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// How long CPU use is sampled over.
pub const SAMPLE: Duration = Duration::from_secs(1);
/// How long the view is waited for: reading a command line takes the
/// process's memory lock, which a process stuck on a hung mount holds.
pub const DEADLINE: Duration = Duration::from_secs(10);
/// The processes listed: by default, and at most.
pub const LIMIT: usize = 20;
pub const MAX_LIMIT: usize = 100;
/// The longest filter taken.
pub const MAX_MATCH: usize = 256;
/// The most processes read, so a machine with very many costs a bound.
const MAX_PROCESSES: usize = 65_536;
/// The most bytes of a command line read, and shown.
const MAX_COMMAND_READ: u64 = 4096;
const MAX_COMMAND_SHOWN: usize = 200;
/// The most bytes of any other procfs or sysfs file read.
const MAX_FILE: u64 = 64 * 1024;
/// The most thermal zones, hwmon chips and power supplies read, and the
/// most temperatures a chip gives.
const MAX_SENSORS: usize = 16;
const MAX_READINGS: usize = 8;
/// procfs counts CPU time in USER_HZ ticks, 100 a second on Linux
/// whatever the kernel's own tick.
const TICKS: f64 = 100.0;

/// Where procfs, sysfs and the user database are: the host's, or a test's.
#[derive(Clone, Debug)]
pub struct Roots {
    pub proc: PathBuf,
    pub sys: PathBuf,
    pub passwd: PathBuf,
}

impl Roots {
    pub fn host() -> Self {
        Self {
            proc: "/proc".into(),
            sys: "/sys".into(),
            passwd: "/etc/passwd".into(),
        }
    }
}

/// What the processes are sorted by.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Sort {
    #[default]
    Cpu,
    Memory,
}

impl Sort {
    pub fn parse(word: &str) -> Option<Self> {
        match word {
            "cpu" => Some(Self::Cpu),
            "memory" => Some(Self::Memory),
            _ => None,
        }
    }
}

/// `system_status`'s arguments.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Ask {
    pub sort: Sort,
    pub limit: usize,
    /// Lists only processes whose name or command line holds it, ASCII
    /// case aside.
    pub filter: Option<String>,
}

/// The CPU time counters at one moment.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct Cpu {
    total: u64,
    idle: u64,
}

/// One process's `stat` at one moment.
#[derive(Clone, Debug, Eq, PartialEq)]
struct Ticks {
    pid: u32,
    /// When it started, in ticks since boot: a pid used again since
    /// has another.
    start: u64,
    ticks: u64,
    name: String,
    state: char,
    threads: u64,
}

/// One moment's counters, for CPU use between two.
#[derive(Clone, Debug, Default)]
pub struct Sample {
    cpu: Option<Cpu>,
    /// Ticks since boot when it was taken, from `uptime`.
    uptime: Option<u64>,
    /// Sorted by pid.
    processes: Vec<Ticks>,
}

/// A process as listed.
#[derive(Clone, Debug)]
struct Process {
    pid: u32,
    name: String,
    state: char,
    /// Percent of one CPU over the sample; none for one whose share
    /// cannot be told, seen only in the second sample but older.
    cpu: Option<f64>,
    rss_kib: u64,
    threads: u64,
    uid: Option<u32>,
    /// Its command line, once a filter has read it.
    command: Option<String>,
}

/// Reads a small file whole, at most `MAX_FILE` bytes, as text.
fn read(path: &Path) -> Option<String> {
    read_bounded(path, MAX_FILE)
}

fn read_bounded(path: &Path, most: u64) -> Option<String> {
    let mut bytes = Vec::new();
    fs::File::open(path)
        .ok()?
        .take(most)
        .read_to_end(&mut bytes)
        .ok()?;
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

/// The process directories under `proc`, by pid, at most `MAX_PROCESSES`.
fn pids(proc: &Path) -> Vec<u32> {
    let mut pids: Vec<u32> = fs::read_dir(proc)
        .map(|dir| {
            dir.filter_map(|entry| entry.ok()?.file_name().to_str()?.parse().ok())
                .take(MAX_PROCESSES)
                .collect()
        })
        .unwrap_or_default();
    pids.sort_unstable();
    pids
}

/// `/proc/<pid>/stat`'s fields after the name, and the name: the name
/// is in parentheses and may hold either, so it ends at the last `)`.
fn stat_fields(stat: &str) -> Option<(&str, Vec<&str>)> {
    let open = stat.find('(')?;
    let close = stat.rfind(')')?;
    let name = stat.get(open + 1..close)?;
    let rest = stat.get(close + 1..)?;
    Some((name, rest.split_whitespace().collect()))
}

/// utime and stime, the 14th and 15th fields, the 12th and 13th after
/// the name.
fn ticks(fields: &[&str]) -> Option<u64> {
    let user: u64 = fields.get(11)?.parse().ok()?;
    let system: u64 = fields.get(12)?.parse().ok()?;
    Some(user.saturating_add(system))
}

/// `/proc/stat`'s first line: every CPU's time together, idle and
/// waiting for I/O counted idle.
fn cpu(proc: &Path) -> Option<Cpu> {
    let stat = read(&proc.join("stat"))?;
    let line = stat.lines().next()?;
    let mut words = line.split_whitespace();
    if words.next()? != "cpu" {
        return None;
    }
    let counts: Vec<u64> = words.filter_map(|w| w.parse().ok()).collect();
    // user nice system idle iowait irq softirq steal; guest time is
    // already counted in user.
    let total = counts
        .iter()
        .take(8)
        .fold(0u64, |a, b| a.saturating_add(*b));
    let idle = counts
        .get(3)
        .copied()
        .unwrap_or(0)
        .saturating_add(counts.get(4).copied().unwrap_or(0));
    Some(Cpu { total, idle })
}

/// The counters now.
pub fn sample(roots: &Roots) -> Sample {
    let uptime = read(&roots.proc.join("uptime"))
        .and_then(|u| u.split_whitespace().next()?.parse::<f64>().ok())
        .map(|seconds| (seconds * TICKS) as u64);
    let processes = pids(&roots.proc)
        .into_iter()
        .filter_map(|pid| {
            let stat = read(&roots.proc.join(pid.to_string()).join("stat"))?;
            let (name, fields) = stat_fields(&stat)?;
            Some(Ticks {
                pid,
                // The 22nd field, the 20th after the name; threads the
                // 20th.
                start: fields.get(19)?.parse().ok()?,
                ticks: ticks(&fields)?,
                name: name.to_string(),
                state: fields.first().and_then(|s| s.chars().next()).unwrap_or('?'),
                threads: fields.get(17).and_then(|t| t.parse().ok()).unwrap_or(0),
            })
        })
        .collect();
    Sample {
        cpu: cpu(&roots.proc),
        uptime,
        processes,
    }
}

/// Whether a view's thread is still reading, perhaps held by the kernel.
static READING: AtomicBool = AtomicBool::new(false);

/// Clears `READING` when the thread that set it ends.
struct Reading;

impl Drop for Reading {
    fn drop(&mut self) {
        READING.store(false, Ordering::SeqCst);
    }
}

/// `system_status`'s answer on the host: sampled over `SAMPLE`, on a
/// thread of its own so that a read the kernel holds cannot hold the
/// conversation past `DEADLINE`; that thread is then left to finish,
/// and until it has, no other is started.
pub fn status(ask: &Ask) -> String {
    if READING.swap(true, Ordering::SeqCst) {
        return "an earlier system view is still being read, held by the kernel; ask again later"
            .into();
    }
    let (send, answer) = std::sync::mpsc::channel();
    let ask = ask.clone();
    let spawned = std::thread::Builder::new()
        .name("system-view".into())
        .spawn(move || {
            // Done reading before the answer is sent, so the next call
            // finds no view still being read.
            let answer = {
                let _reading = Reading;
                sampled(&Roots::host(), &ask, SAMPLE)
            };
            let _ = send.send(answer);
        });
    if let Err(e) = spawned {
        READING.store(false, Ordering::SeqCst);
        return format!("the system view could not start: {e}");
    }
    answer.recv_timeout(DEADLINE).unwrap_or_else(|_| {
        format!(
            "the system view did not answer within {} s: a process's command line could not be read, which a process stuck on a hung mount causes",
            DEADLINE.as_secs()
        )
    })
}

/// Two samples `wait` apart, timed from the middle of each, then the
/// report.
fn sampled(roots: &Roots, ask: &Ask, wait: Duration) -> String {
    let first = Instant::now();
    let before = sample(roots);
    let first = first + first.elapsed() / 2;
    std::thread::sleep(wait);
    let second = Instant::now();
    let after = sample(roots);
    let second = second + second.elapsed() / 2;
    report(roots, &before, &after, second.duration_since(first), ask)
}

/// The report, CPU use measured from `before` to `after`, `elapsed`
/// apart; what else it says is read now.
pub fn report(
    roots: &Roots,
    before: &Sample,
    after: &Sample,
    elapsed: Duration,
    ask: &Ask,
) -> String {
    let seconds = elapsed.as_secs_f64().max(0.001);
    let mut out = String::new();
    overview(roots, before, after, seconds, &mut out);
    let users = users(&roots.passwd);
    let mut processes: Vec<Process> = after
        .processes
        .iter()
        .map(|now| process(roots, now, before, seconds))
        .collect();
    let total = processes.len();
    if let Some(filter) = &ask.filter {
        let filter = filter.to_ascii_lowercase();
        processes.retain_mut(|p| {
            p.command = command(roots, p.pid);
            p.name.to_ascii_lowercase().contains(&filter)
                || p.command
                    .as_ref()
                    .is_some_and(|c| c.to_ascii_lowercase().contains(&filter))
        });
    }
    let cpu = |p: &Process| p.cpu.unwrap_or(0.0);
    match ask.sort {
        Sort::Cpu => processes.sort_by(|a, b| {
            cpu(b)
                .total_cmp(&cpu(a))
                .then(b.rss_kib.cmp(&a.rss_kib))
                .then(a.pid.cmp(&b.pid))
        }),
        Sort::Memory => processes.sort_by(|a, b| {
            b.rss_kib
                .cmp(&a.rss_kib)
                .then(cpu(b).total_cmp(&cpu(a)))
                .then(a.pid.cmp(&b.pid))
        }),
    }
    let sort = match ask.sort {
        Sort::Cpu => "CPU",
        Sort::Memory => "memory",
    };
    let shown = processes.len().min(ask.limit);
    let matching = match &ask.filter {
        Some(filter) => format!(", {} matching {filter:?}", processes.len()),
        None => String::new(),
    };
    out.push_str(&format!(
        "\nProcesses: {total}{matching}; the {shown} busiest by {sort} (CPU% is of one CPU over the sample):\n"
    ));
    out.push_str("    PID   CPU%      RSS  THR S USER       COMMAND\n");
    for p in processes.iter().take(shown) {
        let user = p
            .uid
            .map(|uid| {
                users
                    .iter()
                    .find(|(id, _)| *id == uid)
                    .map_or_else(|| uid.to_string(), |(_, name)| name.clone())
            })
            .unwrap_or_else(|| "?".into());
        let command = p
            .command
            .clone()
            .or_else(|| command(roots, p.pid))
            .unwrap_or_else(|| format!("[{}]", p.name));
        let cpu = p.cpu.map_or_else(|| "?".into(), |cpu| format!("{cpu:.1}"));
        out.push_str(&format!(
            "{:>7} {:>6} {:>8} {:>4} {} {:<10} {}\n",
            p.pid,
            cpu,
            size(p.rss_kib),
            p.threads,
            p.state,
            cut(&user, 10),
            cut(&command, MAX_COMMAND_SHOWN)
        ));
    }
    out
}

/// The machine as a whole; every word read shown as `word` shows it.
fn overview(roots: &Roots, before: &Sample, after: &Sample, seconds: f64, out: &mut String) {
    let proc = &roots.proc;
    let host = read(&proc.join("sys/kernel/hostname"))
        .map(|h| word(h.trim()))
        .unwrap_or_default();
    let cpus = read(&proc.join("stat")).map_or(0, |stat| {
        stat.lines()
            .filter(|l| l.starts_with("cpu") && l.as_bytes().get(3).is_some_and(u8::is_ascii_digit))
            .count()
    });
    let up = read(&proc.join("uptime"))
        .and_then(|u| u.split_whitespace().next()?.parse::<f64>().ok())
        .map(|s| format!(", up {}", duration(s as u64)))
        .unwrap_or_default();
    out.push_str(&format!("Host: {host}{up}, {cpus} CPUs\n"));
    if let Some(load) = read(&proc.join("loadavg")) {
        let words: Vec<&str> = load.split_whitespace().collect();
        if let [one, five, fifteen, tasks, ..] = words.as_slice() {
            let (running, all) = tasks.split_once('/').unwrap_or((tasks, "?"));
            let (one, five, fifteen) = (word(one), word(five), word(fifteen));
            let (running, all) = (word(running), word(all));
            out.push_str(&format!(
                "Load: {one} {five} {fifteen} (1, 5, 15 minutes); {running} running of {all} tasks\n"
            ));
        }
    }
    if let (Some(then), Some(now)) = (before.cpu, after.cpu) {
        let total = now.total.saturating_sub(then.total);
        let idle = now.idle.saturating_sub(then.idle);
        if total > 0 {
            let busy = 100.0 * total.saturating_sub(idle) as f64 / total as f64;
            out.push_str(&format!(
                "CPU: {busy:.0}% busy across all CPUs over {seconds:.1} s\n"
            ));
        }
    }
    if let Some(meminfo) = read(&proc.join("meminfo")) {
        let field = |name: &str| {
            meminfo.lines().find_map(|l| {
                l.strip_prefix(name)?
                    .strip_prefix(':')?
                    .split_whitespace()
                    .next()?
                    .parse::<u64>()
                    .ok()
            })
        };
        if let (Some(total), Some(available)) = (field("MemTotal"), field("MemAvailable")) {
            let swap = match (field("SwapTotal"), field("SwapFree")) {
                (Some(total), Some(free)) if total > 0 => format!(
                    "; swap {} used of {}",
                    size(total.saturating_sub(free)),
                    size(total)
                ),
                _ => "; no swap".into(),
            };
            out.push_str(&format!(
                "Memory: {} used, {} available of {}{swap}\n",
                size(total.saturating_sub(available)),
                size(available),
                size(total)
            ));
        }
    }
    let pressure: Vec<String> = ["cpu", "memory", "io"]
        .iter()
        .filter_map(|kind| {
            let text = read(&proc.join("pressure").join(kind))?;
            let avg = |line: &str| {
                text.lines()
                    .find(|l| l.starts_with(line))?
                    .split_whitespace()
                    .find_map(|w| w.strip_prefix("avg10="))
                    .map(word)
            };
            let some = avg("some")?;
            Some(match avg("full") {
                Some(full) => format!("{kind} {some}% some, {full}% full"),
                None => format!("{kind} {some}% some"),
            })
        })
        .collect();
    if !pressure.is_empty() {
        out.push_str(&format!(
            "Pressure (share of the last 10 s stalled): {}\n",
            pressure.join("; ")
        ));
    }
    let celsius =
        |path: &Path| -> Option<i64> { Some(read(path)?.trim().parse::<i64>().ok()? / 1000) };
    let mut temperatures: Vec<String> = sensors(&roots.sys.join("class/thermal"), "thermal_zone")
        .into_iter()
        .filter_map(|zone| {
            let kind = word(read(&zone.join("type"))?.trim());
            Some(format!("{kind} {}\u{b0}C", celsius(&zone.join("temp"))?))
        })
        .collect();
    // Many machines report only through hwmon: a chip's name, then each
    // of its first readings, by label when it has one.
    for chip in sensors(&roots.sys.join("class/hwmon"), "hwmon") {
        let Some(name) = read(&chip.join("name")).map(|n| word(n.trim())) else {
            continue;
        };
        for n in 1..=MAX_READINGS {
            let Some(degrees) = celsius(&chip.join(format!("temp{n}_input"))) else {
                continue;
            };
            let label = read(&chip.join(format!("temp{n}_label")))
                .map_or_else(|| format!("temp{n}"), |l| word(l.trim()));
            temperatures.push(format!("{name} {label} {degrees}\u{b0}C"));
        }
    }
    temperatures.truncate(MAX_SENSORS * 2);
    if !temperatures.is_empty() {
        out.push_str(&format!("Temperatures: {}\n", temperatures.join(", ")));
    }
    for supply in sensors(&roots.sys.join("class/power_supply"), "") {
        if read(&supply.join("type")).as_deref().map(str::trim) != Some("Battery") {
            continue;
        }
        let name = supply
            .file_name()
            .map(|n| word(&n.to_string_lossy()))
            .unwrap_or_default();
        let said: Vec<String> = [
            read(&supply.join("capacity")).map(|c| format!("{}%", word(c.trim()))),
            read(&supply.join("status")).map(|s| word(s.trim())),
        ]
        .into_iter()
        .flatten()
        .collect();
        out.push_str(&format!("Battery {name}: {}\n", said.join(", ")));
    }
}

/// The entries of `dir` whose names begin `prefix`, in name order, at
/// most `MAX_SENSORS`.
fn sensors(dir: &Path, prefix: &str) -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = fs::read_dir(dir)
        .map(|entries| {
            entries
                .filter_map(|e| e.ok())
                .filter(|e| e.file_name().to_string_lossy().starts_with(prefix))
                .map(|e| e.path())
                .collect()
        })
        .unwrap_or_default();
    found.sort();
    found.truncate(MAX_SENSORS);
    found
}

/// A process as the second sample saw it, its CPU use since `before`:
/// the same process, by pid and start, from its ticks then; one started
/// after `before` was taken from nothing; any other, a pid `before`
/// missed or one used again for an older process, not told.
fn process(roots: &Roots, now: &Ticks, before: &Sample, seconds: f64) -> Process {
    let then = before
        .processes
        .binary_search_by_key(&now.pid, |t| t.pid)
        .ok()
        .and_then(|at| before.processes.get(at))
        .filter(|then| then.start == now.start)
        .map(|then| then.ticks)
        .or_else(|| before.uptime.filter(|up| now.start >= *up).map(|_| 0));
    let cpu = then.map(|then| 100.0 * now.ticks.saturating_sub(then) as f64 / TICKS / seconds);
    // Resident memory and the real owner, as `status` says them.
    let status = read(&roots.proc.join(now.pid.to_string()).join("status"));
    let field = |name: &str| {
        status.as_deref()?.lines().find_map(|l| {
            l.strip_prefix(name)?
                .split_whitespace()
                .next()?
                .parse::<u64>()
                .ok()
        })
    };
    Process {
        pid: now.pid,
        name: now.name.clone(),
        state: now.state,
        cpu,
        rss_kib: field("VmRSS:").unwrap_or(0),
        threads: now.threads,
        uid: field("Uid:").and_then(|uid| u32::try_from(uid).ok()),
        command: None,
    }
}

/// A process's command line, its arguments parted by spaces; none for a
/// kernel thread, which has none, or one gone.
fn command(roots: &Roots, pid: u32) -> Option<String> {
    let text = read_bounded(
        &roots.proc.join(pid.to_string()).join("cmdline"),
        MAX_COMMAND_READ,
    )?;
    let words: Vec<&str> = text.split('\0').filter(|w| !w.is_empty()).collect();
    (!words.is_empty()).then(|| words.join(" "))
}

/// The user database's names by uid.
fn users(passwd: &Path) -> Vec<(u32, String)> {
    read(passwd)
        .map(|text| {
            text.lines()
                .filter_map(|line| {
                    let mut fields = line.split(':');
                    let name = fields.next()?;
                    let uid = fields.nth(1)?.parse().ok()?;
                    Some((uid, name.to_string()))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// A word procfs or sysfs gave, as `cut` shows it, at most 64
/// characters.
fn word(text: &str) -> String {
    cut(text, 64)
}

/// `text` on one line, nothing invisible, at most `most` characters,
/// the last an ellipsis where it was cut.
fn cut(text: &str, most: usize) -> String {
    let visible = crate::tools::visible(text);
    if visible.chars().nth(most).is_none() {
        return visible;
    }
    let kept: String = visible.chars().take(most.saturating_sub(1)).collect();
    format!("{kept}\u{2026}")
}

/// KiB as the largest unit that keeps a whole number in front.
fn size(kib: u64) -> String {
    const UNITS: &[&str] = &["KiB", "MiB", "GiB", "TiB"];
    let mut value = kib as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    let name = UNITS.get(unit).copied().unwrap_or("KiB");
    if unit == 0 {
        format!("{kib} {name}")
    } else {
        format!("{value:.1} {name}")
    }
}

/// Seconds as days, hours and minutes.
fn duration(seconds: u64) -> String {
    let (days, hours, minutes) = (seconds / 86_400, seconds / 3600 % 24, seconds / 60 % 60);
    match (days, hours) {
        (0, 0) => format!("{minutes}m"),
        (0, _) => format!("{hours}h {minutes}m"),
        _ => format!("{days}d {hours}h"),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
    use super::*;

    struct Fixture {
        roots: Roots,
    }

    impl Fixture {
        fn new(name: &str) -> Self {
            let base = std::env::temp_dir()
                .join(format!("td-agent-sysview-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&base);
            let roots = Roots {
                proc: base.join("proc"),
                sys: base.join("sys"),
                passwd: base.join("passwd"),
            };
            fs::create_dir_all(&roots.proc).unwrap();
            fs::create_dir_all(&roots.sys).unwrap();
            fs::write(&roots.passwd, "root:x:0:0::/root:/bin/sh\n").unwrap();
            Self { roots }
        }

        fn write(&self, rel: &str, text: &str) {
            let path = self.roots.proc.join(rel);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, text).unwrap();
        }

        fn sys(&self, rel: &str, text: &str) {
            let path = self.roots.sys.join(rel);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, text).unwrap();
        }

        fn process(&self, pid: u32, name: &str, ticks: u64, rss_kib: u64, cmdline: &str) {
            self.started(pid, name, 100, ticks, rss_kib, cmdline);
        }

        /// A process that started `start` ticks after boot, owned by
        /// root.
        fn started(
            &self,
            pid: u32,
            name: &str,
            start: u64,
            ticks: u64,
            rss_kib: u64,
            cmdline: &str,
        ) {
            self.write(
                &format!("{pid}/stat"),
                &format!(
                    "{pid} ({name}) R 1 1 1 0 -1 0 0 0 0 0 {ticks} 0 0 0 20 0 3 0 {start} 0 0"
                ),
            );
            self.write(
                &format!("{pid}/status"),
                &format!("Name:\t{name}\nUid:\t0\t0\t0\t0\nVmRSS:\t {rss_kib} kB\n"),
            );
            self.write(&format!("{pid}/cmdline"), cmdline);
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            if let Some(base) = self.roots.proc.parent() {
                let _ = fs::remove_dir_all(base);
            }
        }
    }

    /// The overview reads load, CPU, memory, pressure, temperatures and
    /// batteries; the processes are sorted by CPU over the sample, a
    /// name holding parentheses and spaces read whole, a kernel thread
    /// shown by its name, one started inside the sample, or a pid used
    /// again in it, counted from nothing, and an older one the first
    /// sample missed not told.
    #[test]
    fn the_report_reads_the_machine_and_its_busiest_processes() {
        let f = Fixture::new("report");
        f.write("sys/kernel/hostname", "box\n");
        f.write("uptime", "93784.5 1000.0\n");
        f.write("loadavg", "1.50 0.75 0.25 3/412 999\n");
        f.write(
            "meminfo",
            "MemTotal:       16777216 kB\nMemFree: 1 kB\nMemAvailable:    8388608 kB\nSwapTotal: 0 kB\nSwapFree: 0 kB\n",
        );
        f.write(
            "pressure/cpu",
            "some avg10=2.50 avg60=1.00 avg300=0.50 total=1\n",
        );
        f.write(
            "pressure/memory",
            "some avg10=0.00 avg60=0.00 avg300=0.00 total=0\nfull avg10=0.00 avg60=0.00 avg300=0.00 total=0\n",
        );
        f.sys("class/thermal/thermal_zone0/type", "x86_pkg_temp\n");
        f.sys("class/thermal/thermal_zone0/temp", "54000\n");
        f.sys("class/hwmon/hwmon0/name", "k10temp\n");
        f.sys("class/hwmon/hwmon0/temp1_input", "61250\n");
        f.sys("class/hwmon/hwmon0/temp1_label", "Tctl\n");
        f.sys("class/hwmon/hwmon0/temp3_input", "40000\n");
        f.sys("class/hwmon/hwmon1/name", "asus\n");
        f.sys("class/power_supply/BAT0/type", "Battery\n");
        f.sys("class/power_supply/BAT0/capacity", "87\n");
        f.sys("class/power_supply/BAT0/status", "Discharging\n");
        f.sys("class/power_supply/AC/type", "Mains\n");
        f.write("stat", "cpu  100 0 100 800 0 0 0 0 0 0\ncpu0 1\ncpu1 1\n");
        f.process(10, "busy (worker) x", 1000, 2048, "/usr/bin/busy\0--fast\0");
        f.process(20, "kthreadd", 50, 0, "");
        f.process(30, "idle", 0, 1_048_576, "idle\0");
        f.process(50, "old", 500, 8, "old\0");
        let before = sample(&f.roots);
        f.write("stat", "cpu  200 0 200 1400 0 0 0 0 0 0\ncpu0 1\ncpu1 1\n");
        f.process(10, "busy (worker) x", 1150, 2048, "/usr/bin/busy\0--fast\0");
        f.process(20, "kthreadd", 60, 0, "");
        fs::remove_dir_all(f.roots.proc.join("30")).unwrap();
        // Uptime 93784.5 s is 9378450 ticks.
        f.started(40, "new", 9_378_460, 30, 4, "new\0");
        f.started(50, "reused", 9_378_470, 20, 2, "reused\0");
        f.process(60, "unseen", 999, 1, "unseen\0");
        let after = sample(&f.roots);
        let ask = Ask {
            limit: LIMIT,
            ..Ask::default()
        };
        let text = report(&f.roots, &before, &after, Duration::from_secs(1), &ask);
        for want in [
            "Host: box, up 1d 2h, 2 CPUs",
            "Load: 1.50 0.75 0.25 (1, 5, 15 minutes); 3 running of 412 tasks",
            "CPU: 25% busy across all CPUs over 1.0 s",
            "Memory: 8.0 GiB used, 8.0 GiB available of 16.0 GiB; no swap",
            "cpu 2.50% some; memory 0.00% some, 0.00% full",
            "Temperatures: x86_pkg_temp 54\u{b0}C, k10temp Tctl 61\u{b0}C, k10temp temp3 40\u{b0}C\n",
            "Battery BAT0: 87%, Discharging",
            "Processes: 5; the 5 busiest by CPU",
        ] {
            assert!(text.contains(want), "{want:?} in:\n{text}");
        }
        assert!(!text.contains("Battery AC"), "{text}");
        let rows: Vec<&str> = text
            .lines()
            .skip_while(|l| !l.contains("PID"))
            .skip(1)
            .collect();
        assert_eq!(rows.len(), 5, "{text}");
        assert!(
            rows[0].contains("150.0") && rows[0].contains("/usr/bin/busy --fast"),
            "{text}"
        );
        assert!(
            rows[1].contains("30.0") && rows[1].ends_with(" new"),
            "{text}"
        );
        assert!(
            rows[2].contains("20.0") && rows[2].ends_with(" reused"),
            "{text}"
        );
        assert!(
            rows[3].contains("10.0") && rows[3].ends_with("[kthreadd]"),
            "{text}"
        );
        assert!(
            rows[4].contains("     ? ") && rows[4].ends_with(" unseen"),
            "{text}"
        );
        assert!(rows[0].contains("2.0 MiB"), "{text}");
        // The owner is the real uid `status` names.
        assert!(rows[0].contains(" root "), "{text}");
    }

    /// Sorted by memory, filtered by name or command line, cut to the
    /// limit; a uid the user database names is shown by its name.
    #[test]
    fn processes_sort_by_memory_and_filter() {
        let f = Fixture::new("memory");
        f.process(1, "init", 0, 100, "/sbin/init\0");
        f.process(2, "big", 0, 5000, "/opt/Big App\0");
        f.process(3, "bigger", 0, 9000, "/opt/bigger\0");
        let before = sample(&f.roots);
        let after = sample(&f.roots);
        let ask = Ask {
            sort: Sort::Memory,
            limit: 1,
            filter: Some("BIG".into()),
        };
        let text = report(&f.roots, &before, &after, Duration::from_secs(1), &ask);
        assert!(
            text.contains("Processes: 3, 2 matching \"BIG\"; the 1 busiest by memory"),
            "{text}"
        );
        let rows: Vec<&str> = text
            .lines()
            .skip_while(|l| !l.contains("PID"))
            .skip(1)
            .collect();
        assert_eq!(rows.len(), 1, "{text}");
        assert!(rows[0].ends_with("/opt/bigger"), "{text}");
        assert!(text.lines().all(|l| !l.contains("init")), "{text}");
    }

    /// A machine whose files are missing or malformed answers with what
    /// it could read, never a panic.
    #[test]
    fn what_cannot_be_read_is_left_out() {
        let f = Fixture::new("missing");
        f.write("5/stat", "garbage");
        f.write("loadavg", "x");
        f.write("stat", "intr 1\n");
        f.sys("class/thermal/thermal_zone0/type", "zone\u{1b}[2J\n");
        f.sys("class/thermal/thermal_zone0/temp", "1000\n");
        let before = sample(&f.roots);
        let text = report(&f.roots, &before, &before, Duration::ZERO, &Ask::default());
        // What sysfs says is shown as tools show text, nothing invisible.
        assert!(!text.contains('\u{1b}'), "{text}");
        assert!(text.starts_with("Host: , 0 CPUs\n"), "{text}");
        assert!(text.contains("Processes: 0; the 0 busiest"), "{text}");
    }

    /// On the machine itself the report lists this process, found by
    /// its own program's name however busy the machine.
    #[test]
    fn the_host_answers() {
        let roots = Roots::host();
        let exe = std::env::current_exe().unwrap();
        let ask = Ask {
            limit: MAX_LIMIT,
            filter: Some(exe.file_name().unwrap().to_string_lossy().into_owned()),
            ..Ask::default()
        };
        let text = sampled(&roots, &ask, Duration::from_millis(10));
        assert!(text.starts_with("Host: "), "{text}");
        let own = format!("{:>7} ", std::process::id());
        assert!(text.lines().any(|l| l.starts_with(&own)), "{text}");
    }

    #[test]
    fn sizes_and_durations_read_plainly() {
        assert_eq!(size(512), "512 KiB");
        assert_eq!(size(1536), "1.5 MiB");
        assert_eq!(duration(59), "0m");
        assert_eq!(duration(3_660), "1h 1m");
        assert_eq!(duration(90_000), "1d 1h");
        assert_eq!(cut("abcdef", 3), "ab\u{2026}");
        assert_eq!(cut("abc", 3), "abc");
        assert_eq!(cut("a\u{202e}b", 10), crate::tools::visible("a\u{202e}b"));
    }
}

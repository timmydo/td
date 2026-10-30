use crate::{clock_sys, state};
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use td_time::request::{format_utc, parse_utc, Change};

#[path = "../../td-civil/src/tzif.rs"]
#[allow(dead_code, reason = "shared TZif validator")]
mod calendar;

pub(crate) fn utc() -> Result<u64, String> {
    u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|e| e.to_string())?
            .as_millis(),
    )
    .map_err(|e| e.to_string())
}

fn uptime() -> Result<u64, String> {
    let text = fs::read_to_string("/proc/uptime").map_err(|e| e.to_string())?;
    let first = text.split_whitespace().next().ok_or("missing uptime")?;
    let (seconds, fraction) = first.split_once('.').ok_or("invalid uptime")?;
    if fraction.len() != 2 {
        return Err("invalid uptime precision".into());
    }
    seconds
        .parse::<u64>()
        .ok()
        .and_then(|s| s.checked_mul(1000))
        .and_then(|s| {
            fraction
                .parse::<u64>()
                .ok()
                .and_then(|f| s.checked_add(f * 10))
        })
        .ok_or_else(|| "invalid uptime".into())
}

pub(crate) fn worker() -> Result<(), String> {
    let time = td_time::ntp::query()?;
    let stamp = uptime()?;
    writeln!(std::io::stdout(), "{time} {stamp}").map_err(|e| e.to_string())
}

struct Worker(Child);
impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn network() -> Result<u64, String> {
    let (mut reader, writer) = UnixStream::pair().map_err(|e| e.to_string())?;
    reader.set_nonblocking(true).map_err(|e| e.to_string())?;
    let child = Command::new("/bin/td-time")
        .arg("query-worker")
        .env_clear()
        .current_dir("/")
        .uid(65534)
        .gid(65534)
        .stdin(Stdio::null())
        .stdout(Stdio::from(std::os::fd::OwnedFd::from(writer)))
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| e.to_string())?;
    let mut child = Worker(child);
    let started = Instant::now();
    let mut bytes = Vec::new();
    loop {
        if started.elapsed() > Duration::from_secs(20) {
            return Err("network time query timed out".into());
        }
        let mut buffer = [0; 128];
        match reader.read(&mut buffer) {
            Ok(0) => {
                if let Some(status) = child.0.try_wait().map_err(|e| e.to_string())? {
                    if !status.success() {
                        return Err("network time unavailable or sources disagree".into());
                    }
                    break;
                }
            }
            Ok(n) => bytes.extend_from_slice(buffer.get(..n).ok_or("worker output length")?),
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                ) => {}
            Err(e) => return Err(e.to_string()),
        }
        if bytes.len() > 80 {
            return Err("network worker output exceeded limit".into());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let text = std::str::from_utf8(&bytes).map_err(|_| "invalid network result")?;
    let (time, stamp) = text
        .strip_suffix('\n')
        .ok_or("incomplete network result")?
        .split_once(' ')
        .ok_or("invalid network result")?;
    let time = time.parse::<u64>().map_err(|_| "invalid network time")?;
    let stamp = stamp.parse::<u64>().map_err(|_| "invalid network age")?;
    let elapsed = uptime()?
        .checked_sub(stamp)
        .filter(|age| *age <= 1000)
        .ok_or("network result is stale")?;
    time.checked_add(elapsed)
        .ok_or_else(|| "network time overflow".into())
}

pub(crate) fn rtc_time() -> Result<u64, String> {
    for _ in 0..3 {
        let date = fs::read_to_string("/sys/class/rtc/rtc0/date").map_err(|e| e.to_string())?;
        let time = fs::read_to_string("/sys/class/rtc/rtc0/time").map_err(|e| e.to_string())?;
        let again = fs::read_to_string("/sys/class/rtc/rtc0/date").map_err(|e| e.to_string())?;
        if date == again {
            return parse_utc(&format!("{}T{}Z", date.trim(), time.trim()));
        }
    }
    Err("RTC date changed during read".into())
}

fn write_rtc(seconds: u64) -> Result<(), String> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(0x20000 | 0x800)
        .open("/dev/rtc0")
        .map_err(|e| e.to_string())?;
    let meta = file.metadata().map_err(|e| e.to_string())?;
    let device = fs::read_to_string("/sys/class/rtc/rtc0/dev").map_err(|e| e.to_string())?;
    let (major, minor) = device
        .trim()
        .split_once(':')
        .ok_or("invalid RTC device identity")?;
    let major = major.parse::<u64>().map_err(|_| "RTC major")?;
    let minor = minor.parse::<u64>().map_err(|_| "RTC minor")?;
    let rdev = (minor & 0xff)
        | ((major & 0xfff) << 8)
        | ((minor & !0xff) << 12)
        | ((major & !0xfff) << 32);
    if !meta.file_type().is_char_device() || meta.uid() != 0 || meta.rdev() != rdev {
        return Err("RTC device identity mismatch".into());
    }
    clock_sys::rtc(&file, seconds)?;
    if rtc_time()?.abs_diff(seconds) > 2 {
        return Err("RTC write readback mismatch".into());
    }
    Ok(())
}

fn status_record(state: &state::State, result: &str, success: Option<u64>) -> Result<(), String> {
    let boot = fs::read_to_string("/proc/sys/kernel/random/boot_id").map_err(|e| e.to_string())?;
    let previous = state::read(&state.root.join("status"), 2048).unwrap_or_default();
    let last = match success {
        Some(value) => format!(
            "last-sync {} {} {}",
            format_utc(value / 1000)?,
            boot.trim(),
            uptime()?
        ),
        None => previous
            .lines()
            .find(|line| line.starts_with("last-sync "))
            .unwrap_or("last-sync never")
            .to_owned(),
    };
    let message: String = result
        .chars()
        .filter(|c| c.is_ascii_graphic() || *c == ' ')
        .take(512)
        .collect();
    state.publish(
        "status",
        format!("{last}\nlast-result {message}\n").as_bytes(),
    )
}

pub(crate) fn automatic_time(current: u64, observed: u64, enabled: bool) -> Result<u64, String> {
    if !enabled {
        return Err("automatic synchronization was disabled".into());
    }
    format_utc(observed / 1000)?;
    if current.abs_diff(observed) > 86_400_000 {
        return Err("clock differs by more than one day; manual correction required".into());
    }
    Ok(observed)
}

fn set_clock(state: &state::State, milliseconds: u64, automatic: bool) -> Result<(), String> {
    clock_sys::realtime(milliseconds)?;
    if utc()?.abs_diff(milliseconds) > 2000 {
        return Err("system clock changed but readback disagrees".into());
    }
    let rtc = write_rtc(utc()? / 1000);
    let description = match rtc {
        Ok(()) => "system clock corrected; RTC saved in UTC".to_owned(),
        Err(error) => format!("system clock corrected; RTC NOT saved: {error}"),
    };
    status_record(state, &description, automatic.then_some(milliseconds))?;
    writeln!(std::io::stdout(), "{description}").map_err(|e| e.to_string())
}

fn zone_bytes(name: &str) -> Result<Vec<u8>, String> {
    if !td_time::request::timezone_name(name) {
        return Err("invalid timezone".into());
    }
    let mut path = std::path::PathBuf::from("/etc/zoneinfo");
    let parts: Vec<_> = name.split('/').collect();
    for (index, part) in parts.iter().enumerate() {
        path.push(part);
        let meta = fs::symlink_metadata(&path).map_err(|e| e.to_string())?;
        if meta.uid() != 0
            || meta.mode() & 0o022 != 0
            || if index + 1 == parts.len() {
                !meta.is_file()
            } else {
                !meta.is_dir()
            }
        {
            return Err("untrusted timezone data".into());
        }
    }
    let bytes = state::bytes(&path, calendar::MAX_BYTES)?;
    calendar::Zone::parse(&bytes).ok_or("unsupported timezone data")?;
    Ok(bytes)
}

fn timezone(name: &str) -> Result<(), String> {
    let bytes = zone_bytes(name)?;
    // Canonical name is the transaction intent. Startup repairs localtime
    // from it after interruption between these two publications.
    state::publish(
        Path::new("/var/lib/td"),
        "timezone",
        format!("{name}\n").as_bytes(),
    )?;
    state::publish(Path::new("/var/lib/td"), "localtime", &bytes)
}

pub(crate) fn apply(change: Change) -> Result<(), String> {
    let state = state::State::lock()?;
    match change {
        Change::Utc(seconds) => set_clock(&state, seconds * 1000, false),
        Change::Timezone(name) => timezone(&name),
        Change::Automatic(on) => state.publish("automatic", if on { b"on\n" } else { b"off\n" }),
    }
}

pub(crate) fn resumed(boot_elapsed: u64, awake_elapsed: Duration) -> bool {
    u128::from(boot_elapsed) > awake_elapsed.as_millis().saturating_add(5000)
}

pub(crate) fn serve() -> Result<(), String> {
    {
        let _state = state::State::lock()?;
        if let Ok(name) = state::read(Path::new("/var/lib/td/timezone"), 65) {
            timezone(name.trim_end_matches('\n'))?;
        }
    }
    let mut next = Instant::now();
    let mut was_enabled = false;
    let mut last_uptime = uptime()?;
    let mut last_awake = Instant::now();
    loop {
        let current_uptime = uptime()?;
        let current_awake = Instant::now();
        if resumed(
            current_uptime.saturating_sub(last_uptime),
            current_awake.duration_since(last_awake),
        ) {
            next = Instant::now();
        }
        last_uptime = current_uptime;
        last_awake = current_awake;
        let enabled = state::automatic(Path::new("/var/lib/td/time"))?;
        if enabled && (!was_enabled || Instant::now() >= next) {
            let result = network();
            let state = state::State::lock()?;
            if state.automatic()? {
                let result = result.and_then(|time| {
                    let time = automatic_time(utc()?, time, state.automatic()?)?;
                    set_clock(&state, time, true)
                });
                let delay = if let Err(error) = result {
                    status_record(&state, &error, None)?;
                    60
                } else {
                    1800
                };
                next = Instant::now() + Duration::from_secs(delay);
            }
        }
        was_enabled = enabled;
        std::thread::sleep(Duration::from_secs(1));
    }
}

pub(crate) fn status() -> Result<(), String> {
    let now = format_utc(utc()? / 1000)?;
    let timezone =
        state::read(Path::new("/var/lib/td/timezone"), 65).unwrap_or_else(|_| "unavailable".into());
    let automatic = state::automatic(Path::new("/var/lib/td/time"))?;
    let rtc = rtc_time()
        .and_then(format_utc)
        .unwrap_or_else(|e| format!("unavailable ({e})"));
    let record = state::read(Path::new("/var/lib/td/time/status"), 2048)
        .unwrap_or_else(|_| "last-sync never\nlast-result service has not reported\n".into());
    let boot = fs::read_to_string("/proc/sys/kernel/random/boot_id").map_err(|e| e.to_string())?;
    let mut age = "unavailable (no success in this boot)".to_owned();
    if let Some(line) = record.lines().find(|line| line.starts_with("last-sync ")) {
        let fields: Vec<_> = line.split_whitespace().collect();
        if fields.get(2) == Some(&boot.trim()) {
            if let Some(stamp) = fields.get(3).and_then(|s| s.parse::<u64>().ok()) {
                if let Some(elapsed) = uptime()?.checked_sub(stamp) {
                    age = format!("{} seconds", elapsed / 1000);
                }
            }
        }
    }
    let last_sync = record
        .lines()
        .find_map(|line| line.strip_prefix("last-sync "))
        .and_then(|line| line.split_whitespace().next())
        .unwrap_or("never");
    let last_result = record
        .lines()
        .find_map(|line| line.strip_prefix("last-result "))
        .unwrap_or("unavailable");
    writeln!(std::io::stdout(), "UTC: {now}\nTimezone: {}\nRTC (UTC): {rtc}\nAutomatic sync: {}\nAutomatic source: two-source NTP (unauthenticated)\nLast sync: {last_sync}\nLast sync age: {age}\nLast result: {last_result}", timezone.trim(), if automatic { "on" } else { "off" })
        .map_err(|e| e.to_string())
}

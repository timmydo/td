//! Fixed s2idle helper, launched only by the prepared physical session.

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;

pub(crate) const COMPLETE: &[u8] = b"suspend returned\n";

pub(crate) fn run() -> Result<(), String> {
    crate::launch::require_operation_startup()?;
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .mode(0o600)
        .truncate(false)
        .open("/run/td-suspend.lock")
        .map_err(|error| format!("open suspend lock: {error}"))?;
    lock.try_lock()
        .map_err(|error| format!("another suspend is pending: {error}"))?;
    cycle(&mut Kernel)?;
    std::io::stdout()
        .write_all(COMPLETE)
        .map_err(|error| format!("report suspend: {error}"))
}

trait Power {
    fn read(&mut self, name: &str) -> Result<String, String>;
    fn write(&mut self, name: &str, bytes: &[u8]) -> Result<(), String>;
}

struct Kernel;
impl Power for Kernel {
    fn read(&mut self, name: &str) -> Result<String, String> {
        let mut text = String::new();
        File::open(format!("/sys/power/{name}"))
            .and_then(|file| file.take(257).read_to_string(&mut text))
            .map_err(|error| format!("read power {name}: {error}"))?;
        if text.len() > 256 {
            return Err(format!("power {name} exceeds bound"));
        }
        Ok(text)
    }

    fn write(&mut self, name: &str, bytes: &[u8]) -> Result<(), String> {
        let count = OpenOptions::new()
            .write(true)
            .open(format!("/sys/power/{name}"))
            .and_then(|mut file| file.write(bytes))
            .map_err(|error| format!("write power {name}: {error}"))?;
        if count != bytes.len() {
            return Err(format!("short power {name} write"));
        }
        Ok(())
    }
}

fn cycle(power: &mut impl Power) -> Result<(), String> {
    if !power
        .read("state")?
        .split_ascii_whitespace()
        .any(|s| s == "mem")
        || !power
            .read("mem_sleep")?
            .split_ascii_whitespace()
            .any(|s| matches!(s, "s2idle" | "[s2idle]"))
    {
        return Err("suspend-to-idle is unavailable".into());
    }
    power.write("mem_sleep", b"s2idle")?;
    let count = power.read("wakeup_count")?;
    let count = count.trim();
    if count.is_empty()
        || !count.bytes().all(|byte| byte.is_ascii_digit())
        || count.parse::<u64>().is_err()
    {
        return Err("invalid wakeup count".into());
    }
    // A wake between sampling and entry refuses the write; never retry it.
    power.write("wakeup_count", count.as_bytes())?;
    power.write("state", b"mem")
}

#[cfg(test)]
#[path = "../tests/suspend.rs"]
mod tests;

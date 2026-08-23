//! td-profiler continuously records system-wide user-mode CPU samples and
//! produces deterministic local reports suitable for offline human or agent
//! review. The normative contract is ../DESIGN.md.
#![deny(unsafe_code)]

mod collector;
mod cpuset;
mod event;
mod json;
mod perf;
mod raw;
mod report;
mod state;
mod symbol;
#[allow(unsafe_code)]
mod sys;

use std::env;
use std::ffi::{OsStr, OsString};
use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

fn main() -> ExitCode {
    match run(env::args_os().skip(1).collect()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            let _ = writeln!(std::io::stderr(), "td-profiler: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run(arguments: Vec<OsString>) -> Result<(), String> {
    let Some(command) = arguments.first().and_then(|value| value.to_str()) else {
        return Err(usage());
    };
    match command {
        "collect" => collector::run(parse_collect(arguments.get(1..).unwrap_or_default())?),
        "report" => report_command(arguments.get(1..).unwrap_or_default()),
        "probe" => probe(),
        "help" | "--help" | "-h" => {
            println!("{}", usage());
            Ok(())
        }
        _ => Err(usage()),
    }
}

fn parse_collect(arguments: &[OsString]) -> Result<collector::Config, String> {
    let (current_uid, current_gid) = current_credentials()?;
    let mut config = collector::Config {
        root: PathBuf::from(collector::DEFAULT_ROOT),
        index: Some(PathBuf::from(collector::DEFAULT_INDEX)),
        deployment: "unknown".into(),
        profiler_build: env!("CARGO_PKG_VERSION").into(),
        uid: current_uid,
        gid: current_gid,
        rate_hz: collector::DEFAULT_RATE_HZ,
        duration: Duration::from_secs(collector::DEFAULT_DURATION_SECS),
        once: false,
    };
    let mut at = 0usize;
    while at < arguments.len() {
        let option = arguments
            .get(at)
            .and_then(|value| value.to_str())
            .ok_or("collect option is not UTF-8")?;
        at = at.saturating_add(1);
        match option {
            "--once" => config.once = true,
            "--no-object-index" => config.index = None,
            "--root" => config.root = PathBuf::from(take(arguments, &mut at, option)?),
            "--object-index" => {
                config.index = Some(PathBuf::from(take(arguments, &mut at, option)?));
            }
            "--deployment" => config.deployment = text(take(arguments, &mut at, option)?, option)?,
            "--profiler-build" => {
                config.profiler_build = text(take(arguments, &mut at, option)?, option)?;
            }
            "--uid" => config.uid = number(take(arguments, &mut at, option)?, option)?,
            "--gid" => config.gid = number(take(arguments, &mut at, option)?, option)?,
            "--rate-hz" => config.rate_hz = number(take(arguments, &mut at, option)?, option)?,
            "--duration-secs" => {
                let seconds: u64 = number(take(arguments, &mut at, option)?, option)?;
                config.duration = Duration::from_secs(seconds);
            }
            "--duration-ms" => {
                let millis: u64 = number(take(arguments, &mut at, option)?, option)?;
                config.duration = Duration::from_millis(millis);
            }
            _ => return Err(format!("unknown collect option {option}\n{}", usage())),
        }
    }
    Ok(config)
}

fn report_command(arguments: &[OsString]) -> Result<(), String> {
    let capture = arguments.first().ok_or_else(usage).map(PathBuf::from)?;
    let (index, expected) = match arguments.get(1).and_then(|value| value.to_str()) {
        None => (Some(PathBuf::from(collector::DEFAULT_INDEX)), 1),
        Some("--object-index") => (
            Some(PathBuf::from(
                arguments.get(2).ok_or("--object-index requires a path")?,
            )),
            3,
        ),
        Some("--no-object-index") => (None, 2),
        Some(other) => return Err(format!("unknown report option {other}")),
    };
    if arguments.len() != expected {
        return Err("report received extra arguments".into());
    }
    report::regenerate(&capture, index.as_deref())
}

fn probe() -> Result<(), String> {
    let online = std::fs::read_to_string("/sys/devices/system/cpu/online")
        .map_err(|e| format!("read online CPU mask: {e}"))?;
    let cpu = cpuset::parse(&online)?
        .into_iter()
        .next()
        .ok_or("online CPU mask is empty")?;
    match sys::CpuEvents::open(cpu, collector::DEFAULT_RATE_HZ, 1) {
        Ok(events) => {
            println!(
                "td-profiler-supported cpu={} metadata-id={} sample-id={}",
                events.cpu(),
                events.metadata_id,
                events.sample_id
            );
            Ok(())
        }
        Err(error) if matches!(error.raw_os_error(), Some(1 | 13)) => Err(format!(
            "unsupported-permission: perf_event_open was denied ({error})"
        )),
        Err(error) => Err(format!("unsupported-perf-abi: {error}")),
    }
}

fn take<'a>(arguments: &'a [OsString], at: &mut usize, option: &str) -> Result<&'a OsStr, String> {
    let value = arguments
        .get(*at)
        .ok_or_else(|| format!("{option} requires a value"))?;
    *at = at.saturating_add(1);
    Ok(value)
}

fn text(value: &OsStr, option: &str) -> Result<String, String> {
    value
        .to_str()
        .map(str::to_string)
        .ok_or_else(|| format!("{option} value is not UTF-8"))
}

fn number<T: std::str::FromStr>(value: &OsStr, option: &str) -> Result<T, String> {
    value
        .to_str()
        .ok_or_else(|| format!("{option} value is not UTF-8"))?
        .parse()
        .map_err(|_| format!("{option} value is not a valid number"))
}

fn current_credentials() -> Result<(u32, u32), String> {
    let status = std::fs::read_to_string("/proc/self/status")
        .map_err(|e| format!("read current credentials: {e}"))?;
    let uid = credential(&status, "Uid:")?;
    let gid = credential(&status, "Gid:")?;
    Ok((uid, gid))
}

fn credential(status: &str, label: &str) -> Result<u32, String> {
    status
        .lines()
        .find_map(|line| {
            line.strip_prefix(label)
                .and_then(|rest| rest.split_ascii_whitespace().next())
                .and_then(|value| value.parse().ok())
        })
        .ok_or_else(|| format!("/proc/self/status has no valid {label} line"))
}

fn usage() -> String {
    "usage: td-profiler collect [--root PATH] [--object-index PATH|--no-object-index] \
     [--deployment ID] [--profiler-build PATH] [--uid N --gid N] [--rate-hz N] \
     [--duration-secs N|--duration-ms N] [--once]\n       td-profiler report ABSOLUTE-CAPTURE \
     [--object-index PATH|--no-object-index]  # maintenance writer; writes \
     CAPTURE/regenerated\n       td-profiler probe"
        .into()
}

#[cfg(test)]
mod confinement {
    #![allow(clippy::unwrap_used)]

    const MAIN: &str = include_str!("main.rs");
    const COLLECTOR: &str = include_str!("collector.rs");
    const SYS: &str = include_str!("sys.rs");

    #[test]
    fn unsafe_is_confined_to_the_one_raw_linux_module() {
        let shipped = MAIN.split_once("#[cfg(test)]").unwrap().0;
        assert!(shipped.contains("#![deny(unsafe_code)]"));
        assert_eq!(shipped.matches("#[allow(unsafe_code)]").count(), 1);
        assert_eq!(shipped.matches("mod sys;").count(), 1);
        assert!(!COLLECTOR.contains("unsafe"));
        assert_eq!(SYS.matches("core::arch::asm!").count(), 1);
        assert_eq!(SYS.matches("\"syscall\"").count(), 1);
    }

    #[test]
    fn syscall_and_ioctl_rosters_are_exact() {
        for pin in [
            "const SYS_CLOSE: usize = 3;",
            "const SYS_MMAP: usize = 9;",
            "const SYS_MUNMAP: usize = 11;",
            "const SYS_IOCTL: usize = 16;",
            "const SYS_SETUID: usize = 105;",
            "const SYS_SETGID: usize = 106;",
            "const SYS_SETGROUPS: usize = 116;",
            "const SYS_CLOCK_GETTIME: usize = 228;",
            "const SYS_PERF_EVENT_OPEN: usize = 298;",
        ] {
            assert!(SYS.contains(pin), "missing syscall pin {pin}");
        }
        assert_eq!(SYS.matches("const SYS_").count(), 9);
        for request in [
            "const PERF_EVENT_IOC_ENABLE: usize = 0x2400;",
            "const PERF_EVENT_IOC_DISABLE: usize = 0x2401;",
            "const PERF_EVENT_IOC_SET_OUTPUT: usize = 0x2405;",
            "const PERF_EVENT_IOC_ID: usize = 0x8008_2407;",
        ] {
            assert!(SYS.contains(request), "missing ioctl pin {request}");
        }
        assert_eq!(SYS.matches("const PERF_EVENT_IOC_").count(), 4);
    }

    #[test]
    fn collector_has_no_socket_or_export_surface() {
        let shipped = MAIN.split_once("#[cfg(test)]").unwrap().0;
        for forbidden in [
            "TcpStream",
            "UnixStream",
            "bind(",
            "listen(",
            "sendmsg",
            "recvmsg",
        ] {
            assert!(!shipped.contains(forbidden));
            assert!(!COLLECTOR.contains(forbidden));
            assert!(!SYS.contains(forbidden));
        }
    }
}

#![forbid(unsafe_code)]
mod window;
use std::io::{self, Write};
use std::process::ExitCode;
use std::sync::atomic::AtomicBool;
use td_taskmgr::budget::{Budget, LIMIT};
use td_taskmgr::collector::Collector;
use td_taskmgr::devices::{default_disk, default_network};
use td_taskmgr::history::Interval;
use td_taskmgr::model::{Admission, Model};
use td_taskmgr::worker::Update;
fn unavailable(value: Option<u64>) -> String {
    value
        .map(|value| value.to_string())
        .unwrap_or_else(|| "Unavailable".into())
}
fn run() -> io::Result<()> {
    let mut args = std::env::args().skip(1);
    let command = args.next();
    if command.is_none() {
        return window::open(None);
    }
    if command.as_deref() == Some("--control-socket") {
        let path = args
            .next()
            .ok_or_else(|| io::Error::other("--control-socket requires an absolute path"))?;
        if args.next().is_some() {
            return Err(io::Error::other("unexpected argument"));
        }
        return window::open(Some(std::path::PathBuf::from(path)));
    }
    if command.as_deref() == Some("--font-license") {
        if args.next().is_some() {
            return Err(io::Error::other("unexpected argument"));
        }
        return writeln!(
            io::stdout().lock(),
            "{}\n{}\n{}",
            td_ui::notices::FONT_PROVENANCE,
            td_ui::notices::FONT_COPYING,
            td_ui::notices::FONT_LICENSE
        );
    }
    if command.as_deref() == Some("--preview") {
        let size = args.next().unwrap_or_else(|| "1280x960".into());
        let (width, height) = size
            .split_once('x')
            .and_then(|(w, h)| Some((w.parse::<usize>().ok()?, h.parse::<usize>().ok()?)))
            .ok_or_else(|| io::Error::other("preview expects WIDTHxHEIGHT"))?;
        if args.next().is_some() {
            return Err(io::Error::other("unexpected preview argument"));
        }
        return window::preview(width, height);
    }
    if command
        .as_deref()
        .is_none_or(|arg| matches!(arg, "--help" | "-h"))
    {
        if args.next().is_some() {
            return Err(io::Error::other("unexpected argument"));
        }
        return writeln!(io::stdout().lock(), "td-taskmgr [--control-socket ABSOLUTE-PATH]\n  Open the Wayland task manager with confirmed process signal actions.\n\ntd-taskmgr --sample [COUNT] [--interval 0.5|1|2|5]\n  Print bounded resource snapshots; COUNT defaults to 2, maximum 240.\ntd-taskmgr --preview [WIDTHxHEIGHT]\n  Write a PPM preview of live observations (default 1280x960).\ntd-taskmgr --font-license\n  Print embedded font notices.\n\nTab/Shift+Tab: focus controls. Arrows: navigate. Ctrl+F: search.\nCtrl+L: return to Live. Ctrl+I: interval. Ctrl+Q: quit.\nSelect a process to filter its CPU/RSS plot; Ctrl+A outside search compares all.\nCPU/RSS headings rank all processes; Process restores the tree.\nGraphs: arrows inspect; Ctrl+Tab changes plot; Page Up/Down scroll.\nDevice list: Space toggles a member. Tree: Left/Right collapse/expand.\nDouble-click a process or press Enter for CPU/RSS history and its subtree.\nIn details: Escape returns; Alt+Up opens the parent. CPU time is lifetime user+kernel time.\nF10/Shift+F10: process actions. Right-click: selected row context menu.\nEvery signal requires confirmation; Cancel is initially focused.\nThe optional local socket uses the td-ui driven protocol.");
    }
    if command.as_deref() != Some("--sample") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "unknown argument; use --help",
        ));
    }
    let mut count = 2;
    let mut interval = Interval::Second;
    if let Some(value) = args.next() {
        if value == "--interval" {
            interval = parse_interval(args.next().as_deref())?;
        } else {
            count = value
                .parse::<u32>()
                .ok()
                .filter(|count| *count > 0 && *count <= 240)
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "sample count must be 1 through 240",
                    )
                })?;
            if let Some(flag) = args.next() {
                if flag != "--interval" {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "expected --interval",
                    ));
                }
                interval = parse_interval(args.next().as_deref())?;
            }
        }
    }
    if args.next().is_some() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "unexpected argument",
        ));
    }
    let budget = Budget::new(LIMIT).map_err(io::Error::other)?;
    let mut model = Model::new(&budget, interval)?;
    let mut collector = Collector::new(&budget, 1)?;
    let cancel = AtomicBool::new(false);
    let mut out = io::stdout().lock();
    for index in 0..count {
        if index > 0 {
            std::thread::sleep(std::time::Duration::from_nanos(interval.nanoseconds()));
        }
        let batch = collector.sample(&cancel)?;
        model.receive(Update {
            batch: Some(batch),
            skipped: 0,
            failure: None,
        });
        loop {
            match model.admit_pending() {
                Admission::Admitted(_) => break,
                Admission::Reclaimed => continue,
                admission => return Err(io::Error::other(admission_error(admission))),
            }
        }
        let sample = &model
            .history()
            .selected()
            .ok_or_else(|| io::Error::other("missing retained sample"))?
            .value;
        writeln!(out,"sample {}: processes={}{} cpu_busy_basis_points={} memory_used_bytes={} model_bytes={}",index+1,sample.processes.processes().len(),if sample.coverage.partial(){" (partial)"}else{""},unavailable(sample.cpu.map(|cpu|cpu.busy)),unavailable(sample.memory.and_then(|memory|memory.used())),budget.used())?;
        if let Some(devices) = &sample.devices {
            if let Some(network) =
                default_network(&devices.networks).and_then(|index| devices.networks.get(index))
            {
                writeln!(
                    out,
                    "network {}: receive_bytes_per_second={} send_bytes_per_second={}",
                    network.name.as_str(),
                    unavailable(network.receive_rate),
                    unavailable(network.send_rate)
                )?;
            } else {
                writeln!(out, "network: Unavailable")?;
            }
            if let Some(disk) =
                default_disk(&devices.disks).and_then(|index| devices.disks.get(index))
            {
                writeln!(
                    out,
                    "disk {} ({}:{}): read_bytes_per_second={} write_bytes_per_second={}",
                    disk.name.as_str(),
                    disk.major,
                    disk.minor,
                    unavailable(disk.read_rate),
                    unavailable(disk.write_rate)
                )?;
            } else {
                writeln!(out, "disk: Unavailable")?;
            }
        }
    }
    Ok(())
}
fn parse_interval(value: Option<&str>) -> io::Result<Interval> {
    match value {
        Some("0.5") => Ok(Interval::HalfSecond),
        Some("1") => Ok(Interval::Second),
        Some("2") => Ok(Interval::TwoSeconds),
        Some("5") => Ok(Interval::FiveSeconds),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "interval must be 0.5, 1, 2, or 5 seconds",
        )),
    }
}
fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            let _ = writeln!(io::stderr().lock(), "td-taskmgr: {error}");
            ExitCode::FAILURE
        }
    }
}

fn admission_error(admission: Admission) -> &'static str {
    match admission {
        Admission::Blocked => "could not retain the resource snapshot within its budget",
        Admission::Rejected => "resource observation failed model validation",
        _ => "resource observation was not admitted",
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn admission_diagnostics_distinguish_validation_and_budget_failures() {
        assert_eq!(
            admission_error(Admission::Rejected),
            "resource observation failed model validation"
        );
        assert_eq!(
            admission_error(Admission::Blocked),
            "could not retain the resource snapshot within its budget"
        );
    }
}

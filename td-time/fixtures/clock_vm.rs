//! Disposable clock/RTC oracle. Only its guest starts clock-changing helpers.
#![forbid(unsafe_code)]
use std::fs::{self, File};
use std::io::Write;
use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::Path;
use std::process::{Child, Command, ExitCode, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[path = "../../engine/src/cpio.rs"]
#[allow(dead_code, reason = "oracle uses only the archive writer")]
mod cpio;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

struct Process(Child);
impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn run(program: &str, args: &[&str]) -> Result<String> {
    let output = Command::new(program)
        .args(args)
        .stdin(Stdio::inherit())
        .output()?;
    if !output.status.success() {
        return Err(format!(
            "{program} {args:?}: {} {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        )
        .into());
    }
    Ok(String::from_utf8(output.stdout)?)
}

fn apply(action: &str, value: &str) -> Result<()> {
    run("/time-real", &["apply", action, value])?;
    Ok(())
}

fn utc() -> Result<u64> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs())
}

fn until(mut condition: impl FnMut() -> Result<bool>) -> Result<()> {
    let end = Instant::now() + Duration::from_secs(10);
    loop {
        if condition()? {
            return Ok(());
        }
        if Instant::now() >= end {
            return Err("guest condition timed out".into());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn service() -> Result<Process> {
    Ok(Process(
        Command::new("/time-real")
            .arg("serve")
            .stdin(Stdio::null())
            .spawn()?,
    ))
}

fn guest() -> Result<()> {
    for name in ["/dev", "/proc", "/sys", "/run", "/var/lib/td"] {
        fs::create_dir_all(name)?;
    }
    for path in ["/", "/var", "/var/lib", "/var/lib/td"] {
        fs::set_permissions(path, fs::Permissions::from_mode(0o755))?;
    }
    for (kind, path) in [("devtmpfs", "/dev"), ("proc", "/proc"), ("sysfs", "/sys")] {
        run("/td-init", &["mount", "-t", kind, kind, path])?;
    }
    let wrong = td_time::request::parse_utc("2026-10-29T23:11:00Z")?;
    let correct = wrong + 7 * 3600;
    if utc()?.abs_diff(wrong) > 30 {
        return Err("VM did not start seven hours behind".into());
    }
    apply("automatic", "off")?;
    fs::write("/query-mode", "fail")?;
    let mut daemon = service()?;
    std::thread::sleep(Duration::from_millis(1200));
    if utc()?.abs_diff(wrong) > 30 {
        return Err("disabled service moved the clock".into());
    }
    apply("automatic", "on")?;
    until(|| Ok(run("/time-real", &["status"])?.contains("network time unavailable")))?;
    if utc()?.abs_diff(wrong) > 30 {
        return Err("offline service moved the clock".into());
    }
    apply("automatic", "off")?;
    std::thread::sleep(Duration::from_millis(1200));
    fs::write("/query-mode", "ok")?;
    apply("automatic", "on")?;
    until(|| Ok(utc()?.abs_diff(correct) < 30))?;
    let report = run("/time-real", &["status"])?;
    if !report.contains("RTC saved in UTC") || !report.contains("Last sync age:") {
        return Err(format!("missing successful RTC report: {report}").into());
    }
    apply("automatic", "off")?;
    apply("set", "2026-10-30T07:00:00Z")?;
    let manual = td_time::request::parse_utc("2026-10-30T07:00:00Z")?;
    if utc()?.abs_diff(manual) > 2 {
        return Err("manual UTC was not applied".into());
    }
    let before = utc()?;
    apply("timezone", "America/Los_Angeles")?;
    if utc()?.abs_diff(before) > 2 {
        return Err("timezone changed UTC".into());
    }
    if fs::read("/etc/localtime")? != fs::read("/etc/zoneinfo/America/Los_Angeles")? {
        return Err("timezone data was not published".into());
    }
    daemon.0.kill()?;
    daemon.0.wait()?;
    // Model interruption after the canonical name was committed.
    fs::write("/var/lib/td/localtime", "interrupted")?;
    let mut restarted = service()?;
    until(|| Ok(fs::read("/etc/localtime")? == fs::read("/etc/zoneinfo/America/Los_Angeles")?))?;
    let report = run("/time-real", &["status"])?;
    if !report.contains("Automatic sync: off") || !report.contains("Timezone: America/Los_Angeles")
    {
        return Err("settings did not survive service restart".into());
    }
    let rtc = format!(
        "{}T{}Z",
        fs::read_to_string("/sys/class/rtc/rtc0/date")?.trim(),
        fs::read_to_string("/sys/class/rtc/rtc0/time")?.trim()
    );
    if td_time::request::parse_utc(&rtc)?.abs_diff(utc()?) > 2 {
        return Err("RTC no longer agrees with UTC after restart".into());
    }
    restarted.0.kill()?;
    restarted.0.wait()?;
    println!("TD-TIME-CLOCK-RTC-PASS");
    Ok(())
}

// The production service's fixed query child is replaced only inside this
// oracle archive. Packet validation is tested independently by the library.
fn query_fixture() -> Result<()> {
    if fs::read_to_string("/query-mode")? != "ok" {
        return Err("offline fixture".into());
    }
    let uptime = fs::read_to_string("/proc/uptime")?;
    let (seconds, hundredths) = uptime
        .split_whitespace()
        .next()
        .ok_or("uptime")?
        .split_once('.')
        .ok_or("uptime fraction")?;
    let millis = seconds.parse::<u64>()? * 1000 + hundredths.parse::<u64>()? * 10;
    let base = td_time::request::parse_utc("2026-10-30T06:11:00Z")? * 1000;
    writeln!(std::io::stdout(), "{} {millis}", base + millis)?;
    Ok(())
}

fn host(args: &[String]) -> Result<()> {
    let [kernel, init, binary, zones, destination] = args else {
        return Err("usage: clock-vm KERNEL TD-INIT TD-TIME ZONEINFO NEW-LOG".into());
    };
    if Path::new(destination).exists() {
        return Err("log already exists".into());
    }
    let scratch = td_fs::TempDir::new("td-time-clock-vm")?;
    let helper = fs::read(std::env::current_exe()?)?;
    let init = fs::read(init)?;
    let binary = fs::read(binary)?;
    let la = fs::read(Path::new(zones).join("America/Los_Angeles"))?;
    let mut entries = Vec::new();
    for name in ["dev", "bin", "etc", "etc/zoneinfo", "etc/zoneinfo/America"] {
        entries.push(cpio::Entry {
            name,
            mode: 0o755,
            kind: cpio::Kind::Directory,
        });
    }
    for (name, bytes) in [
        ("init", &helper),
        ("bin/td-time", &helper),
        ("time-real", &binary),
        ("td-init", &init),
    ] {
        entries.push(cpio::Entry {
            name,
            mode: 0o755,
            kind: cpio::Kind::File(bytes),
        });
    }
    entries.push(cpio::Entry {
        name: "etc/zoneinfo/America/Los_Angeles",
        mode: 0o644,
        kind: cpio::Kind::File(&la),
    });
    let archive = scratch.path().join("initramfs");
    fs::write(&archive, cpio::build(&entries)?)?;
    let log = scratch.path().join("guest.log");
    let mut child = Process(
        Command::new("qemu-system-x86_64")
            .args([
                "-enable-kvm",
                "-m",
                "256",
                "-nographic",
                "-no-reboot",
                "-nic",
                "none",
                "-rtc",
                "base=2026-10-29T23:11:00,clock=vm",
                "-kernel",
            ])
            .arg(kernel)
            .arg("-initrd")
            .arg(archive)
            .args(["-append", "console=ttyS0 panic=-1"])
            .stdin(Stdio::null())
            .stdout(File::create(&log)?)
            .stderr(File::options().append(true).open(&log)?)
            .spawn()?,
    );
    let deadline = Instant::now() + Duration::from_secs(60);
    let status = loop {
        if let Some(status) = child.0.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            fs::copy(&log, destination)?;
            return Err(format!("QEMU timed out; see {destination}").into());
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    let bytes = fs::read(&log)?;
    fs::write(destination, &bytes)?;
    if !status.success()
        || !String::from_utf8_lossy(&bytes)
            .lines()
            .any(|line| line.trim() == "TD-TIME-CLOCK-RTC-PASS")
    {
        return Err(format!("clock oracle failed; see {destination}").into());
    }
    scratch.cleanup()?;
    Ok(())
}

fn main() -> ExitCode {
    let args: Vec<_> = std::env::args().skip(1).collect();
    let result = if args == ["query-worker"] {
        query_fixture()
    } else if std::process::id() == 1 {
        let links = symlink("/var/lib/td/timezone", "/etc/timezone")
            .and_then(|()| symlink("/var/lib/td/localtime", "/etc/localtime"));
        let result = links.map_err(Into::into).and_then(|()| guest());
        if let Err(error) = &result {
            eprintln!("TD-TIME-CLOCK-RTC-FAIL: {error}");
        }
        let _ = run("/td-init", &["poweroff", "-f"]);
        result
    } else {
        host(&args)
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}

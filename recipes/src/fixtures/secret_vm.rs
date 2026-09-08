#![deny(unsafe_code)]

use std::fs::{self, File};
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Stdio};

pub const CASES: &[(&str, &str)] = &[
    (
        "intake",
        "secret_intake::tests::root_public_client_uses_the_human_identity_and_immutable_descriptor",
    ),
    (
        "prepare",
        "session::tests::root_session_preparation_failure_and_generation_exit_relock",
    ),
    (
        "inspect",
        "session::tests::root_inspection_observes_file_state_without_publishing_or_repairing",
    ),
    (
        "supervise",
        "unlock::tests::root_supervisor_relocks_after_the_production_worker_refuses",
    ),
];
pub const PASS: &str = "TD-SECRET-VM-PASS";
pub const FAIL: &str = "TD-SECRET-VM-FAIL";

pub fn test_passed(success: bool, output: &str) -> bool {
    success
        && output
            .lines()
            .rfind(|line| line.starts_with("test result: "))
            .is_some_and(|line| line.starts_with("test result: ok. 1 passed; 0 failed; 0 ignored;"))
}

fn applet(args: &[&str]) -> Result<(), String> {
    let status = Command::new("/bin/td-init")
        .args(args)
        .status()
        .map_err(|e| format!("td-init {args:?}: {e}"))?;
    if !status.success() {
        return Err(format!("td-init {args:?}: {status}"));
    }
    Ok(())
}

fn run() -> Result<(), String> {
    if std::process::id() != 1 {
        return Err("fixture must be guest PID 1".into());
    }
    for dir in ["/proc", "/sys", "/dev", "/run", "/tmp"] {
        fs::create_dir_all(dir).map_err(|e| format!("create {dir}: {e}"))?;
    }
    for (kind, path) in [
        ("proc", "/proc"),
        ("sysfs", "/sys"),
        ("devtmpfs", "/dev"),
        ("tmpfs", "/run"),
    ] {
        applet(&["mount", "-t", kind, kind, path])?;
    }
    // Authority configuration needs protected ancestors, including initramfs root.
    for path in ["/", "/run"] {
        fs::set_permissions(path, fs::Permissions::from_mode(0o755))
            .map_err(|e| format!("chmod {path}: {e}"))?;
    }
    fs::set_permissions("/tmp", fs::Permissions::from_mode(0o1777))
        .map_err(|e| format!("chmod /tmp: {e}"))?;
    let selected = fs::read_to_string("/case").map_err(|e| format!("read case: {e}"))?;
    let (_, test) = CASES
        .iter()
        .find(|(name, _)| *name == selected)
        .ok_or_else(|| "unknown VM case".to_string())?;
    let log = File::create("/run/test.log").map_err(|e| format!("test log: {e}"))?;
    let errors = log
        .try_clone()
        .map_err(|e| format!("clone test log: {e}"))?;
    let status = Command::new("/bin/td-authd-tests")
        .args(["--exact", test, "--ignored", "--test-threads=1"])
        .env_clear()
        .current_dir("/")
        .stdin(Stdio::null())
        .stdout(log)
        .stderr(errors)
        .status()
        .map_err(|e| format!("run {test}: {e}"))?;
    let mut bytes = Vec::new();
    File::open("/run/test.log")
        .map_err(|e| format!("read test log: {e}"))?
        .take(1_048_577)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("read test log: {e}"))?;
    print!("{}", String::from_utf8_lossy(&bytes));
    if bytes.len() > 1_048_576 {
        return Err("test log exceeded 1 MiB".into());
    }
    let output = std::str::from_utf8(&bytes).map_err(|e| format!("decode test log: {e}"))?;
    if !test_passed(status.success(), output) {
        return Err(format!("{test} did not pass exactly one test: {status}"));
    }
    println!("{PASS}");
    Ok(())
}

fn main() -> std::process::ExitCode {
    if std::process::id() != 1 {
        eprintln!("{FAIL}: fixture must be guest PID 1");
        return std::process::ExitCode::FAILURE;
    }
    if let Err(error) = run() {
        eprintln!("{FAIL}: {error}");
    }
    if let Err(error) = applet(&["poweroff", "-f"]) {
        eprintln!("{FAIL}: {error}");
    }
    loop {
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
}

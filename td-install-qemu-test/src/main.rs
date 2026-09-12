//! Test-only native init for an oracle-owned VM; never packed in a system image.
#![forbid(unsafe_code)]
use std::fs::{self, File};
use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, FileTypeExt};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::Duration;

mod protocol;
use protocol::*;

fn command(program: &str, args: &[&str]) -> Result<(), String> {
    let status = Command::new(program)
        .args(args)
        .status()
        .map_err(|error| format!("execute {program}: {error}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{program} failed: {status}"))
    }
}

fn applet(args: &[&str]) -> Result<(), String> {
    command("/bin/td-init", args)
}

fn read(path: &Path, limit: u64) -> Result<Vec<u8>, String> {
    let file = File::open(path).map_err(|error| format!("open {}: {error}", path.display()))?;
    read_file(file, path, limit)
}

fn read_file(file: File, path: &Path, limit: u64) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    file.take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("read {}: {error}", path.display()))?;
    if bytes.len() as u64 > limit {
        return Err(format!("{} exceeds {limit} bytes", path.display()));
    }
    Ok(bytes)
}

fn target() -> Result<String, String> {
    let mut matched = None;
    for entry in
        fs::read_dir("/sys/class/block").map_err(|error| format!("list block devices: {error}"))?
    {
        let entry = entry.map_err(|error| format!("read block device entry: {error}"))?;
        let name = entry.file_name();
        let name = name.to_str().ok_or("non-UTF-8 block device name")?;
        // The oracle attaches only virtio targets; exclude partitions and paths.
        if !name.starts_with("vd") || !name.bytes().all(|byte| byte.is_ascii_lowercase()) {
            continue;
        }
        let serial = entry.path().join("serial");
        let serial = read(&serial, 128)?;
        if serial.strip_suffix(b"\n").unwrap_or(&serial) != TARGET_SERIAL.as_bytes() {
            continue;
        }
        if matched.is_some() {
            return Err("ambiguous oracle target serial".into());
        }
        matched = Some(format!("/dev/{name}"));
    }
    let path = matched.ok_or("oracle target serial is absent")?;
    if !fs::metadata(&path)
        .map_err(|error| format!("stat {path}: {error}"))?
        .file_type()
        .is_block_device()
    {
        return Err("oracle target is not a block device".into());
    }
    Ok(path)
}

fn directories() -> Result<(), String> {
    for path in [
        "/dev",
        "/run",
        "/proc",
        "/sys",
        "/scratch",
        "/volume",
        "/state",
        "/root-image",
        "/ack",
    ] {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(path)
            .map_err(|error| format!("create {path}: {error}"))?;
    }
    applet(&["mount", "-t", "devtmpfs", "dev", "/dev"])?;
    applet(&["mount", "-t", "proc", "proc", "/proc"])?;
    applet(&["mount", "-t", "sysfs", "sysfs", "/sys"])
}

fn install(device: &str) -> Result<(), String> {
    // Every path is fixture-owned. The ISO already contains the signed source
    // and public trust root; no private key enters the guest.
    command(
        "/bin/td-install",
        &["layout", device, "/source/bzImage", "/selector.cpio"],
    )?;
    command(
        "/bin/td-install",
        &[
            "volume",
            device,
            "/bin/mkfs.btrfs",
            "/scratch",
            "/bin/td-boot",
            "/source",
            "/trusted.pub",
        ],
    )?;
    applet(&["sync"])?;
    writeln!(std::io::stdout(), "{INSTALL_MARKER}").map_err(|error| error.to_string())
}

fn selector(device: &str) -> Result<(), String> {
    let cmdline = String::from_utf8(read(Path::new("/proc/cmdline"), 2048)?)
        .map_err(|_| "non-UTF-8 command line")?;
    command(
        "/bin/td-boot",
        &["boot", &format!("{device}2"), "/volume", cmdline.trim_end()],
    )
}

fn installed(device: &str) -> Result<(), String> {
    let cmdline = String::from_utf8(read(Path::new("/proc/cmdline"), 2048)?)
        .map_err(|_| "non-UTF-8 command line")?;
    let ids: Vec<&str> = cmdline
        .split_ascii_whitespace()
        .filter_map(|word| word.strip_prefix("td.deployment="))
        .collect();
    let [id] = ids.as_slice() else {
        return Err("missing or duplicate selected deployment".into());
    };
    let partition = format!("{device}2");
    applet(&[
        "mount",
        "-t",
        "btrfs",
        "-o",
        "ro,nodev,nosuid,noexec",
        &partition,
        "/volume",
    ])?;
    if !Path::new("/dev/loop0").exists() {
        applet(&["mknod", "/dev/loop0", "b", "7", "0"])?;
    }
    command("/bin/td-boot", &["root-loop", "/volume", id, "/dev/loop0"])?;
    applet(&[
        "mount",
        "-t",
        "erofs",
        "-o",
        "ro",
        "/dev/loop0",
        "/root-image",
    ])?;
    if read(Path::new("/root-image/installed.txt"), 128)? != b"td installation fixture\n" {
        return Err("wrong installed EROFS payload".into());
    }
    applet(&[
        "mount",
        "-t",
        "btrfs",
        "-o",
        "rw,nodev,nosuid,subvol=@var",
        &partition,
        "/state",
    ])?;
    let path = PathBuf::from("/state/installation-count");
    let count = match File::open(&path) {
        Ok(file) => match read_file(file, &path, 8)?.as_slice() {
            b"1\n" => 2,
            other => return Err(format!("unexpected persisted count: {other:?}")),
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => 1,
        Err(error) => return Err(format!("open {}: {error}", path.display())),
    };
    let mut file =
        File::create(&path).map_err(|error| format!("create {}: {error}", path.display()))?;
    writeln!(file, "{count}")
        .and_then(|()| file.sync_all())
        .map_err(|error| error.to_string())?;
    applet(&["sync"])?;
    command("/bin/td-boot", &["success", &partition, "/ack", id])?;
    let marker = if count == 1 {
        FIRST_BOOT_MARKER
    } else {
        SECOND_BOOT_MARKER
    };
    writeln!(std::io::stdout(), "{marker} {id}").map_err(|error| error.to_string())
}

fn run() -> Result<(), String> {
    if std::process::id() != 1 {
        return Err("installation fixture must be guest PID 1".into());
    }
    directories()?;
    let device = target()?;
    match read(Path::new("/fixture-phase"), 32)?.as_slice() {
        b"install\n" => install(&device),
        b"selector\n" => selector(&device),
        b"installed\n" => installed(&device),
        _ => Err("invalid fixture phase".into()),
    }
}

fn main() -> ExitCode {
    let result = run();
    if let Err(error) = result {
        let _ = writeln!(std::io::stderr(), "{REFUSED_PREFIX} {error}");
        if std::process::id() != 1 {
            return ExitCode::FAILURE;
        }
    }
    // Keep PID 1 alive after either outcome. The host owns a bounded VM lifetime.
    loop {
        std::thread::sleep(Duration::from_secs(1));
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn refuses_a_normal_process_before_accessing_devices() {
        assert_ne!(std::process::id(), 1);
        assert_eq!(
            super::run(),
            Err("installation fixture must be guest PID 1".into())
        );
    }
}

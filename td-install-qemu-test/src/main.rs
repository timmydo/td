//! Test-only native init for an oracle-owned VM; never packed in a system image.
#![forbid(unsafe_code)]
use std::fs::{self, File};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::{Duration, Instant};

mod protocol;
use protocol::*;

const ENOMEDIUM: i32 = 123; // Linux: an optical drive with no readable medium.

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
        "/media",
        "/source",
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

fn media_device() -> Result<&'static str, String> {
    let started = Instant::now();
    loop {
        let mut found = None;
        // Exactly one source in this fixed QEMU profile: SATA optical or USB.
        // USB mass-storage probing can finish after PID 1 starts.
        for path in ["/dev/sr0", "/dev/sda"] {
            match fs::metadata(path) {
                Ok(meta) if meta.file_type().is_block_device() => {
                    // sr can publish a placeholder capacity for an empty tray.
                    // Opening read-only asks the block driver to check media.
                    let _medium = match File::open(path) {
                        Ok(file) => file,
                        Err(error) if error.raw_os_error() == Some(ENOMEDIUM) => continue,
                        Err(error) => return Err(format!("open media {path}: {error}")),
                    };
                    let name = path.strip_prefix("/dev/").ok_or("invalid fixture device")?;
                    let capacity = read(Path::new(&format!("/sys/class/block/{name}/size")), 32)?;
                    let capacity = std::str::from_utf8(&capacity)
                        .map_err(|_| format!("non-ASCII capacity for {path}"))?
                        .trim_end()
                        .parse::<u64>()
                        .map_err(|error| format!("invalid capacity for {path}: {error}"))?;
                    if capacity == 0 {
                        continue;
                    }
                    if found.replace(path).is_some() {
                        return Err("ambiguous fixture installation media".into());
                    }
                }
                Ok(_) => return Err(format!("{path} is not a block device")),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(format!("stat {path}: {error}")),
            }
        }
        if let Some(path) = found {
            return Ok(path);
        }
        if started.elapsed() >= Duration::from_secs(30) {
            return Err("fixture installation media did not appear".into());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn mount_source() -> Result<(), String> {
    let device = media_device()?;
    applet(&[
        "mount",
        "-t",
        "iso9660",
        "-o",
        "ro,nodev,nosuid,noexec,map=normal",
        device,
        "/media",
    ])?;
    for (iso_name, name) in MEDIA_FILES {
        let source = format!("/media/{}", iso_name.to_ascii_lowercase());
        let destination = format!("/{name}");
        // File binds adapt ISO spelling without duplicating payloads into RAM
        // or passing symlinks to td-boot's real-file verifier.
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&destination)
            .map_err(|error| format!("create {destination}: {error}"))?;
        applet(&["mount", "-o", "bind", &source, &destination])?;
        applet(&[
            "mount",
            "-o",
            "remount,bind,ro,nodev,nosuid,noexec",
            &destination,
        ])?;
        match fs::OpenOptions::new().write(true).open(&destination) {
            Err(error) if error.kind() == std::io::ErrorKind::ReadOnlyFilesystem => {}
            Err(error) => return Err(format!("check read-only {destination}: {error}")),
            Ok(_) => return Err(format!("media payload is writable: {destination}")),
        }
    }
    writeln!(std::io::stdout(), "{MEDIA_MARKER} {device}").map_err(|error| error.to_string())
}

fn configured_uuid() -> Result<String, String> {
    let bytes = read(Path::new("/etc/td/volume-uuid"), 37)?;
    let text = std::str::from_utf8(&bytes).map_err(|_| "non-ASCII configured volume UUID")?;
    text.strip_suffix('\n')
        .map(str::to_owned)
        .ok_or_else(|| "configured volume UUID lacks its newline".into())
}

fn install(device: &str) -> Result<(), String> {
    // The ISO carries the signed payloads and the live initramfs's public key.
    // Every path is fixture-owned; no private key enters the guest.
    mount_source()?;
    let uuid = configured_uuid()?;
    command(
        "/bin/td-install",
        &["layout", device, "/source/bzImage", "/selector.cpio"],
    )?;
    command(
        "/bin/td-install",
        &[
            "volume",
            "--uuid",
            &uuid,
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

fn volume(uuid: &str) -> Result<(String, String), String> {
    let mut cmd = Command::new("/bin/td-boot");
    cmd.arg("volume").arg(uuid);
    let output = cmd
        .output()
        .map_err(|error| format!("resolve volume: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "volume resolution failed: {}",
            String::from_utf8_lossy(&output.stderr).trim_end()
        ));
    }
    let text = String::from_utf8(output.stdout).map_err(|_| "non-ASCII volume result")?;
    let fields: Vec<_> = text.split_ascii_whitespace().collect();
    let [found, path] = fields.as_slice() else {
        return Err("invalid volume result".into());
    };
    if uuid != *found {
        return Err("resolved wrong volume UUID".into());
    }
    writeln!(std::io::stdout(), "TD-INSTALL-VOLUME {found} {path}")
        .map_err(|error| error.to_string())?;
    Ok(((*found).into(), (*path).into()))
}

fn selector() -> Result<(), String> {
    let cmdline = String::from_utf8(read(Path::new("/proc/cmdline"), 2048)?)
        .map_err(|_| "non-UTF-8 command line")?;
    let uuid = configured_uuid()?;
    volume(&uuid)?;
    command(
        "/bin/td-boot",
        &["on-volume", "boot", "/volume", cmdline.trim_end()],
    )
}

fn installed() -> Result<(), String> {
    let cmdline = String::from_utf8(read(Path::new("/proc/cmdline"), 2048)?)
        .map_err(|_| "non-UTF-8 command line")?;
    let ids: Vec<&str> = cmdline
        .split_ascii_whitespace()
        .filter_map(|word| word.strip_prefix("td.deployment="))
        .collect();
    let [id] = ids.as_slice() else {
        return Err("missing or duplicate selected deployment".into());
    };
    let volumes: Vec<_> = cmdline
        .split_ascii_whitespace()
        .filter_map(|word| word.strip_prefix("td.volume="))
        .collect();
    let [uuid] = volumes.as_slice() else {
        return Err("missing or duplicate volume handoff".into());
    };
    let (_, partition) = volume(uuid)?;
    command("/bin/td-boot", &["on-volume", "mount-root", "/volume"])?;
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
    command("/bin/td-boot", &["on-volume", "mount-var", "/state"])?;
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
    // Simulate a dead writer whose mount table still names its closed descriptor.
    let held =
        File::open(&partition).map_err(|error| format!("open stale-mount fixture: {error}"))?;
    let descriptor = format!("/proc/{}/fd/{}", std::process::id(), held.as_raw_fd());
    applet(&[
        "mount",
        "-t",
        "btrfs",
        "-o",
        "rw,nodev,nosuid,noexec",
        &descriptor,
        "/ack",
    ])?;
    drop(held);
    if Path::new(&descriptor).exists() {
        return Err("stale-mount fixture retained its descriptor".into());
    }
    command("/bin/td-boot", &["on-volume", "success", "/ack", id])?;
    let mounts = read(Path::new("/proc/self/mountinfo"), 1024 * 1024)?;
    if mounts
        .split(|byte| *byte == b'\n')
        .any(|line| line.split(|byte| *byte == b' ').nth(4) == Some(b"/ack".as_slice()))
    {
        return Err("acknowledgement left its stale mount active".into());
    }
    println!("TD-INSTALL-STALE-MOUNT-RECOVERED");
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
    match read(Path::new("/fixture-phase"), 32)?.as_slice() {
        b"install\n" => install(&target()?),
        b"selector\n" => selector(),
        b"installed\n" => installed(),
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

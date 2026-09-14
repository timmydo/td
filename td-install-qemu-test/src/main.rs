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

fn report(mut output: impl Write, message: std::fmt::Arguments<'_>) -> Result<(), String> {
    // Formatting directly to stderr can split one protocol line across writes.
    let line = format!("{message}\n");
    output
        .write_all(line.as_bytes())
        .map_err(|error| error.to_string())
}

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
    report(std::io::stdout(), format_args!("{MEDIA_MARKER} {device}"))
}

fn configured_uuid() -> Result<String, String> {
    let bytes = read(Path::new("/etc/td/volume-uuid"), 37)?;
    let text = std::str::from_utf8(&bytes).map_err(|_| "non-ASCII configured volume UUID")?;
    text.strip_suffix('\n')
        .map(str::to_owned)
        .ok_or_else(|| "configured volume UUID lacks its newline".into())
}

fn install(device: &str, interrupt: bool) -> Result<(), String> {
    // The ISO carries the signed payloads and the live initramfs's public key.
    // Every path is fixture-owned; no private key enters the guest.
    mount_source()?;
    let uuid = configured_uuid()?;
    let name = device
        .strip_prefix("/dev/")
        .ok_or("invalid target device")?;
    let geometry = read(
        Path::new(&format!("/sys/class/block/{name}/queue/logical_block_size")),
        16,
    )?;
    let geometry = match geometry.as_slice() {
        b"512\n" => 512,
        b"4096\n" => 4096,
        _ => {
            return Err(format!(
                "unsupported fixture target sector size: {:?}",
                String::from_utf8_lossy(&geometry)
            ))
        }
    };
    report(
        std::io::stdout(),
        format_args!("{SECTOR_BYTES_MARKER} {geometry}"),
    )?;
    // Validate the stable read-only source before the first destructive command.
    // Publication below still rechecks the copied payloads and signature.
    command(
        "/bin/td-boot",
        &["validate-source", "/source", "/trusted.pub"],
    )?;
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
            "--trusted-key",
            "/trusted.pub",
        ],
    )?;
    for directory in ["@var", "td/boot", "td/deployments", "td/incoming"] {
        let staged = Path::new("/scratch/td-volume-root").join(directory);
        let mut entries = fs::read_dir(&staged)
            .map_err(|error| format!("inspect staging {}: {error}", staged.display()))?;
        if let Some(entry) = entries.next() {
            let entry = entry.map_err(|error| format!("read staging entry: {error}"))?;
            return Err(format!(
                "unexpected staged content in guest RAM: {}",
                entry.path().display()
            ));
        }
    }
    let partition = refresh_partitions(device, &uuid)?;
    fs::remove_dir_all("/scratch").map_err(|error| format!("remove formatter scratch: {error}"))?;
    // The volume image contains only filesystem metadata and the trust layout.
    // Publication now streams from read-only media straight onto the disk.
    if interrupt {
        return interrupt_publication(&partition);
    }
    command(
        "/bin/td-boot",
        &["install", &partition, "/volume", "/source", "/trusted.pub"],
    )?;
    applet(&["sync"])?;
    report(std::io::stdout(), format_args!("{DIRECT_MARKER}"))?;
    report(std::io::stdout(), format_args!("{INSTALL_MARKER}"))
}

/// Observe the publisher's private staging tree, never a production control hook.
fn staged_kernel(root: &Path) -> Result<Option<PathBuf>, String> {
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("inspect publication staging: {error}")),
    };
    let mut found = None;
    for entry in entries {
        let entry = entry.map_err(|error| error.to_string())?;
        if found.is_some()
            || !entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.starts_with(".install-"))
            || !entry
                .file_type()
                .map_err(|error| error.to_string())?
                .is_dir()
        {
            return Err("publication did not leave one private staging directory".into());
        }
        found = Some(entry.path().join("bzImage"));
    }
    Ok(found)
}

fn partial_length(path: &Path, expected: u64) -> Result<Option<u64>, String> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("stat interrupted payload: {error}")),
    };
    if !metadata.is_file() {
        return Err("interrupted payload is not a real file".into());
    }
    let len = metadata.len();
    if len >= expected {
        return Err("publisher finished its kernel before interruption".into());
    }
    Ok((len > 0).then_some(len))
}

fn observe_partial(child: &mut std::process::Child, expected: u64) -> Result<PathBuf, String> {
    let start = Instant::now();
    let mut kernel = None;
    loop {
        if let Some(status) = child.try_wait().map_err(|error| error.to_string())? {
            return Err(format!("publisher exited before interruption: {status}"));
        }
        if kernel.is_none() {
            kernel = staged_kernel(Path::new("/volume/td/deployments"))?;
        }
        if let Some(path) = &kernel {
            if partial_length(path, expected)?.is_some() {
                return Ok(path.clone());
            }
        }
        if start.elapsed() >= Duration::from_secs(120) {
            return Err("publisher never exposed an incomplete kernel".into());
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn interrupt_publication(partition: &str) -> Result<(), String> {
    let expected = fs::metadata("/source/bzImage")
        .map_err(|error| error.to_string())?
        .len();
    let mut child = Command::new("/bin/td-boot")
        .args(["install", partition, "/volume", "/source", "/trusted.pub"])
        .spawn()
        .map_err(|error| format!("start interrupted publisher: {error}"))?;
    let observed = observe_partial(&mut child, expected);
    // Always terminate and reap this owned child, including observer failures.
    let killed = child.kill();
    let waited = child.wait();
    let path = observed?;
    killed.map_err(|error| format!("stop interrupted publisher: {error}"))?;
    let status = waited.map_err(|error| format!("reap interrupted publisher: {error}"))?;
    if status.success() {
        return Err("interrupted publisher exited successfully".into());
    }
    let len =
        partial_length(&path, expected)?.ok_or("interrupted payload disappeared or is empty")?;
    for slot in ["current", "previous"] {
        match fs::symlink_metadata(Path::new("/volume/td/boot").join(slot)) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(format!("inspect interrupted selector: {error}")),
            Ok(_) => return Err("interrupted publication advertised a boot selector".into()),
        }
    }
    applet(&["sync"])?;
    report(
        std::io::stdout(),
        format_args!("{INTERRUPTED_MARKER} {len} {expected}"),
    )
}

fn refresh_partitions(device: &str, uuid: &str) -> Result<String, String> {
    applet(&["reread-partitions", device])?;
    let (_, partition) = volume(uuid)?;
    if partition != format!("{device}2") {
        return Err("partition reread resolved an unexpected fixture device".into());
    }
    command("/bin/td-boot", &["mount-root", &partition, "/volume"])?;
    let refused = Command::new("/bin/td-init")
        .args(["reread-partitions", device])
        .output()
        .map_err(|error| format!("execute busy partition reread: {error}"))?;
    let diagnostic = String::from_utf8_lossy(&refused.stderr);
    if refused.status.success()
        || !refused.stdout.is_empty()
        || !diagnostic.contains(&format!("reread partitions on {device}:"))
        || !diagnostic.contains("(os error 16)")
    {
        return Err(format!(
            "mounted disk reread did not refuse as busy: {diagnostic}"
        ));
    }
    applet(&["umount", "/volume"])?;
    applet(&["reread-partitions", device])?;
    report(
        std::io::stdout(),
        format_args!("{PARTITIONS_MARKER} {uuid} {partition}"),
    )?;
    Ok(partition)
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
    report(
        std::io::stdout(),
        format_args!("TD-INSTALL-VOLUME {found} {path}"),
    )?;
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
    report(
        std::io::stdout(),
        format_args!("TD-INSTALL-STALE-MOUNT-RECOVERED"),
    )?;
    let marker = if count == 1 {
        FIRST_BOOT_MARKER
    } else {
        SECOND_BOOT_MARKER
    };
    report(std::io::stdout(), format_args!("{marker} {id}"))
}

fn run() -> Result<(), String> {
    if std::process::id() != 1 {
        return Err("installation fixture must be guest PID 1".into());
    }
    directories()?;
    match read(Path::new("/fixture-phase"), 32)?.as_slice() {
        b"install\n" => install(&target()?, false),
        b"interrupt\n" => install(&target()?, true),
        b"selector\n" => selector(),
        b"installed\n" => installed(),
        _ => Err("invalid fixture phase".into()),
    }
}

fn main() -> ExitCode {
    let result = run();
    if let Err(error) = result {
        let _ = report(std::io::stderr(), format_args!("{REFUSED_PREFIX} {error}"));
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
    #![allow(clippy::unwrap_used)]
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct Scratch(PathBuf);
    impl Scratch {
        fn new() -> Self {
            static SEQ: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "td-install-interrupt-{}-{}",
                std::process::id(),
                SEQ.fetch_add(1, Ordering::Relaxed)
            ));
            fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn protocol_formatting_does_not_split_a_line_across_writes() {
        #[derive(Default)]
        struct Output(Vec<Vec<u8>>);
        impl Write for Output {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.push(bytes.to_vec());
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let mut output = Output::default();
        let error = String::from("/bin/td-boot failed: exit status: 1");
        report(&mut output, format_args!("{REFUSED_PREFIX} {error}")).unwrap();
        assert_eq!(
            output.0,
            vec![format!("{REFUSED_PREFIX} {error}\n").into_bytes()]
        );
    }

    #[test]
    fn interruption_requires_one_real_private_staging_directory() {
        let scratch = Scratch::new();
        assert_eq!(staged_kernel(&scratch.0.join("missing")).unwrap(), None);
        assert_eq!(staged_kernel(&scratch.0).unwrap(), None);
        let staged = scratch.0.join(".install-fixture");
        fs::write(&staged, b"file").unwrap();
        assert!(staged_kernel(&scratch.0).is_err());
        fs::remove_file(&staged).unwrap();
        std::os::unix::fs::symlink("/", &staged).unwrap();
        assert!(staged_kernel(&scratch.0).is_err());
        fs::remove_file(&staged).unwrap();
        fs::create_dir(&staged).unwrap();
        assert_eq!(
            staged_kernel(&scratch.0).unwrap(),
            Some(staged.join("bzImage"))
        );
        fs::create_dir(scratch.0.join(".install-another")).unwrap();
        assert!(staged_kernel(&scratch.0).is_err());
        fs::remove_dir(scratch.0.join(".install-another")).unwrap();
        fs::rename(&staged, scratch.0.join("published")).unwrap();
        assert!(staged_kernel(&scratch.0).is_err());
    }

    #[test]
    fn interruption_refuses_complete_empty_and_indirect_payloads() {
        let scratch = Scratch::new();
        let payload = scratch.0.join("bzImage");
        assert_eq!(partial_length(&payload, 4).unwrap(), None);
        fs::write(&payload, b"").unwrap();
        assert_eq!(partial_length(&payload, 4).unwrap(), None);
        fs::write(&payload, b"abc").unwrap();
        assert_eq!(partial_length(&payload, 4).unwrap(), Some(3));
        assert!(partial_length(&payload, 3).is_err());
        assert!(partial_length(&payload, 2).is_err());
        let alias = scratch.0.join("alias");
        std::os::unix::fs::symlink(&payload, &alias).unwrap();
        assert!(partial_length(&alias, 4).is_err());
        assert!(partial_length(&scratch.0, 4).is_err());
    }

    #[test]
    fn refuses_a_normal_process_before_accessing_devices() {
        assert_ne!(std::process::id(), 1);
        assert_eq!(
            super::run(),
            Err("installation fixture must be guest PID 1".into())
        );
    }
}

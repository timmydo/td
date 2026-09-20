//! Test-only native init for an oracle-owned VM; never packed in a system image.
#![forbid(unsafe_code)]
use std::fs::{self, File};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::time::{Duration, Instant};

mod protocol;
use protocol::*;

const ENXIO: i32 = 6; // Linux: no such device or address.
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

fn target_serial_attribute(name: &str) -> Option<&'static str> {
    if name.bytes().all(|byte| byte.is_ascii_lowercase()) {
        if name.starts_with("vd") {
            return Some("serial");
        }
        if name.starts_with("sd") {
            return Some("device/serial");
        }
    }
    let (controller, namespace) = name.strip_prefix("nvme")?.split_once('n')?;
    if !controller.is_empty()
        && !namespace.is_empty()
        && controller.bytes().all(|byte| byte.is_ascii_digit())
        && namespace.bytes().all(|byte| byte.is_ascii_digit())
    {
        Some("device/serial")
    } else {
        None
    }
}

fn serial_matches(name: &str, bytes: &[u8]) -> bool {
    let serial = bytes.strip_suffix(b"\n").unwrap_or(bytes);
    // NVMe's fixed-width Identify Controller serial is space-padded in sysfs.
    let serial = if name.starts_with("nvme") {
        serial.trim_ascii_end()
    } else {
        serial
    };
    serial == TARGET_SERIAL.as_bytes()
}

fn scan_target() -> Result<Option<String>, String> {
    let mut matched = None;
    for entry in
        fs::read_dir("/sys/class/block").map_err(|error| format!("list block devices: {error}"))?
    {
        let entry = entry.map_err(|error| format!("read block device entry: {error}"))?;
        let name = entry.file_name();
        let name = name.to_str().ok_or("non-UTF-8 block device name")?;
        // Match only whole disks in the oracle's fixed attachment families.
        let Some(attribute) = target_serial_attribute(name) else {
            continue;
        };
        let serial = entry.path().join(attribute);
        let serial_file = match File::open(&serial) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(format!("open target serial {}: {error}", serial.display())),
        };
        let mut bytes = Vec::new();
        match serial_file.take(129).read_to_end(&mut bytes) {
            Ok(_) => {}
            // Pinned Linux SCSI serial attributes report absent VPD with ENXIO.
            Err(error) if error.raw_os_error() == Some(ENXIO) => continue,
            Err(error) => return Err(format!("read target serial {}: {error}", serial.display())),
        }
        if bytes.len() > 128 {
            return Err(format!(
                "target serial {} exceeds 128 bytes",
                serial.display()
            ));
        }
        let serial = bytes;
        if !serial_matches(name, &serial) {
            continue;
        }
        if matched.is_some() {
            return Err("ambiguous oracle target serial".into());
        }
        matched = Some(format!("/dev/{name}"));
    }
    let Some(path) = matched else {
        return Ok(None);
    };
    let metadata = match fs::metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("stat {path}: {error}")),
    };
    if !metadata.file_type().is_block_device() {
        return Err("oracle target is not a block device".into());
    }
    match File::open(&path) {
        Ok(_) => Ok(Some(path)),
        Err(error)
            if error.raw_os_error() == Some(ENXIO)
                || error.kind() == std::io::ErrorKind::NotFound =>
        {
            Ok(None)
        }
        Err(error) => Err(format!("open oracle target {path}: {error}")),
    }
}

fn target() -> Result<String, String> {
    let started = Instant::now();
    loop {
        if let Some(path) = scan_target()? {
            return Ok(path);
        }
        if started.elapsed() >= Duration::from_secs(30) {
            return Err("oracle target serial did not appear".into());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
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

fn media_device(target: &str) -> Result<&'static str, String> {
    let started = Instant::now();
    loop {
        let mut found = None;
        // Exactly one source in this fixed QEMU profile: SATA optical or USB.
        // USB mass-storage probing can finish after PID 1 starts.
        for path in ["/dev/sr0", "/dev/sda", "/dev/sdb"] {
            if path == target {
                continue;
            }
            match fs::metadata(path) {
                Ok(meta) if meta.file_type().is_block_device() => {
                    // sr can publish a placeholder capacity for an empty tray.
                    // Opening read-only asks the block driver to check media.
                    let _medium = match File::open(path) {
                        Ok(file) => file,
                        Err(error) if matches!(error.raw_os_error(), Some(ENOMEDIUM | ENXIO)) => {
                            continue
                        }
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

fn mount_source(target: &str) -> Result<(), String> {
    let device = media_device(target)?;
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

fn diagnostic_line(bytes: Vec<u8>, limit: usize, label: &str) -> Result<String, String> {
    if bytes.len() > limit {
        return Err(format!("fixture {label} exceeds its byte limit"));
    }
    let mut text = String::from_utf8(bytes).map_err(|_| format!("{label} is not UTF-8"))?;
    if !text.ends_with('\n') {
        return Err(format!("{label} lacks final newline"));
    }
    text.pop();
    if text.is_empty() || text.contains(['\n', '\r']) {
        return Err(format!("{label} is not one complete line"));
    }
    Ok(text)
}

fn diagnostic(marker: &str, args: &[&str], limit: usize, label: &str) -> Result<(), String> {
    let mut child = Command::new("/bin/td-install")
        .args(args)
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|error| format!("start {label}: {error}"))?;
    let captured = (|| {
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| format!("{label} stdout is not piped"))?;
        let mut bytes = Vec::new();
        stdout
            .take(limit as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|error| format!("read {label}: {error}"))?;
        diagnostic_line(bytes, limit, label)
    })();
    if captured.is_err() {
        let _ = child.kill();
    }
    let waited = child
        .wait()
        .map_err(|error| format!("reap {label}: {error}"));
    let json = captured?;
    let status = waited?;
    if !status.success() {
        return Err(format!("{label} failed: {status}"));
    }
    report(
        std::io::stdout(),
        format_args!("{marker} {} {json}", json.len()),
    )
}

fn inventory(marker: &str) -> Result<(), String> {
    diagnostic(marker, &["inventory"], MAX_INVENTORY_BYTES, "inventory")
}

fn preview(name: &str, geometry: u64) -> Result<(), String> {
    let path = format!("/sys/class/block/{name}/size");
    // A decimal u64 has at most twenty digits, followed by sysfs newline.
    let bytes = read(Path::new(&path), 21)?;
    let text = std::str::from_utf8(&bytes).map_err(|_| format!("{path}: non-ASCII capacity"))?;
    let sectors = text
        .strip_suffix('\n')
        .ok_or_else(|| format!("{path}: missing newline"))?;
    if sectors.is_empty() || !sectors.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(format!("{path}: invalid capacity"));
    }
    let capacity = sectors
        .parse::<u64>()
        .ok()
        .and_then(|sectors| sectors.checked_mul(512))
        .ok_or_else(|| format!("{path}: capacity overflow"))?;
    diagnostic(
        PREVIEW_MARKER,
        &[
            "layout-preview",
            &geometry.to_string(),
            &capacity.to_string(),
        ],
        MAX_PREVIEW_BYTES,
        "layout preview",
    )
}

fn install(device: &str, interrupt: bool, system_autotest: bool) -> Result<(), String> {
    // The ISO carries the signed payloads and the live initramfs's public key.
    // Every path is fixture-owned; no deployment-signing key enters the guest.
    // Only install-system carries the disposable SSH administrator test key.
    mount_source(device)?;
    inventory(INVENTORY_BEFORE_MARKER)?;
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
    if system_autotest {
        if !Path::new("/dev/loop0").exists() {
            applet(&["mknod", "/dev/loop0", "b", "7", "0"])?;
        }
        command("/bin/losetup", &["-r", "/dev/loop0", "/source/root.erofs"])?;
        applet(&["mount", "-t", "erofs", "-o", "ro,nodev,nosuid,noexec",
            "/dev/loop0", "/root-image"])?;
        command("/bin/td-firstboot", &["check-primary-name", "/root-image", USERNAME])?;
    }
    command(
        "/bin/td-install",
        &["layout", device, "/source/bzImage", "/selector.cpio"],
    )?;
    // Keep negative cases testing the real writer's refusal before this report.
    preview(name, geometry)?;
    let mut volume_arguments = vec!["volume", "--uuid", &uuid,
        "--timezone", TIMEZONE_ID, "--hostname", HOSTNAME];
    if system_autotest {
        volume_arguments.extend(["--username", USERNAME, "/root-image", "/bin/td-firstboot"]);
    }
    volume_arguments.extend([device, "/bin/mkfs.btrfs", "/scratch", "--trusted-key", "/trusted.pub"]);
    command("/bin/td-install", &volume_arguments)?;
    if system_autotest {
        check_username(Path::new("/scratch/td-volume-root/@var"))?;
        // The read-only loop binding lasts until this one-purpose VM ends.
        command("/bin/umount", &["/root-image"])?;
    }
    check_timezone(Path::new("/scratch/td-volume-root/@var"))?;
    check_hostname(Path::new("/scratch/td-volume-root/@var"))?;
    for (directory, expected) in [
        ("@var", &["lib"][..]),
        ("@var/lib", &["td"][..]),
        ("@var/lib/td", if system_autotest { &["hostname", "timezone", "username"][..] } else { &["hostname", "timezone"][..] }),
    ] {
        let staged = Path::new("/scratch/td-volume-root").join(directory);
        let mut names = fs::read_dir(&staged)
            .map_err(|error| format!("inspect {}: {error}", staged.display()))?
            .map(|entry| entry.map(|entry| entry.file_name()))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| format!("read {}: {error}", staged.display()))?;
        names.sort();
        if names != expected.iter().map(std::ffi::OsString::from).collect::<Vec<_>>() {
            return Err(format!(
                "unexpected staged settings in {}",
                staged.display()
            ));
        }
    }
    for directory in ["td/boot", "td/deployments", "td/incoming"] {
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
    inventory(INVENTORY_AFTER_MARKER)?;
    fs::remove_dir_all("/scratch").map_err(|error| format!("remove formatter scratch: {error}"))?;
    // The volume image contains metadata, trust layout and bounded settings.
    // Publication now streams from read-only media straight onto the disk.
    if interrupt {
        return interrupt_publication(&partition);
    }
    command(
        "/bin/td-boot",
        &["install", &partition, "/volume", "/source", "/trusted.pub"],
    )?;
    if system_autotest {
        command("/bin/td-boot", &["mount-var", &partition, "/state"])?;
        seed_system_autotest(Path::new("/"), Path::new("/state"))?;
        check_timezone(Path::new("/state"))?;
        check_hostname(Path::new("/state"))?;
        check_username(Path::new("/state"))?;
        command("/bin/umount", &["/state"])?;
    }
    applet(&["sync"])?;
    report(std::io::stdout(), format_args!("{DIRECT_MARKER}"))?;
    report(std::io::stdout(), format_args!("{INSTALL_MARKER}"))
}

/// The installed application oracle uses the standard VM's loopback-only SSH
/// fixture. Its inputs exist only on the full-system diagnostic ISO.
fn seed_system_autotest(source: &Path, state: &Path) -> Result<(), String> {
    let private = read(&source.join(SYSTEM_AUTOTEST_PRIVATE), 4096)?;
    let authorized = read(&source.join(SYSTEM_AUTOTEST_AUTHORIZED), 4096)?;
    if private.is_empty() || authorized.is_empty() {
        return Err("empty system autotest SSH fixture".into());
    }
    for (relative, mode) in [("lib/td-test", 0o755), ("lib/td/ssh", 0o700)] {
        let path = state.join(relative);
        fs::create_dir_all(&path).map_err(|error| format!("create {}: {error}", path.display()))?;
        fs::set_permissions(&path, fs::Permissions::from_mode(mode))
            .map_err(|error| format!("chmod {}: {error}", path.display()))?;
    }
    for (relative, bytes) in [
        ("lib/td-test/openssh-admin-selftest", private),
        ("lib/td/ssh/authorized_keys", authorized),
    ] {
        let path = state.join(relative);
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .map_err(|error| format!("create {}: {error}", path.display()))?;
        file.set_permissions(fs::Permissions::from_mode(0o600))
            .and_then(|()| file.write_all(&bytes))
            .and_then(|()| file.sync_all())
            .map_err(|error| format!("write {}: {error}", path.display()))?;
    }
    Ok(())
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

fn require_volume_partition(device: &str, partition: &str) -> Result<(), String> {
    // Linux inserts p after digit-ending disk names, including NVMe namespaces.
    let separator = if device.as_bytes().last().is_some_and(u8::is_ascii_digit) {
        "p"
    } else {
        ""
    };
    if partition != format!("{device}{separator}2") {
        return Err("partition reread resolved an unexpected fixture device".into());
    }
    Ok(())
}

fn refresh_partitions(device: &str, uuid: &str) -> Result<String, String> {
    applet(&["reread-partitions", device])?;
    let (_, partition) = volume(uuid)?;
    require_volume_partition(device, &partition)?;
    command("/bin/td-boot", &["mount-root", &partition, "/volume"])?;
    reject_mounted_writers(device)?;
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

fn primary_metadata(device: &str) -> Result<Vec<u8>, String> {
    // Covers protective MBR, primary GPT header and entry array at 512/4096.
    let mut bytes = vec![0; 64 * 1024];
    fs::File::open(device)
        .and_then(|mut file| file.read_exact(&mut bytes))
        .map_err(|error| format!("read primary disk metadata {device}: {error}"))?;
    Ok(bytes)
}

fn reject_mounted_writers(device: &str) -> Result<(), String> {
    let baseline = primary_metadata(device)?;
    let commands: &[&[&str]] = &[
        &["layout", device],
        &["volume", device, "/bin/mkfs.btrfs", "/scratch"],
    ];
    for arguments in commands {
        let refused = Command::new("/bin/td-install")
            .args(*arguments)
            .output()
            .map_err(|error| format!("execute mounted formatter refusal: {error}"))?;
        let diagnostic = String::from_utf8_lossy(&refused.stderr);
        if primary_metadata(device)? != baseline {
            return Err(format!(
                "mounted formatter {arguments:?} changed the first 64 KiB"
            ));
        }
        if refused.status.code() != Some(1)
            || !refused.stdout.is_empty()
            || !diagnostic.contains(&format!("{device}:"))
            || !diagnostic.contains("(os error 16)")
        {
            return Err(format!(
                "mounted formatter {arguments:?} did not refuse its open as busy: status {}; stdout {:?}; stderr {diagnostic}",
                refused.status,
                String::from_utf8_lossy(&refused.stdout)
            ));
        }
    }
    Ok(())
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

fn check_username(state: &Path) -> Result<(), String> {
    check_setting(state, "lib/td/username", USERNAME)
}

fn check_hostname(state: &Path) -> Result<(), String> {
    check_setting(state, "lib/td/hostname", HOSTNAME)
}

fn check_timezone(state: &Path) -> Result<(), String> {
    check_setting(state, "lib/td/timezone", TIMEZONE_ID)
}

fn check_setting(state: &Path, relative: &str, expected: &str) -> Result<(), String> {
    let path = state.join(relative);
    let metadata = fs::symlink_metadata(&path)
        .map_err(|error| format!("inspect {}: {error}", path.display()))?;
    if !metadata.is_file() || metadata.permissions().mode() & 0o7777 != 0o644 {
        return Err(format!(
            "{} is not a mode-0644 regular setting",
            path.display()
        ));
    }
    if read(&path, 65)? != format!("{expected}\n").as_bytes() {
        return Err(format!("wrong saved setting in {}", path.display()));
    }
    Ok(())
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
    check_timezone(Path::new("/state"))?;
    check_hostname(Path::new("/state"))?;
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
        b"install\n" => install(&target()?, false, false),
        b"install-system\n" => install(&target()?, false, true),
        b"interrupt\n" => install(&target()?, true, false),
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
    fn system_autotest_requires_explicit_inputs_and_preserves_existing_state() {
        let source = Scratch::new();
        let state = Scratch::new();
        assert!(seed_system_autotest(&source.0, &state.0).is_err());
        assert!(!state.0.join("lib").exists());
        fs::create_dir(source.0.join("system-autotest")).unwrap();
        fs::write(source.0.join(SYSTEM_AUTOTEST_PRIVATE), b"private fixture").unwrap();
        fs::write(source.0.join(SYSTEM_AUTOTEST_AUTHORIZED), b"public fixture").unwrap();
        seed_system_autotest(&source.0, &state.0).unwrap();
        for (relative, expected) in [
            (
                "lib/td-test/openssh-admin-selftest",
                b"private fixture".as_slice(),
            ),
            ("lib/td/ssh/authorized_keys", b"public fixture".as_slice()),
        ] {
            let path = state.0.join(relative);
            assert_eq!(fs::read(&path).unwrap(), expected);
            assert_eq!(
                fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        assert_eq!(
            fs::metadata(state.0.join("lib/td/ssh"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        fs::write(state.0.join("lib/td/ssh/authorized_keys"), b"preserve").unwrap();
        assert!(seed_system_autotest(&source.0, &state.0).is_err());
        assert_eq!(
            fs::read(state.0.join("lib/td/ssh/authorized_keys")).unwrap(),
            b"preserve"
        );
    }

    #[test]
    fn cold_boot_settings_refuse_absence_wrong_names_and_links() {
        for (relative, expected, check) in [
            ("hostname", HOSTNAME, check_hostname as fn(&Path) -> Result<(), String>),
            ("timezone", TIMEZONE_ID, check_timezone),
        ] {
            let scratch = Scratch::new();
            let path = scratch.0.join("lib/td").join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            assert!(check(&scratch.0).is_err());
            for value in ["wrong\n".into(), expected.into(), format!("{expected}\nextra\n")] {
                fs::write(&path, value).unwrap();
                fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
                assert!(check(&scratch.0).is_err());
            }
            fs::write(&path, format!("{expected}\n")).unwrap();
            check(&scratch.0).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o666)).unwrap();
            assert!(check(&scratch.0).is_err());
            let target = scratch.0.join("choice");
            fs::rename(&path, &target).unwrap();
            fs::set_permissions(&target, fs::Permissions::from_mode(0o644)).unwrap();
            std::os::unix::fs::symlink(&target, &path).unwrap();
            assert!(check(&scratch.0).is_err());
        }
    }

    #[test]
    fn refreshed_volume_partition_requires_the_expected_disk_and_separator() {
        for (disk, partition) in [
            ("/dev/vda", "/dev/vda2"),
            ("/dev/sda", "/dev/sda2"),
            ("/dev/nvme0n1", "/dev/nvme0n1p2"),
            ("/dev/nvme12n34", "/dev/nvme12n34p2"),
        ] {
            assert!(require_volume_partition(disk, partition).is_ok());
        }
        for partition in ["/dev/nvme0n12", "/dev/nvme0n1p1", "/dev/nvme1n1p2"] {
            assert!(require_volume_partition("/dev/nvme0n1", partition).is_err());
        }
        assert!(require_volume_partition("/dev/vda", "/dev/vdap2").is_err());
    }

    #[test]
    fn target_serials_admit_only_whole_supported_disk_names() {
        for (name, attribute) in [
            ("vda", "serial"),
            ("sdaa", "device/serial"),
            ("nvme0n1", "device/serial"),
            ("nvme12n34", "device/serial"),
        ] {
            assert_eq!(target_serial_attribute(name), Some(attribute));
            assert!(serial_matches(
                name,
                format!("{TARGET_SERIAL}\n").as_bytes()
            ));
        }
        for name in [
            "vda2",
            "sda1",
            "nvme0n1p2",
            "nvme0c0n1",
            "nvmen1",
            "nvme0n",
            "nvme0n1x",
            "../nvme0n1",
            "loop0",
        ] {
            assert_eq!(target_serial_attribute(name), None, "{name}");
        }
        let padded = format!("{TARGET_SERIAL:<20}\n");
        assert!(serial_matches("nvme0n1", padded.as_bytes()));
        assert!(!serial_matches("vda", padded.as_bytes()));
        assert!(!serial_matches(
            "nvme0n1",
            format!(" {TARGET_SERIAL}\n").as_bytes()
        ));
        assert!(!serial_matches("nvme0n1", b"another-disk         \n"));
    }

    #[test]
    fn diagnostic_reports_require_one_bounded_complete_utf8_line() {
        for (limit, label) in [
            (MAX_INVENTORY_BYTES, "inventory"),
            (MAX_PREVIEW_BYTES, "layout preview"),
        ] {
            assert_eq!(
                diagnostic_line(b"{}\n".to_vec(), limit, label).unwrap(),
                "{}"
            );
            for bytes in [
                b"".to_vec(),
                b"{}".to_vec(),
                b"{}\n{}\n".to_vec(),
                b"{}\r\n".to_vec(),
                vec![0xff, b'\n'],
            ] {
                assert!(diagnostic_line(bytes, limit, label).is_err());
            }
            let mut exact = vec![b' '; limit];
            if let Some(last) = exact.last_mut() {
                *last = b'\n';
            }
            assert!(diagnostic_line(exact.clone(), limit, label).is_ok());
            exact.insert(0, b' ');
            assert!(diagnostic_line(exact, limit, label).is_err());
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

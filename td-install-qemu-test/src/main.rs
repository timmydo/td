//! Test-only native init for an oracle-owned VM; never packed in a system image.
#![forbid(unsafe_code)]
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom, Write};
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

fn report_refusal(output: impl Write, error: &str) -> Result<(), String> {
    // Serial transmission can split even one write; the last record fences it.
    report(output, format_args!("{REFUSED_PREFIX} {error}\n{REFUSAL_COMPLETE_MARKER}"))
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

fn quiet_kernel_console(
    control: &Path,
    override_level: &Path,
    no_auto_verbose: &Path,
) -> Result<(), String> {
    if read(override_level, 2)? != b"N\n" {
        return Err("kernel console ignores its configured log level".into());
    }
    if read(no_auto_verbose, 2)? != b"N\n" {
        return Err("kernel console cannot raise its log level on faults".into());
    }
    // Keep console_verbose functional while suppressing routine kernel messages.
    let mut file = fs::OpenOptions::new()
        .write(true)
        .open(control)
        .map_err(|error| format!("open kernel console control: {error}"))?;
    file.write_all(b"1\n")
        .map_err(|error| format!("quiet kernel console: {error}"))?;
    let bytes = read(control, 64)?;
    let text = std::str::from_utf8(&bytes).map_err(|_| "non-UTF-8 kernel console control")?;
    let fields: Vec<_> = text.split_ascii_whitespace().collect();
    if fields.len() != 4
        || fields.first() != Some(&"1")
        || !fields.iter().all(|field| field.parse::<i32>().is_ok())
    {
        return Err("kernel console did not retain the quiet log level".into());
    }
    Ok(())
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

fn mount_source(target: &str) -> Result<&'static str, String> {
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
    report(std::io::stdout(), format_args!("{MEDIA_MARKER} {device}"))?;
    Ok(device)
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

fn command_line(args: &[&str], limit: usize, label: &str) -> Result<String, String> {
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
    Ok(json)
}

fn diagnostic(marker: &str, args: &[&str], limit: usize, label: &str) -> Result<(), String> {
    let json = command_line(args, limit, label)?;
    report(
        std::io::stdout(),
        format_args!("{marker} {} {json}", json.len()),
    )
}

fn candidates(marker: &str) -> Result<(), String> {
    diagnostic(marker, &["destinations"], MAX_INVENTORY_BYTES, "destination candidates")
}

fn attribute(path: &Path, optional: bool) -> Result<Option<String>, String> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if optional && error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("open {}: {error}", path.display())),
    };
    let mut bytes = Vec::new();
    match file.take(257).read_to_end(&mut bytes) {
        Ok(_) => {}
        Err(error) if optional && error.raw_os_error() == Some(ENXIO) => return Ok(None),
        Err(error) => return Err(format!("read {}: {error}", path.display())),
    }
    if bytes.len() > 256 {
        return Err(format!("{} exceeds 256 bytes", path.display()));
    }
    let value = String::from_utf8(bytes).map_err(|_| format!("{} is not UTF-8", path.display()))?;
    Ok(Some(value.trim().to_owned()))
}

fn required_attribute(path: &Path) -> Result<String, String> {
    attribute(path, false)?.ok_or_else(|| format!("{} is missing", path.display()))
}

fn put_plan_text(bytes: &mut Vec<u8>, value: &str) -> Result<(), String> {
    let length = u16::try_from(value.len()).map_err(|_| "fixture plan text is too long")?;
    bytes.extend_from_slice(&length.to_be_bytes());
    bytes.extend_from_slice(value.as_bytes());
    Ok(())
}

fn put_plan_optional(bytes: &mut Vec<u8>, value: Option<String>) -> Result<(), String> {
    match value {
        Some(value) => {
            bytes.push(1);
            put_plan_text(bytes, &value)
        }
        None => {
            bytes.push(0);
            Ok(())
        }
    }
}

/// Independently frame the QEMU device observations, so the guest checks the
/// shipped decoder against bytes that its own codec did not produce.
fn observed_plan(device: &str) -> Result<Vec<u8>, String> {
    let name = device.strip_prefix("/dev/").ok_or("invalid target path")?;
    let base = Path::new("/sys/class/block").join(name);
    let number = required_attribute(&base.join("dev"))?;
    let (major, minor) = number.split_once(':').ok_or("invalid target device number")?;
    let major = major.parse::<u32>().map_err(|_| "invalid target major")?;
    let minor = minor.parse::<u32>().map_err(|_| "invalid target minor")?;
    let sequence = required_attribute(&base.join("diskseq"))?.parse::<u64>()
        .map_err(|_| "invalid target disk sequence")?;
    let capacity = required_attribute(&base.join("size"))?.parse::<u64>()
        .map_err(|_| "invalid target capacity")?.checked_mul(512)
        .ok_or("target capacity overflow")?;
    let sector = required_attribute(&base.join("queue/logical_block_size"))?.parse::<u32>()
        .map_err(|_| "invalid target sector size")?;
    let removable = match required_attribute(&base.join("removable"))?.as_str() {
        "0" => 0, "1" => 1, _ => return Err("invalid target removable flag".into()),
    };
    let model = attribute(&base.join("device/model"), true)?;
    let serial = match attribute(&base.join("serial"), true)? {
        Some(value) => Some(value),
        None => attribute(&base.join("device/serial"), true)?,
    };
    let wwid = match attribute(&base.join("wwid"), true)? {
        Some(value) => Some(value),
        None => attribute(&base.join("device/wwid"), true)?,
    };
    let mut bytes = Vec::with_capacity(256);
    bytes.extend_from_slice(b"TDPLAN01");
    bytes.extend_from_slice(&[1; 32]);
    bytes.extend_from_slice(&[0; 32]);
    let mut uuid = [0; 16];
    *uuid.get_mut(6).ok_or("fixture UUID has no version byte")? = 0x40;
    *uuid.get_mut(8).ok_or("fixture UUID has no variant byte")? = 0x80;
    bytes.extend_from_slice(&uuid);
    bytes.extend_from_slice(&major.to_be_bytes());
    bytes.extend_from_slice(&minor.to_be_bytes());
    bytes.extend_from_slice(&sequence.to_be_bytes());
    bytes.extend_from_slice(&capacity.to_be_bytes());
    bytes.extend_from_slice(&sector.to_be_bytes());
    bytes.push(removable);
    put_plan_text(&mut bytes, name)?;
    for label in [model, serial, wwid] {
        put_plan_optional(&mut bytes, label)?;
    }
    for choice in ["alice", "td-qemu-installed", "us", "Europe/London"] {
        put_plan_text(&mut bytes, choice)?;
    }
    Ok(bytes)
}

fn plan_observation(plan: &[u8]) -> Result<std::process::Output, String> {
    let mut child = Command::new("/bin/td-install")
        .arg("observe-plan")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("start plan observation: {error}"))?;
    let fed = child.stdin.take().ok_or("plan observation stdin is unavailable")?
        .write_all(plan).map_err(|error| format!("feed plan observation: {error}"));
    let result = child.wait_with_output()
        .map_err(|error| format!("finish plan observation: {error}"));
    fed?;
    result
}

fn canaries(device: &str) -> Result<Vec<u8>, String> {
    let mut file = File::open(device).map_err(|error| error.to_string())?;
    let len = file.seek(SeekFrom::End(0)).map_err(|error| error.to_string())?;
    let mut bytes = Vec::with_capacity(96);
    for offset in [0, len / 2, len.saturating_sub(32)] {
        file.seek(SeekFrom::Start(offset)).map_err(|error| error.to_string())?;
        let mut sample = [0; 32];
        file.read_exact(&mut sample).map_err(|error| error.to_string())?;
        bytes.extend_from_slice(&sample);
    }
    Ok(bytes)
}

fn check_plan_observation(device: &str) -> Result<(), String> {
    let before = canaries(device)?;
    let plan = observed_plan(device)?;
    let response = plan_observation(&plan)?;
    if !response.status.success() {
        return Err(format!("current plan observation failed: {}", response.status));
    }
    let name = device.strip_prefix("/dev/").ok_or("invalid target path")?;
    let expected = format!("{{\"version\":1,\"scope\":\"plan-observation-only\",\"destination\":\"{name}\"}}\n");
    if response.stdout != expected.as_bytes() {
        return Err("current plan observation returned a different report".into());
    }
    report(std::io::stdout(), format_args!("{PLAN_OBSERVATION_MARKER} {} {}", expected.trim_end().len(), expected.trim_end()))?;
    let sequence = required_attribute(&Path::new("/sys/class/block").join(name).join("diskseq"))?
        .parse::<u64>().map_err(|_| "invalid target disk sequence")?;
    let changed = sequence.checked_add(1).ok_or("fixture disk sequence overflow")?;
    let mut stale = plan;
    stale.get_mut(96..104).ok_or("fixture plan lacks disk sequence")?
        .copy_from_slice(&changed.to_be_bytes());
    let response = plan_observation(&stale)?;
    let diagnostic = String::from_utf8_lossy(&response.stderr);
    if response.status.success() || !response.stdout.is_empty()
        || !diagnostic.contains("reviewed destination is no longer an unchanged candidate") {
        return Err("stale plan observation was accepted or reported success".into());
    }
    if before != canaries(device)? {
        return Err("plan observation changed target disk canaries".into());
    }
    report(std::io::stdout(), format_args!("{PLAN_STALE_MARKER}"))
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
    candidates(CANDIDATES_BEFORE_MARKER)?;
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
    let sysfs = Path::new("/sys/class/block").join(name);
    let writable = required_attribute(&sysfs.join("ro"))? == "0";
    let sectors = required_attribute(&sysfs.join("size"))?.parse::<u64>()
        .map_err(|_| "invalid target sector count")?;
    if writable && sectors >= PLAN_PROBE_MINIMUM_SECTORS {
        check_plan_observation(device)?;
    }
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
    let uuid = command_line(&["new-volume-uuid"], 37, "new volume identity")?;
    if !is_v4_volume_uuid(&uuid) {
        return Err("generated volume identity is not a canonical version-4 UUID".into());
    }
    command("/bin/td-install", &["prepare-selector", "/selector.cpio", &uuid, "/prepared-selector.cpio"])?;
    let mut format_arguments = vec!["format", "/source/bzImage", "/prepared-selector.cpio", "--uuid", &uuid,
        "--timezone", TIMEZONE_ID, "--hostname", HOSTNAME];
    if system_autotest {
        format_arguments.extend(["--username", USERNAME, "/root-image", "/bin/td-firstboot"]);
    }
    format_arguments.extend([device, "/bin/mkfs.btrfs", "/scratch", "--trusted-key", "/trusted.pub"]);
    command("/bin/td-install", &format_arguments)?;
    // The real writer must refuse bad targets before the diagnostic preview.
    preview(name, geometry)?;
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
    reject_held_disk_users(device, &partition)?;
    command("/bin/td-boot", &["mount-root", &partition, "/volume"])?;
    reject_busy_formatters(device, "mounted partition")?;
    candidates(CANDIDATES_MOUNTED_MARKER)?;
    let name = device.strip_prefix("/dev/").ok_or("invalid target path")?;
    let sectors = required_attribute(&Path::new("/sys/class/block").join(name).join("size"))?
        .parse::<u64>().map_err(|_| "invalid target sector count")?;
    if sectors >= PLAN_PROBE_MINIMUM_SECTORS {
        let before = canaries(device)?;
        let refused = plan_observation(&observed_plan(device)?)?;
        let diagnostic = String::from_utf8_lossy(&refused.stderr);
        if refused.status.success() || !refused.stdout.is_empty()
            || !diagnostic.contains("(os error 16)") {
            return Err(format!("mounted target plan observation did not refuse as busy: {diagnostic}"));
        }
        if before != canaries(device)? {
            return Err("busy plan observation changed target disk canaries".into());
        }
        report(std::io::stdout(), format_args!("{PLAN_BUSY_MARKER}"))?;
    }
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

fn claim_fixture_disk(device: &str) -> Result<File, String> {
    if !cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        return Err("whole-disk claim fixture requires x86-64 Linux".into());
    }
    // Match td-install/src/main.rs::paths::open_destination_claim.
    const O_EXCL: i32 = 0x80;
    let claim = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(O_EXCL)
        .open(device)
        .map_err(|error| format!("claim fixture disk {device}: {error}"))?;
    if !claim
        .metadata()
        .map_err(|error| format!("inspect claimed fixture disk {device}: {error}"))?
        .file_type()
        .is_block_device()
    {
        return Err(format!("claimed fixture disk {device} is not a block device"));
    }
    Ok(claim)
}

fn reject_held_disk_users(device: &str, partition: &str) -> Result<(), String> {
    let claim = claim_fixture_disk(device)?;
    reject_busy_formatters(device, "held whole disk")?;
    candidates(CANDIDATES_HELD_MARKER)?;
    let refused = Command::new("/bin/td-boot")
        .args(["mount-root", partition, "/volume"])
        .output()
        .map_err(|error| format!("execute claimed partition mount refusal: {error}"))?;
    let diagnostic = String::from_utf8_lossy(&refused.stderr);
    if refused.status.code() != Some(1)
        || !refused.stdout.is_empty()
        || !diagnostic.contains(&format!("mounting {partition} on /volume:"))
        || !diagnostic.contains("(os error 16)")
    {
        return Err(format!(
            "claimed partition mount did not refuse as busy: status {}; stdout {:?}; stderr {diagnostic}",
            refused.status,
            String::from_utf8_lossy(&refused.stdout)
        ));
    }
    drop(claim);
    Ok(())
}

fn reject_busy_formatters(device: &str, state: &str) -> Result<(), String> {
    let baseline = primary_metadata(device)?;
    let commands: &[&[&str]] = &[
        &["layout", device],
        &["volume", device, "/bin/mkfs.btrfs", "/scratch"],
        &["format", "/source/bzImage", "/selector.cpio", device, "/bin/mkfs.btrfs", "/scratch"],
    ];
    for arguments in commands {
        let refused = Command::new("/bin/td-install")
            .args(*arguments)
            .output()
            .map_err(|error| format!("execute {state} formatter refusal: {error}"))?;
        let diagnostic = String::from_utf8_lossy(&refused.stderr);
        if primary_metadata(device)? != baseline {
            return Err(format!(
                "{state} formatter {arguments:?} changed the first 64 KiB"
            ));
        }
        if refused.status.code() != Some(1)
            || !refused.stdout.is_empty()
            || !diagnostic.contains(&format!("{device}:"))
            || !diagnostic.contains("(os error 16)")
        {
            return Err(format!(
                "{state} formatter {arguments:?} did not refuse its open as busy: status {}; stdout {:?}; stderr {diagnostic}",
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

fn protect_writable_media(target: &str) -> Result<(), String> {
    let device = mount_source(target)?;
    if device != "/dev/sda"
        || read(Path::new("/sys/class/block/sda/ro"), 2)? != b"0\n"
        || read(Path::new("/sys/class/block/sda/queue/logical_block_size"), 16)? != b"512\n"
    {
        return Err("media-claim fixture requires writable 512-byte USB media".into());
    }
    inventory(INVENTORY_BEFORE_MARKER)?;
    candidates(CANDIDATES_BEFORE_MARKER)?;
    command("/bin/td-boot", &["validate-source", "/source", "/trusted.pub"])?;
    reject_busy_formatters(device, "mounted installer medium")?;
    report(std::io::stdout(), format_args!("{MEDIA_BUSY_MARKER}"))?;
    // Every file bind also retains the ISO filesystem's block-device claim.
    for (_, name) in MEDIA_FILES.iter().rev() {
        applet(&["umount", &format!("/{name}")])?;
    }
    applet(&["umount", "/media"])?;
    drop(claim_fixture_disk(device)?);
    report(std::io::stdout(), format_args!("{MEDIA_RELEASED_MARKER}"))
}

fn require_scratch_mount(bytes: &[u8]) -> Result<(), String> {
    let text = std::str::from_utf8(bytes).map_err(|_| "non-UTF-8 scratch mount report")?;
    let mut matching = text.lines().filter_map(|line| {
        let (mount, filesystem) = line.split_once(" - ")?;
        (mount.split_ascii_whitespace().nth(4) == Some("/scratch")).then_some(filesystem)
    });
    let filesystem = matching.next().ok_or("missing scratch mount")?;
    if matching.next().is_some() {
        return Err("duplicate scratch mount".into());
    }
    let mut fields = filesystem.split_ascii_whitespace();
    if fields.next() != Some("tmpfs")
        || fields.next() != Some("tmpfs")
        || !fields
            .next()
            .is_some_and(|options| options.split(',').any(|o| o == "size=64k"))
        || fields.next().is_some()
    {
        return Err("scratch is not the expected 64 KiB tmpfs".into());
    }
    Ok(())
}

fn scratch_limited_install(device: &str) -> Result<(), String> {
    // Only the formatter's private staging filesystem is constrained.
    applet(&[
        "mount",
        "-t",
        "tmpfs",
        "-o",
        "size=64k,mode=0700,nodev,nosuid",
        "tmpfs",
        "/scratch",
    ])?;
    require_scratch_mount(&read(Path::new("/proc/self/mountinfo"), 64 * 1024)?)?;
    report(std::io::stdout(), format_args!("{SCRATCH_LIMIT_MARKER}"))?;
    let result = install(device, false, false);
    if result.is_err() {
        // The pinned mkfs reports a zeroing failure without retaining errno.
        let mut probe = File::options()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open("/scratch/exhaustion-probe")
            .map_err(|error| format!("create scratch exhaustion probe: {error}"))?;
        match probe.write_all(&[0u8; 4096]) {
            Err(error) if error.raw_os_error() == Some(28) => {
                report(
                    std::io::stdout(),
                    format_args!("{SCRATCH_EXHAUSTED_MARKER}"),
                )?;
            }
            Err(error) => return Err(format!("scratch probe failed without ENOSPC: {error}")),
            Ok(()) => return Err("scratch refusal left space for a probe page".into()),
        }
    }
    result
}

fn run() -> Result<(), String> {
    if std::process::id() != 1 {
        return Err("installation fixture must be guest PID 1".into());
    }
    directories()?;
    quiet_kernel_console(
        Path::new("/proc/sys/kernel/printk"),
        Path::new("/sys/module/printk/parameters/ignore_loglevel"),
        Path::new("/sys/module/printk/parameters/console_no_auto_verbose"),
    )?;
    match read(Path::new("/fixture-phase"), 32)?.as_slice() {
        b"install\n" => install(&target()?, false, false),
        b"install-system\n" => install(&target()?, false, true),
        b"interrupt\n" => install(&target()?, true, false),
        b"install-scratch\n" => scratch_limited_install(&target()?),
        b"protect-media\n" => protect_writable_media(&target()?),
        b"selector\n" => selector(),
        b"installed\n" => installed(),
        _ => Err("invalid fixture phase".into()),
    }
}

fn main() -> ExitCode {
    let result = run();
    if let Err(error) = result {
        let _ = report_refusal(std::io::stderr(), &error);
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
    fn scratch_mount_requires_one_observed_tmpfs_with_the_configured_size() {
        let good = "20 1 0:20 / /scratch rw,nosuid,nodev - tmpfs tmpfs rw,size=64k,mode=700\n";
        require_scratch_mount(good.as_bytes()).unwrap();
        for bad in [
            String::new(), good.repeat(2), good.replace("size=64k", "size=64m"),
            good.replace("size=64k", "size=65536k"), good.replace("size=64k,", ""),
            good.replace(" - tmpfs ", " - ext4 "), good.replace("/scratch", "/other"),
            good.replace(" - ", " "), format!("{good}20 1 0:20 / /scratch rw - ext4 /dev/vda2 rw\n"),
        ] {
            assert!(require_scratch_mount(bad.as_bytes()).is_err(), "{bad:?}");
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
    fn refusal_formatting_prepares_error_and_completion_in_one_write() {
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
        report_refusal(&mut output, &error).unwrap();
        assert_eq!(
            output.0,
            vec![format!("{REFUSED_PREFIX} {error}\n{REFUSAL_COMPLETE_MARKER}\n").into_bytes()]
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
    fn kernel_console_is_quieted_and_read_back_before_reports() {
        let dir = Scratch::new();
        let control = dir.0.join("printk");
        let override_level = dir.0.join("ignore_loglevel");
        let no_auto_verbose = dir.0.join("console_no_auto_verbose");
        fs::write(&control, b"7\t4\t1\t7\n").unwrap();
        fs::write(&override_level, b"N\n").unwrap();
        fs::write(&no_auto_verbose, b"N\n").unwrap();
        quiet_kernel_console(&control, &override_level, &no_auto_verbose).unwrap();
        // A plain fixture file retains the suffix just as procfs retains its
        // other controls; opening with truncation would lose those fields.
        assert_eq!(fs::read(&control).unwrap(), b"1\n4\t1\t7\n");
    }

    #[test]
    fn kernel_console_override_refuses_before_changing_the_level() {
        let dir = Scratch::new();
        let control = dir.0.join("printk");
        let override_level = dir.0.join("ignore_loglevel");
        let no_auto_verbose = dir.0.join("console_no_auto_verbose");
        for parameter in [&override_level, &no_auto_verbose] {
            fs::write(&override_level, b"N\n").unwrap();
            fs::write(&no_auto_verbose, b"N\n").unwrap();
            for value in [b"Y\n".as_slice(), b"N", b"N\nextra"] {
                fs::write(&control, b"7\t4\t1\t7\n").unwrap();
                fs::write(parameter, value).unwrap();
                assert!(quiet_kernel_console(&control, &override_level, &no_auto_verbose).is_err());
                assert_eq!(fs::read(&control).unwrap(), b"7\t4\t1\t7\n");
            }
            fs::remove_file(parameter).unwrap();
            assert!(quiet_kernel_console(&control, &override_level, &no_auto_verbose).is_err());
            assert_eq!(fs::read(&control).unwrap(), b"7\t4\t1\t7\n");
        }
    }

    #[test]
    fn kernel_console_requires_complete_bounded_readback() {
        let dir = Scratch::new();
        let control = dir.0.join("printk");
        let override_level = dir.0.join("ignore_loglevel");
        let no_auto_verbose = dir.0.join("console_no_auto_verbose");
        fs::write(&override_level, b"N\n").unwrap();
        fs::write(&no_auto_verbose, b"N\n").unwrap();
        for value in [
            b"7\n".as_slice(),
            b"7\t4\t1\n",
            b"7\tx\t1\t7\n",
            b"7\t4\t1\t7\t0\n",
            b"7\t4\t1\t999999999999999999999999999999999999999999999999999999999999999999999\n",
        ] {
            fs::write(&control, value).unwrap();
            assert!(quiet_kernel_console(&control, &override_level, &no_auto_verbose).is_err());
        }
    }

    #[test]
    fn kernel_console_refuses_in_bounds_integer_overflow() {
        let dir = Scratch::new();
        let control = dir.0.join("printk");
        let override_level = dir.0.join("ignore_loglevel");
        let no_auto_verbose = dir.0.join("console_no_auto_verbose");
        fs::write(&control, b"7\t4\t1\t99999999999\n").unwrap();
        fs::write(&override_level, b"N\n").unwrap();
        fs::write(&no_auto_verbose, b"N\n").unwrap();
        assert_eq!(
            quiet_kernel_console(&control, &override_level, &no_auto_verbose),
            Err("kernel console did not retain the quiet log level".into())
        );
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

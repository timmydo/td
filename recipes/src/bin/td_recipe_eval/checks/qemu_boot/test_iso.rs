//! Interactive firmware boot of an operator-supplied ISO with a private disk.
use super::*;
use std::os::unix::fs::MetadataExt;

const DISK_BYTES: u64 = 16 * 1024 * 1024 * 1024;
const USAGE: &str = "usage: test-iso ISO [--usb]";
const HELD_ISO: &str = "/proc/self/fd/0";

pub(crate) fn cli(args: &[String]) -> Result<(), String> {
    let (iso_arg, usb) = match args {
        [iso] => (iso, false),
        [iso, flag] if flag == "--usb" => (iso, true),
        _ => return Err(USAGE.into()),
    };
    let iso = Path::new(iso_arg);
    let opened = open_iso(iso)?;
    let qemu = find_qemu()?;
    let (code, vars_template) = efi::firmware(&qemu)?;
    if !env::var_os("DISPLAY").is_some_and(|value| !value.is_empty())
        && !env::var_os("WAYLAND_DISPLAY").is_some_and(|value| !value.is_empty())
    {
        return Err(
            "test-iso requires a graphical host display (DISPLAY or WAYLAND_DISPLAY)".into(),
        );
    }
    let accelerator = acceleration()?;
    let scratch = private_scratch()?;
    let disk = scratch.dir.join("destination.raw");
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&disk)
        .map_err(|error| format!("create {}: {error}", disk.display()))?;
    file.set_len(DISK_BYTES)
        .map_err(|error| format!("size {}: {error}", disk.display()))?;
    file.sync_all()
        .map_err(|error| format!("sync {}: {error}", disk.display()))?;
    drop(file);

    eprintln!("test-iso: accelerator order: {}", accelerator.join(", "));

    let install_vars = scratch.dir.join("install-vars.fd");
    efi::copy_input(&vars_template, &install_vars)?;
    eprintln!(
        "test-iso: booting {} with a private 16 GiB sparse destination",
        iso.display()
    );
    eprintln!("test-iso: finish installation and reboot or shut down the guest to boot the destination without the ISO");
    let install_serial = scratch.dir.join("install-serial.log");
    eprintln!(
        "test-iso: installer serial log: {}",
        install_serial.display()
    );
    let status = command(
        &qemu,
        &code,
        &install_vars,
        &disk,
        &install_serial,
        accelerator,
        Some(usb),
    )
    .stdin(Stdio::from(opened))
    .status()
    .map_err(|error| format!("start QEMU installer: {error}"))?;
    if !status.success() {
        return Err(qemu_failure("installer", status, &install_serial));
    }

    eprint!("test-iso: type boot to start the destination, or press Enter to stop: ");
    std::io::stderr()
        .flush()
        .map_err(|error| format!("write boot prompt: {error}"))?;
    let mut answer = String::new();
    let read = std::io::stdin()
        .read_line(&mut answer)
        .map_err(|error| format!("read boot choice: {error}"))?;
    if read == 0 || answer.trim() != "boot" {
        eprintln!("test-iso: installed-disk boot skipped");
        return Ok(());
    }

    let installed_vars = scratch.dir.join("installed-vars.fd");
    efi::copy_input(&vars_template, &installed_vars)?;
    eprintln!(
        "test-iso: booting the destination with fresh firmware variables and no installation media"
    );
    let installed_serial = scratch.dir.join("installed-serial.log");
    eprintln!(
        "test-iso: installed serial log: {}",
        installed_serial.display()
    );
    let status = command(
        &qemu,
        &code,
        &installed_vars,
        &disk,
        &installed_serial,
        accelerator,
        None,
    )
    .stdin(Stdio::null())
    .status()
    .map_err(|error| format!("start QEMU installed disk: {error}"))?;
    if !status.success() {
        return Err(qemu_failure("installed disk", status, &installed_serial));
    }
    eprintln!("test-iso: QEMU sessions ended");
    Ok(())
}

fn qemu_failure(phase: &str, status: ExitStatus, serial_path: &Path) -> String {
    const TAIL_BYTES: u64 = 8192;
    let mut detail = format!("QEMU {phase} exited {status}");
    let tail = (|| -> Result<Vec<u8>, std::io::Error> {
        let mut log = File::open(serial_path)?;
        let len = log.metadata()?.len();
        log.seek(SeekFrom::Start(len.saturating_sub(TAIL_BYTES)))?;
        let mut bytes = Vec::new();
        log.take(TAIL_BYTES).read_to_end(&mut bytes)?;
        Ok(bytes)
    })();
    match tail {
        Ok(bytes) if !bytes.is_empty() => {
            detail.push_str("; serial log tail:\n");
            detail.push_str(&String::from_utf8_lossy(&bytes));
        }
        Ok(_) => detail.push_str("; serial log is empty"),
        Err(error) => detail.push_str(&format!("; cannot read serial log: {error}")),
    }
    detail
}

fn open_iso(iso: &Path) -> Result<File, String> {
    let metadata =
        fs::symlink_metadata(iso).map_err(|error| format!("inspect {}: {error}", iso.display()))?;
    if !metadata.file_type().is_file() || metadata.len() == 0 {
        return Err(format!(
            "{} must be a nonempty regular ISO file",
            iso.display()
        ));
    }
    let opened = File::open(iso).map_err(|error| format!("open {}: {error}", iso.display()))?;
    let held = opened
        .metadata()
        .map_err(|error| format!("stat opened {}: {error}", iso.display()))?;
    if !held.is_file()
        || held.len() == 0
        || held.dev() != metadata.dev()
        || held.ino() != metadata.ino()
    {
        return Err(format!(
            "{} changed while opening its regular ISO file",
            iso.display()
        ));
    }
    Ok(opened)
}

fn acceleration() -> Result<&'static [&'static str], String> {
    let kvm = std::env::consts::ARCH == "x86_64"
        && OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/kvm")
            .is_ok();
    let forced = match env::var("TD_QEMU_ACCEL") {
        Ok(value) => value,
        Err(env::VarError::NotPresent) => String::new(),
        Err(error) => return Err(format!("TD_QEMU_ACCEL: {error}")),
    };
    match forced.trim() {
        "tcg" => Ok(&["tcg"]),
        "kvm" if kvm => Ok(&["kvm"]),
        "kvm" => Err("TD_QEMU_ACCEL=kvm requires usable x86-64 /dev/kvm".into()),
        "" if kvm => Ok(&["kvm", "tcg"]),
        "" => Ok(&["tcg"]),
        _ => Err(format!("TD_QEMU_ACCEL={forced:?} must be kvm or tcg")),
    }
}

fn private_scratch() -> Result<Scratch, String> {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let base = match env::var_os("TMPDIR") {
        Some(value) if !value.is_empty() => PathBuf::from(value),
        _ => Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .ok_or("cannot locate repository target directory")?
            .join("target"),
    };
    for _ in 0..64 {
        let sequence = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = base.join(format!("td-test-iso-{}-{sequence}", std::process::id()));
        match fs::DirBuilder::new().mode(0o700).create(&dir) {
            Ok(()) => return Ok(Scratch { dir }),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(format!("create {}: {error}", dir.display())),
        }
    }
    Err(format!(
        "could not create private scratch under {}",
        base.display()
    ))
}

fn command(
    qemu: &str,
    code: &Path,
    vars: &Path,
    disk: &Path,
    serial_path: &Path,
    accelerator: &[&str],
    media: Option<bool>,
) -> Command {
    let mut command = Command::new(qemu);
    let mut serial = OsString::from("file:");
    serial.push(serial_path.as_os_str());
    let mut destination_arg = drive_arg_with_id(disk, false, "destination");
    destination_arg.push(",werror=report,rerror=report");
    command
        .args(["-M", "q35", "-cpu", "Nehalem"])
        .args(["-smp", "2", "-m", "4096"])
        .args(["-no-reboot", "-no-user-config", "-monitor", "none"])
        .args(["-vga", "none", "-device", "virtio-vga"])
        .args(["-device", "virtio-tablet-pci", "-nic", "none"])
        .arg("-serial")
        .arg(serial)
        .arg("-drive")
        .arg(efi::pflash_arg(code, 0, true))
        .arg("-drive")
        .arg(efi::pflash_arg(vars, 1, false))
        .arg("-drive")
        .arg(destination_arg)
        .args([
            "-device",
            "virtio-blk-pci,drive=destination,serial=TD-TEST-ISO-TARGET",
        ]);
    for name in accelerator {
        command.args(["-accel", name]);
    }
    attach_system_audio(&mut command, None);
    if let Some(usb) = media {
        let iso = Path::new(HELD_ISO);
        if usb {
            command.args(["-device", "qemu-xhci,id=media-xhci"]);
            command
                .arg("-drive")
                .arg(drive_arg_with_id(iso, true, "media"));
            command.args([
                "-device",
                "usb-storage,bus=media-xhci.0,drive=media,removable=on,bootindex=1",
            ]);
        } else {
            command.arg("-drive").arg(media::optical_drive_arg(iso));
            command.args(["-boot", "order=d"]);
        }
    } else {
        command.args(["-boot", "order=c"]);
    }
    command
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_nonempty_regular_iso_files_are_opened() {
        let scratch = private_scratch().unwrap();
        let empty = scratch.dir.join("empty.iso");
        let image = scratch.dir.join("image.iso");
        let link = scratch.dir.join("link.iso");
        fs::write(&empty, []).unwrap();
        fs::write(&image, b"iso").unwrap();
        symlink(&image, &link).unwrap();
        assert!(open_iso(&scratch.dir).is_err());
        assert!(open_iso(&empty).is_err());
        assert!(open_iso(&link).is_err());
        assert_eq!(open_iso(&image).unwrap().metadata().unwrap().len(), 3);
    }

    #[test]
    fn boot_phases_keep_source_read_only_and_detach_it() {
        let qemu = "qemu-system-x86_64";
        let code = Path::new("/firmware/code");
        let vars = Path::new("/firmware/vars");
        let disk = Path::new("/private/disk");
        let serial = Path::new("/private/serial.log");
        for usb in [false, true] {
            let install = command(qemu, code, vars, disk, serial, &["tcg"], Some(usb));
            let args: Vec<_> = install
                .get_args()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect();
            assert!(args
                .iter()
                .any(|arg| arg.contains("file=/proc/self/fd/0") && arg.contains("readonly=on")));
            assert!(args
                .iter()
                .any(|arg| arg.contains("id=destination") && !arg.contains("readonly=on")));
            assert!(!args.iter().any(|arg| arg == "-kernel" || arg == "-initrd"));
            assert!(args.windows(2).any(|pair| pair == ["-nic", "none"]));
            assert_eq!(args.iter().filter(|arg| arg.contains(HELD_ISO)).count(), 1);
        }
        let installed = command(qemu, code, vars, disk, serial, &["tcg"], None);
        let args: Vec<_> = installed
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert!(!args.iter().any(|arg| arg.contains("/proc/self/fd/0")));
        assert!(args.windows(2).any(|pair| pair == ["-boot", "order=c"]));

        let fallback = command(qemu, code, vars, disk, serial, &["kvm", "tcg"], None);
        let args: Vec<_> = fallback
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        let accelerators: Vec<_> = args
            .windows(2)
            .filter(|pair| pair.first().is_some_and(|arg| arg == "-accel"))
            .filter_map(|pair| pair.get(1).cloned())
            .collect();
        assert_eq!(accelerators, ["kvm", "tcg"]);
    }
}

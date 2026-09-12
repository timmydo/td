//! Host firmware oracle; its disposable disk never names an operator device.
use super::*;
const DISK_BYTES: u64 = 6 * 1024 * 1024 * 1024;
const NEGATIVE_SECONDS: u64 = 20;
const MAX_INPUT: u64 = 256 * 1024 * 1024;

pub(crate) fn run(runner: &RecipeCheckRunner) -> Result<(), String> {
    let qemu = find_qemu()?;
    let (code, vars_template) = firmware(&qemu)?;
    let (kernel, initramfs) = build_kernel(runner)?;
    runner.prepare_recipe_target("td-install")?;
    let build_out = runner.build_plan("td-install")?;
    let installer = runner
        .ladder_out_from(&build_out, "td-install")?
        .join("bin/td-install");
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let scratch = Scratch {
        dir: create_scratch_dir(runner.scratch_dir(), &SEQ)?,
    };
    println!("   [qemu-boot-uefi] kernel: {}\n      initramfs: {}\n      firmware: {}\n      vars template: {}",
        kernel.display(), initramfs.display(), code.display(), vars_template.display());
    for (phase, present) in [("boot", true), ("reboot", true), ("missing", false)] {
        let disk = scratch
            .dir
            .join(if present { "boot.img" } else { "missing.img" });
        if phase != "reboot" {
            write_disk(&installer, &disk, &kernel, &initramfs, present)?;
        }
        let vars = scratch.dir.join(format!("{phase}-vars.fd"));
        copy_input(&vars_template, &vars)?;
        println!("   [qemu-boot-uefi] {phase}: cold firmware boot, EFI entry present: {present}");
        let result = boot_source(
            &qemu,
            BootSource::Firmware {
                code: &code,
                vars: &vars,
                attachment: FirmwareAttachment::Virtio,
            },
            BootPlan {
                disk: Some(BootDisk {
                    path: &disk,
                    read_only: phase != "boot",
                }),
                mem: "512",
                target_marker: MARKER,
                kill_on_marker: true,
                extra_append: "",
                user_net: false,
                audio: false,
                physical_input: false,
                capture_firefox_audio: false,
                tpm_socket: None,
            },
            &scratch.dir,
            if present {
                boot_timeout()
            } else {
                Duration::from_secs(NEGATIVE_SECONDS)
            },
        )?;
        if result.evidence.target != present {
            return Err(format!(
                "UEFI entry present={present}, userspace marker={} — {}\n{}",
                result.evidence.target,
                result.reason,
                tail(&result.console, 60),
            ));
        }
        // A missing executable should leave firmware running, not crash QEMU.
        if !present
            && (result.exited_clean
                || result.elapsed < Duration::from_secs(NEGATIVE_SECONDS)
                || !missing_entry_observed(&result.console))
        {
            return Err(format!("missing-entry firmware did not complete the expected refusal (elapsed {:?}) — {}\n{}",
                result.elapsed, result.reason, tail(&result.console, 60)));
        }
    }
    println!("PASS: x86-64 UEFI boots td-install's GPT/FAT BOOTX64.EFI and INITRD twice to {MARKER}; missing-entry control has no userspace marker");
    Ok(())
}

fn missing_entry_observed(console: &str) -> bool {
    console.lines().any(|line| {
        line.contains("BdsDxe: failed to load Boot")
            && line.contains("\"UEFI Misc Device\"")
            && line.ends_with(": Not Found")
    })
}

pub(super) fn firmware(qemu: &str) -> Result<(PathBuf, PathBuf), String> {
    match (
        env::var_os("TD_QEMU_EFI_CODE"),
        env::var_os("TD_QEMU_EFI_VARS"),
    ) {
        (Some(code), Some(vars)) => checked_firmware(code.into(), vars.into()),
        (None, None) => {
            let executable = fs::canonicalize(qemu).map_err(|e| format!("resolve {qemu}: {e}"))?;
            let prefix = executable
                .parent()
                .and_then(Path::parent)
                .ok_or("qemu has no installation prefix")?;
            let share = prefix.join("share/qemu");
            let candidates = [
                (
                    share.join("edk2-x86_64-code.fd"),
                    share.join("edk2-i386-vars.fd"),
                ),
                (
                    PathBuf::from("/usr/share/OVMF/OVMF_CODE_4M.fd"),
                    PathBuf::from("/usr/share/OVMF/OVMF_VARS_4M.fd"),
                ),
                (
                    PathBuf::from("/usr/share/OVMF/OVMF_CODE.fd"),
                    PathBuf::from("/usr/share/OVMF/OVMF_VARS.fd"),
                ),
            ];
            for (code, vars) in candidates {
                if code.is_file() && vars.is_file() {
                    return checked_firmware(code, vars);
                }
            }
            Err("UEFI firmware missing; set TD_QEMU_EFI_CODE and TD_QEMU_EFI_VARS to a matching non-Secure-Boot x86-64 firmware pair".into())
        }
        _ => Err("set both TD_QEMU_EFI_CODE and TD_QEMU_EFI_VARS, or neither".into()),
    }
}

fn checked_firmware(code: PathBuf, vars: PathBuf) -> Result<(PathBuf, PathBuf), String> {
    input(&code)?;
    input(&vars)?;
    Ok((code, vars))
}

pub(super) fn input(path: &Path) -> Result<(File, u64), String> {
    let file = File::open(path).map_err(|e| format!("open {}: {e}", path.display()))?;
    let meta = file
        .metadata()
        .map_err(|e| format!("stat {}: {e}", path.display()))?;
    if !meta.is_file() || meta.len() == 0 || meta.len() > MAX_INPUT {
        return Err(format!(
            "{} must be a nonempty regular file of at most 256 MiB",
            path.display()
        ));
    }
    Ok((file, meta.len()))
}

pub(super) fn copy_input(source: &Path, destination: &Path) -> Result<(), String> {
    let (file, len) = input(source)?;
    let mut out = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(destination)
        .map_err(|e| format!("create {}: {e}", destination.display()))?;
    let count = std::io::copy(&mut file.take(len), &mut out).map_err(|e| e.to_string())?;
    if count != len {
        return Err("firmware template shortened during copy".into());
    }
    out.sync_all().map_err(|e| e.to_string())
}

fn write_disk(
    installer: &Path,
    path: &Path,
    kernel: &Path,
    initramfs: &Path,
    present: bool,
) -> Result<(), String> {
    // The source-built installer receives only an exclusively created scratch
    // file. Firmware subsequently reads exactly those bytes, with no media.
    let out = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(|e| format!("create {}: {e}", path.display()))?;
    out.set_len(DISK_BYTES)
        .map_err(|e| format!("size {}: {e}", path.display()))?;
    drop(out);
    let mut command = Command::new(installer);
    command
        .arg("layout")
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::null());
    if present {
        command.arg(kernel).arg(initramfs);
    }
    let status = command
        .status()
        .map_err(|e| format!("run {}: {e}", installer.display()))?;
    if !status.success() {
        return Err(format!("td-install EFI layout failed: {status}"));
    }
    Ok(())
}

pub(super) fn pflash_arg(path: &Path, unit: u8, read_only: bool) -> OsString {
    let mut arg = OsString::from(format!(
        "if=pflash,format=raw,unit={unit},readonly={},file=",
        if read_only { "on" } else { "off" }
    ));
    let mut escaped = Vec::new();
    for &byte in path.as_os_str().as_bytes() {
        if byte == b',' {
            escaped.push(b',');
        }
        escaped.push(byte);
    }
    arg.push(OsString::from_vec(escaped));
    arg
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn negative_requires_the_firmware_disk_lookup_refusal() {
        assert!(missing_entry_observed("BdsDxe: failed to load Boot0001 \"UEFI Misc Device\" from PciRoot(0x0)/Pci(0x2,0x0): Not Found\r\n"));
        assert!(!missing_entry_observed(""));
        assert!(!missing_entry_observed("UEFI Interactive Shell v2.2"));
        assert!(!missing_entry_observed("BdsDxe: failed to load Boot0001 \"UEFI Misc Device\" from PciRoot(0x0): Security Violation"));
    }

    #[test]
    fn flash_paths_preserve_literal_commas_and_units() {
        assert_eq!(
            pflash_arg(Path::new("/scratch/a,b/code.fd"), 0, true),
            "if=pflash,format=raw,unit=0,readonly=on,file=/scratch/a,,b/code.fd"
        );
        assert_eq!(
            pflash_arg(Path::new("/scratch/vars.fd"), 1, false),
            "if=pflash,format=raw,unit=1,readonly=off,file=/scratch/vars.fd"
        );
    }

    #[test]
    fn disk_creation_refuses_an_existing_destination() {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let scratch = Scratch {
            dir: create_scratch_dir(&env::temp_dir(), &SEQ).unwrap(),
        };
        let source = scratch.dir.join("source");
        let disk = scratch.dir.join("disk");
        fs::write(&source, b"input").unwrap();
        fs::write(&disk, b"preserve").unwrap();
        assert!(write_disk(
            Path::new("/does-not-exist/td-install"),
            &disk,
            &source,
            &source,
            true
        )
        .is_err());
        assert_eq!(fs::read(&disk).unwrap(), b"preserve");
    }
}

//! Cold firmware boots of the same disposable hybrid image as optical and USB.
use super::*;
pub(super) use crate::iso_image::{write_image, write_image_with_payloads};

pub(crate) fn run(runner: &RecipeCheckRunner) -> Result<(), String> {
    let qemu = find_qemu()?;
    let (code, template) = efi::firmware(&qemu)?;
    let (kernel, initramfs) = build_kernel(runner)?;
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let scratch = Scratch {
        dir: create_scratch_dir(runner.scratch_dir(), &SEQ)?,
    };
    let disk = scratch.dir.join("boot.iso");
    write_image(&disk, &kernel, &initramfs)?;
    println!(
        "   [qemu-boot-media] kernel: {}\n      initramfs: {}\n      firmware: {}\n      vars template: {}",
        kernel.display(),
        initramfs.display(),
        code.display(),
        template.display()
    );
    for (name, attachment) in [
        ("optical", FirmwareAttachment::Optical),
        ("usb", FirmwareAttachment::Usb),
    ] {
        let vars = scratch.dir.join(format!("{name}-vars.fd"));
        efi::copy_input(&template, &vars)?;
        println!(
            "   [qemu-boot-media] cold {name} boot of {}",
            disk.display()
        );
        let result = boot_source(
            &qemu,
            BootSource::Firmware {
                code: &code,
                vars: &vars,
                attachment,
                installation_target: None,
            },
            BootPlan {
                disk: Some(BootDisk {
                    path: &disk,
                    read_only: true,
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
            boot_timeout(),
        )?;
        if !result.evidence.target {
            return Err(format!(
                "hybrid {name} boot failed — {}\n{}",
                result.reason,
                tail(&result.console, 60)
            ));
        }
    }
    println!("PASS: the same hybrid ISO boots through optical and USB firmware to {MARKER}");
    Ok(())
}

pub(super) fn optical_drive_arg(path: &Path) -> OsString {
    let mut arg = OsString::from("format=raw,media=cdrom,readonly=on,file=");
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
    fn optical_paths_preserve_commas_and_non_utf8_bytes() {
        let path = PathBuf::from(OsString::from_vec(b"/a,\xff.iso".to_vec()));
        assert_eq!(
            optical_drive_arg(&path).as_bytes(),
            b"format=raw,media=cdrom,readonly=on,file=/a,,\xff.iso"
        );
    }

    #[test]
    fn writable_media_plans_refuse_before_opening_files_or_spawning_qemu() {
        for attachment in [FirmwareAttachment::Optical, FirmwareAttachment::Usb] {
            let absent = Path::new("/td-media-test-must-not-open");
            let result = boot_source(
                "/td-media-test-must-not-execute",
                BootSource::Firmware {
                    code: absent,
                    vars: absent,
                    attachment,
                    installation_target: None,
                },
                BootPlan {
                    disk: Some(BootDisk {
                        path: absent,
                        read_only: false,
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
                absent,
                Duration::from_secs(1),
            );
            assert!(matches!(result, Err(message) if message ==
                "optical and USB media oracles require read-only disks"));
        }
    }
}

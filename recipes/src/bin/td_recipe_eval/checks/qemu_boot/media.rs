//! Cold firmware boots of the same disposable hybrid image as optical and USB.
use super::*;
use td_engine::{fat, gpt, iso9660};

const ESP_BYTES: u64 = 64 * 1024 * 1024;

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
    println!("   [qemu-boot-media] kernel: {}\n      initramfs: {}\n      firmware: {}\n      vars template: {}",
        kernel.display(), initramfs.display(), code.display(), template.display());
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

pub(super) fn write_image(path: &Path, kernel: &Path, initramfs: &Path) -> Result<(), String> {
    let (kernel, kernel_len) = efi::input(kernel)?;
    let (initramfs, initramfs_len) = efi::input(initramfs)?;
    let image = iso9660::build(&iso9660::Volume {
        disk_guid: gpt::Guid([0x41; 16]),
        esp_guid: gpt::Guid([0x42; 16]),
        esp_bytes: ESP_BYTES,
        files: Vec::new(),
    })?;
    let esp = fat::build(&fat::Volume {
        bytes_per_sector: 512,
        total_sectors: ESP_BYTES / 512,
        hidden_sectors: u32::try_from(image.esp_offset / 512)
            .map_err(|_| "ESP offset exceeds FAT field")?,
        volume_id: 0x54444953,
        label: "TD ISO TEST".into(),
        sectors_per_cluster: None,
        root: vec![(
            "EFI".into(),
            fat::Node::Dir(vec![(
                "BOOT".into(),
                fat::Node::Dir(vec![
                    (
                        td_recipe::ladder::EFI_BOOT_FILE.into(),
                        fat::Node::Stream(kernel_len),
                    ),
                    ("INITRD".into(), fat::Node::Stream(initramfs_len)),
                ]),
            )]),
        )],
    })
    .map_err(|e| format!("64 MiB media test ESP: {e}"))?;
    let mut out = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(|e| format!("create {}: {e}", path.display()))?;
    let write = |out: &mut File, offset: u64, bytes: &[u8]| -> Result<(), String> {
        out.seek(SeekFrom::Start(offset))
            .map_err(|e| e.to_string())?;
        out.write_all(bytes).map_err(|e| e.to_string())
    };
    // Exclusive creation plus extension supplies zeroes for every unused byte.
    out.set_len(image.total_bytes).map_err(|e| e.to_string())?;
    for extent in &image.extents {
        write(&mut out, extent.offset, &extent.bytes)?;
    }
    for extent in &esp.extents {
        write(&mut out, image.esp_offset + extent.offset, &extent.bytes)?;
    }
    let boot_path = format!(r"\EFI\BOOT\{}", td_recipe::ladder::EFI_BOOT_FILE);
    for placement in &esp.placements {
        let source = match placement.path.as_str() {
            td_recipe::ladder::EFI_INITRD_PATH => &initramfs,
            name if name == boot_path => &kernel,
            _ => return Err(format!("unexpected ESP file {}", placement.path)),
        };
        out.seek(SeekFrom::Start(image.esp_offset + placement.offset))
            .map_err(|e| e.to_string())?;
        copy_exact(source, &mut out, placement.len)?;
    }
    out.sync_all()
        .map_err(|e| format!("sync {}: {e}", path.display()))
}

fn copy_exact(source: impl Read, out: &mut impl Write, len: u64) -> Result<(), String> {
    let count = std::io::copy(&mut source.take(len), out).map_err(|e| e.to_string())?;
    if count != len {
        return Err("ISO input shortened during copy".into());
    }
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
    fn streams_refuse_short_inputs_and_copy_only_declared_bytes() {
        let mut out = Vec::new();
        assert!(copy_exact(b"short".as_slice(), &mut out, 6).is_err());
        out.clear();
        copy_exact(b"payload suffix".as_slice(), &mut out, 7).unwrap();
        assert_eq!(out, b"payload");
    }

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

    #[test]
    fn image_creation_preserves_existing_destination() {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let scratch = Scratch {
            dir: create_scratch_dir(&env::temp_dir(), &SEQ).unwrap(),
        };
        let source = scratch.dir.join("source");
        let disk = scratch.dir.join("disk");
        fs::write(&source, b"input").unwrap();
        fs::write(&disk, b"preserve").unwrap();
        let error = write_image(&disk, &source, &source).unwrap_err();
        assert!(error.starts_with(&format!("create {}:", disk.display())));
        assert_eq!(fs::read(&disk).unwrap(), b"preserve");
    }
}

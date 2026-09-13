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

pub(super) fn write_image(path: &Path, kernel: &Path, initramfs: &Path) -> Result<(), String> {
    write_image_with_payloads(path, kernel, initramfs, &[])
}

/// Sources belong to the host oracle's private staging tree and remain stable.
pub(super) fn write_image_with_payloads(
    path: &Path,
    kernel: &Path,
    initramfs: &Path,
    payloads: &[(&str, PathBuf)],
) -> Result<(), String> {
    let (kernel, kernel_len) = efi::input(kernel)?;
    let (initramfs, initramfs_len) = efi::input(initramfs)?;
    let mut files = Vec::new();
    let mut inputs = Vec::new();
    for (name, payload_path) in payloads {
        let (file, len) = efi::input(payload_path)?;
        files.push(iso9660::FileSpec {
            name: (*name).into(),
            len,
        });
        inputs.push((*name, file));
    }
    let image = iso9660::build(&iso9660::Volume {
        disk_guid: gpt::Guid([0x41; 16]),
        esp_guid: gpt::Guid([0x42; 16]),
        esp_bytes: ESP_BYTES,
        files,
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
    for placement in &image.placements {
        let (_, source) = inputs
            .iter()
            .find(|(name, _)| *name == placement.name)
            .ok_or_else(|| format!("missing ISO source {}", placement.name))?;
        out.seek(SeekFrom::Start(placement.offset))
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
    fn iso_payloads_stream_to_their_named_extents_with_zero_padding() {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let scratch = Scratch {
            dir: create_scratch_dir(&env::temp_dir(), &SEQ).unwrap(),
        };
        let source = scratch.dir.join("boot");
        fs::write(&source, b"boot bytes").unwrap();
        let mut payloads = Vec::new();
        for (name, bytes) in [
            ("Z.IMG", vec![0x5a; 4097]),
            ("A.IMG", vec![0x41; 1300]),
            ("M.IMG", vec![0x4d; 2048]),
        ] {
            let path = scratch.dir.join(name);
            fs::write(&path, bytes).unwrap();
            payloads.push((name, path));
        }
        let disk = scratch.dir.join("payload.iso");
        write_image_with_payloads(&disk, &source, &source, &payloads).unwrap();
        let metadata = iso9660::build(&iso9660::Volume {
            disk_guid: gpt::Guid([0x41; 16]),
            esp_guid: gpt::Guid([0x42; 16]),
            esp_bytes: ESP_BYTES,
            files: vec![
                iso9660::FileSpec {
                    name: "A.IMG".into(),
                    len: 1300,
                },
                iso9660::FileSpec {
                    name: "Z.IMG".into(),
                    len: 4097,
                },
                iso9660::FileSpec {
                    name: "M.IMG".into(),
                    len: 2048,
                },
            ],
        })
        .unwrap();
        let mut file = File::open(&disk).unwrap();
        assert_eq!(file.metadata().unwrap().len(), metadata.total_bytes);
        for placement in metadata.placements {
            let expected = fs::read(scratch.dir.join(&placement.name)).unwrap();
            file.seek(SeekFrom::Start(placement.offset)).unwrap();
            let mut got = vec![0; expected.len()];
            file.read_exact(&mut got).unwrap();
            assert_eq!(got, expected);
            let padding_len = (iso9660::BLOCK - placement.len % iso9660::BLOCK) % iso9660::BLOCK;
            let mut padding = vec![1; padding_len as usize];
            file.read_exact(&mut padding).unwrap();
            assert!(padding.iter().all(|byte| *byte == 0));
        }
    }

    #[test]
    fn invalid_or_missing_payload_refuses_before_output_creation() {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let scratch = Scratch {
            dir: create_scratch_dir(&env::temp_dir(), &SEQ).unwrap(),
        };
        let source = scratch.dir.join("source");
        fs::write(&source, b"input").unwrap();
        let disk = scratch.dir.join("never-created.iso");
        for payloads in [
            vec![("ABSENT", scratch.dir.join("absent"))],
            vec![("EFI.IMG", source.clone())],
            vec![("DUP", source.clone()), ("DUP.", source.clone())],
        ] {
            assert!(write_image_with_payloads(&disk, &source, &source, &payloads).is_err());
            assert!(!disk.exists());
        }
    }

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

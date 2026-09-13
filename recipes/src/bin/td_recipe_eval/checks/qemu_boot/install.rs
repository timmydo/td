//! Native guest installation into an exclusively created, disposable QEMU disk.
use super::*;
use td_engine::cpio::{Entry, Kind};

pub(super) use td_recipe::td_install_qemu_protocol as protocol;

pub(super) const TARGET_DRIVE_ID: &str = "install-target";

/// Only this module can create a writable installation target, in owned scratch.
pub(super) struct TargetDisk {
    path: PathBuf,
}

impl TargetDisk {
    fn copy_volume_identity(&self, source: &TargetDisk) -> Result<(), String> {
        let mut input = File::open(&source.path).map_err(|error| error.to_string())?;
        // The formatter's fixed GPT profile places the volume after the ESP.
        let offset = td_boot_protocol::PARTITION_ALIGN_BYTES + td_boot_protocol::ESP_BYTES + 65536;
        input
            .seek(SeekFrom::Start(offset))
            .map_err(|error| error.to_string())?;
        let mut superblock = [0; 4096];
        input
            .read_exact(&mut superblock)
            .map_err(|error| error.to_string())?;
        if superblock.get(64..72) != Some(b"_BHRfS_M".as_slice()) {
            return Err("fixture did not find the installed Btrfs superblock".into());
        }
        let mut output = OpenOptions::new()
            .write(true)
            .open(&self.path)
            .map_err(|error| error.to_string())?;
        output
            .seek(SeekFrom::Start(65536))
            .map_err(|error| error.to_string())?;
        output
            .write_all(&superblock)
            .and_then(|()| output.sync_all())
            .map_err(|error| error.to_string())
    }

    fn create(scratch: &Path, name: &str) -> Result<Self, String> {
        let path = scratch.join(name);
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .map_err(|e| format!("create {}: {e}", path.display()))?;
        file.set_len(6 * 1024 * 1024 * 1024)
            .map_err(|e| format!("size {}: {e}", path.display()))?;
        Ok(Self { path })
    }
}

pub(super) fn target_drive_arg(target: &TargetDisk) -> OsString {
    let mut arg = OsString::from(format!("if=none,format=raw,id={TARGET_DRIVE_ID},file="));
    let mut bytes = Vec::new();
    for &byte in target.path.as_os_str().as_bytes() {
        if byte == b',' {
            bytes.push(b',');
        }
        bytes.push(byte);
    }
    arg.push(OsString::from_vec(bytes));
    arg
}

const OUTPUTS: &[&str] = &[
    "td-install-qemu-test",
    "linux-x86-64",
    "td-install",
    "td-init",
    "td-boot",
    "td-kexec",
    "btrfs-progs-x86-64",
];

fn read(path: &Path) -> Result<Vec<u8>, String> {
    let (file, len) = efi::input(path)?;
    let mut bytes = Vec::new();
    file.take(len + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("read {}: {e}", path.display()))?;
    if bytes.len() as u64 != len {
        return Err(format!("{} changed length", path.display()));
    }
    Ok(bytes)
}

fn write(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(|e| format!("create {}: {e}", path.display()))?;
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|e| format!("write {}: {e}", path.display()))
}

type PackedFile = (String, u32, Vec<u8>);

fn initramfs(
    base: &[u8],
    common: &[PackedFile],
    phase: &str,
    extra: &[PackedFile],
) -> Result<Vec<u8>, String> {
    let mut entries = Vec::new();
    for name in key_path_parents() {
        entries.push(Entry {
            name,
            mode: 0o755,
            kind: Kind::Directory,
        });
    }
    entries.push(Entry {
        name: "fixture-phase",
        mode: 0o644,
        kind: Kind::File(phase.as_bytes()),
    });
    for (name, mode, bytes) in common.iter().chain(extra) {
        entries.push(Entry {
            name,
            mode: *mode,
            kind: Kind::File(bytes),
        });
    }
    let mut image = base.to_vec();
    image.resize(
        image.len() + td_engine::cpio::alignment_padding(image.len()),
        0,
    );
    image.extend_from_slice(&td_engine::cpio::build(&entries)?);
    Ok(image)
}

pub(crate) fn run(runner: &RecipeCheckRunner) -> Result<(), String> {
    let qemu = find_qemu()?;
    let timeout = installation_timeout(env::var("TD_QEMU_BOOT_TIMEOUT_SECS").ok().as_deref());
    let (code, vars_template) = efi::firmware(&qemu)?;
    let outputs = runner.build_and_stage("td-install-qemu-test", OUTPUTS)?;
    let [probe, linux, installer, init, boot, kexec, btrfs] = outputs.as_slice() else {
        return Err("installation fixture output roster mismatch".into());
    };
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let scratch = Scratch {
        dir: create_scratch_dir(runner.scratch_dir(), &SEQ)?,
    };
    let kernel = linux.join("bzImage");
    let base = read(&linux.join("initramfs.cpio"))?;
    let mut common = Vec::new();
    for (name, source) in [
        ("init", probe.join("bin/td-install-qemu-test")),
        ("bin/td-init", init.join("bin/td-init")),
        ("bin/mount", init.join("bin/td-init")),
        ("bin/umount", init.join("bin/td-init")),
        ("bin/losetup", init.join("bin/td-init")),
        ("bin/td-boot", boot.join("bin/td-boot")),
        ("bin/td-kexec", kexec.join("bin/td-kexec")),
    ] {
        common.push((name.to_owned(), 0o755, read(&source)?));
    }
    let trust = RunTrust::generate()?;
    let installed = initramfs(&base, &common, "installed\n", &[])?;
    let deployment = scratch.dir.join("source");
    fs::create_dir(&deployment).map_err(|e| format!("create deployment: {e}"))?;
    write(&deployment.join("bzImage"), &read(&kernel)?)?;
    write(&deployment.join("initramfs.cpio"), &installed)?;
    let root = scratch.dir.join("root");
    fs::create_dir(&root).map_err(|e| format!("create fixture root: {e}"))?;
    write(&root.join("installed.txt"), b"td installation fixture\n")?;
    let status = runner
        .builder_command()
        .arg("mkfs-erofs")
        .arg(&root)
        .arg(deployment.join("root.erofs"))
        .status()
        .map_err(|e| format!("build fixture EROFS: {e}"))?;
    if !status.success() {
        return Err(format!("fixture EROFS build failed: {status}"));
    }
    let mut manifest = String::from("td-deployment-v1\n");
    for name in ["bzImage", "initramfs.cpio", "root.erofs"] {
        let digest = crate::sha256::sha256_file(&deployment.join(name))
            .map_err(|e| format!("hash {name}: {e}"))?;
        manifest.push_str(&format!("{digest}  {name}\n"));
    }
    write(&deployment.join("manifest"), manifest.as_bytes())?;
    trust.sign_deployment(&deployment)?;
    verify_deployment(&deployment)?;
    let id = crate::sha256::sha256_file(&deployment.join("manifest"))
        .map_err(|e| format!("hash manifest: {e}"))?;
    let key = trust.trusted_key_line();
    let selector = initramfs(
        &base,
        &common,
        "selector\n",
        &[(
            td_boot_protocol::TRUSTED_KEY_PATH.into(),
            0o644,
            key.clone(),
        )],
    )?;
    write(&scratch.dir.join("selector.cpio"), &selector)?;
    let mut extra = vec![
        (
            "bin/td-install".into(),
            0o755,
            read(&installer.join("bin/td-install"))?,
        ),
        (
            "bin/mkfs.btrfs".into(),
            0o755,
            read(&btrfs.join("bin/mkfs.btrfs"))?,
        ),
        ("trusted.pub".into(), 0o644, key),
    ];
    let payloads: Vec<_> = protocol::MEDIA_FILES
        .iter()
        .map(|(iso_name, name)| (*iso_name, scratch.dir.join(name)))
        .collect();
    let live = scratch.dir.join("installer.cpio");
    write(&live, &initramfs(&base, &common, "install\n", &extra)?)?;
    let iso = scratch.dir.join("installer.iso");
    media::write_image_with_payloads(&iso, &kernel, &live, &payloads)?;
    for (name, attachment, source_device) in [
        ("optical", FirmwareAttachment::Optical, "/dev/sr0"),
        ("usb", FirmwareAttachment::Usb, "/dev/sda"),
    ] {
        let target = TargetDisk::create(&scratch.dir, &format!("{name}.img"))?;
        let vars = scratch.dir.join(format!("{name}-install-vars.fd"));
        efi::copy_input(&vars_template, &vars)?;
        println!("   [qemu-install] installing through {name} media");
        let result = boot_source(
            &qemu,
            BootSource::Firmware {
                code: &code,
                vars: &vars,
                attachment,
                installation_target: Some(&target),
            },
            plan(&iso, true, protocol::INSTALL_MARKER),
            &scratch.dir,
            timeout,
        )?;
        require(&result, protocol::INSTALL_MARKER, "guest installation")?;
        let media_evidence = format!("{} {source_device}", protocol::MEDIA_MARKER);
        if !result
            .console
            .lines()
            .any(|line| line.trim_end() == media_evidence)
        {
            return Err("guest did not prove read-only ISO payload access".into());
        }
        let duplicate = TargetDisk::create(&scratch.dir, &format!("{name}-duplicate.img"))?;
        duplicate.copy_volume_identity(&target)?;
        let vars = scratch.dir.join(format!("{name}-duplicate-vars.fd"));
        efi::copy_input(&vars_template, &vars)?;
        let refused =
            "TD-INSTALL-REFUSED: volume resolution failed: td-boot: ambiguous td volume identity";
        println!("   [qemu-install] refusing duplicate volume identity before selection");
        let result = boot_source(
            &qemu,
            BootSource::Firmware {
                code: &code,
                vars: &vars,
                attachment: FirmwareAttachment::InstalledFixtureReordered,
                installation_target: Some(&duplicate),
            },
            plan(&target.path, false, refused),
            &scratch.dir,
            timeout,
        )?;
        require(&result, refused, "duplicate volume refusal")?;
        if result.evidence.selected_current || result.evidence.selected_previous {
            return Err("ambiguous volume reached deployment selection".into());
        }
        let mut previous_identity = None;
        for (count, marker) in [
            (1, protocol::FIRST_BOOT_MARKER),
            (2, protocol::SECOND_BOOT_MARKER),
        ] {
            let decoy = if count == 2 {
                Some(TargetDisk::create(
                    &scratch.dir,
                    &format!("{name}-decoy.img"),
                )?)
            } else {
                None
            };
            let vars = scratch.dir.join(format!("{name}-boot-{count}-vars.fd"));
            efi::copy_input(&vars_template, &vars)?;
            println!("   [qemu-install] cold installed boot {count}, {name} media detached");
            let expected = format!("{marker} {id}");
            let result = boot_source(
                &qemu,
                BootSource::Firmware {
                    code: &code,
                    vars: &vars,
                    attachment: if count == 2 {
                        FirmwareAttachment::InstalledFixtureReordered
                    } else {
                        FirmwareAttachment::InstalledFixture
                    },
                    installation_target: decoy.as_ref(),
                },
                plan(&target.path, false, &expected),
                &scratch.dir,
                timeout,
            )?;
            require(&result, marker, "installed boot")?;
            require(&result, "TD-INSTALL-STALE-MOUNT-RECOVERED", "closed-descriptor mount recovery")?;
            let expected_device = if count == 2 { "/dev/vdb2" } else { "/dev/vda2" };
            let discovered: Vec<_> = result
                .console
                .lines()
                .map(str::trim_end)
                .filter_map(|line| line.strip_prefix("TD-INSTALL-VOLUME "))
                .collect();
            if discovered.len() != 2
                || discovered.first() != discovered.last()
                || !discovered
                    .first()
                    .is_some_and(|line| line.ends_with(expected_device))
            {
                return Err(format!(
                    "selector/deployment did not resolve the same UUID on {expected_device}"
                ));
            }
            let identity = discovered
                .first()
                .and_then(|line| line.split_once(' '))
                .map(|(uuid, _)| uuid.to_owned())
                .ok_or("missing resolved UUID")?;
            if previous_identity
                .as_ref()
                .is_some_and(|previous| previous != &identity)
            {
                return Err("volume UUID changed across cold boots".into());
            }
            previous_identity = Some(identity);
            if !result.evidence.selected_current
                || result.evidence.selected_previous
                || result.evidence.bookkeeping_unavailable
                || result.evidence.selected_current_id.as_deref() != Some(id.as_str())
            {
                return Err(format!("installed selector did not choose current {id}"));
            }
            if !result
                .console
                .lines()
                .any(|line| line.trim_end() == expected)
            {
                return Err(format!(
                    "installed boot did not report expected deployment {id}"
                ));
            }
        }
    }
    // A second public key must refuse the otherwise identical signed source.
    let wrong_key = RunTrust::generate()?.trusted_key_line();
    let mut replaced = 0;
    for (name, _, bytes) in &mut extra {
        if name == "trusted.pub" {
            *bytes = wrong_key.clone();
            replaced += 1;
        }
    }
    if replaced != 1 {
        return Err("wrong-key fixture has no unique trust input".into());
    }
    let bad_live = scratch.dir.join("wrong-key.cpio");
    write(&bad_live, &initramfs(&base, &common, "install\n", &extra)?)?;
    let bad_iso = scratch.dir.join("wrong-key.iso");
    media::write_image_with_payloads(&bad_iso, &kernel, &bad_live, &payloads)?;
    let target = TargetDisk::create(&scratch.dir, "wrong-key.img")?;
    let vars = scratch.dir.join("wrong-key-vars.fd");
    efi::copy_input(&vars_template, &vars)?;
    let refused = boot_source(
        &qemu,
        BootSource::Firmware {
            code: &code,
            vars: &vars,
            attachment: FirmwareAttachment::Optical,
            installation_target: Some(&target),
        },
        plan(&bad_iso, true, protocol::REFUSED_PREFIX),
        &scratch.dir,
        timeout,
    )?;
    require(&refused, protocol::REFUSED_PREFIX, "wrong-key installation")?;
    if !refused
        .console
        .contains(td_boot_protocol::MANIFEST_UNAUTHENTICATED)
        || refused.console.contains(protocol::INSTALL_MARKER)
    {
        return Err(format!(
            "wrong-key fixture did not prove authentication refusal\n{}",
            tail(&refused.console, 80)
        ));
    }
    println!(
        "PASS: native optical/USB installation; UUID discovery across verified kexec and reordered disks; duplicate identity and wrong-key refusals; two persistent installed boots"
    );
    Ok(())
}

fn installation_timeout(value: Option<&str>) -> Duration {
    value
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|seconds| *seconds > 0)
        .map(Duration::from_secs)
        .unwrap_or(Duration::from_secs(180))
}

fn plan<'a>(path: &'a Path, read_only: bool, marker: &'a str) -> BootPlan<'a> {
    BootPlan {
        disk: Some(BootDisk { path, read_only }),
        mem: "2048",
        target_marker: marker,
        kill_on_marker: true,
        extra_append: "",
        user_net: false,
        audio: false,
        physical_input: false,
        capture_firefox_audio: false,
        tpm_socket: None,
    }
}

fn require(result: &BootResult, marker: &str, phase: &str) -> Result<(), String> {
    if result.evidence.target {
        Ok(())
    } else {
        Err(format!(
            "{phase} did not reach {marker}: {}; selected current={:?}, previous={}\n{}",
            result.reason,
            result.evidence.selected_current_id,
            result.evidence.selected_previous,
            tail(&result.console, 160)
        ))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn installation_deadline_defaults_and_accepts_positive_overrides() {
        for value in [None, Some(""), Some("0"), Some("invalid"), Some("-1")] {
            assert_eq!(installation_timeout(value), Duration::from_secs(180));
        }
        assert_eq!(installation_timeout(Some("60")), Duration::from_secs(60));
        assert_eq!(installation_timeout(Some("7200")), Duration::from_secs(7200));
    }

    #[test]
    fn diagnostic_recipe_is_outside_the_system_closure() {
        let system = crate::check_runner::recipe_closure(&["system-x86-64"]).unwrap();
        assert!(!system.iter().any(|node| node.stem == "td-install-qemu-test"));
        let recipe = td_recipe::catalog::lookup("td-install-qemu-test").unwrap();
        assert!(recipe.checks.is_none());
    }

    #[test]
    fn target_creation_is_exclusive_and_drive_paths_escape_commas() {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let scratch = Scratch {
            dir: create_scratch_dir(&env::temp_dir(), &SEQ).unwrap(),
        };
        let target = TargetDisk::create(&scratch.dir, "a,b.img").unwrap();
        fs::write(&target.path, b"preserve").unwrap();
        assert!(TargetDisk::create(&scratch.dir, "a,b.img").is_err());
        assert_eq!(fs::read(&target.path).unwrap(), b"preserve");
        let arg = target_drive_arg(&target);
        let text = arg.to_str().unwrap();
        assert!(text.starts_with("if=none,format=raw,id=install-target,file="));
        assert!(text.ends_with("/a,,b.img"));
    }

    #[test]
    fn additional_target_requires_source_media_before_any_firmware_io() {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let scratch = Scratch {
            dir: create_scratch_dir(&env::temp_dir(), &SEQ).unwrap(),
        };
        let target = TargetDisk::create(&scratch.dir, "target.img").unwrap();
        let absent = Path::new("/td-install-fixture-absent");
        let result = boot_source(
            "/td-install-fixture-no-qemu",
            BootSource::Firmware {
                code: absent,
                vars: absent,
                attachment: FirmwareAttachment::Virtio,
                installation_target: Some(&target),
            },
            plan(absent, true, protocol::INSTALL_MARKER),
            &scratch.dir,
            Duration::from_secs(1),
        );
        assert_eq!(
            result.err().unwrap(),
            "an installation target requires optical or USB source media"
        );
    }
}

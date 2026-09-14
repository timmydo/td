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
    fn fingerprint(&self) -> Result<(u64, String), String> {
        let len = fs::metadata(&self.path)
            .map_err(|error| format!("stat {}: {error}", self.path.display()))?
            .len();
        let digest = crate::sha256::sha256_file(&self.path)
            .map_err(|error| format!("hash {}: {error}", self.path.display()))?;
        Ok((len, digest))
    }

    fn seed_preservation_canaries(&self) -> Result<(), String> {
        let mut file = OpenOptions::new()
            .write(true)
            .open(&self.path)
            .map_err(|error| format!("open {}: {error}", self.path.display()))?;
        let bytes = b"existing installation target contents\n";
        let len = file
            .metadata()
            .map_err(|error| format!("stat {}: {error}", self.path.display()))?
            .len();
        let end = len.checked_sub(bytes.len() as u64).ok_or_else(|| {
            format!(
                "{} has {len} bytes; canaries require at least {}",
                self.path.display(),
                bytes.len()
            )
        })?;
        for offset in [0, end / 2, end] {
            file.seek(SeekFrom::Start(offset))
                .and_then(|_| file.write_all(bytes))
                .map_err(|error| format!("seed {} at {offset}: {error}", self.path.display()))?;
        }
        file.sync_all()
            .map_err(|error| format!("sync {}: {error}", self.path.display()))
    }

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
    let uuid = installation_uuid(&trust.public);
    let uuid_line = format!("{uuid}\n").into_bytes();
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
        &[
            (td_boot_protocol::TRUSTED_KEY_PATH.into(), 0o644, key.clone()),
            (td_boot_protocol::VOLUME_UUID_PATH.into(), 0o644, uuid_line.clone()),
        ],
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
        (td_boot_protocol::VOLUME_UUID_PATH.into(), 0o644, uuid_line),
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
        let partition_evidence = format!("{} {uuid} /dev/vda2", protocol::PARTITIONS_MARKER);
        if !result.console.lines().any(|line| line.trim_end() == partition_evidence) {
            return Err("guest did not prove refreshed partitions and busy-disk refusal".into());
        }
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
            require(&result, &expected, "installed boot")?;
            require(&result, "TD-INSTALL-STALE-MOUNT-RECOVERED", "closed-descriptor mount recovery")?;
            let expected_device = if count == 2 { "/dev/vdb2" } else { "/dev/vda2" };
            let discovered: Vec<_> = result
                .console
                .lines()
                .map(str::trim_end)
                .filter_map(|line| line.strip_prefix("TD-INSTALL-VOLUME "))
                .collect();
            let expected_identity = format!("{uuid} {expected_device}");
            if discovered.len() != 2
                || discovered.iter().any(|line| *line != expected_identity)
            {
                return Err(format!(
                    "selector/deployment did not resolve {expected_identity}: {discovered:?}\n{}",
                    tail(&result.console, 80)
                ));
            }
            if !bound_selector_before_selection(&result.console, &expected_identity, &id) {
                return Err(format!(
                    "installed selector did not bind {expected_identity} before selecting {id}\n{}",
                    tail(&result.console, 80)
                ));
            }
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
    let refuse = |case: &str, image: &Path, diagnostic: &str| -> Result<(), String> {
        for (name, attachment, source_device) in [
            ("optical", FirmwareAttachment::Optical, "/dev/sr0"),
            ("usb", FirmwareAttachment::Usb, "/dev/sda"),
        ] {
            let target = TargetDisk::create(&scratch.dir, &format!("{case}-{name}.img"))?;
            target.seed_preservation_canaries()?;
            let before = target.fingerprint()?;
            let vars = scratch.dir.join(format!("{case}-{name}-vars.fd"));
            efi::copy_input(&vars_template, &vars)?;
            println!("   [qemu-install] refusing {case} {name} media before disk writes");
            let refused = boot_source(
                &qemu,
                BootSource::Firmware {
                    code: &code,
                    vars: &vars,
                    attachment,
                    installation_target: Some(&target),
                },
                plan(image, true, protocol::REFUSED_PREFIX),
                &scratch.dir,
                timeout,
            )?;
            // boot_source has reaped QEMU; compare all bytes, including sparse gaps.
            if target.fingerprint()? != before {
                return Err(format!(
                    "{case} {name} installation changed the destination\n{}",
                    tail(&refused.console, 80)
                ));
            }
            require(
                &refused,
                &format!("{} {source_device}", protocol::MEDIA_MARKER),
                "refused media access",
            )?;
            if !refused.evidence.target
                || !refused.console.lines().any(|line| {
                    line.strip_prefix(protocol::REFUSED_PREFIX)
                        .is_some_and(|rest| rest.starts_with(' '))
                })
                || !refused.console.contains(diagnostic)
                || refused.console.contains(protocol::INSTALL_MARKER)
            {
                return Err(format!(
                    "{case} {name} fixture did not prove {diagnostic}\n{}",
                    tail(&refused.console, 80)
                ));
            }
        }
        Ok(())
    };
    // Retain the authentic manifest/signature and replace only one payload.
    let corrupt_root = scratch.dir.join("corrupt-root.erofs");
    write(&corrupt_root, b"tampered installation payload\n")?;
    let mut corrupt_payloads = payloads.clone();
    let root_payload = corrupt_payloads
        .iter_mut()
        .find(|(name, _)| *name == "ROOT.EROFS")
        .ok_or("installation media has no root payload")?;
    root_payload.1 = corrupt_root;
    let corrupt_iso = scratch.dir.join("corrupt-root.iso");
    media::write_image_with_payloads(&corrupt_iso, &kernel, &live, &corrupt_payloads)?;
    refuse("corrupt-root", &corrupt_iso, "root.erofs hash mismatch:")?;
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
    refuse(
        "wrong-key",
        &bad_iso,
        td_boot_protocol::MANIFEST_UNAUTHENTICATED,
    )?;
    println!(
        "PASS: native optical/USB installation; provisioned UUID binding across verified kexec and reordered disks; duplicate identity refusal; wrong-key and corrupt-payload optical/USB refusals preserve every target byte; two persistent installed boots"
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

fn bound_selector_before_selection(console: &str, identity: &str, id: &str) -> bool {
    let bound = format!("TD-BOOT-VOLUME {identity}");
    let selected = format!("TD-BOOT-SELECTED-CURRENT {id}");
    let binding = console.lines().position(|line| line.trim_end() == bound);
    let selection = console.lines().position(|line| line.trim_end() == selected);
    matches!((binding, selection), (Some(binding), Some(selection)) if binding < selection)
}

fn require(result: &BootResult, marker: &str, phase: &str) -> Result<(), String> {
    if result.evidence.target && result.console.lines().any(|line| line.trim_end() == marker) {
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
    fn a_later_bound_mount_cannot_stand_in_for_the_selector() {
        let binding = "TD-BOOT-VOLUME uuid /dev/vda2\n";
        let selection = "TD-BOOT-SELECTED-CURRENT deployment\n";
        assert!(bound_selector_before_selection(
            &format!("{binding}{selection}"),
            "uuid /dev/vda2",
            "deployment"
        ));
        assert!(!bound_selector_before_selection(
            &format!("{selection}{binding}"),
            "uuid /dev/vda2",
            "deployment"
        ));
        assert!(!bound_selector_before_selection(
            binding,
            "uuid /dev/vda2",
            "deployment"
        ));
        assert!(!bound_selector_before_selection(
            &format!("{binding}{selection}"),
            "other /dev/vda2",
            "deployment"
        ));
    }

    #[test]
    fn a_named_marker_is_required_even_after_the_boot_target_was_reached() {
        let mut result = BootResult {
            evidence: ConsoleEvidence {
                target: true,
                ..ConsoleEvidence::default()
            },
            exited_clean: false,
            marker_killed: true,
            reason: "fixture".into(),
            console: "TD-INSTALL-PERSISTED-1 deployment\n".into(),
            elapsed: Duration::ZERO,
            firefox_audio: FirefoxAudioCapture::NotRequested,
        };
        assert!(require(&result, "TD-INSTALL-STALE-MOUNT-RECOVERED", "recovery").is_err());
        assert!(require(&result, "TD-INSTALL-PERSISTED-1 deployment", "installed").is_ok());
        result.console =
            "noiseTD-INSTALL-STALE-MOUNT-RECOVERED\nTD-INSTALL-STALE-MOUNT-RECOVERED-extra\n".into();
        assert!(require(&result, "TD-INSTALL-STALE-MOUNT-RECOVERED", "recovery").is_err());
        result.console = "TD-INSTALL-STALE-MOUNT-RECOVERED extra\n".into();
        assert!(require(&result, "TD-INSTALL-STALE-MOUNT-RECOVERED", "recovery").is_err());
        result.console = "TD-INSTALL-STALE-MOUNT-RECOVERED\r\n".into();
        assert!(require(&result, "TD-INSTALL-STALE-MOUNT-RECOVERED", "recovery").is_ok());
    }

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
    fn refusal_fingerprints_cover_canaries_sparse_gaps_and_length() {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let scratch = Scratch {
            dir: create_scratch_dir(&env::temp_dir(), &SEQ).unwrap(),
        };
        let target = TargetDisk::create(&scratch.dir, "preservation.img").unwrap();
        let mut file = OpenOptions::new().write(true).open(&target.path).unwrap();
        file.set_len(1024 * 1024).unwrap();
        target.seed_preservation_canaries().unwrap();
        let before = target.fingerprint().unwrap();
        for offset in [0, 32768, 512 * 1024, 768 * 1024, 1024 * 1024 - 1] {
            file.seek(SeekFrom::Start(offset)).unwrap();
            file.write_all(b"!").unwrap();
            assert_ne!(target.fingerprint().unwrap(), before, "offset {offset}");
            file.set_len(0).unwrap();
            file.set_len(1024 * 1024).unwrap();
            target.seed_preservation_canaries().unwrap();
            assert_eq!(target.fingerprint().unwrap(), before);
        }
        file.set_len(1024 * 1024 + 1).unwrap();
        assert_ne!(target.fingerprint().unwrap(), before);
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

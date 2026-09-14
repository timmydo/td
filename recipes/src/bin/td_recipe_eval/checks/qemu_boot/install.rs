//! Native guest installation into an exclusively created, disposable QEMU disk.
use super::*;
use td_engine::cpio::{Entry, Kind};

pub(super) use td_recipe::td_install_qemu_protocol as protocol;

pub(super) const TARGET_DRIVE_ID: &str = "install-target";
const MINIMUM_TARGET_BYTES: u64 = 6 * 1024 * 1024 * 1024;

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
        Self::with_capacity(scratch, name, MINIMUM_TARGET_BYTES)
    }

    fn with_capacity(scratch: &Path, name: &str, bytes: u64) -> Result<Self, String> {
        let path = scratch.join(name);
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .map_err(|e| format!("create {}: {e}", path.display()))?;
        file.set_len(bytes)
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

struct LiveInstaller {
    kernel: PathBuf,
    base: Vec<u8>,
    common: Vec<PackedFile>,
    extra: Vec<PackedFile>,
    uuid: String,
}

impl LiveInstaller {
    fn load(runner: &RecipeCheckRunner, trust: &RunTrust) -> Result<Self, String> {
        let outputs = runner.build_and_stage("td-install-qemu-test", OUTPUTS)?;
        let [probe, linux, installer, init, boot, kexec, btrfs] = outputs.as_slice() else {
            return Err("installation fixture output roster mismatch".into());
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
        let uuid = installation_uuid(&trust.public);
        let uuid_line = format!("{uuid}\n").into_bytes();
        let extra = vec![
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
            ("trusted.pub".into(), 0o644, trust.trusted_key_line()),
            (td_boot_protocol::VOLUME_UUID_PATH.into(), 0o644, uuid_line),
        ];
        Ok(Self {
            kernel,
            base,
            common,
            extra,
            uuid,
        })
    }
}

pub(crate) fn run(runner: &RecipeCheckRunner) -> Result<(), String> {
    let qemu = find_qemu()?;
    let timeout = installation_timeout(env::var("TD_QEMU_BOOT_TIMEOUT_SECS").ok().as_deref(), 180);
    let (code, vars_template) = efi::firmware(&qemu)?;
    let trust = RunTrust::generate()?;
    let LiveInstaller {
        kernel,
        base,
        common,
        mut extra,
        uuid,
    } = LiveInstaller::load(runner, &trust)?;
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let scratch = Scratch {
        dir: create_scratch_dir(runner.scratch_dir(), &SEQ)?,
    };
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
            (td_boot_protocol::TRUSTED_KEY_PATH.into(), 0o644, key),
            (td_boot_protocol::VOLUME_UUID_PATH.into(), 0o644, uuid_line),
        ],
    )?;
    write(&scratch.dir.join("selector.cpio"), &selector)?;
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
        require_installation(&result, &uuid, source_device)?;
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
            require(
                &result,
                "TD-INSTALL-STALE-MOUNT-RECOVERED",
                "closed-descriptor mount recovery",
            )?;
            let expected_device = if count == 2 { "/dev/vdb2" } else { "/dev/vda2" };
            let discovered: Vec<_> = result
                .console
                .lines()
                .map(str::trim_end)
                .filter_map(|line| line.strip_prefix("TD-INSTALL-VOLUME "))
                .collect();
            let expected_identity = format!("{uuid} {expected_device}");
            if discovered.len() != 2 || discovered.iter().any(|line| *line != expected_identity) {
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

/// Install the stock system through firmware, then retain identity on a cold reboot.
pub(crate) fn run_system(runner: &RecipeCheckRunner) -> Result<(), String> {
    let qemu = find_qemu()?;
    let timeout = installation_timeout(env::var("TD_QEMU_BOOT_TIMEOUT_SECS").ok().as_deref(), 900);
    let (code, vars_template) = efi::firmware(&qemu)?;
    let (_, selector, source) = build_system(runner)?;
    let trust = RunTrust::generate()?;
    let LiveInstaller {
        kernel,
        base,
        common,
        extra,
        uuid,
    } = LiveInstaller::load(runner, &trust)?;
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let scratch = Scratch {
        dir: create_scratch_dir(runner.scratch_dir(), &SEQ)?,
    };
    let deployment = scratch.dir.join("source");
    fs::create_dir(&deployment).map_err(|error| format!("create system deployment: {error}"))?;
    let mut payload_bytes = 0u64;
    for name in ["bzImage", "initramfs.cpio", "root.erofs", "manifest"] {
        let copied = fs::copy(source.join(name), deployment.join(name))
            .map_err(|error| format!("stage system {name}: {error}"))?;
        payload_bytes = payload_bytes
            .checked_add(copied)
            .ok_or("system payload length overflow")?;
    }
    trust.sign_deployment(&deployment)?;
    verify_deployment(&deployment)?;
    let id = crate::sha256::sha256_file(&deployment.join("manifest"))
        .map_err(|error| format!("hash system manifest: {error}"))?;
    let provisioned = provision_selector(&selector, &scratch.dir, &trust)?;
    efi::copy_input(&provisioned, &scratch.dir.join("selector.cpio"))?;
    let payloads: Vec<_> = protocol::MEDIA_FILES
        .iter()
        .map(|(iso_name, name)| (*iso_name, scratch.dir.join(name)))
        .collect();
    let live = scratch.dir.join("installer.cpio");
    write(&live, &initramfs(&base, &common, "install\n", &extra)?)?;
    let iso = scratch.dir.join("installer.iso");
    media::write_image_with_payloads(&iso, &kernel, &live, &payloads)?;
    let capacity = system_target_capacity(payload_bytes)?;
    let mut previous_installation = None;
    for (name, attachment, source_device) in [
        ("optical", FirmwareAttachment::Optical, "/dev/sr0"),
        ("usb", FirmwareAttachment::Usb, "/dev/sda"),
    ] {
        let target = TargetDisk::with_capacity(&scratch.dir, &format!("{name}.img"), capacity)?;
        let vars = scratch.dir.join(format!("{name}-install-vars.fd"));
        efi::copy_input(&vars_template, &vars)?;
        println!("   [qemu-install-system] installing {payload_bytes} bytes through {name} media");
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
        require_installation(&result, &uuid, source_device)?;
        let mut first = None;
        for count in 1..=2 {
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
            let mut boot_plan = plan(&target.path, false, SYSTEM_BOOT_SUCCESS_MARKER);
            // Stock audio supervision needs the emulated sound device.
            boot_plan.audio = true;
            println!("   [qemu-install-system] cold system boot {count}, {name} media detached");
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
                boot_plan,
                &scratch.dir,
                timeout,
            )?;
            let device = if count == 2 { "/dev/vdb2" } else { "/dev/vda2" };
            validate_installed_system(&result, &uuid, device, &id, count == 1)?;
            if let Some(first) = &first {
                require_same_identity(
                    first,
                    &result,
                    "first installed boot",
                    "second installed boot",
                )?;
            } else {
                if let Some(previous) = &previous_installation {
                    require_distinct_identity(
                        previous,
                        &result,
                        "optical installation",
                        "USB installation",
                    )?;
                }
                first = Some(result);
            }
        }
        previous_installation = first;
    }
    println!("PASS: stock system installed offline through optical/USB ISO firmware; immutable root, compositor page flips, acknowledged deployment and stable machine identity across reordered cold boots");
    Ok(())
}

fn system_target_capacity(payload_bytes: u64) -> Result<u64, String> {
    let alignment = td_boot_protocol::PARTITION_ALIGN_BYTES;
    payload_bytes
        .checked_add(2 * 1024 * 1024 * 1024)
        .and_then(|bytes| bytes.checked_add(td_boot_protocol::ESP_BYTES))
        .and_then(|bytes| bytes.checked_add(2 * alignment))
        .and_then(|bytes| bytes.checked_add(alignment - 1))
        .map(|bytes| (bytes / alignment * alignment).max(MINIMUM_TARGET_BYTES))
        .ok_or_else(|| "system installation capacity overflow".into())
}

fn validate_installed_system(
    result: &BootResult,
    uuid: &str,
    device: &str,
    id: &str,
    fresh: bool,
) -> Result<(), String> {
    require(
        result,
        SYSTEM_BOOT_SUCCESS_MARKER,
        "installed system health",
    )?;
    if result.evidence.kernel_panic
        || !result.evidence.boot_success
        || result.evidence.bookkeeping_unavailable
        || result.evidence.attempts_exhausted
    {
        return Err(format!("installed system did not acknowledge a healthy deployment: panic={}, success={}, bookkeeping unavailable={}, attempts exhausted={}\n{}", result.evidence.kernel_panic, result.evidence.boot_success, result.evidence.bookkeeping_unavailable, result.evidence.attempts_exhausted, tail(&result.console, 100)));
    }
    validate_primary_selection(result, "installed system")?;
    if result.evidence.selected_current_id.as_deref() != Some(id)
        || !bound_selector_before_selection(&result.console, &format!("{uuid} {device}"), id)
    {
        return Err(format!(
            "installed system did not bind {uuid} {device} and deployment {id}: selected={:?}\n{}",
            result.evidence.selected_current_id,
            tail(&result.console, 100)
        ));
    }
    for (name, present) in [
        ("greeter", result.evidence.greeter),
        ("read-only root", result.evidence.root_read_only),
        ("read-only configuration", result.evidence.etc_read_only),
        (
            "persistent identity configuration",
            result.evidence.etc_mutable,
        ),
        ("writable state", result.evidence.state_writable),
        ("state ownership", result.evidence.state_owner),
        ("principals", result.evidence.principals_enrolled),
        (
            "private compositor devices",
            result.evidence.compositor_devices_private,
        ),
    ] {
        if !present {
            return Err(format!(
                "installed system lacks {name} evidence\n{}",
                tail(&result.console, 100)
            ));
        }
    }
    if result.evidence.firstboot_new != fresh
        || result.evidence.firstboot_stable == fresh
        || result.evidence.host_key.is_none()
    {
        return Err(format!("installed system identity: expected fresh={fresh}, new={}, stable={}, host key present={}\n{}", result.evidence.firstboot_new, result.evidence.firstboot_stable, result.evidence.host_key.is_some(), tail(&result.console, 100)));
    }
    validate_compositor_boot(result)
}

fn require_installation(
    result: &BootResult,
    uuid: &str,
    source_device: &str,
) -> Result<(), String> {
    require(result, protocol::INSTALL_MARKER, "guest installation")?;
    if !result
        .console
        .lines()
        .any(|line| line.trim_end() == protocol::DIRECT_MARKER)
    {
        return Err("guest did not report direct publication after staging checks".into());
    }
    let partition_evidence = format!("{} {uuid} /dev/vda2", protocol::PARTITIONS_MARKER);
    if !result
        .console
        .lines()
        .any(|line| line.trim_end() == partition_evidence)
    {
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
    Ok(())
}

fn installation_timeout(value: Option<&str>, default_secs: u64) -> Duration {
    value
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|seconds| *seconds > 0)
        .map(Duration::from_secs)
        .unwrap_or(Duration::from_secs(default_secs))
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

    fn healthy_system() -> BootResult {
        let mut evidence = ConsoleEvidence {
            target: true,
            boot_success: true,
            selected_current: true,
            selected_current_id: Some("deployment".into()),
            greeter: true,
            root_read_only: true,
            etc_read_only: true,
            etc_mutable: true,
            state_writable: true,
            state_owner: true,
            principals_enrolled: true,
            compositor_devices_private: true,
            firstboot_new: true,
            host_key: Some("ssh-ed25519 AAAA".into()),
            ..ConsoleEvidence::default()
        };
        let display = "driver=virtio_gpu connector=Virtual-1#31 status=connected crtc=29 encoder=30 mode=1280x800@60 name=1280x800 preferred=true mm=0x0 output=1280x800";
        evidence.td_compositor_drm = Some(format!(
            "{display} buffer=1280x800 pitch=5120 bytes=4096000 mapping=ok"
        ));
        evidence.td_compositor_kms = Some(format!(
            "{display} buffer=1280x800 pitch=5120 bytes=4096000 mapping=ok fb=7 modeset=ok"
        ));
        evidence.td_compositor_flip = Some(format!(
            "{display} fb=7 modeset=ok flipfb=8 cookie=0x2 seq=41 flip=ok"
        ));
        BootResult {
            evidence, exited_clean: false, marker_killed: true,
            reason: "fixture".into(),
            console: format!("TD-BOOT-VOLUME uuid /dev/vda2\nTD-BOOT-SELECTED-CURRENT deployment\n{SYSTEM_BOOT_SUCCESS_MARKER}\n"),
            elapsed: Duration::ZERO, firefox_audio: FirefoxAudioCapture::NotRequested,
        }
    }

    #[test]
    fn installed_system_requires_bound_identity_and_complete_health() {
        let validate = |result: &BootResult| {
            validate_installed_system(result, "uuid", "/dev/vda2", "deployment", true)
        };
        assert!(validate(&healthy_system()).is_ok());
        for mutate in [
            |e: &mut ConsoleEvidence| e.target = false,
            |e: &mut ConsoleEvidence| e.kernel_panic = true,
            |e: &mut ConsoleEvidence| e.boot_success = false,
            |e: &mut ConsoleEvidence| e.bookkeeping_unavailable = true,
            |e: &mut ConsoleEvidence| e.attempts_exhausted = true,
            |e: &mut ConsoleEvidence| e.selected_current = false,
            |e: &mut ConsoleEvidence| e.selected_current_id = Some("wrong".into()),
            |e: &mut ConsoleEvidence| e.greeter = false,
            |e: &mut ConsoleEvidence| e.root_read_only = false,
            |e: &mut ConsoleEvidence| e.etc_read_only = false,
            |e: &mut ConsoleEvidence| e.etc_mutable = false,
            |e: &mut ConsoleEvidence| e.state_writable = false,
            |e: &mut ConsoleEvidence| e.state_owner = false,
            |e: &mut ConsoleEvidence| e.principals_enrolled = false,
            |e: &mut ConsoleEvidence| e.compositor_devices_private = false,
            |e: &mut ConsoleEvidence| e.firstboot_new = false,
            |e: &mut ConsoleEvidence| e.firstboot_stable = true,
            |e: &mut ConsoleEvidence| e.host_key = None,
            |e: &mut ConsoleEvidence| e.td_compositor_drm = None,
            |e: &mut ConsoleEvidence| e.td_compositor_kms = None,
            |e: &mut ConsoleEvidence| e.td_compositor_flip = None,
        ] {
            let mut result = healthy_system();
            mutate(&mut result.evidence);
            assert!(validate(&result).is_err());
        }
        for (from, to) in [
            ("uuid", "wrong"),
            ("/dev/vda2", "/dev/vdb2"),
            ("CURRENT deployment", "CURRENT wrong"),
            (
                SYSTEM_BOOT_SUCCESS_MARKER,
                &format!("noise{SYSTEM_BOOT_SUCCESS_MARKER}"),
            ),
        ] {
            let mut result = healthy_system();
            result.console = result.console.replace(from, to);
            assert!(validate(&result).is_err());
        }
    }

    #[test]
    fn installed_system_reboot_requires_stable_matching_identity() {
        let first = healthy_system();
        let mut second = healthy_system();
        assert!(
            validate_installed_system(&second, "uuid", "/dev/vda2", "deployment", false).is_err()
        );
        second.evidence.firstboot_new = false;
        second.evidence.firstboot_stable = true;
        assert!(
            validate_installed_system(&second, "uuid", "/dev/vda2", "deployment", false).is_ok()
        );
        assert!(require_same_identity(&first, &second, "first", "second").is_ok());
        assert!(require_distinct_identity(&first, &second, "optical", "USB").is_err());
        second.evidence.host_key = Some("ssh-ed25519 BBBB".into());
        assert!(require_same_identity(&first, &second, "first", "second").is_err());
        assert!(require_distinct_identity(&first, &second, "optical", "USB").is_ok());
    }

    #[test]
    fn system_disk_capacity_is_aligned_bounded_and_checked() {
        let gib = 1024 * 1024 * 1024;
        assert_eq!(system_target_capacity(0).unwrap(), 6 * gib);
        let size = system_target_capacity(7 * gib + 1).unwrap();
        assert_eq!(size % td_boot_protocol::PARTITION_ALIGN_BYTES, 0);
        assert!(size > 9 * gib + td_boot_protocol::ESP_BYTES);
        assert!(system_target_capacity(u64::MAX).is_err());
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let scratch = Scratch {
            dir: create_scratch_dir(&env::temp_dir(), &SEQ).unwrap(),
        };
        let target = TargetDisk::with_capacity(&scratch.dir, "sized.img", 4096).unwrap();
        assert_eq!(fs::metadata(&target.path).unwrap().len(), 4096);
        assert!(TargetDisk::with_capacity(&scratch.dir, "sized.img", 1).is_err());
        assert_eq!(fs::metadata(&target.path).unwrap().len(), 4096);
    }

    #[test]
    fn system_install_cli_refuses_operator_destinations() {
        for args in [
            vec!["linux-x86-64".into()],
            vec!["/dev/sda".into()],
            vec!["system-x86-64".into(), "/dev/sda".into()],
        ] {
            assert_eq!(
                crate::check_runner::qemu_install_system_cli(&args).unwrap_err(),
                "usage: qemu-install-system [system-x86-64]"
            );
        }
    }

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
            "noiseTD-INSTALL-STALE-MOUNT-RECOVERED\nTD-INSTALL-STALE-MOUNT-RECOVERED-extra\n"
                .into();
        assert!(require(&result, "TD-INSTALL-STALE-MOUNT-RECOVERED", "recovery").is_err());
        result.console = "TD-INSTALL-STALE-MOUNT-RECOVERED extra\n".into();
        assert!(require(&result, "TD-INSTALL-STALE-MOUNT-RECOVERED", "recovery").is_err());
        result.console = "TD-INSTALL-STALE-MOUNT-RECOVERED\r\n".into();
        assert!(require(&result, "TD-INSTALL-STALE-MOUNT-RECOVERED", "recovery").is_ok());
    }

    #[test]
    fn installation_deadline_defaults_and_accepts_positive_overrides() {
        for default in [180, 900] {
            for value in [None, Some(""), Some("0"), Some("invalid"), Some("-1")] {
                assert_eq!(
                    installation_timeout(value, default),
                    Duration::from_secs(default)
                );
            }
            assert_eq!(
                installation_timeout(Some("60"), default),
                Duration::from_secs(60)
            );
            assert_eq!(
                installation_timeout(Some("7200"), default),
                Duration::from_secs(7200)
            );
        }
    }

    #[test]
    fn diagnostic_recipe_is_outside_the_system_closure() {
        let system = crate::check_runner::recipe_closure(&["system-x86-64"]).unwrap();
        assert!(!system
            .iter()
            .any(|node| node.stem == "td-install-qemu-test"));
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

//! Native guest installation into an exclusively created, disposable QEMU disk.
use super::*;
use td_engine::cpio::{Entry, Kind};

pub(super) use td_recipe::td_install_qemu_protocol as protocol;

pub(super) const TARGET_DRIVE_ID: &str = "install-target";
const MINIMUM_TARGET_BYTES: u64 = 6 * 1024 * 1024 * 1024;
// Verifying the full deployment fills page cache before kexec allocates its
// control page without reclaim retries. Reserve room above the 3 GiB payload.
const INSTALLED_SYSTEM_MEMORY_MIB: &str = "4096";

/// Only this module can create a writable installation target, in owned scratch.
pub(super) struct TargetDisk {
    path: PathBuf,
    read_only: bool,
    sector_size: SectorSize,
    bus: DiskBus,
}

impl TargetDisk {
    pub(super) fn attach(
        &self,
        command: &mut Command,
        serial: &str,
        boot_index: Option<u8>,
    ) -> Result<(), String> {
        self.bus.attach(
            command,
            TARGET_DRIVE_ID,
            serial,
            boot_index,
            self.sector_size,
        )
    }

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
        Ok(Self {
            path,
            read_only: false,
            sector_size: SectorSize::Bytes512,
            bus: DiskBus::Virtio,
        })
    }
}

pub(super) fn target_drive_arg(target: &TargetDisk) -> OsString {
    let protection = if target.read_only { ",readonly=on" } else { "" };
    let mut arg = OsString::from(format!(
        "if=none,format=raw,id={TARGET_DRIVE_ID}{protection},file="
    ));
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
    "tzdata",
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
    let mut parents: std::collections::BTreeSet<String> =
        key_path_parents().into_iter().map(str::to_owned).collect();
    for (name, _, _) in common.iter().chain(extra) {
        let mut parent = Path::new(name).parent();
        while let Some(path) = parent.filter(|path| !path.as_os_str().is_empty()) {
            parents.insert(path.to_str().ok_or("non-UTF-8 fixture parent")?.to_owned());
            parent = path.parent();
        }
    }
    let mut entries = Vec::new();
    for name in &parents {
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
        let [probe, linux, installer, init, boot, kexec, btrfs, tzdata] = outputs.as_slice() else {
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
            ("trusted.pub".into(), 0o644, trust.trusted_key_line()),
            (td_boot_protocol::VOLUME_UUID_PATH.into(), 0o644, uuid_line),
        ];
        let zoneinfo = tzdata.join("share/zoneinfo");
        let catalog = td_recipe::td_install_timezones::Catalog::load(&zoneinfo)
            .map_err(|error| format!("load fixture timezone catalog: {error}"))?;
        for name in ["iso3166.tab", "zone1970.tab"]
            .into_iter()
            .chain(catalog.ids())
        {
            extra.push((
                format!("etc/zoneinfo/{name}"),
                0o644,
                read(&zoneinfo.join(name))?,
            ));
        }
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
    for (bus, sector_size) in [
        (DiskBus::Virtio, SectorSize::Bytes512),
        (DiskBus::Virtio, SectorSize::Bytes4096),
        (DiskBus::Ahci, SectorSize::Bytes512),
        (DiskBus::Nvme, SectorSize::Bytes512),
        (DiskBus::Nvme, SectorSize::Bytes4096),
    ] {
        for (name, attachment, source_device) in [
            ("optical", FirmwareAttachment::Optical, "/dev/sr0"),
            ("usb", FirmwareAttachment::Usb, "/dev/sda"),
        ] {
            let source_device = if matches!(bus, DiskBus::Ahci) && name == "usb" {
                "/dev/sdb"
            } else {
                source_device
            };
            let name = format!("{name}-{}-{}", bus.label(), sector_size.bytes());
            let mut target = TargetDisk::create(&scratch.dir, &format!("{name}.img"))?;
            target.sector_size = sector_size;
            target.bus = bus;
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
            require_installation(&result, &uuid, source_device, &target)?;
            require_live_reports(
                &result,
                &target,
                source_device,
                &iso,
                true,
                InventoryBefore::Fresh,
            )?;
            require(
                &result,
                &format!("{} {}", protocol::SECTOR_BYTES_MARKER, sector_size.bytes()),
                "target sector geometry",
            )?;
            let mut duplicate = TargetDisk::create(&scratch.dir, &format!("{name}-duplicate.img"))?;
            duplicate.sector_size = sector_size;
            duplicate.bus = bus;
            duplicate.copy_volume_identity(&target)?;
            let vars = scratch.dir.join(format!("{name}-duplicate-vars.fd"));
            efi::copy_input(&vars_template, &vars)?;
            let refused = concat!(
                "TD-INSTALL-REFUSED: volume resolution failed: ",
                "td-boot: ambiguous td volume identity"
            );
            println!("   [qemu-install] refusing duplicate volume identity before selection");
            let result = boot_source(
                &qemu,
                BootSource::Firmware {
                    code: &code,
                    vars: &vars,
                    attachment: FirmwareAttachment::InstalledFixtureReordered,
                    installation_target: Some(&duplicate),
                },
                target_plan(&target, refused),
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
                    let mut disk = TargetDisk::create(&scratch.dir, &format!("{name}-decoy.img"))?;
                    disk.sector_size = sector_size;
                    disk.bus = bus;
                    Some(disk)
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
                    target_plan(&target, &expected),
                    &scratch.dir,
                    timeout,
                )?;
                let expected_device = format!("/dev/{}", partition_name(bus.name(count == 2), 2));
                validate_fixture_boot(&result, &expected, &uuid, &expected_device, &id)?;
            }
        }
    }
    let interrupted_live = scratch.dir.join("interrupted.cpio");
    write(
        &interrupted_live,
        &initramfs(&base, &common, "interrupt\n", &extra)?,
    )?;
    let interrupted_iso = scratch.dir.join("interrupted.iso");
    media::write_image_with_payloads(&interrupted_iso, &kernel, &interrupted_live, &payloads)?;
    let kernel_bytes = fs::metadata(&kernel)
        .map_err(|error| error.to_string())?
        .len();
    for (name, attachment, source_device) in [
        ("optical", FirmwareAttachment::Optical, "/dev/sr0"),
        ("usb", FirmwareAttachment::Usb, "/dev/sda"),
    ] {
        let target = TargetDisk::create(&scratch.dir, &format!("interrupted-{name}.img"))?;
        let boot = |phase: &str, source: &Path, attachment, live: bool, marker: &str| {
            let vars = scratch
                .dir
                .join(format!("interrupted-{name}-{phase}-vars.fd"));
            efi::copy_input(&vars_template, &vars)?;
            boot_source(
                &qemu,
                BootSource::Firmware {
                    code: &code,
                    vars: &vars,
                    attachment,
                    installation_target: if live { Some(&target) } else { None },
                },
                plan(source, live, marker),
                &scratch.dir,
                timeout,
            )
        };
        println!("   [qemu-install] interrupting mounted publication through {name} media");
        let interrupted = boot(
            "copy",
            &interrupted_iso,
            attachment,
            true,
            protocol::INTERRUPTED_MARKER,
        )?;
        validate_interruption(&interrupted, kernel_bytes, &uuid, source_device)?;
        require_live_reports(
            &interrupted,
            &target,
            source_device,
            &interrupted_iso,
            true,
            InventoryBefore::Fresh,
        )?;
        println!("   [qemu-install] refusing an incomplete installation, {name} media detached");
        let refused = format!(
            "{} /bin/td-boot failed: exit status: 1",
            protocol::REFUSED_PREFIX
        );
        let broken = boot(
            "refuse",
            &target.path,
            FirmwareAttachment::InstalledFixture,
            false,
            &refused,
        )?;
        validate_interrupted_boot(&broken, &refused, &uuid)?;
        println!(
            "   [qemu-install] reinstalling after interrupted publication through {name} media"
        );
        let repaired = boot("repair", &iso, attachment, true, protocol::INSTALL_MARKER)?;
        require_installation(&repaired, &uuid, source_device, &target)?;
        require_live_reports(
            &repaired,
            &target,
            source_device,
            &iso,
            true,
            InventoryBefore::Reinstall,
        )?;
        let expected = format!("{} {id}", protocol::FIRST_BOOT_MARKER);
        let installed = boot(
            "reboot",
            &target.path,
            FirmwareAttachment::InstalledFixture,
            false,
            &expected,
        )?;
        validate_fixture_boot(&installed, &expected, &uuid, "/dev/vda2", &id)?;
    }
    for (case, bytes, read_only, diagnostic) in [
        (
            "undersized",
            td_boot_protocol::ESP_BYTES,
            false,
            "destination is too small",
        ),
        (
            "read-only",
            MINIMUM_TARGET_BYTES,
            true,
            "td-install: Operation not permitted (os error 1)",
        ),
    ] {
        for (name, attachment, source_device) in [
            ("optical", FirmwareAttachment::Optical, "/dev/sr0"),
            ("usb", FirmwareAttachment::Usb, "/dev/sda"),
        ] {
            let mut target =
                TargetDisk::with_capacity(&scratch.dir, &format!("{case}-{name}.img"), bytes)?;
            target.seed_preservation_canaries()?;
            let before = target.fingerprint()?;
            target.read_only = read_only;
            let vars = scratch.dir.join(format!("{case}-{name}-vars.fd"));
            efi::copy_input(&vars_template, &vars)?;
            println!("   [qemu-install] refusing {case} target through {name} media");
            let refused = boot_source(
                &qemu,
                BootSource::Firmware {
                    code: &code,
                    vars: &vars,
                    attachment,
                    installation_target: Some(&target),
                },
                plan(&iso, true, protocol::REFUSED_PREFIX),
                &scratch.dir,
                timeout,
            )?;
            if target.fingerprint()? != before {
                return Err(format!(
                    "{case} {name} refusal changed the target\n{}",
                    tail(&refused.console, 80)
                ));
            }
            validate_target_refusal(&refused, source_device, diagnostic)?;
            require_live_reports(
                &refused,
                &target,
                source_device,
                &iso,
                false,
                InventoryBefore::Fresh,
            )?;
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
            require_live_reports(
                &refused,
                &target,
                source_device,
                image,
                false,
                InventoryBefore::Fresh,
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
        "PASS: native optical/USB virtio/NVMe 512-byte/4Kn and AHCI 512-byte installation and interrupted-publication refusal/reinstallation; provisioned UUID binding across verified kexec and reordered disks; duplicate identity refusal; undersized/read-only targets and wrong-key/corrupt-payload optical/USB refusals preserve every target byte; two persistent installed boots"
    );
    Ok(())
}

#[derive(Clone, Copy)]
enum InventoryBefore {
    Fresh,
    Reinstall,
}

struct InventoryExpected<'a> {
    target_bytes: u64,
    source_bytes: u64,
    sector_bytes: u64,
    read_only: bool,
    source_name: &'a str,
    target_name: &'a str,
    before: InventoryBefore,
}

#[derive(Debug, Eq, PartialEq)]
struct InventoryIdentity {
    target_number: String,
    target_sequence: u64,
    source_number: String,
    source_sequence: u64,
}

fn report_field<'a>(
    value: &'a td_engine::json::Json,
    name: &str,
) -> Result<&'a td_engine::json::Json, String> {
    let td_engine::json::Json::Obj(fields) = value else {
        return Err("report value is not an object".into());
    };
    let mut found = fields.iter().filter(|(key, _)| key == name);
    let (_, value) = found.next().ok_or_else(|| format!("report lacks {name}"))?;
    if found.next().is_some() {
        return Err(format!("report duplicates {name}"));
    }
    Ok(value)
}

fn report_number(value: &td_engine::json::Json, name: &str) -> Result<u64, String> {
    let td_engine::json::Json::Num(number) = report_field(value, name)? else {
        return Err(format!("report {name} is not an integer"));
    };
    number
        .parse()
        .map_err(|_| format!("report {name} is not an unsigned integer"))
}

fn report_expect_field(
    value: &td_engine::json::Json,
    subject: &str,
    field: &str,
    expected: &td_engine::json::Json,
) -> Result<(), String> {
    let actual = report_field(value, field).map_err(|error| format!("{subject}: {error}"))?;
    if actual != expected {
        return Err(format!(
            "report {subject}.{field}: expected {expected:?}, observed {actual:?}"
        ));
    }
    Ok(())
}

fn report_expect_number(
    value: &td_engine::json::Json,
    subject: &str,
    field: &str,
    expected: u64,
) -> Result<(), String> {
    let actual = report_number(value, field).map_err(|error| format!("{subject}: {error}"))?;
    if actual != expected {
        return Err(format!(
            "report {subject}.{field}: expected {expected}, observed {actual}"
        ));
    }
    Ok(())
}

fn diagnostic_document(text: &str, limit: usize) -> Result<td_engine::json::Json, String> {
    if text.len() >= limit {
        return Err("report exceeds fixture byte limit".into());
    }
    // Bound nesting before entering the shared recursive JSON parser.
    const MAX_REPORT_DEPTH: usize = 8;
    let mut depth = 0usize;
    let mut quoted = false;
    let mut escaped = false;
    for byte in text.bytes() {
        if quoted {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                quoted = false;
            }
        } else {
            match byte {
                b'"' => quoted = true,
                b'{' | b'[' => {
                    depth += 1;
                    if depth > MAX_REPORT_DEPTH {
                        return Err("report nesting exceeds fixture limit".into());
                    }
                }
                b'}' | b']' => depth = depth.checked_sub(1).ok_or("unbalanced report JSON")?,
                _ => {}
            }
        }
    }
    td_engine::json::parse(text).map_err(|error| format!("invalid report JSON: {error}"))
}

fn diagnostic_frame(
    console: &str,
    marker: &str,
    limit: usize,
) -> Result<td_engine::json::Json, String> {
    let prefix = format!("{marker} ");
    let mut lines = console.lines().filter(|line| line.starts_with(marker));
    let line = lines.next().ok_or_else(|| format!("missing {marker}"))?;
    if lines.next().is_some() {
        return Err(format!("duplicate {marker}"));
    }
    let line = line
        .strip_prefix(&prefix)
        .ok_or_else(|| format!("malformed {marker} prefix"))?;
    let (length, payload) = line.split_once(' ').ok_or("report lacks byte framing")?;
    let length = length
        .parse::<usize>()
        .map_err(|_| "invalid report byte length")?;
    if length >= limit {
        return Err("report exceeds fixture byte limit".into());
    }
    let json = payload.get(..length).ok_or("truncated report bytes")?;
    diagnostic_document(json, limit)
}

fn inventory_snapshot(
    console: &str,
    marker: &str,
    expected: &InventoryExpected<'_>,
    partitioned: bool,
) -> Result<InventoryIdentity, String> {
    use td_engine::json::Json;
    let document = diagnostic_frame(console, marker, protocol::MAX_INVENTORY_BYTES)?;
    if report_number(&document, "version")? != 1
        || report_field(&document, "scope")?.as_str() != Some("inventory-only")
    {
        return Err("inventory has the wrong version or scope".into());
    }
    let devices = report_field(&document, "devices")?
        .as_arr()
        .ok_or("inventory devices is not an array")?;
    let mut by_name = std::collections::BTreeMap::new();
    let mut numbers = std::collections::BTreeSet::new();
    let mut target_partitions = 0usize;
    for device in devices {
        let name = report_field(device, "name")?
            .as_str()
            .ok_or("inventory device name is not text")?;
        let number = report_field(device, "device_number")?
            .as_str()
            .ok_or("inventory device number is not text")?;
        if !numbers.insert(number) {
            return Err(format!(
                "inventory {name}.device_number duplicates {number}"
            ));
        }
        match report_field(device, "parent")? {
            Json::Null => {}
            Json::Str(parent) => {
                if parent == expected.target_name {
                    target_partitions += 1;
                }
            }
            _ => return Err(format!("inventory {name}.parent is not text or null")),
        }
        if by_name.insert(name, device).is_some() {
            return Err("inventory duplicates a device name".into());
        }
    }
    let target = by_name
        .get(expected.target_name)
        .ok_or("inventory lacks the fixture target")?;
    let source = by_name
        .get(expected.source_name)
        .ok_or("inventory lacks the source media")?;
    for (name, device, capacity, read_only) in [
        (
            expected.target_name,
            target,
            expected.target_bytes,
            expected.read_only,
        ),
        (expected.source_name, source, expected.source_bytes, true),
    ] {
        report_expect_number(device, name, "capacity_bytes", capacity)?;
        report_expect_field(device, name, "read_only", &Json::Bool(read_only))?;
        report_expect_field(device, name, "parent", &Json::Null)?;
        report_expect_field(device, name, "partition_number", &Json::Null)?;
    }
    let target_disk = report_field(target, "disk")?;
    let source_disk = report_field(source, "disk")?;
    report_expect_number(
        target_disk,
        &format!("{}.disk", expected.target_name),
        "logical_sector_bytes",
        expected.sector_bytes,
    )?;
    report_expect_field(
        target_disk,
        &format!("{}.disk", expected.target_name),
        "serial",
        &Json::Str(protocol::TARGET_SERIAL.into()),
    )?;
    report_expect_number(
        source_disk,
        &format!("{}.disk", expected.source_name),
        "logical_sector_bytes",
        if expected.source_name == "sr0" {
            2048
        } else {
            512
        },
    )?;
    let identity = InventoryIdentity {
        target_number: report_field(target, "device_number")?
            .as_str()
            .ok_or("missing target device number")?
            .into(),
        target_sequence: report_number(target_disk, "sequence")?,
        source_number: report_field(source, "device_number")?
            .as_str()
            .ok_or("missing source device number")?
            .into(),
        source_sequence: report_number(source_disk, "sequence")?,
    };
    if identity.target_sequence == 0
        || identity.source_sequence == 0
        || identity.target_number == identity.source_number
    {
        return Err(format!(
            "inventory has invalid whole-disk identities: {identity:?}"
        ));
    }
    if !partitioned && matches!(expected.before, InventoryBefore::Fresh) && target_partitions != 0 {
        return Err(format!(
            "inventory fresh {} has {target_partitions} partitions",
            expected.target_name
        ));
    }
    if partitioned {
        let disk_sectors = expected
            .target_bytes
            .checked_div(expected.sector_bytes)
            .ok_or("invalid fixture target sector size")?;
        let last_usable = td_engine::gpt::last_usable_lba(expected.sector_bytes, disk_sectors)?;
        let volume_bytes = last_usable
            .checked_add(1)
            .and_then(|end| end.checked_mul(expected.sector_bytes))
            .and_then(|end| end.checked_sub(td_boot_protocol::PARTITION_ALIGN_BYTES))
            .and_then(|end| end.checked_sub(td_boot_protocol::ESP_BYTES))
            .filter(|bytes| *bytes >= td_boot_protocol::MIN_VOLUME_BYTES)
            .ok_or("invalid fixture volume capacity")?;
        if target_partitions != 2 {
            return Err(format!(
                "inventory {}: expected two target partitions, observed {target_partitions}",
                expected.target_name
            ));
        }
        for number in 1..=2 {
            let name = partition_name(expected.target_name, number);
            let partition = by_name
                .get(name.as_str())
                .ok_or_else(|| format!("inventory lacks {name} after formatting"))?;
            report_expect_field(
                partition,
                &name,
                "parent",
                &Json::Str(expected.target_name.into()),
            )?;
            report_expect_number(partition, &name, "partition_number", number)?;
            report_expect_field(partition, &name, "disk", &Json::Null)?;
            report_expect_field(partition, &name, "read_only", &Json::Bool(false))?;
            report_expect_number(
                partition,
                &name,
                "capacity_bytes",
                if number == 1 {
                    td_boot_protocol::ESP_BYTES
                } else {
                    volume_bytes
                },
            )?;
        }
    }
    Ok(identity)
}

fn validate_inventories(
    console: &str,
    expected: &InventoryExpected<'_>,
    partitioned: bool,
) -> Result<(), String> {
    let before = inventory_snapshot(console, protocol::INVENTORY_BEFORE_MARKER, expected, false)?;
    if partitioned {
        let after = inventory_snapshot(console, protocol::INVENTORY_AFTER_MARKER, expected, true)?;
        if before != after {
            return Err(format!(
                "inventory whole-disk identity changed: before {before:?}, after {after:?}"
            ));
        }
    } else if console
        .lines()
        .any(|line| line.starts_with(protocol::INVENTORY_AFTER_MARKER))
    {
        return Err("refusal unexpectedly reported a formatted inventory".into());
    }
    Ok(())
}

fn validate_preview(
    console: &str,
    capacity: u64,
    sector: u64,
    partitioned: bool,
) -> Result<(), String> {
    if !partitioned {
        if console
            .lines()
            .any(|line| line.starts_with(protocol::PREVIEW_MARKER))
        {
            return Err("refusal unexpectedly reported a layout preview".into());
        }
        return Ok(());
    }
    if !matches!(sector, 512 | 4096) || !capacity.is_multiple_of(sector) {
        return Err("invalid preview oracle geometry".into());
    }
    let document = diagnostic_frame(
        console,
        protocol::PREVIEW_MARKER,
        protocol::MAX_PREVIEW_BYTES,
    )?;
    report_expect_number(&document, "layout-preview", "version", 1)?;
    report_expect_field(
        &document,
        "layout-preview",
        "scope",
        &td_engine::json::Json::Str("layout-preview".into()),
    )?;
    report_expect_number(&document, "layout-preview", "logical_sector_bytes", sector)?;
    report_expect_number(&document, "layout-preview", "capacity_bytes", capacity)?;
    let parts = report_field(&document, "partitions")?
        .as_arr()
        .ok_or("preview partitions is not an array")?;
    if parts.len() != 2 {
        return Err("preview must describe exactly two partitions".into());
    }
    let esp_start = td_boot_protocol::PARTITION_ALIGN_BYTES / sector;
    let volume_start =
        (td_boot_protocol::PARTITION_ALIGN_BYTES + td_boot_protocol::ESP_BYTES) / sector;
    let last = td_engine::gpt::last_usable_lba(sector, capacity / sector)?;
    for (part, (number, purpose, start, end)) in parts.iter().zip([
        (1, "efi-system", esp_start, volume_start - 1),
        (2, "system-volume", volume_start, last),
    ]) {
        let bytes = end
            .checked_sub(start)
            .and_then(|v| v.checked_add(1))
            .and_then(|v| v.checked_mul(sector))
            .ok_or("invalid preview oracle partition range")?;
        report_expect_field(
            part,
            purpose,
            "purpose",
            &td_engine::json::Json::Str(purpose.into()),
        )?;
        let offset = start
            .checked_mul(sector)
            .ok_or("preview oracle offset overflow")?;
        for (field, expected) in [
            ("number", number),
            ("start_lba", start),
            ("end_lba", end),
            ("offset_bytes", offset),
            ("capacity_bytes", bytes),
        ] {
            report_expect_number(part, purpose, field, expected)?;
        }
    }
    Ok(())
}

fn require_live_reports(
    result: &BootResult,
    target: &TargetDisk,
    source_device: &str,
    iso: &Path,
    partitioned: bool,
    before: InventoryBefore,
) -> Result<(), String> {
    let expected = InventoryExpected {
        target_bytes: fs::metadata(&target.path)
            .map_err(|error| error.to_string())?
            .len(),
        source_bytes: fs::metadata(iso).map_err(|error| error.to_string())?.len(),
        sector_bytes: target.sector_size.bytes(),
        read_only: target.read_only,
        source_name: source_device
            .strip_prefix("/dev/")
            .ok_or("invalid fixture media path")?,
        target_name: target.bus.name(false),
        before,
    };
    validate_preview(
        &result.console,
        expected.target_bytes,
        expected.sector_bytes,
        partitioned,
    )
    .map_err(|error| {
        format!(
            "installer layout preview: {error}\n{}",
            tail(&result.console, 80)
        )
    })?;
    validate_inventories(&result.console, &expected, partitioned).map_err(|error| {
        format!(
            "installer inventory: {error}\n{}",
            tail(&result.console, 80)
        )
    })
}

fn validate_target_refusal(
    result: &BootResult,
    source_device: &str,
    diagnostic: &str,
) -> Result<(), String> {
    require(
        result,
        &format!("{} {source_device}", protocol::MEDIA_MARKER),
        "target refusal media access",
    )?;
    require(
        result,
        &format!(
            "{} /bin/td-install failed: exit status: 1",
            protocol::REFUSED_PREFIX
        ),
        "td-install failure",
    )?;
    let missing = !result.console.contains(diagnostic);
    let unexpected = [
        protocol::PARTITIONS_MARKER,
        protocol::DIRECT_MARKER,
        protocol::INSTALL_MARKER,
    ]
    .into_iter()
    .find(|marker| result.console.contains(marker));
    if missing
        || result.evidence.selected_current
        || result.evidence.selected_previous
        || unexpected.is_some()
    {
        return Err(format!(
            "target refusal expected={diagnostic:?}, missing={missing}, current={}, previous={}, unexpected={unexpected:?}\n{}",
            result.evidence.selected_current,
            result.evidence.selected_previous,
            tail(&result.console, 80)
        ));
    }
    Ok(())
}

fn validate_interruption(
    result: &BootResult,
    kernel_bytes: u64,
    uuid: &str,
    source_device: &str,
) -> Result<(), String> {
    let diagnostic = |reason: &str| {
        format!(
            "{reason}: expected={kernel_bytes}, target={}, killed={}, clean={}, current={}, previous={}\n{}",
            result.evidence.target, result.marker_killed, result.exited_clean,
            result.evidence.selected_current, result.evidence.selected_previous,
            tail(&result.console, 80)
        )
    };
    let prefix = format!("{} ", protocol::INTERRUPTED_MARKER);
    let mut reports = result
        .console
        .lines()
        .filter_map(|line| line.strip_prefix(&prefix));
    let report = reports
        .next()
        .ok_or_else(|| diagnostic("guest did not prove partial publication"))?;
    if reports.next().is_some() {
        return Err("duplicate interruption evidence".into());
    }
    let words: Vec<_> = report.split_ascii_whitespace().collect();
    let [written, total] = words.as_slice() else {
        return Err("malformed interruption evidence".into());
    };
    let written = written
        .parse::<u64>()
        .map_err(|_| "invalid interrupted length")?;
    let total = total
        .parse::<u64>()
        .map_err(|_| "invalid interrupted total")?;
    if !result.evidence.target
        || !result.marker_killed
        || result.exited_clean
        || result.evidence.selected_current
        || result.evidence.selected_previous
        || written == 0
        || written >= total
        || total != kernel_bytes
        || result.console.contains(protocol::INSTALL_MARKER)
        || result.console.contains(protocol::DIRECT_MARKER)
    {
        return Err(diagnostic(&format!(
            "guest did not stop before completing publication: written={written}, total={total}"
        )));
    }
    for expected in [
        format!("{} {uuid} /dev/vda2", protocol::PARTITIONS_MARKER),
        format!("{} {source_device}", protocol::MEDIA_MARKER),
    ] {
        if !result
            .console
            .lines()
            .any(|line| line.trim_end() == expected)
        {
            return Err(format!("interrupted installation lacks {expected}"));
        }
    }
    Ok(())
}

fn validate_interrupted_boot(result: &BootResult, refused: &str, uuid: &str) -> Result<(), String> {
    require(result, refused, "interrupted installation boot refusal")?;
    if result.evidence.selected_current || result.evidence.selected_previous {
        return Err("incomplete installation reached deployment selection".into());
    }
    for expected in [
        format!("TD-BOOT-VOLUME {uuid} /dev/vda2"),
        format!("TD-INSTALL-VOLUME {uuid} /dev/vda2"),
    ] {
        if !result
            .console
            .lines()
            .any(|line| line.trim_end() == expected)
        {
            return Err(format!("interrupted boot lacks {expected}"));
        }
    }
    for slot in ["current", "previous"] {
        let missing = format!(
            "{slot} selector /volume/td/boot/{slot}: No such file or directory (os error 2)"
        );
        if !result.console.contains(&missing) {
            return Err(format!(
                "interrupted boot did not refuse the missing {slot} selector\n{}",
                tail(&result.console, 100)
            ));
        }
    }
    Ok(())
}

fn validate_fixture_boot(
    result: &BootResult,
    expected: &str,
    uuid: &str,
    expected_device: &str,
    id: &str,
) -> Result<(), String> {
    require(result, expected, "installed boot")?;
    require(
        result,
        "TD-INSTALL-STALE-MOUNT-RECOVERED",
        "closed-descriptor mount recovery",
    )?;
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
    if !bound_selector_before_selection(&result.console, &expected_identity, id) {
        return Err(format!(
            "installed selector did not bind {expected_identity} before selecting {id}\n{}console tail (last 80 lines):\n{}",
            selector_binding_diagnostic(&result.console, &expected_identity, id),
            tail(&result.console, 80)
        ));
    }
    if !result.evidence.selected_current
        || result.evidence.selected_previous
        || result.evidence.bookkeeping_unavailable
        || result.evidence.selected_current_id.as_deref() != Some(id)
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
        mut extra,
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
    extra.extend([
        (
            protocol::SYSTEM_AUTOTEST_PRIVATE.into(),
            0o600,
            OPENSSH_ADMIN_PRIVATE_KEY.as_bytes().to_vec(),
        ),
        (
            protocol::SYSTEM_AUTOTEST_AUTHORIZED.into(),
            0o600,
            OPENSSH_ADMIN_AUTHORIZATION.as_bytes().to_vec(),
        ),
    ]);
    let live = scratch.dir.join("installer.cpio");
    write(
        &live,
        &initramfs(&base, &common, "install-system\n", &extra)?,
    )?;
    let iso = scratch.dir.join("installer.iso");
    media::write_image_with_payloads(&iso, &kernel, &live, &payloads)?;
    let capacity = system_target_capacity(payload_bytes)?;
    let mut installations: Vec<(String, BootResult)> = Vec::new();
    for bus in [DiskBus::Virtio, DiskBus::Ahci] {
        for (media_name, attachment, source_device) in [
            ("optical", FirmwareAttachment::Optical, "/dev/sr0"),
            ("usb", FirmwareAttachment::Usb, "/dev/sda"),
        ] {
            let source_device = if matches!(bus, DiskBus::Ahci) && media_name == "usb" {
                "/dev/sdb"
            } else {
                source_device
            };
            let name = format!("{media_name}-{}", bus.label());
            let mut target =
                TargetDisk::with_capacity(&scratch.dir, &format!("{name}.img"), capacity)?;
            target.bus = bus;
            let vars = scratch.dir.join(format!("{name}-install-vars.fd"));
            efi::copy_input(&vars_template, &vars)?;
            println!(
                "   [qemu-install-system] installing {payload_bytes} bytes through {name} media"
            );
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
            println!(
                "   [qemu-install-system] {name} installation elapsed: {:.2}s",
                result.elapsed.as_secs_f64()
            );
            require_installation(&result, &uuid, source_device, &target)?;
            require_live_reports(
                &result,
                &target,
                source_device,
                &iso,
                true,
                InventoryBefore::Fresh,
            )?;
            let mut first = None;
            for count in 1..=2 {
                let decoy = if count == 2 {
                    let mut decoy = TargetDisk::create(&scratch.dir, &format!("{name}-decoy.img"))?;
                    decoy.bus = bus;
                    Some(decoy)
                } else {
                    None
                };
                let vars = scratch.dir.join(format!("{name}-boot-{count}-vars.fd"));
                efi::copy_input(&vars_template, &vars)?;
                let mut boot_plan = target_plan(&target, SYSTEM_BOOT_SUCCESS_MARKER);
                boot_plan.mem = INSTALLED_SYSTEM_MEMORY_MIB;
                // Stock audio supervision needs the emulated sound device.
                boot_plan.audio = true;
                println!(
                    "   [qemu-install-system] cold system boot {count}, {name} media detached"
                );
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
                println!(
                    "   [qemu-install-system] {name} cold boot {count} elapsed: {:.2}s",
                    result.elapsed.as_secs_f64()
                );
                let device = format!("/dev/{}", partition_name(bus.name(count == 2), 2));
                validate_installed_system(&result, &uuid, &device, &id, count == 1)?;
                if let Some(first) = &first {
                    require_same_identity(
                        first,
                        &result,
                        &format!("{name} first boot"),
                        &format!("{name} second boot"),
                    )?;
                } else {
                    require_new_installation(&installations, &result, &name)?;
                    first = Some(result);
                }
                if let Some(decoy) = decoy {
                    fs::remove_file(&decoy.path)
                        .map_err(|error| format!("remove {}: {error}", decoy.path.display()))?;
                }
            }
            if installations.is_empty() {
                // Firmware cannot inject autotest tokens. Keep both stock firmware
                // boots above and add one direct selector boot of that SAME disk.
                let app_timeout = boot_timeout();
                let tokens =
                    format!("{AUTOTEST_CMDLINE_TOKEN} {}", autotest_wait_token(app_timeout));
                let mut app_plan = target_plan(&target, GREETER_MARKER);
                app_plan.kill_on_marker = false;
                app_plan.mem = INSTALLED_SYSTEM_MEMORY_MIB;
                app_plan.audio = true;
                app_plan.extra_append = &tokens;
                println!("   [qemu-install-system] checking all four jailed applications with the installed timezone");
                let result = boot_with_timeout(
                    &qemu,
                    &deployment.join("bzImage"),
                    &provisioned,
                    app_plan,
                    &scratch.dir,
                    app_timeout,
                )?;
                let device = format!("/dev/{}", partition_name(bus.name(false), 2));
                validate_installed_system(&result, &uuid, &device, &id, false)?;
                require_installed_applications(&result)?;
                require_same_identity(
                    first.as_ref().ok_or("missing firmware boot identity")?,
                    &result,
                    "firmware installed boot",
                    "application evidence boot",
                )?;
            }
            fs::remove_file(&target.path)
                .map_err(|error| format!("remove {}: {error}", target.path.display()))?;
            let first = first.ok_or("installed system has no first-boot evidence")?;
            installations.push((name, first));
        }
    }
    println!("PASS: stock system installed offline through optical/USB ISO firmware onto virtio/AHCI disks; immutable root, compositor page flips, acknowledged deployment and stable machine identity across reordered cold boots; all four jailed applications start in an additional direct selector boot of the timezone-configured installed disk");
    Ok(())
}

fn require_installed_applications(result: &BootResult) -> Result<(), String> {
    if !result.exited_clean || result.marker_killed {
        return Err(format!(
            "installed application boot did not shut down cleanly: {}\n{}",
            result.reason,
            tail(&result.console, 100)
        ));
    }
    for (marker, present) in [
        (TD_MAIL_BOOT_MARKER, result.evidence.td_mail_running),
        (TD_NEWS_BOOT_MARKER, result.evidence.td_news_running),
        (TD_FIREFOX_BOOT_MARKER, result.evidence.td_firefox),
        (
            TD_CLAUDE_TERMINAL_MARKER,
            result.evidence.td_claude_terminal,
        ),
    ] {
        if !present {
            return Err(format!(
                "installed timezone application proof lacks {marker}\n{}",
                tail(&result.console, 100)
            ));
        }
    }
    Ok(())
}

fn require_new_installation(
    previous: &[(String, BootResult)],
    next: &BootResult,
    name: &str,
) -> Result<(), String> {
    for (previous_name, previous) in previous {
        require_distinct_identity(previous, next, previous_name, name)?;
    }
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
    let expected_identity = format!("{uuid} {device}");
    if result.evidence.selected_current_id.as_deref() != Some(id)
        || !bound_selector_before_selection(&result.console, &expected_identity, id)
    {
        return Err(format!(
            "installed system did not bind {uuid} {device} and deployment {id}: selected={:?}\n{}console tail (last 100 lines):\n{}",
            result.evidence.selected_current_id,
            selector_binding_diagnostic(&result.console, &expected_identity, id),
            tail(&result.console, 100)
        ));
    }
    let expected_hostname = format!("TD-HOSTNAME-READY {}", protocol::HOSTNAME);
    let hostnames: Vec<_> = result.console.lines().map(str::trim_end)
        .filter(|line| line.starts_with("TD-HOSTNAME-READY ")).collect();
    if hostnames != [expected_hostname.as_str()] {
        return Err(format!("installed system did not activate its saved hostname: {hostnames:?}\n{}", tail(&result.console, 100)));
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
    target: &TargetDisk,
) -> Result<(), String> {
    require(result, protocol::INSTALL_MARKER, "guest installation")?;
    if !result
        .console
        .lines()
        .any(|line| line.trim_end() == protocol::DIRECT_MARKER)
    {
        return Err("guest did not report direct publication after staging checks".into());
    }
    let partition_evidence = format!(
        "{} {uuid} /dev/{}",
        protocol::PARTITIONS_MARKER,
        partition_name(target.bus.name(false), 2)
    );
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

fn target_plan<'a>(target: &'a TargetDisk, marker: &'a str) -> BootPlan<'a> {
    let mut result = plan(&target.path, false, marker);
    result.disk = Some(BootDisk {
        path: &target.path,
        read_only: false,
        sector_size: target.sector_size,
        bus: target.bus,
    });
    result
}

fn plan<'a>(path: &'a Path, read_only: bool, marker: &'a str) -> BootPlan<'a> {
    BootPlan {
        disk: Some(BootDisk::new(path, read_only)),
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

fn selector_offsets(console: &str, identity: &str, id: &str) -> (Option<usize>, Option<usize>) {
    let bound = format!("TD-BOOT-VOLUME {identity}");
    let selected = format!("{} {id}", td_boot_protocol::SELECTED_CURRENT_MARKER);
    let mut binding = None;
    let mut selection = None;
    for (index, line) in console.lines().enumerate() {
        let line = line.trim_end();
        if binding.is_none() && line == bound {
            binding = Some(index);
        }
        if selection.is_none() && line == selected {
            selection = Some(index);
        }
        if binding.is_some() && selection.is_some() {
            break;
        }
    }
    (binding, selection)
}

fn bound_selector_before_selection(console: &str, identity: &str, id: &str) -> bool {
    let (binding, selection) = selector_offsets(console, identity, id);
    matches!((binding, selection), (Some(binding), Some(selection)) if binding < selection)
}

fn selector_binding_diagnostic(console: &str, identity: &str, id: &str) -> String {
    const MAX_RECORDS: usize = 8;
    const MAX_RECORD_BYTES: usize = 256;
    let (binding, selection) = selector_offsets(console, identity, id);
    let lines = console.lines().count();
    let bytes = console.len();
    let mut report = format!(
        "retained console: {lines} lines, {bytes} bytes; line offsets (zero-based): first exact binding={binding:?}, first exact selection={selection:?}\nselector record excerpts (at most {MAX_RECORDS}, {MAX_RECORD_BYTES} UTF-8 bytes each before escaping):\n"
    );
    let records = console
        .lines()
        .enumerate()
        .filter(|(_, line)| line.contains("TD-BOOT-VOLUME") || line.contains("TD-BOOT-SELECTED"));
    let mut emitted = false;
    for (count, (index, line)) in records.take(MAX_RECORDS + 1).enumerate() {
        if count == MAX_RECORDS {
            report.push_str("  further matching records omitted\n");
            break;
        }
        let mut end = line.len().min(MAX_RECORD_BYTES);
        while !line.is_char_boundary(end) {
            end -= 1;
        }
        let excerpt = line.get(..end).unwrap_or_default();
        let suffix = if end < line.len() { " [truncated]" } else { "" };
        // Debug formatting escapes control characters from the guest console.
        report.push_str(&format!("  {index}: {excerpt:?}{suffix}\n"));
        emitted = true;
    }
    if !emitted {
        report.push_str("  no matching records in retained console\n");
    }
    report
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
    fn installed_hostname_requires_exactly_one_verified_saved_name() {
        let good = healthy_system();
        let expected = format!("TD-HOSTNAME-READY {}\n", protocol::HOSTNAME);
        for replacement in [String::new(), "TD-HOSTNAME-READY td\n".into(), expected.repeat(2)] {
            let mut bad = healthy_system();
            bad.console = bad.console.replace(&expected, &replacement);
            assert!(validate_installed_system(&bad, "uuid", "/dev/vda2", "deployment", true).is_err());
        }
        assert!(validate_installed_system(&good, "uuid", "/dev/vda2", "deployment", true).is_ok());
    }

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
            console: format!("TD-BOOT-VOLUME uuid /dev/vda2\nTD-BOOT-SELECTED-CURRENT deployment\nTD-HOSTNAME-READY {}\n{SYSTEM_BOOT_SUCCESS_MARKER}\n", protocol::HOSTNAME),
            elapsed: Duration::ZERO, firefox_audio: FirefoxAudioCapture::NotRequested,
        }
    }

    fn interrupted_result() -> BootResult {
        BootResult {
            evidence: ConsoleEvidence {
                target: true,
                ..ConsoleEvidence::default()
            },
            exited_clean: false,
            marker_killed: true,
            reason: "fixture".into(),
            console: format!(
                "{} uuid /dev/vda2\n{} /dev/sr0\n{} 4 10\n",
                protocol::PARTITIONS_MARKER,
                protocol::MEDIA_MARKER,
                protocol::INTERRUPTED_MARKER
            ),
            elapsed: Duration::ZERO,
            firefox_audio: FirefoxAudioCapture::NotRequested,
        }
    }

    #[test]
    fn target_refusal_requires_layout_failure_and_no_publication() {
        let validate = |result: &BootResult| {
            validate_target_refusal(result, "/dev/sr0", "destination is too small")
        };
        let mut result = interrupted_result();
        result.console = format!(
            "{} /dev/sr0\ntd-install: destination is too small\n{} /bin/td-install failed: exit status: 1\n",
            protocol::MEDIA_MARKER, protocol::REFUSED_PREFIX
        );
        assert!(validate(&result).is_ok());
        for missing in [
            protocol::MEDIA_MARKER,
            protocol::REFUSED_PREFIX,
            "destination is too small",
        ] {
            let console = result.console.clone();
            result.console = console.replace(missing, "wrong");
            assert!(validate(&result).is_err());
            result.console = console;
        }
        for extra in [
            protocol::PARTITIONS_MARKER,
            protocol::DIRECT_MARKER,
            protocol::INSTALL_MARKER,
        ] {
            let console = result.console.clone();
            result.console.push_str(extra);
            assert!(validate(&result).is_err());
            result.console = console;
        }
        result.evidence.selected_current = true;
        assert!(validate(&result).is_err());
        result.evidence.selected_current = false;
        result.evidence.selected_previous = true;
        assert!(validate(&result).is_err());
    }

    #[test]
    fn interruption_evidence_proves_a_partial_planned_payload() {
        let validate = |result: &BootResult| validate_interruption(result, 10, "uuid", "/dev/sr0");
        assert!(validate(&interrupted_result()).is_ok());
        for bad in ["0 10", "10 10", "11 10", "4 11", "a 10", "4 10 extra", "4"] {
            let mut result = interrupted_result();
            result.console = result.console.replace("4 10", bad);
            assert!(validate(&result).is_err(), "{bad}");
        }
        for extra in [
            protocol::INSTALL_MARKER,
            protocol::DIRECT_MARKER,
            "TD-INSTALL-PUBLICATION-INTERRUPTED 4 10",
        ] {
            let mut result = interrupted_result();
            result.console.push_str(extra);
            assert!(validate(&result).is_err());
        }
        let mut result = interrupted_result();
        result.console = result.console.replace("INTERRUPTED 4", "INTERRUPTED4");
        assert!(validate(&result).is_err());
        let mut result = interrupted_result();
        result.evidence.target = false;
        assert!(validate(&result).is_err());
        result.evidence.target = true;
        result.evidence.selected_current = true;
        assert!(validate(&result).is_err());
        let mut result = interrupted_result();
        result.evidence.selected_previous = true;
        assert!(validate(&result).is_err());
        let mut result = interrupted_result();
        result.marker_killed = false;
        assert!(validate(&result).is_err());
        let mut result = interrupted_result();
        result.exited_clean = true;
        assert!(validate(&result).is_err());
        let mut result = interrupted_result();
        result.console = result.console.replace("uuid", "wrong");
        assert!(validate(&result).is_err());
        let mut result = interrupted_result();
        result.console = result.console.replace("/dev/sr0", "/dev/sda");
        assert!(validate(&result).is_err());
    }

    #[test]
    fn interrupted_boot_refusal_requires_both_missing_selectors() {
        let refused = format!(
            "{} /bin/td-boot failed: exit status: 1",
            protocol::REFUSED_PREFIX
        );
        let mut result = interrupted_result();
        result.console = format!("TD-BOOT-VOLUME uuid /dev/vda2\nTD-INSTALL-VOLUME uuid /dev/vda2\ncurrent selector /volume/td/boot/current: No such file or directory (os error 2)\nprevious selector /volume/td/boot/previous: No such file or directory (os error 2)\n{refused}\n");
        assert!(validate_interrupted_boot(&result, &refused, "uuid").is_ok());
        for missing in [refused.as_str(), "TD-BOOT-VOLUME uuid /dev/vda2"] {
            let console = result.console.clone();
            result.console = console.replace(missing, "");
            assert!(validate_interrupted_boot(&result, &refused, "uuid").is_err());
            result.console = console;
        }
        for slot in ["current", "previous"] {
            let console = result.console.clone();
            result.console = console.replace(&format!("{slot} selector"), "other failure");
            assert!(validate_interrupted_boot(&result, &refused, "uuid").is_err());
            result.console = console;
        }
        assert!(validate_interrupted_boot(&result, &refused, "wrong").is_err());
        result.evidence.selected_previous = true;
        assert!(validate_interrupted_boot(&result, &refused, "uuid").is_err());
    }

    #[test]
    fn installed_timezone_proof_requires_every_shipped_application() {
        assert!(require_installed_applications(&healthy_system()).is_err());
        let healthy = || {
            let mut result = healthy_system();
            result.evidence.td_mail_running = true;
            result.evidence.td_news_running = true;
            result.evidence.td_firefox = true;
            result.evidence.td_claude_terminal = true;
            result.exited_clean = true;
            result.marker_killed = false;
            result
        };
        require_installed_applications(&healthy()).unwrap();
        let mut unclean = healthy();
        unclean.exited_clean = false;
        assert!(require_installed_applications(&unclean).is_err());
        let mut killed = healthy();
        killed.marker_killed = true;
        assert!(require_installed_applications(&killed).is_err());
        for remove in [
            |e: &mut ConsoleEvidence| e.td_mail_running = false,
            |e: &mut ConsoleEvidence| e.td_news_running = false,
            |e: &mut ConsoleEvidence| e.td_firefox = false,
            |e: &mut ConsoleEvidence| e.td_claude_terminal = false,
        ] {
            let mut missing = healthy();
            remove(&mut missing.evidence);
            assert!(require_installed_applications(&missing).is_err());
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
    fn new_installation_rejects_collision_with_every_previous_machine() {
        let mut previous = Vec::new();
        for (name, key) in [
            ("optical-virtio", "A"),
            ("usb-virtio", "B"),
            ("optical-ahci", "C"),
        ] {
            let mut result = healthy_system();
            result.evidence.host_key = Some(key.into());
            previous.push((name.into(), result));
        }
        let mut next = healthy_system();
        next.evidence.host_key = Some("D".into());
        require_new_installation(&previous, &next, "usb-ahci").unwrap();
        for (name, result) in &previous {
            next.evidence.host_key = result.evidence.host_key.clone();
            let error = require_new_installation(&previous, &next, "usb-ahci").unwrap_err();
            assert!(error.contains(name), "{error}");
            assert!(error.contains("usb-ahci"), "{error}");
        }
        next.evidence.host_key = None;
        assert!(require_new_installation(&previous, &next, "usb-ahci").is_err());
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
    fn binding_failures_retain_early_selector_records_beyond_the_console_tail() {
        let mut result = healthy_system();
        let early = "TD-BOOT-VOLUME uuid [kernel interleave] /dev/vda2";
        result.console = format!(
            "{early}\nTD-BOOT-SELECTED-CURRENT deployment\n{}{SYSTEM_BOOT_SUCCESS_MARKER}\n",
            "later kernel output\n".repeat(200)
        );
        assert!(!tail(&result.console, 100).contains(early));
        let error = validate_installed_system(&result, "uuid", "/dev/vda2", "deployment", true)
            .unwrap_err();
        assert!(error.contains(early));
        assert!(error.contains("retained console:"));
        assert!(error.contains("console tail (last "));
        assert!(error.contains("first exact binding=None, first exact selection=Some(1)"));
        result.console.push_str("TD-INSTALL-VOLUME uuid /dev/vda2\nTD-INSTALL-VOLUME uuid /dev/vda2\nTD-INSTALL-STALE-MOUNT-RECOVERED\nTD-INSTALL-PERSISTED-1 deployment\n");
        let error = validate_fixture_boot(
            &result,
            "TD-INSTALL-PERSISTED-1 deployment",
            "uuid",
            "/dev/vda2",
            "deployment",
        )
        .unwrap_err();
        assert!(error.contains(early));
        assert!(error.contains("retained console:"));
        assert!(error.contains("console tail (last "));
        assert!(error.contains("first exact binding=None, first exact selection=Some(1)"));
        let reversed = "TD-BOOT-SELECTED-CURRENT deployment\nTD-BOOT-VOLUME uuid /dev/vda2\n";
        assert!(!bound_selector_before_selection(
            reversed,
            "uuid /dev/vda2",
            "deployment"
        ));
        assert!(
            selector_binding_diagnostic(reversed, "uuid /dev/vda2", "deployment")
                .contains("first exact binding=Some(1), first exact selection=Some(0)")
        );
        assert!(
            selector_binding_diagnostic("kernel only\n", "uuid /dev/vda2", "deployment")
                .contains("no matching records in retained console")
        );
    }

    #[test]
    fn selector_record_excerpts_bound_unicode_and_escape_guest_control_bytes() {
        let line = format!("noises\u{1b}[31m TD-BOOT-VOLUME {}", "é".repeat(200));
        let console = format!(
            "{}TD-BOOT-SELECTED-CURRENT ninth\n",
            format!("{line}\n").repeat(8)
        );
        let report = selector_binding_diagnostic(&console, "uuid /dev/vda2", "deployment");
        assert!(report.contains("\\u{1b}"));
        assert!(!report.contains('\u{1b}'));
        assert_eq!(report.matches(" [truncated]").count(), 8);
        assert_eq!(report.matches("é\" [truncated]").count(), 8);
        let eight = selector_binding_diagnostic(
            &format!("{line}\n").repeat(8),
            "uuid /dev/vda2",
            "deployment",
        );
        assert_eq!(eight.matches("é\" [truncated]").count(), 8);
        assert!(!eight.contains("further matching records omitted"));
        for malformed in [
            "TD-BOOT-SELECTED CURRENT",
            "TD-BOOT-SELECTED: CURRENT",
            "TD-BOOT-SELECTED",
        ] {
            let report = selector_binding_diagnostic(malformed, "uuid /dev/vda2", "deployment");
            assert!(report.contains(&format!("0: {malformed:?}")));
            assert!(report.contains("first exact selection=None"));
        }
        let controls = format!("TD-BOOT-VOLUME {}\n", "\u{1b}".repeat(256)).repeat(8);
        let escaped = selector_binding_diagnostic(&controls, "uuid /dev/vda2", "deployment");
        assert!(escaped.len() > 4096 && escaped.len() < 16 * 1024);
        assert!(!escaped.contains('\u{1b}'));
        assert!(report.contains("further matching records omitted"));
        assert!(!report.contains("ninth"));
        assert!(report.len() < 4096);
        assert!(!bound_selector_before_selection(
            &console,
            "uuid /dev/vda2",
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
        let mut target = TargetDisk::create(&scratch.dir, "a,b.img").unwrap();
        fs::write(&target.path, b"preserve").unwrap();
        assert!(TargetDisk::create(&scratch.dir, "a,b.img").is_err());
        assert_eq!(fs::read(&target.path).unwrap(), b"preserve");
        let arg = target_drive_arg(&target);
        let text = arg.to_str().unwrap();
        assert!(text.starts_with("if=none,format=raw,id=install-target,file="));
        assert!(text.ends_with("/a,,b.img"));
        target.read_only = true;
        let arg = target_drive_arg(&target);
        let text = arg.to_str().unwrap();
        assert!(text.starts_with("if=none,format=raw,id=install-target,readonly=on,file="));
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
    const PREVIEW_512: &str = r#"{"version":1,"scope":"layout-preview","logical_sector_bytes":512,"capacity_bytes":6442450944,"partitions":[{"number":1,"purpose":"efi-system","start_lba":2048,"end_lba":1050623,"offset_bytes":1048576,"capacity_bytes":536870912},{"number":2,"purpose":"system-volume","start_lba":1050624,"end_lba":12582878,"offset_bytes":537919488,"capacity_bytes":5904514560}]}"#;
    const PREVIEW_4KN: &str = r#"{"version":1,"scope":"layout-preview","logical_sector_bytes":4096,"capacity_bytes":6442450944,"partitions":[{"number":1,"purpose":"efi-system","start_lba":256,"end_lba":131327,"offset_bytes":1048576,"capacity_bytes":536870912},{"number":2,"purpose":"system-volume","start_lba":131328,"end_lba":1572858,"offset_bytes":537919488,"capacity_bytes":5904510976}]}"#;

    fn preview_console(json: &str) -> String {
        format!("{} {} {json}\n", protocol::PREVIEW_MARKER, json.len())
    }

    #[test]
    fn preview_oracle_checks_geometry_and_every_partition_field() {
        for (sector, json) in [(512, PREVIEW_512), (4096, PREVIEW_4KN)] {
            validate_preview(&preview_console(json), MINIMUM_TARGET_BYTES, sector, true).unwrap();
        }
        for (old, new, field) in [
            ("\"version\":1", "\"version\":2", "version"),
            ("layout-preview", "install-plan", "scope"),
            ("4096", "512", "logical_sector_bytes"),
            ("6442450944", "6442451456", "capacity_bytes"),
            ("\"number\":2", "\"number\":3", "number"),
            ("system-volume", "efi-system", "purpose"),
            ("\"start_lba\":256", "\"start_lba\":257", "start_lba"),
            ("\"end_lba\":1572858", "\"end_lba\":1572857", "end_lba"),
            ("537919488", "537919489", "offset_bytes"),
            ("5904510976", "5904506880", "capacity_bytes"),
        ] {
            let changed = PREVIEW_4KN.replace(old, new);
            assert_ne!(changed, PREVIEW_4KN);
            let error =
                validate_preview(&preview_console(&changed), MINIMUM_TARGET_BYTES, 4096, true)
                    .unwrap_err();
            assert!(
                error.contains(field) && error.contains("expected") && error.contains("observed"),
                "{error}"
            );
        }
        for invalid in [
            PREVIEW_4KN.replace("\"version\":1", "\"version\":1,\"version\":1"),
            PREVIEW_4KN.replace("5904510976", "5904510976.0"),
            PREVIEW_4KN.replace("5904510976", "-1"),
            PREVIEW_4KN.replace("\"partitions\":[", "\"partitions\":[{},"),
        ] {
            assert!(
                validate_preview(&preview_console(&invalid), MINIMUM_TARGET_BYTES, 4096, true)
                    .is_err()
            );
        }
    }

    #[test]
    fn preview_frames_are_bounded_unique_and_absent_on_writer_refusals() {
        let valid = preview_console(PREVIEW_512);
        assert!(validate_preview("", MINIMUM_TARGET_BYTES, 512, false).is_ok());
        assert!(validate_preview(&valid, MINIMUM_TARGET_BYTES, 512, false).is_err());
        assert!(validate_preview("", MINIMUM_TARGET_BYTES, 512, true).is_err());
        assert!(
            validate_preview(&format!("{valid}{valid}"), MINIMUM_TARGET_BYTES, 512, true).is_err()
        );
        for malformed in [
            protocol::PREVIEW_MARKER.to_owned(),
            format!("{}broken", protocol::PREVIEW_MARKER),
        ] {
            assert!(
                validate_preview(&malformed, MINIMUM_TARGET_BYTES, 512, true)
                    .unwrap_err()
                    .contains("malformed")
            );
            assert!(validate_preview(
                &format!("{valid}{malformed}\n"),
                MINIMUM_TARGET_BYTES,
                512,
                true
            )
            .unwrap_err()
            .contains("duplicate"));
        }
        let joined = valid.replace("}]}\n", "}]}[kernel console]\n");
        validate_preview(&joined, MINIMUM_TARGET_BYTES, 512, true).unwrap();
        for bad in [
            format!("{} 1024 {{}}\n", protocol::PREVIEW_MARKER),
            format!("{} 20 {{}}\n", protocol::PREVIEW_MARKER),
            format!("{} nope {{}}\n", protocol::PREVIEW_MARKER),
            preview_console(&format!("{}0{}", "[".repeat(9), "]".repeat(9))),
            preview_console(&" ".repeat(protocol::MAX_PREVIEW_BYTES)),
        ] {
            assert!(validate_preview(&bad, MINIMUM_TARGET_BYTES, 512, true).is_err());
        }
        assert!(validate_preview(&valid, MINIMUM_TARGET_BYTES, 0, true).is_err());
        assert!(validate_preview(&valid, MINIMUM_TARGET_BYTES + 1, 512, true).is_err());
    }

    const INVENTORY_FIXTURE: &str = r#"{"version":1,"scope":"inventory-only","devices":[{"name":"vda","device_number":"252:0","capacity_bytes":6442450944,"read_only":false,"parent":null,"partition_number":null,"disk":{"sequence":11,"logical_sector_bytes":4096,"serial":"td-install-test","removable":false,"model":null,"wwid":null},"holders":[],"slaves":[]},{"name":"sr0","device_number":"11:0","capacity_bytes":1048576,"read_only":true,"parent":null,"partition_number":null,"disk":{"sequence":12,"logical_sector_bytes":2048,"removable":true,"model":null,"wwid":null,"serial":null},"holders":[],"slaves":[]},{"name":"vda1","device_number":"252:1","read_only":false,"capacity_bytes":536870912,"parent":"vda","partition_number":1,"disk":null,"holders":[],"slaves":[]},{"name":"vda2","device_number":"252:2","read_only":false,"capacity_bytes":5904510976,"parent":"vda","partition_number":2,"disk":null,"holders":[],"slaves":[]}]}"#;

    fn inventory_expectation() -> InventoryExpected<'static> {
        InventoryExpected {
            target_bytes: 6 * 1024 * 1024 * 1024,
            source_bytes: 1024 * 1024,
            sector_bytes: 4096,
            read_only: false,
            source_name: "sr0",
            target_name: "vda",
            before: InventoryBefore::Reinstall,
        }
    }

    fn inventory_console(before: &str, after: Option<&str>) -> String {
        let mut console = format!(
            "{} {} {before}\n",
            protocol::INVENTORY_BEFORE_MARKER,
            before.len()
        );
        if let Some(after) = after {
            console.push_str(&format!(
                "{} {} {after}\n",
                protocol::INVENTORY_AFTER_MARKER,
                after.len()
            ));
        }
        console
    }

    #[test]
    fn inventory_oracle_checks_host_geometry_capacity_identity_and_partitions() {
        let expected = inventory_expectation();
        let valid = inventory_console(INVENTORY_FIXTURE, Some(INVENTORY_FIXTURE));
        assert!(validate_inventories(&valid, &expected, true).is_ok());
        for (old, new) in [
            ("6442450944", "6442450943"),
            ("1048576", "1048575"),
            (
                "\"logical_sector_bytes\":4096",
                "\"logical_sector_bytes\":512",
            ),
            ("\"read_only\":false", "\"read_only\":true"),
            ("td-install-test", "some-other-disk"),
            ("\"parent\":\"vda\"", "\"parent\":\"vdb\""),
            ("\"partition_number\":2", "\"partition_number\":1"),
            ("\"name\":\"vda1\"", "\"name\":\"missing\""),
            ("536870912", "536870911"),
            ("5904510976", "1"),
            ("5904510976", "6442450943"),
            ("inventory-only", "eligible-targets"),
            ("\"name\":\"vda\"", "\"name\":\"vda\",\"name\":\"vdb\""),
        ] {
            let changed = INVENTORY_FIXTURE.replace(old, new);
            assert_ne!(changed, INVENTORY_FIXTURE);
            assert!(
                validate_inventories(
                    &inventory_console(&changed, Some(&changed)),
                    &expected,
                    true
                )
                .is_err(),
                "accepted {old} -> {new}"
            );
        }
        let extra = INVENTORY_FIXTURE.replace(
            "\"devices\":[",
            "\"devices\":[{\"name\":\"vda3\",\"device_number\":\"252:3\",\"parent\":\"vda\"},",
        );
        assert!(
            validate_inventories(&inventory_console(&extra, Some(&extra)), &expected, true)
                .is_err()
        );
        let changed = INVENTORY_FIXTURE.replace("\"sequence\":11", "\"sequence\":13");
        assert!(validate_inventories(
            &inventory_console(INVENTORY_FIXTURE, Some(&changed)),
            &expected,
            true
        )
        .is_err());
    }

    #[test]
    fn inventory_oracle_requires_unique_reports_and_rejects_post_format_refusals() {
        let expected = inventory_expectation();
        let before = inventory_console(INVENTORY_FIXTURE, None);
        assert!(validate_inventories(&before, &expected, false).is_ok());
        let trailing_kernel = before.replace('\n', "clocksource: Switched to clocksource tsc\n");
        assert!(validate_inventories(&trailing_kernel, &expected, false).is_ok());
        let interleaved = before.replace("\"capacity_bytes\"", "kernel message\"capacity_bytes\"");
        assert!(validate_inventories(&interleaved, &expected, false).is_err());
        for length in [
            "0".to_owned(),
            "invalid".to_owned(),
            protocol::MAX_INVENTORY_BYTES.to_string(),
            (INVENTORY_FIXTURE.len() - 1).to_string(),
            (INVENTORY_FIXTURE.len() + 1).to_string(),
        ] {
            let malformed = format!(
                "{} {length} {INVENTORY_FIXTURE}\n",
                protocol::INVENTORY_BEFORE_MARKER
            );
            assert!(validate_inventories(&malformed, &expected, false).is_err());
        }
        assert!(validate_inventories(&before, &expected, true).is_err());
        assert!(validate_inventories(&format!("{before}{before}"), &expected, false).is_err());
        assert!(validate_inventories(
            &inventory_console(INVENTORY_FIXTURE, Some(INVENTORY_FIXTURE)),
            &expected,
            false
        )
        .is_err());
        let mut expected = expected;
        expected.read_only = true;
        let readonly = INVENTORY_FIXTURE.replace("\"read_only\":false", "\"read_only\":true");
        assert!(
            validate_inventories(&inventory_console(&readonly, None), &expected, false).is_ok()
        );
    }

    #[test]
    fn inventory_observes_fresh_targets_usb_metadata_and_unique_identities() {
        let mut expected = inventory_expectation();
        let prefix = INVENTORY_FIXTURE
            .split_once(",{\"name\":\"vda1\"")
            .expect("fixture partition boundary")
            .0;
        let fresh = format!("{prefix}]}}");
        expected.before = InventoryBefore::Fresh;
        assert!(validate_inventories(
            &inventory_console(&fresh, Some(INVENTORY_FIXTURE)),
            &expected,
            true
        )
        .is_ok());
        assert!(validate_inventories(&inventory_console(&fresh, None), &expected, false).is_ok());
        let stale = validate_inventories(
            &inventory_console(INVENTORY_FIXTURE, None),
            &expected,
            false,
        )
        .unwrap_err();
        assert!(stale.contains("fresh vda has 2 partitions"), "{stale}");
        expected.before = InventoryBefore::Reinstall;
        let usb = INVENTORY_FIXTURE.replace("\"sr0\"", "\"sda\"").replace(
            "\"logical_sector_bytes\":2048",
            "\"logical_sector_bytes\":512",
        );
        expected.source_name = "sda";
        validate_inventories(&inventory_console(&usb, Some(&usb)), &expected, true).unwrap();
        let mut rejected = Command::new("qemu");
        let error = DiskBus::Ahci
            .attach(
                &mut rejected,
                "fixture",
                protocol::TARGET_SERIAL,
                None,
                SectorSize::Bytes4096,
            )
            .unwrap_err();
        assert!(error.contains("512-byte"), "{error}");
        assert_eq!(rejected.get_args().count(), 0);
        expected.source_name = "sr0";
        for (old, new, reason) in [
            (
                "\"sequence\":11",
                "\"sequence\":0",
                "invalid whole-disk identities",
            ),
            ("11:0", "252:0", "device_number duplicates"),
            ("252:1", "252:0", "device_number duplicates"),
            (
                "\"logical_sector_bytes\":4096",
                "\"logical_sector_bytes\":512",
                "vda.disk.logical_sector_bytes: expected 4096, observed 512",
            ),
            (
                "6442450944",
                "6442450943",
                "vda.capacity_bytes: expected 6442450944, observed 6442450943",
            ),
        ] {
            let bad = INVENTORY_FIXTURE.replace(old, new);
            let error = validate_inventories(&inventory_console(&bad, Some(&bad)), &expected, true)
                .unwrap_err();
            assert!(error.contains(reason), "{error}");
        }
        let readonly_partition = INVENTORY_FIXTURE.replace(
            "\"device_number\":\"252:2\",\"read_only\":false",
            "\"device_number\":\"252:2\",\"read_only\":true",
        );
        let error = validate_inventories(
            &inventory_console(INVENTORY_FIXTURE, Some(&readonly_partition)),
            &expected,
            true,
        )
        .unwrap_err();
        assert!(error.contains("vda2.read_only"), "{error}");
    }

    #[test]
    fn ahci_attachments_and_inventory_keep_the_planned_bus_and_names() {
        let mut command = Command::new("qemu");
        DiskBus::Ahci
            .attach(
                &mut command,
                "fixture",
                protocol::TARGET_SERIAL,
                Some(1),
                SectorSize::Bytes512,
            )
            .unwrap();
        let args: Vec<_> = command
            .get_args()
            .map(|arg| arg.to_str().unwrap())
            .collect();
        assert_eq!(
            args,
            [
                "-device",
                "ich9-ahci,id=fixture-ahci",
                "-device",
                "ide-hd,bus=fixture-ahci.0,drive=fixture,serial=td-install-test,bootindex=1"
            ]
        );
        assert_eq!(DiskBus::Ahci.name(false), "sda");
        assert_eq!(DiskBus::Ahci.name(true), "sdb");
        assert!(matches!(
            BootDisk::new(Path::new("image"), false).bus,
            DiskBus::Virtio
        ));
        let mut expected = inventory_expectation();
        expected.target_name = "sda";
        expected.sector_bytes = 512;
        let sata = INVENTORY_FIXTURE
            .replace("vda", "sda")
            .replace("252:", "8:")
            .replace(
                "\"logical_sector_bytes\":4096",
                "\"logical_sector_bytes\":512",
            )
            .replace("5904510976", "5904514560");
        validate_inventories(&inventory_console(&sata, Some(&sata)), &expected, true).unwrap();
        expected.source_name = "sdb";
        let usb = sata.replace("sr0", "sdb").replace("11:0", "8:16").replace(
            "\"logical_sector_bytes\":2048",
            "\"logical_sector_bytes\":512",
        );
        validate_inventories(&inventory_console(&usb, Some(&usb)), &expected, true).unwrap();
        let mut rejected = Command::new("qemu");
        let error = DiskBus::Ahci
            .attach(
                &mut rejected,
                "fixture",
                protocol::TARGET_SERIAL,
                None,
                SectorSize::Bytes4096,
            )
            .unwrap_err();
        assert!(error.contains("512-byte"), "{error}");
        assert_eq!(rejected.get_args().count(), 0);
    }

    #[test]
    fn ahci_installation_evidence_and_cold_plan_retain_the_bus() {
        let target = TargetDisk {
            path: PathBuf::from("owned-fixture.img"),
            read_only: false,
            sector_size: SectorSize::Bytes512,
            bus: DiskBus::Ahci,
        };
        let plan = target_plan(&target, "cold-marker");
        let disk = plan.disk.as_ref().unwrap();
        assert!(matches!(disk.bus, DiskBus::Ahci));
        assert_eq!(disk.sector_size.bytes(), 512);
        assert_eq!(disk.path, target.path.as_path());
        assert!(!disk.read_only);
        assert_eq!(plan.target_marker, "cold-marker");
        let mut result = healthy_system();
        result.console = format!(
            "{}\n{}\n{} uuid /dev/sda2\n{} /dev/sdb\n",
            protocol::INSTALL_MARKER,
            protocol::DIRECT_MARKER,
            protocol::PARTITIONS_MARKER,
            protocol::MEDIA_MARKER
        );
        require_installation(&result, "uuid", "/dev/sdb", &target).unwrap();
        result.console = result.console.replace("/dev/sda2", "/dev/vda2");
        assert!(require_installation(&result, "uuid", "/dev/sdb", &target).is_err());
    }

    #[test]
    fn nvme_attachments_preserve_sector_sizes_and_namespace_partition_names() {
        for sector in [SectorSize::Bytes512, SectorSize::Bytes4096] {
            let mut command = Command::new("qemu");
            DiskBus::Nvme
                .attach(
                    &mut command,
                    "target",
                    protocol::TARGET_SERIAL,
                    Some(1),
                    sector,
                )
                .unwrap();
            let args: Vec<_> = command
                .get_args()
                .map(|arg| arg.to_str().unwrap())
                .collect();
            assert_eq!(
                args,
                [
                    "-device",
                    &format!(
                        "nvme,drive=target,serial={},bootindex=1{}",
                        protocol::TARGET_SERIAL,
                        sector.device_suffix()
                    )
                ]
            );
        }
        let mut oversized = Command::new("qemu");
        assert!(DiskBus::Nvme
            .attach(
                &mut oversized,
                "target",
                &"x".repeat(21),
                None,
                SectorSize::Bytes512
            )
            .is_err());
        assert_eq!(oversized.get_args().count(), 0);
        assert_eq!(partition_name(DiskBus::Nvme.name(false), 2), "nvme0n1p2");
        assert_eq!(partition_name(DiskBus::Nvme.name(true), 2), "nvme1n1p2");
        assert_eq!(partition_name(DiskBus::Virtio.name(true), 2), "vdb2");
        assert_eq!(partition_name(DiskBus::Ahci.name(true), 2), "sdb2");
        let mut expected = inventory_expectation();
        expected.target_name = "nvme0n1";
        let nvme = INVENTORY_FIXTURE
            .replace("vda1", "nvme0n1p1")
            .replace("vda2", "nvme0n1p2")
            .replace("\"vda\"", "\"nvme0n1\"");
        validate_inventories(&inventory_console(&nvme, Some(&nvme)), &expected, true).unwrap();
        let invalid = nvme.replace("nvme0n1p2", "nvme0n12");
        assert!(validate_inventories(
            &inventory_console(&invalid, Some(&invalid)),
            &expected,
            true
        )
        .is_err());
        let target = TargetDisk {
            path: PathBuf::from("owned-fixture.img"),
            read_only: false,
            bus: DiskBus::Nvme,
            sector_size: SectorSize::Bytes4096,
        };
        let planned = target_plan(&target, "marker");
        let disk = planned.disk.as_ref().unwrap();
        assert!(matches!(disk.bus, DiskBus::Nvme));
        assert!(matches!(disk.sector_size, SectorSize::Bytes4096));
        let mut result = healthy_system();
        result.console = format!(
            "{} /dev/sr0\n{} uuid /dev/nvme0n1p2\n{}\n{}\n",
            protocol::MEDIA_MARKER,
            protocol::PARTITIONS_MARKER,
            protocol::DIRECT_MARKER,
            protocol::INSTALL_MARKER
        );
        require_installation(&result, "uuid", "/dev/sr0", &target).unwrap();
    }

    fn inventory_document(text: &str) -> Result<td_engine::json::Json, String> {
        diagnostic_document(text, protocol::MAX_INVENTORY_BYTES)
    }

    #[test]
    fn inventory_json_has_a_byte_and_nesting_bound_before_recursive_parse() {
        assert!(inventory_document(&format!(
            "{}{}",
            "{}",
            " ".repeat(protocol::MAX_INVENTORY_BYTES - 3)
        ))
        .is_ok());
        assert!(inventory_document(&format!(
            "{}{}",
            "{}",
            " ".repeat(protocol::MAX_INVENTORY_BYTES - 2)
        ))
        .is_err());
        assert!(inventory_document(&" ".repeat(protocol::MAX_INVENTORY_BYTES + 1)).is_err());
        assert!(inventory_document(&format!("{}0{}", "[".repeat(9), "]".repeat(9))).is_err());
        assert!(inventory_document(r#"{"text":"[[[[[[[[[\"{\\]"}"#).is_ok());
        assert!(inventory_document("{} trailing").is_err());
        assert!(inventory_document("}").is_err());
    }
}

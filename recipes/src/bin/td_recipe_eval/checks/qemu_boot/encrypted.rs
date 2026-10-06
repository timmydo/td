//! `td-recipe-eval qemu-install-encrypted --tpm /absolute/path/to/swtpm`:
//! td-install/ENCRYPTION.md increment 5's encrypted-installation oracle.
//! Outside the integration tier. Like `qemu-secret-system` it takes an
//! explicit swtpm (td-secret/DESIGN.md "TPM validation"), run from a fresh
//! state directory behind a private socket for each leg, never a host TPM;
//! without one it is an unprovisioned host gap, not a usage error. The
//! installed system is not booted: selector release is increment 6.
use super::install::{self, protocol, LiveInstaller, TargetDisk};
use super::*;

/// What this oracle builds: the system and the guest fixture.
pub(crate) const TARGETS: &[&str] = &["system-x86-64", "td-install-qemu-test"];

/// The guest's attachment and the paths it sees.
const SOURCE_DEVICE: &str = "/dev/sr0";
const TARGET_DEVICE: &str = "/dev/vda";
const VOLUME_DEVICE: &str = "/dev/vda2";

/// ENCRYPTION.md "Device-bound formatting": both header copies and the
/// keyslots area, where the data segment starts.
const HEADER_BYTES: u64 = 16 * 1024 * 1024;
const LUKS2_HEADER_BYTES: u64 = 16 * 1024;
const LUKS2_BINARY_BYTES: usize = 4096;
const ENCRYPTION_SECTOR: u64 = 4096;
/// A GPT entry array: 128 entries of 128 bytes.
const GPT_ENTRY_BYTES: u64 = 128 * 128;
const RECOVERY_DIGITS: usize = 48;

pub(crate) fn options(args: &[String]) -> Result<Option<PathBuf>, String> {
    match args {
        [] => Ok(None),
        [flag, path] if flag == "--tpm" && Path::new(path).is_absolute() => {
            Ok(Some(PathBuf::from(path)))
        }
        _ => Err(
            "usage: td-recipe-eval qemu-install-encrypted [--tpm /absolute/path/to/swtpm]".into(),
        ),
    }
}

/// What every device-bound leg builds once: the host's qemu and firmware,
/// the signed system deployment the fixture installs, the fixture's live
/// medium parts and the encrypted target's capacity, in a scratch
/// directory removed when the bench is dropped.
pub(super) struct Bench {
    pub(super) qemu: String,
    pub(super) code: PathBuf,
    pub(super) vars_template: PathBuf,
    pub(super) timeout: Duration,
    pub(super) kernel: PathBuf,
    base: Vec<u8>,
    common: Vec<install::PackedFile>,
    extra: Vec<install::PackedFile>,
    payloads: Vec<(&'static str, PathBuf)>,
    pub(super) payload_bytes: u64,
    pub(super) capacity: u64,
    /// The installed deployment's id.
    pub(super) id: String,
    /// The store's verified selector and deployment, for a live medium.
    pub(super) selector: VerifiedSelector,
    pub(super) store_deployment: PathBuf,
    pub(super) trust: RunTrust,
    pub(super) scratch: Scratch,
}

impl Bench {
    pub(super) fn new(runner: &RecipeCheckRunner) -> Result<Self, String> {
        let qemu = find_qemu()?;
        let timeout = install::installation_timeout(
            env::var("TD_QEMU_BOOT_TIMEOUT_SECS").ok().as_deref(),
            1800,
        );
        let (code, vars_template) = efi::firmware(&qemu)?;
        let (_, selector, source) = build_system(runner)?;
        let trust = RunTrust::generate()?;
        let LiveInstaller {
            kernel,
            base,
            common,
            extra,
        } = LiveInstaller::load(runner, &trust)?;
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let scratch = Scratch {
            dir: create_scratch_dir(runner.scratch_dir(), &SEQ)?,
        };
        let deployment = scratch.dir.join("source");
        fs::create_dir(&deployment)
            .map_err(|error| format!("create system deployment: {error}"))?;
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
        stage_stock_selector_template(&selector, &scratch.dir.join("selector.cpio"))?;
        let payloads: Vec<_> = protocol::MEDIA_FILES
            .iter()
            .map(|(iso_name, name)| (*iso_name, scratch.dir.join(name)))
            .collect();
        // The installation volume loses its LUKS2 header beside the system's
        // own margin.
        let capacity = install::system_target_capacity(payload_bytes)?
            .checked_add(HEADER_BYTES)
            .ok_or("encrypted installation capacity overflow")?;
        Ok(Self {
            qemu,
            code,
            vars_template,
            timeout,
            kernel,
            base,
            common,
            extra,
            payloads,
            payload_bytes,
            capacity,
            id,
            selector,
            store_deployment: source,
            trust,
            scratch,
        })
    }

    /// The fixture's live medium for `phase`, `files` added to its
    /// initramfs. The bytes this builds from `files`, which may be a key,
    /// are zeroed once the medium is written; the medium file itself is
    /// the caller's to remove, and the ISO writer's own buffers are not
    /// reached.
    pub(super) fn medium(
        &self,
        phase: &str,
        files: Vec<install::PackedFile>,
    ) -> Result<PathBuf, String> {
        let live = self.scratch.dir.join(format!("{phase}.cpio"));
        let mut extra = self.extra.clone();
        let fixed = extra.len();
        extra.extend(files);
        let built = install::initramfs(&self.base, &self.common, &format!("{phase}\n"), &extra);
        for (_, _, bytes) in extra.iter_mut().skip(fixed) {
            bytes.fill(0);
            std::hint::black_box(&bytes);
        }
        let mut image = built?;
        let wrote = install::write(&live, &image);
        image.fill(0);
        std::hint::black_box(&image);
        wrote?;
        let iso = self.scratch.dir.join(format!("{phase}.iso"));
        let written = media::write_image_with_payloads(&iso, &self.kernel, &live, &self.payloads);
        fs::remove_file(&live).map_err(|error| format!("remove {}: {error}", live.display()))?;
        written.map(|()| iso)
    }

    /// A fresh copy of the firmware's variable template.
    pub(super) fn vars(&self, name: &str) -> Result<PathBuf, String> {
        let vars = self.scratch.dir.join(format!("{name}-vars.fd"));
        efi::copy_input(&self.vars_template, &vars)?;
        Ok(vars)
    }

    pub(super) fn host(&self) -> Host<'_> {
        Host {
            qemu: &self.qemu,
            code: &self.code,
            scratch: &self.scratch.dir,
            timeout: self.timeout,
        }
    }
}

/// The legs, each on a fresh disk from one ISO apiece: the service's
/// refusal without a TPM, the device-bound installation typed back and
/// verified, and a power cut in the recovery-key phase.
pub(crate) fn run(runner: &RecipeCheckRunner, tpm: &Path) -> Result<(), String> {
    secret::verify_swtpm(tpm)?;
    let bench = Bench::new(runner)?;
    let scratch = &bench.scratch;
    let (capacity, payload_bytes, id) = (bench.capacity, bench.payload_bytes, &bench.id);
    let medium = |phase: &str| bench.medium(phase, Vec::new());
    let firmware = |name: &str| bench.vars(name);
    let host = bench.host();
    static SEQ: AtomicU64 = AtomicU64::new(0);

    // Without a TPM the operand refuses, the disk untouched.
    {
        let iso = medium("install-no-tpm")?;
        let target = TargetDisk::with_capacity(&scratch.dir, "no-tpm.img", capacity)?;
        target.seed_preservation_canaries()?;
        let before = target.fingerprint()?;
        let vars = firmware("no-tpm")?;
        println!("   [qemu-install-encrypted] no TPM: the device-bound service must refuse");
        let result = host.boot(&iso, &vars, &target, protocol::NO_TPM_MARKER, None)?;
        validate_no_tpm(&result)?;
        if target.fingerprint()? != before {
            return Err("the no-TPM leg changed its disk".into());
        }
        remove(&target.path)?;
        remove(&iso)?;
    }

    // Installed, typed back and verified.
    {
        let iso = medium("install-encrypted")?;
        let target = TargetDisk::with_capacity(&scratch.dir, "encrypted.img", capacity)?;
        seed_table(&target.path, capacity)?;
        let vars = firmware("encrypted")?;
        let tpm_scratch = Scratch {
            dir: create_qmp_scratch_dir(&env::temp_dir(), &SEQ)?,
        };
        let emulator = secret::Emulator::start(tpm, &tpm_scratch.dir, "encrypted")?;
        println!(
            "   [qemu-install-encrypted] installing {payload_bytes} bytes device-bound under swtpm"
        );
        let result = host
            .boot(
                &iso,
                &vars,
                &target,
                protocol::ENCRYPTED_END_MARKER,
                Some(emulator.socket.as_path()),
            )
            .map_err(|error| redacted(&emulator.diagnostic(&error)))?;
        println!(
            "   [qemu-install-encrypted] installation elapsed: {:.2}s",
            result.elapsed.as_secs_f64()
        );
        save_console(runner, "encrypted", &result)?;
        let (uuid, mapped) = validate_installation(&result, capacity, id)
            .map_err(|error| redacted(&emulator.diagnostic(&error)))?;
        emulator.finish()?;
        let image = verify_image(&target.path, 512, &uuid, true)?;
        if mapped != image.data_bytes {
            return Err(format!(
                "the guest scanned {mapped} bytes of the opened volume; its data segment is {}",
                image.data_bytes
            ));
        }
        println!(
            "   [qemu-install-encrypted] host: GPT, LUKS2 header {uuid} (keyslots 0 and 1, \
             first-boot token on 1), {} ciphertext bytes free of plaintext markers",
            image.data_bytes
        );
        remove(&target.path)?;
        remove(&iso)?;
    }

    // Power cut in the recovery-key phase: no table, so nothing boots.
    {
        let iso = medium("install-encrypted-cut")?;
        let target = TargetDisk::with_capacity(&scratch.dir, "cut.img", capacity)?;
        seed_table(&target.path, capacity)?;
        let vars = firmware("cut")?;
        let tpm_scratch = Scratch {
            dir: create_qmp_scratch_dir(&env::temp_dir(), &SEQ)?,
        };
        let emulator = secret::Emulator::start(tpm, &tpm_scratch.dir, "cut")?;
        println!("   [qemu-install-encrypted] power cut before the recovery key is typed back");
        let result = host
            .boot(
                &iso,
                &vars,
                &target,
                protocol::ENCRYPTED_END_MARKER,
                Some(emulator.socket.as_path()),
            )
            .map_err(|error| redacted(&emulator.diagnostic(&error)))?;
        save_console(runner, "cut", &result)?;
        let uuid = validate_cut(&result).map_err(|error| redacted(&emulator.diagnostic(&error)))?;
        emulator.finish()?;
        verify_image(&target.path, 512, &uuid, false)?;
        remove(&target.path)?;
        remove(&iso)?;
    }
    println!(
        "PASS: device-bound installation through the installation service under the pinned \
         swtpm: refused without a TPM with its disk unchanged; LUKS2 formatted with the plan's \
         UUID, keyslots 0 and 1 and the first-boot token, verified in the guest by td's reader \
         and the TPM; recovery key sent once, a second ask and a mistyped type-back refused, \
         confirmed, then the table written over the seeded one's cleared ranges; the recovery \
         key opens the volume holding the published deployment and settings and a mistyped \
         one does not; no recovery key on the disk, in the opened volume, on the console or in \
         sampled command lines, every sampled cryptsetup line one of td-install's, luksFormat \
         and luksAddKey among them; no Btrfs or staged plaintext in a data segment that \
         started zeroed (no erasure claim); a power cut before the type-back leaves the seeded \
         table's ranges zero. The installed system is not booted: selector release is \
         increment 6"
    );
    Ok(())
}

/// What every leg's boot shares: the host's qemu and firmware code, the
/// scratch directory and the deadline.
pub(super) struct Host<'a> {
    pub(super) qemu: &'a str,
    pub(super) code: &'a Path,
    pub(super) scratch: &'a Path,
    pub(super) timeout: Duration,
}

impl Host<'_> {
    /// One firmware boot of `iso` as optical media, `target` the virtio
    /// installation disk and `socket` the emulator's, if any. The runner's
    /// own failures (a deadline, a console read) carry the console's last
    /// lines, so they are redacted here, as nowhere else.
    fn boot(
        &self,
        iso: &Path,
        vars: &Path,
        target: &TargetDisk,
        marker: &str,
        socket: Option<&Path>,
    ) -> Result<BootResult, String> {
        self.boot_with(iso, vars, target, marker, socket, None, None)
    }

    /// `boot`, the guest's private virtio port written to `side_channel`
    /// and the whole raw console to `console` (`BootPlan::keep_console`).
    #[allow(clippy::too_many_arguments)]
    pub(super) fn boot_with(
        &self,
        iso: &Path,
        vars: &Path,
        target: &TargetDisk,
        marker: &str,
        socket: Option<&Path>,
        side_channel: Option<&Path>,
        console: Option<&Path>,
    ) -> Result<BootResult, String> {
        let mut plan = install::plan(iso, true, marker);
        plan.tpm_socket = socket;
        plan.side_channel = side_channel;
        plan.keep_console = console;
        boot_source(
            self.qemu,
            BootSource::Firmware {
                code: self.code,
                vars,
                attachment: FirmwareAttachment::Optical,
                installation_target: Some(target),
            },
            plan,
            self.scratch,
            self.timeout,
        )
        .map_err(|error| redacted(&error))
    }
}

pub(super) fn remove(path: &Path) -> Result<(), String> {
    fs::remove_file(path).map_err(|error| format!("remove {}: {error}", path.display()))
}

/// The console kept for diagnosis, key-shaped text redacted, after the
/// key-text check: a console that fails it is not saved.
fn save_console(runner: &RecipeCheckRunner, leg: &str, result: &BootResult) -> Result<(), String> {
    require_no_key_text(&result.console)?;
    fs::write(
        runner
            .scratch_dir()
            .join(format!("install-encrypted-{leg}.log")),
        redacted(&result.console),
    )
    .map_err(|error| format!("save the {leg} console: {error}"))
}

/// The console's last lines for an error, key-shaped text redacted.
fn shown(result: &BootResult) -> String {
    tail(&redacted(&result.console), 160)
}

/// `console` with every run of digits and hyphens holding a recovery key's
/// worth of digits replaced: what `require_no_key_text` refuses never
/// reaches a log.
pub(super) fn redacted(console: &str) -> String {
    fn flush(run: &mut String, digits: &mut usize, out: &mut String) {
        if *digits >= RECOVERY_DIGITS {
            out.push_str("[key-shaped text redacted]");
        } else {
            out.push_str(run);
        }
        run.clear();
        *digits = 0;
    }
    let mut out = String::with_capacity(console.len());
    let mut run = String::new();
    let mut digits = 0;
    for character in console.chars() {
        if character.is_ascii_digit() || character == '-' {
            if character.is_ascii_digit() {
                digits += 1;
            }
            run.push(character);
        } else {
            flush(&mut run, &mut digits, &mut out);
            out.push(character);
        }
    }
    flush(&mut run, &mut digits, &mut out);
    out
}

/// Writes a valid GPT, one Linux partition over the usable range, into
/// only the two table ranges of the zeroed `path`: the service must clear
/// it, and the data segment stays as zero as the checks need.
pub(super) fn seed_table(path: &Path, capacity: u64) -> Result<(), String> {
    use std::os::unix::fs::FileExt;
    let sector = 512;
    let disk_sectors = capacity / sector;
    let align = td_boot_protocol::PARTITION_ALIGN_BYTES / sector;
    let layout = td_engine::gpt::Layout {
        sector_size: sector,
        disk_sectors,
        disk_guid: td_engine::gpt::Guid([0x5e; 16]),
        align_sectors: align,
        partitions: vec![td_engine::gpt::Partition {
            type_guid: td_engine::gpt::TYPE_LINUX_FS,
            unique_guid: td_engine::gpt::Guid([0xa5; 16]),
            start_lba: align,
            end_lba: td_engine::gpt::last_usable_lba(sector, disk_sectors)?,
            attributes: 0,
            name: "existing".into(),
        }],
    };
    let table = td_engine::gpt::build(&layout)?;
    let file = OpenOptions::new()
        .write(true)
        .open(path)
        .map_err(|error| format!("open {}: {error}", path.display()))?;
    for (offset, bytes) in [
        (table.primary_offset, &table.primary),
        (table.backup_offset, &table.backup),
    ] {
        file.write_all_at(bytes, offset)
            .map_err(|error| format!("seed a table in {}: {error}", path.display()))?;
    }
    file.sync_all()
        .map_err(|error| format!("sync {}: {error}", path.display()))?;
    let file = File::open(path).map_err(|error| format!("open {}: {error}", path.display()))?;
    read_table(&file, capacity, sector).map(drop)
}

/// Exactly one line equal to `line`.
fn require_once(result: &BootResult, line: &str, what: &str) -> Result<(), String> {
    let count = result
        .console
        .lines()
        .filter(|each| each.trim_end() == line)
        .count();
    if count == 1 {
        Ok(())
    } else {
        Err(format!(
            "{what}: {count} lines read {line:?}, not one; {}\n{}",
            result.reason,
            shown(result)
        ))
    }
}

/// The one line starting with `prefix` and a space, its remainder.
fn field<'a>(result: &'a BootResult, prefix: &str) -> Result<&'a str, String> {
    let lead = format!("{prefix} ");
    let mut found = result
        .console
        .lines()
        .filter_map(|line| line.trim_end().strip_prefix(lead.as_str()));
    match (found.next(), found.next()) {
        (Some(rest), None) => Ok(rest),
        _ => Err(format!(
            "expected one {prefix} record; {}\n{}",
            result.reason,
            shown(result)
        )),
    }
}

/// The boot stopped on the bare `ENCRYPTED_END_MARKER` line, which the
/// guest prints after `record`, so `record` and every line before it
/// arrived whole: the one record's arguments.
fn reached<'a>(result: &'a BootResult, record: &str, phase: &str) -> Result<&'a str, String> {
    let lines: Vec<&str> = result.console.lines().map(str::trim_end).collect();
    let end = protocol::ENCRYPTED_END_MARKER;
    let ends: Vec<usize> = (0..lines.len())
        .filter(|i| lines.get(*i) == Some(&end))
        .collect();
    let lead = format!("{record} ");
    let records: Vec<usize> = (0..lines.len())
        .filter(|i| lines.get(*i).is_some_and(|line| line.starts_with(&lead)))
        .collect();
    match (result.evidence.target, ends.as_slice(), records.as_slice()) {
        (true, [end], [at]) if at < end => field(result, record),
        _ => Err(format!(
            "{phase} did not reach {record} then {end}: {}\n{}",
            result.reason,
            shown(result)
        )),
    }
}

/// Exactly one line equal to `marker`, which ended the boot.
fn require_end(result: &BootResult, marker: &str, phase: &str) -> Result<(), String> {
    if result.evidence.target {
        require_once(result, marker, phase)
    } else {
        Err(format!(
            "{phase} did not reach {marker}: {}\n{}",
            result.reason,
            shown(result)
        ))
    }
}

fn refused(result: &BootResult) -> bool {
    result
        .console
        .lines()
        .any(|line| line.starts_with(protocol::REFUSED_PREFIX))
}

fn validate_no_tpm(result: &BootResult) -> Result<(), String> {
    require_no_key_text(&result.console)?;
    require_end(result, protocol::NO_TPM_MARKER, "no-TPM refusal")?;
    require_once(
        result,
        &format!("{} {SOURCE_DEVICE}", protocol::MEDIA_MARKER),
        "read-only medium",
    )?;
    if !result.console.contains(protocol::NO_TPM_DIAGNOSTIC)
        || refused(result)
        || result.console.contains(protocol::SERVED_MARKER)
    {
        return Err(format!(
            "the no-TPM leg did not refuse as the operand requires\n{}",
            shown(result)
        ));
    }
    Ok(())
}

/// The guest's evidence for the installed leg; returns the plan's UUID and
/// the bytes the guest scanned of the opened volume.
pub(super) fn validate_installation(
    result: &BootResult,
    capacity: u64,
    id: &str,
) -> Result<(String, u64), String> {
    require_no_key_text(&result.console)?;
    let ended = reached(
        result,
        protocol::ENCRYPTED_MARKER,
        "device-bound installation",
    )?;
    if refused(result) {
        return Err(format!("the guest refused\n{}", shown(result)));
    }
    require_once(
        result,
        &format!("{} {SOURCE_DEVICE}", protocol::MEDIA_MARKER),
        "read-only medium",
    )?;
    let uuid = ended
        .strip_suffix(&format!(" {TARGET_DEVICE}"))
        .filter(|uuid| protocol::is_v4_volume_uuid(uuid))
        .ok_or_else(|| format!("malformed {} record", protocol::ENCRYPTED_MARKER))?;
    for line in [
        format!(
            "{} {uuid} {TARGET_DEVICE}",
            protocol::ENCRYPTED_NO_TABLE_MARKER
        ),
        protocol::RECOVERY_TYPED_BACK_MARKER.to_string(),
        format!("{} {uuid} {TARGET_DEVICE}", protocol::SERVED_MARKER),
        format!("{} {capacity}", protocol::ENCRYPTED_KEY_ABSENT_MARKER),
    ] {
        require_once(result, &line, "device-bound installation")?;
    }
    let mapped = field(result, protocol::ENCRYPTED_VOLUME_MARKER)?
        .strip_prefix(&format!("{uuid} {VOLUME_DEVICE} {id} "))
        .and_then(|bytes| bytes.parse::<u64>().ok())
        .ok_or_else(|| format!("malformed {} record", protocol::ENCRYPTED_VOLUME_MARKER))?;
    sampled_invocations(field(result, protocol::ENCRYPTED_CMDLINES_MARKER)?)?;
    if result.console.contains(protocol::ENCRYPTED_CUT_MARKER) {
        return Err("the installed leg reported a power cut".into());
    }
    Ok((uuid.to_string(), mapped))
}

/// The cryptsetup invocations the guest must have sampled: the format and
/// the protector's keyslot, which every installation runs once.
const REQUIRED_INVOCATIONS: &[&str] = &["luksAddKey", "luksFormat"];

/// The CMDLINES record: distinct lines, cryptsetup's among them, and the
/// invocations seen, which include `REQUIRED_INVOCATIONS`.
fn sampled_invocations(sampled: &str) -> Result<(), String> {
    let mut fields = sampled.split(' ');
    let counts = (fields.next(), fields.next(), fields.next(), fields.next());
    let (Some(lines), Some(cryptsetup), Some(seen), None) = counts else {
        return Err(format!(
            "malformed {} record",
            protocol::ENCRYPTED_CMDLINES_MARKER
        ));
    };
    let (Ok(lines), Ok(cryptsetup)) = (lines.parse::<u64>(), cryptsetup.parse::<u64>()) else {
        return Err(format!(
            "malformed {} record",
            protocol::ENCRYPTED_CMDLINES_MARKER
        ));
    };
    let seen: Vec<&str> = seen.split(',').collect();
    if cryptsetup == 0
        || lines < cryptsetup
        || REQUIRED_INVOCATIONS
            .iter()
            .any(|required| !seen.contains(required))
    {
        return Err(format!(
            "the guest did not sample luksFormat and luksAddKey: {sampled}"
        ));
    }
    Ok(())
}

/// The guest's evidence for the power-cut leg; returns the plan's UUID.
fn validate_cut(result: &BootResult) -> Result<String, String> {
    require_no_key_text(&result.console)?;
    let cut = reached(
        result,
        protocol::ENCRYPTED_CUT_MARKER,
        "recovery-key power cut",
    )?;
    if !result.marker_killed || result.exited_clean || refused(result) {
        return Err(format!(
            "the cut leg did not end in the host's power cut: {}\n{}",
            result.reason,
            shown(result)
        ));
    }
    let uuid = cut
        .strip_suffix(&format!(" {TARGET_DEVICE}"))
        .filter(|uuid| protocol::is_v4_volume_uuid(uuid))
        .ok_or_else(|| format!("malformed {} record", protocol::ENCRYPTED_CUT_MARKER))?;
    require_once(
        result,
        &format!(
            "{} {uuid} {TARGET_DEVICE}",
            protocol::ENCRYPTED_NO_TABLE_MARKER
        ),
        "recovery-key phase",
    )?;
    for later in [
        protocol::RECOVERY_TYPED_BACK_MARKER,
        protocol::SERVED_MARKER,
        protocol::ENCRYPTED_MARKER,
    ] {
        if result.console.contains(later) {
            return Err(format!("the cut leg went on past the cut: {later}"));
        }
    }
    Ok(uuid.to_string())
}

/// No run of a key's 48 digits, nor its hyphen-grouped display form, in
/// what the guest printed: the oracle never learns the key, so it looks
/// for its shape.
pub(super) fn require_no_key_text(console: &str) -> Result<(), String> {
    let bytes = console.as_bytes();
    let mut run = 0;
    for byte in bytes {
        run = if byte.is_ascii_digit() { run + 1 } else { 0 };
        if run >= RECOVERY_DIGITS {
            return Err("the console carries a run of 48 digits".into());
        }
    }
    let display = |window: &[u8]| {
        window
            .iter()
            .enumerate()
            .all(|(index, byte)| match index % 7 {
                6 => *byte == b'-',
                _ => byte.is_ascii_digit(),
            })
    };
    if bytes.windows(55).any(display) {
        return Err("the console carries a recovery key's display form".into());
    }
    Ok(())
}

/// What the image holds where a check found it.
pub(super) struct Image {
    pub(super) data_bytes: u64,
}

pub(super) fn read_at(file: &File, offset: u64, len: u64) -> Result<Vec<u8>, String> {
    use std::os::unix::fs::FileExt;
    let mut bytes = vec![0; usize::try_from(len).map_err(|_| "read length overflow")?];
    file.read_exact_at(&mut bytes, offset)
        .map_err(|error| format!("read {len} bytes at {offset}: {error}"))?;
    Ok(bytes)
}

fn be64(bytes: &[u8], at: usize) -> Result<u64, String> {
    bytes
        .get(at..at + 8)
        .and_then(|field| field.try_into().ok())
        .map(u64::from_be_bytes)
        .ok_or_else(|| format!("short field at {at}"))
}

/// A NUL-padded text field.
fn text(bytes: &[u8], range: std::ops::Range<usize>) -> Result<&str, String> {
    let field = bytes.get(range).ok_or("short text field")?;
    let end = field
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(field.len());
    std::str::from_utf8(field.get(..end).unwrap_or_default())
        .map_err(|_| "non-UTF-8 text field".to_string())
}

/// The written image, read on the host after QEMU is gone. Complete: the
/// protective MBR and both GPT copies, an ESP of the fixed layout and the
/// volume after it. Cut: both table ranges zero. Either way the volume
/// carries the LUKS2 header ENCRYPTION.md formats, and, complete, its data
/// segment holds no plaintext marker.
pub(super) fn verify_image(
    path: &Path,
    sector: u64,
    uuid: &str,
    complete: bool,
) -> Result<Image, String> {
    let file = File::open(path).map_err(|error| format!("open {}: {error}", path.display()))?;
    let len = file
        .metadata()
        .map_err(|error| format!("stat {}: {error}", path.display()))?
        .len();
    let volume = td_boot_protocol::PARTITION_ALIGN_BYTES
        .checked_add(td_boot_protocol::ESP_BYTES)
        .ok_or("volume offset overflow")?;
    let extent = if complete {
        // Stock cryptsetup maps the whole partition, so the table must place
        // whole encryption sectors.
        whole_sectors(volume, verify_table(&file, len, sector, volume)?)?
    } else {
        verify_no_table(&file, len, sector)?;
        // No table to read: the layout's own rule, the usable range cut to
        // whole encryption sectors.
        len.checked_sub(sector + GPT_ENTRY_BYTES)
            .and_then(|end| end.checked_sub(volume))
            .map(|bytes| bytes / ENCRYPTION_SECTOR * ENCRYPTION_SECTOR)
            .ok_or("image smaller than its backup table")?
    };
    if extent <= HEADER_BYTES {
        return Err("the volume holds no data segment".into());
    }
    let header = read_at(&file, volume, 2 * LUKS2_HEADER_BYTES)?;
    verify_luks2(&header, uuid)?;
    let data = volume + HEADER_BYTES;
    let data_bytes = extent - HEADER_BYTES;
    if complete {
        verify_ciphertext(&file, data, data_bytes, &[])?;
    }
    Ok(Image { data_bytes })
}

/// The volume partition's length, which must start and end on whole
/// encryption sectors for cryptsetup's dynamic segment to map it.
fn whole_sectors(start: u64, end: u64) -> Result<u64, String> {
    end.checked_sub(start)
        .filter(|bytes| {
            start.is_multiple_of(ENCRYPTION_SECTOR) && bytes.is_multiple_of(ENCRYPTION_SECTOR)
        })
        .ok_or_else(|| {
            format!(
                "the volume partition [{start}, {end}) is not whole \
                 {ENCRYPTION_SECTOR}-byte encryption sectors"
            )
        })
}

/// Both GPT copies of a `capacity`-byte image, through td-engine's reader:
/// signatures, both CRCs, each copy's own LBAs, the copies agreeing, and
/// every entry within the usable range.
fn read_table(file: &File, capacity: u64, sector: u64) -> Result<td_engine::gpt::Table, String> {
    let disk_sectors = capacity / sector;
    let array = GPT_ENTRY_BYTES / sector;
    let span = |sectors: u64| sectors.checked_mul(sector).ok_or("GPT size overflow");
    let primary = read_at(file, 0, span(array + 2)?)?;
    let backup_at = disk_sectors
        .checked_sub(array + 1)
        .ok_or("the image is smaller than a GPT")?;
    let backup = read_at(file, span(backup_at)?, span(array + 1)?)?;
    let table = td_engine::gpt::parse(&primary, &backup, sector)?;
    if table.disk_sectors != disk_sectors {
        return Err(format!(
            "the GPT describes {} sectors of a {disk_sectors}-sector image",
            table.disk_sectors
        ));
    }
    Ok(table)
}

/// A complete table of the fixed layout: the ESP, holding a FAT boot
/// sector, and the volume after it; returns the volume partition's end.
fn verify_table(file: &File, len: u64, sector: u64, volume: u64) -> Result<u64, String> {
    let table = read_table(file, len, sector)?;
    let align = td_boot_protocol::PARTITION_ALIGN_BYTES / sector;
    let esp_end = align + td_boot_protocol::ESP_BYTES / sector - 1;
    let last = td_engine::gpt::last_usable_lba(sector, len / sector)?;
    let [esp, system] = table.partitions.as_slice() else {
        return Err(format!(
            "the table holds {} partitions, not the ESP and the volume",
            table.partitions.len()
        ));
    };
    if esp.type_guid != td_engine::gpt::TYPE_ESP
        || esp.name != td_boot_protocol::ESP_PARTITION_NAME
        || (esp.start_lba, esp.end_lba) != (align, esp_end)
    {
        return Err("partition 1 is not the fixed layout's ESP".into());
    }
    let start = system.start_lba.checked_mul(sector);
    let end = system
        .end_lba
        .checked_add(1)
        .and_then(|lba| lba.checked_mul(sector));
    if system.type_guid != td_engine::gpt::TYPE_LINUX_FS
        || system.name != td_boot_protocol::VOLUME_PARTITION_NAME
        || start != Some(volume)
        || system.end_lba <= system.start_lba
        || system.end_lba > last
    {
        return Err("partition 2 is not the volume after the ESP".into());
    }
    let esp_boot = read_at(file, td_boot_protocol::PARTITION_ALIGN_BYTES, 512)?;
    if esp_boot.get(510..512) != Some(&[0x55, 0xaa][..]) {
        return Err("the ESP holds no FAT boot sector".into());
    }
    end.ok_or_else(|| "partition bound overflow".into())
}

/// Both GPT ranges zero: the protective MBR, primary header and entry
/// array, and the backup array and header ending the disk.
fn verify_no_table(file: &File, len: u64, sector: u64) -> Result<(), String> {
    let primary = 2 * sector + GPT_ENTRY_BYTES;
    let backup = sector + GPT_ENTRY_BYTES;
    for (copy, offset, bytes) in [
        ("primary", 0, primary),
        (
            "backup",
            len.checked_sub(backup).ok_or("image too small")?,
            backup,
        ),
    ] {
        if read_at(file, offset, bytes)?.iter().any(|byte| *byte != 0) {
            return Err(format!(
                "the {copy} table range is written although the key was never typed back"
            ));
        }
    }
    Ok(())
}

/// One LUKS2 token as the header holds it: its number, type, td role (if
/// it has one) and the keyslots it names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct HeaderToken {
    pub(super) number: u8,
    pub(super) kind: String,
    pub(super) role: Option<String>,
    pub(super) keyslots: Vec<u8>,
}

/// What a LUKS2 header holds where these oracles look: its keyslot
/// numbers and its tokens, in ascending order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct HeaderState {
    pub(super) keyslots: Vec<u8>,
    pub(super) tokens: Vec<HeaderToken>,
}

impl HeaderState {
    /// The td-protector tokens, as `number role keyslots` text for a
    /// report.
    pub(super) fn describe(&self) -> String {
        let tokens: Vec<String> = self
            .tokens
            .iter()
            .map(|token| {
                format!(
                    "{}:{}@{:?}",
                    token.number,
                    token.role.as_deref().unwrap_or(&token.kind),
                    token.keyslots
                )
            })
            .collect();
        format!(
            "keyslots {:?} tokens [{}]",
            self.keyslots,
            tokens.join(", ")
        )
    }
}

/// Both LUKS2 header copies, read independently of td-protector's reader:
/// magic, version, the formatted 16 KiB size, each copy's own offset, a
/// valid SHA-256 checksum, the plan's UUID and label, the copies' JSON
/// agreeing, every keyslot PBKDF2-SHA256 at 1000 iterations over a 512-bit
/// key and the one AES-XTS segment at 16 MiB in 4 KiB sectors. Returns the
/// keyslots and tokens.
pub(super) fn parse_luks2(header: &[u8], uuid: &str) -> Result<HeaderState, String> {
    let (_, primary) = header_copy(header, uuid, false)?;
    let (_, secondary) = header_copy(header, uuid, true)?;
    if primary != secondary {
        return Err("the LUKS2 header copies disagree".into());
    }
    header_state(&primary)
}

/// The header after a power cut, read as cryptsetup and td's reader read
/// it: the checksum-valid copy with the higher sequence number, both
/// agreeing when they share one. A cut inside cryptsetup's header write
/// leaves one copy torn or a step behind; that is reported, not refused.
pub(super) fn parse_luks2_after_cut(
    header: &[u8],
    uuid: &str,
) -> Result<(HeaderState, Option<String>), String> {
    let (chosen, note) = match (
        header_copy(header, uuid, false),
        header_copy(header, uuid, true),
    ) {
        (Ok(primary), Ok(secondary)) if primary.0 == secondary.0 => {
            if primary.1 != secondary.1 {
                return Err(format!(
                    "the LUKS2 header copies disagree at one sequence number, {}",
                    primary.0
                ));
            }
            (primary.1, None)
        }
        (Ok(primary), Ok(secondary)) => {
            let (newer, stale, which) = if primary.0 > secondary.0 {
                (primary, secondary, "secondary")
            } else {
                (secondary, primary, "primary")
            };
            let note = format!(
                "the {which} copy is a write behind (sequence {} after {})",
                stale.0, newer.0
            );
            (newer.1, Some(note))
        }
        (Ok(primary), Err(torn)) => (
            primary.1,
            Some(format!("the secondary copy is torn: {torn}")),
        ),
        (Err(torn), Ok(secondary)) => (
            secondary.1,
            Some(format!("the primary copy is torn: {torn}")),
        ),
        (Err(primary), Err(secondary)) => {
            return Err(format!(
                "both LUKS2 header copies refused: {primary}; {secondary}"
            ))
        }
    };
    Ok((header_state(&chosen)?, note))
}

/// One header copy, independently of the other: its sequence number and
/// JSON text once its binary fields, label, UUID and checksum check out.
fn header_copy(header: &[u8], uuid: &str, secondary: bool) -> Result<(u64, String), String> {
    let size = usize::try_from(LUKS2_HEADER_BYTES).map_err(|_| "header size overflow")?;
    let (copy, magic, at) = if secondary {
        ("secondary", &b"SKUL\xba\xbe"[..], size)
    } else {
        ("primary", &b"LUKS\xba\xbe"[..], 0usize)
    };
    let area = header.get(at..at + size).ok_or("short LUKS2 header")?;
    if area.get(..6) != Some(magic)
        || area.get(6..8) != Some(&[0, 2][..])
        || be64(area, 8)? != LUKS2_HEADER_BYTES
        || be64(area, 256)? != at as u64
        || text(area, 72..104)? != "sha256"
        || text(area, 24..72)? != "td-system"
        || text(area, 168..208)? != uuid
    {
        return Err(format!(
            "the {copy} LUKS2 header is not the one formatted for {uuid}"
        ));
    }
    let mut zeroed = area.to_vec();
    zeroed
        .get_mut(448..512)
        .ok_or("short LUKS2 header")?
        .fill(0);
    let mut hash = crate::sha256::Sha256::new();
    hash.update(&zeroed);
    if area.get(448..480) != Some(&hash.finalize()[..]) {
        return Err(format!("the {copy} LUKS2 header's checksum is wrong"));
    }
    let seqid = be64(area, 16)?;
    let area = area.get(LUKS2_BINARY_BYTES..).ok_or("short LUKS2 header")?;
    let end = area
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(area.len());
    let text = std::str::from_utf8(area.get(..end).unwrap_or_default())
        .map_err(|_| "non-UTF-8 LUKS2 JSON")?;
    Ok((seqid, text.to_string()))
}

/// The keyslots and tokens of one copy's JSON, every keyslot PBKDF2-SHA256
/// at 1000 iterations over a 512-bit key and the segment and layout the
/// format leaves.
fn header_state(json: &str) -> Result<HeaderState, String> {
    let json = td_engine::json::parse(json)?;
    fn keys(value: Option<&td_engine::json::Json>) -> Option<Vec<&str>> {
        match value {
            Some(td_engine::json::Json::Obj(entries)) => {
                Some(entries.iter().map(|(key, _)| key.as_str()).collect())
            }
            _ => None,
        }
    }
    let get = |path: &[&str]| path.iter().try_fold(&json, |value, key| value.get(key));
    let number = |path: &[&str]| match get(path) {
        Some(td_engine::json::Json::Num(value)) => Some(value.as_str()),
        _ => None,
    };
    let string = |path: &[&str]| get(path).and_then(td_engine::json::Json::as_str);
    let slot = |text: &str| -> Result<u8, String> {
        text.parse::<u8>()
            .ok()
            .filter(|slot| *slot < 32 && slot.to_string() == text)
            .ok_or_else(|| format!("a keyslot number {text:?} is not one LUKS2 writes"))
    };
    let mut keyslots = Vec::new();
    for name in keys(get(&["keyslots"])).ok_or("the LUKS2 header has no keyslots object")? {
        if string(&["keyslots", name, "kdf", "type"]) != Some("pbkdf2")
            || string(&["keyslots", name, "kdf", "hash"]) != Some("sha256")
            || number(&["keyslots", name, "kdf", "iterations"]) != Some("1000")
            || number(&["keyslots", name, "key_size"]) != Some("64")
        {
            return Err(format!(
                "keyslot {name} is not PBKDF2-SHA256 at 1000 iterations over a 512-bit key"
            ));
        }
        keyslots.push(slot(name)?);
    }
    keyslots.sort_unstable();
    let mut tokens = Vec::new();
    for name in keys(get(&["tokens"])).ok_or("the LUKS2 header has no tokens object")? {
        let kind = string(&["tokens", name, "type"])
            .ok_or_else(|| format!("token {name} has no type"))?
            .to_string();
        let mut named = Vec::new();
        for each in get(&["tokens", name, "keyslots"])
            .and_then(td_engine::json::Json::as_arr)
            .ok_or_else(|| format!("token {name} has no keyslots array"))?
        {
            named.push(slot(each.as_str().ok_or_else(|| {
                format!("token {name} names a keyslot as a non-string")
            })?)?);
        }
        tokens.push(HeaderToken {
            number: slot(name)?,
            role: string(&["tokens", name, "role"]).map(str::to_string),
            kind,
            keyslots: named,
        });
    }
    tokens.sort_by_key(|token| token.number);
    if keys(get(&["segments"])) != Some(vec!["0"])
        || string(&["segments", "0", "offset"]) != Some("16777216")
        || string(&["segments", "0", "encryption"]) != Some("aes-xts-plain64")
        || number(&["segments", "0", "sector_size"]) != Some("4096")
        || string(&["config", "json_size"]) != Some("12288")
        || string(&["config", "keyslots_size"]) != Some("16744448")
    {
        return Err("the LUKS2 segment or layout is not ENCRYPTION.md's".into());
    }
    Ok(HeaderState { keyslots, tokens })
}

/// The header the installer formats: keyslots 0 and 1 and one
/// td-protector first-boot token, numbered 0, on keyslot 1.
fn verify_luks2(header: &[u8], uuid: &str) -> Result<(), String> {
    let state = parse_luks2(header, uuid)?;
    if state.keyslots != [0, 1] {
        return Err("the LUKS2 header does not hold exactly keyslots 0 and 1".into());
    }
    if state.tokens
        != [HeaderToken {
            number: 0,
            kind: "td-protector".into(),
            role: Some("first-boot".into()),
            keyslots: vec![1],
        }]
    {
        return Err("the LUKS2 header does not carry one first-boot token on keyslot 1".into());
    }
    Ok(())
}

/// Both LUKS2 header copies of the volume in the image at `path`, after
/// its fixed GPT layout's ESP.
pub(super) fn image_header(path: &Path, uuid: &str) -> Result<HeaderState, String> {
    let file = File::open(path).map_err(|error| format!("open {}: {error}", path.display()))?;
    parse_luks2(
        &read_at(&file, volume_offset()?, 2 * LUKS2_HEADER_BYTES)?,
        uuid,
    )
}

/// `image_header` after a power cut: see `parse_luks2_after_cut`.
pub(super) fn image_header_after_cut(
    path: &Path,
    uuid: &str,
) -> Result<(HeaderState, Option<String>), String> {
    let file = File::open(path).map_err(|error| format!("open {}: {error}", path.display()))?;
    parse_luks2_after_cut(
        &read_at(&file, volume_offset()?, 2 * LUKS2_HEADER_BYTES)?,
        uuid,
    )
}

/// The installed volume's first byte: the ESP's, past the alignment.
pub(super) fn volume_offset() -> Result<u64, String> {
    td_boot_protocol::PARTITION_ALIGN_BYTES
        .checked_add(td_boot_protocol::ESP_BYTES)
        .ok_or_else(|| "volume offset overflow".into())
}

/// The data segment as the disk holds it: ciphertext where Btrfs keeps its
/// superblocks, and no Btrfs magic, staged setting or manifest header at
/// any offset. The disk started zeroed, so this shows the filesystem only
/// inside the mapping; it claims nothing about erasure.
fn verify_ciphertext(file: &File, data: u64, bytes: u64, extra: &[&[u8]]) -> Result<(), String> {
    for superblock in [64 * 1024, 64 * 1024 * 1024] {
        let block = read_at(file, data + superblock, 4096)?;
        if block.iter().all(|byte| *byte == 0) {
            return Err(format!(
                "nothing was written where the Btrfs superblock at {superblock} lives"
            ));
        }
        if block.get(64..72) == Some(&b"_BHRfS_M"[..]) {
            return Err(format!(
                "a plaintext Btrfs superblock at data offset {superblock}"
            ));
        }
    }
    let mut needles: Vec<&[u8]> = vec![
        b"_BHRfS_M",
        protocol::HOSTNAME.as_bytes(),
        protocol::TIMEZONE_ID.as_bytes(),
        td_boot_protocol::MANIFEST_HEADER,
    ];
    needles.extend_from_slice(extra);
    scan(file, data, bytes, &needles)
}

/// An installed image's data segment, after its systems ran: written where
/// Btrfs keeps its superblocks, and no plaintext marker, `extra` among
/// them, at any offset. Returns the segment's length.
pub(super) fn installed_ciphertext(path: &Path, extra: &[&[u8]]) -> Result<u64, String> {
    let file = File::open(path).map_err(|error| format!("open {}: {error}", path.display()))?;
    let len = file
        .metadata()
        .map_err(|error| format!("stat {}: {error}", path.display()))?
        .len();
    let volume = volume_offset()?;
    let extent = whole_sectors(volume, verify_table(&file, len, 512, volume)?)?;
    let data_bytes = extent
        .checked_sub(HEADER_BYTES)
        .filter(|bytes| *bytes > 0)
        .ok_or("the volume holds no data segment")?;
    verify_ciphertext(&file, volume + HEADER_BYTES, data_bytes, extra)?;
    Ok(data_bytes)
}

/// `needles` absent from `bytes` of `file` from `offset`, read in chunks
/// that overlap by the longest needle.
fn scan(file: &File, offset: u64, bytes: u64, needles: &[&[u8]]) -> Result<(), String> {
    use std::os::unix::fs::FileExt;
    const CHUNK: usize = 8 << 20;
    let keep = needles.iter().map(|needle| needle.len()).max().unwrap_or(1) - 1;
    let mut buffer = vec![0; keep + CHUNK];
    let mut carried = 0;
    let mut at = 0u64;
    while at < bytes {
        let want = usize::try_from((bytes - at).min(CHUNK as u64)).unwrap_or(CHUNK);
        let free = buffer
            .get_mut(carried..carried + want)
            .ok_or("scan buffer overflow")?;
        file.read_exact_at(free, offset + at)
            .map_err(|error| format!("read the data segment at {at}: {error}"))?;
        let filled = carried + want;
        let window = buffer.get(..filled).ok_or("scan buffer overflow")?;
        for needle in needles {
            if window
                .windows(needle.len())
                .any(|candidate| candidate == *needle)
            {
                return Err(format!(
                    "plaintext {:?} in the data segment near byte {at}",
                    String::from_utf8_lossy(needle)
                ));
            }
        }
        let start = filled.saturating_sub(keep);
        buffer.copy_within(start..filled, 0);
        carried = filled - start;
        at += want as u64;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const UUID: &str = "5a5a5a5a-5a5a-405a-805a-5a5a5a5a5a5a";

    fn json() -> String {
        let keyslot = r#"{"type":"luks2","key_size":64,"kdf":{"type":"pbkdf2","hash":"sha256","iterations":1000,"salt":"AA=="}}"#;
        format!(
            r#"{{"keyslots":{{"0":{keyslot},"1":{keyslot}}},"tokens":{{"0":{{"type":"td-protector","keyslots":["1"],"role":"first-boot","public":"00","private":"00"}}}},"segments":{{"0":{{"type":"crypt","offset":"16777216","size":"dynamic","iv_tweak":"0","encryption":"aes-xts-plain64","sector_size":4096}}}},"digests":{{}},"config":{{"json_size":"12288","keyslots_size":"16744448"}}}}"#
        )
    }

    fn copy(magic: &[u8], at: u64, json: &str) -> Vec<u8> {
        let mut area = vec![0u8; LUKS2_HEADER_BYTES as usize];
        area[..6].copy_from_slice(magic);
        area[6..8].copy_from_slice(&2u16.to_be_bytes());
        area[8..16].copy_from_slice(&LUKS2_HEADER_BYTES.to_be_bytes());
        area[24..33].copy_from_slice(b"td-system");
        area[72..78].copy_from_slice(b"sha256");
        area[168..168 + UUID.len()].copy_from_slice(UUID.as_bytes());
        area[256..264].copy_from_slice(&at.to_be_bytes());
        area[4096..4096 + json.len()].copy_from_slice(json.as_bytes());
        let mut hash = crate::sha256::Sha256::new();
        hash.update(&area);
        area[448..480].copy_from_slice(&hash.finalize());
        area
    }

    fn header(json: &str) -> Vec<u8> {
        let mut bytes = copy(b"LUKS\xba\xbe", 0, json);
        bytes.extend(copy(b"SKUL\xba\xbe", LUKS2_HEADER_BYTES, json));
        bytes
    }

    #[test]
    fn options_need_an_absolute_emulator_or_nothing() {
        assert_eq!(options(&[]).unwrap(), None);
        assert_eq!(
            options(&["--tpm".into(), "/tmp/swtpm".into()]).unwrap(),
            Some(PathBuf::from("/tmp/swtpm"))
        );
        for args in [
            vec!["--tpm"],
            vec!["--tpm", "swtpm"],
            vec!["--tpm", "/tmp/swtpm", "extra"],
            vec!["system-x86-64"],
        ] {
            let args: Vec<String> = args.into_iter().map(str::to_string).collect();
            assert!(options(&args).is_err());
        }
    }

    #[test]
    fn the_formatted_header_passes_and_each_departure_refuses() {
        verify_luks2(&header(&json()), UUID).unwrap();
        assert!(verify_luks2(&header(&json()), "6a5a5a5a-5a5a-405a-805a-5a5a5a5a5a5a").is_err());
        for (from, to) in [
            (r#""1":{"type":"luks2""#, r#""2":{"type":"luks2""#),
            (
                r#""iterations":1000,"salt":"AA=="}},"1""#,
                r#""iterations":2000,"salt":"AA=="}},"1""#,
            ),
            (r#""keyslots":["1"]"#, r#""keyslots":["0"]"#),
            (r#""keyslots":["1"]"#, r#""keyslots":[]"#),
            (r#""role":"first-boot""#, r#""role":"device-bound""#),
            (r#""offset":"16777216""#, r#""offset":"32768""#),
            (r#""sector_size":4096"#, r#""sector_size":512"#),
        ] {
            let changed = json().replacen(from, to, 1);
            assert_ne!(changed, json(), "{from}");
            assert!(verify_luks2(&header(&changed), UUID).is_err(), "{to}");
        }
        let mut corrupt = header(&json());
        corrupt[4100] ^= 1;
        assert!(verify_luks2(&corrupt, UUID).is_err());
        let mut disagree = header(&json());
        let other = copy(
            b"SKUL\xba\xbe",
            LUKS2_HEADER_BYTES,
            &json().replace("AA==", "AB=="),
        );
        disagree[LUKS2_HEADER_BYTES as usize..].copy_from_slice(&other);
        assert!(verify_luks2(&disagree, UUID).is_err());
    }

    /// `area` at sequence number `seqid`, its checksum made again.
    fn reseq(mut area: Vec<u8>, seqid: u64) -> Vec<u8> {
        area[16..24].copy_from_slice(&seqid.to_be_bytes());
        area[448..512].fill(0);
        let mut hash = crate::sha256::Sha256::new();
        hash.update(&area);
        area[448..480].copy_from_slice(&hash.finalize());
        area
    }

    /// After a cut the newer checksum-valid copy is read and the other
    /// reported; a strict read refuses each of those states.
    #[test]
    fn a_cut_header_is_read_as_cryptsetup_reads_it() {
        let size = LUKS2_HEADER_BYTES as usize;
        let old = json();
        let new = json().replace(r#""1":{"type":"luks2""#, r#""2":{"type":"luks2""#);
        let pair = |primary: Vec<u8>, secondary: Vec<u8>| {
            let mut bytes = primary;
            bytes.extend(secondary);
            bytes
        };
        let primary = |json: &str, seq| reseq(copy(b"LUKS\xba\xbe", 0, json), seq);
        let secondary =
            |json: &str, seq| reseq(copy(b"SKUL\xba\xbe", LUKS2_HEADER_BYTES, json), seq);
        let keyslots = |bytes: &[u8]| {
            parse_luks2_after_cut(bytes, UUID).map(|(s, n)| (s.keyslots, n.is_some()))
        };
        // Whole and agreeing.
        let whole = pair(primary(&old, 4), secondary(&old, 4));
        assert_eq!(keyslots(&whole).unwrap(), (vec![0, 1], false));
        parse_luks2(&whole, UUID).unwrap();
        // The secondary written first, the primary not yet.
        let behind = pair(primary(&old, 4), secondary(&new, 5));
        assert_eq!(keyslots(&behind).unwrap(), (vec![0, 2], true));
        assert!(parse_luks2(&behind, UUID).is_err());
        // The primary torn mid-write.
        let mut torn = pair(primary(&new, 5), secondary(&new, 5));
        torn[4100] ^= 1;
        assert_eq!(keyslots(&torn).unwrap(), (vec![0, 2], true));
        assert!(parse_luks2(&torn, UUID).is_err());
        // Both torn, or disagreeing at one sequence number.
        let mut both = torn.clone();
        both[size + 4100] ^= 1;
        assert!(keyslots(&both).is_err());
        assert!(keyslots(&pair(primary(&old, 5), secondary(&new, 5))).is_err());
    }

    #[test]
    fn the_console_must_not_carry_a_key_shape() {
        require_no_key_text("TD-INSTALL-SERVED 5a5a 123456-789012 0123456789").unwrap();
        let digits = "1".repeat(48);
        assert!(require_no_key_text(&format!("x {digits} y")).is_err());
        require_no_key_text(&"1".repeat(47)).unwrap();
        let display = ["123456"; 8].join("-");
        assert!(require_no_key_text(&format!("key: {display}.")).is_err());
        require_no_key_text(&["123456"; 7].join("-")).unwrap();
    }

    #[test]
    fn the_scan_finds_a_needle_across_chunks() {
        let dir = env::temp_dir().join(format!("td-encrypted-scan-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("image");
        let mut bytes = vec![0x5a; (8 << 20) + 4096];
        fs::write(&path, &bytes).unwrap();
        let file = File::open(&path).unwrap();
        scan(&file, 0, bytes.len() as u64, &[b"_BHRfS_M"]).unwrap();
        bytes[(8 << 20) - 3..(8 << 20) + 5].copy_from_slice(b"_BHRfS_M");
        fs::write(&path, &bytes).unwrap();
        let file = File::open(&path).unwrap();
        assert!(scan(&file, 0, bytes.len() as u64, &[b"_BHRfS_M"]).is_err());
        // Only partly inside the scanned range, it is not seen.
        scan(&file, 0, 8 << 20, &[b"_BHRfS_M"]).unwrap();
        scan(&file, (8 << 20) + 5, 4091, &[b"_BHRfS_M"]).unwrap();
        fs::remove_dir_all(&dir).unwrap();
    }

    fn boot(target: bool, console: &str) -> BootResult {
        BootResult {
            evidence: ConsoleEvidence {
                target,
                ..ConsoleEvidence::default()
            },
            exited_clean: false,
            marker_killed: true,
            reason: "fixture".into(),
            console: console.into(),
            elapsed: Duration::ZERO,
            firefox_audio: FirefoxAudioCapture::NotRequested,
        }
    }

    #[test]
    fn a_record_counts_only_when_the_end_line_follows_it() {
        let record = protocol::ENCRYPTED_MARKER;
        let end = protocol::ENCRYPTED_END_MARKER;
        let whole = format!("{record} uuid /dev/vda\n{end}\n");
        assert_eq!(
            reached(&boot(true, &whole), record, "leg"),
            Ok("uuid /dev/vda")
        );
        for console in [
            // A record cut short by the console read, the end never seen.
            format!("{record} uu"),
            format!("{record} uuid /dev/vda\n"),
            format!("{end}\n{record} uuid /dev/vda\n"),
            format!("{record} uuid /dev/vda\n{end}\n{end}\n"),
            format!("{record} uuid /dev/vda\n{record} uuid /dev/vda\n{end}\n"),
            format!("{record}\n{end}\n"),
        ] {
            assert!(
                reached(&boot(true, &console), record, "leg").is_err(),
                "{console:?}"
            );
        }
        assert!(reached(&boot(false, &whole), record, "leg").is_err());
        let refused = format!("{}\n", protocol::NO_TPM_MARKER);
        require_end(&boot(true, &refused), protocol::NO_TPM_MARKER, "leg").unwrap();
        assert!(require_end(&boot(false, &refused), protocol::NO_TPM_MARKER, "leg").is_err());
    }

    #[test]
    fn the_sampled_invocations_include_both_secret_carriers() {
        sampled_invocations("15 8 close,luksAddKey,luksFormat,open,status").unwrap();
        sampled_invocations("2 2 luksAddKey,luksFormat").unwrap();
        for bad in [
            "15 8 close,luksFormat,open",
            "15 8 luksAddKey,open",
            "15 8",
            "1 2 luksAddKey,luksFormat",
            "15 0 luksAddKey,luksFormat",
            "15 8 luksAddKey,luksFormat extra",
            "x 8 luksAddKey,luksFormat",
        ] {
            assert!(sampled_invocations(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn key_shaped_text_is_redacted_before_it_is_shown_or_saved() {
        let digits = "123456".repeat(8);
        let display = ["123456"; 8].join("-");
        let console = format!("a {digits} b\nkey: {display}.\nsize 5904510976 -- 2026-10-05\n");
        let shown = redacted(&console);
        assert!(!shown.contains(&digits) && !shown.contains(&display));
        require_no_key_text(&shown).unwrap();
        assert!(shown.ends_with("size 5904510976 -- 2026-10-05\n"));
        assert!(require_no_key_text(&console).is_err());
    }

    fn image(name: &str, len: u64) -> (PathBuf, PathBuf) {
        let dir = env::temp_dir().join(format!("td-encrypted-{name}-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("image");
        File::create(&path).unwrap().set_len(len).unwrap();
        (dir, path)
    }

    #[test]
    fn a_seeded_table_is_a_valid_gpt_in_the_table_ranges_only() {
        use std::os::unix::fs::FileExt;
        let len = 1u64 << 30;
        let (dir, path) = image("seed", len);
        seed_table(&path, len).unwrap();
        let file = File::open(&path).unwrap();
        assert_eq!(read_table(&file, len, 512).unwrap().partitions.len(), 1);
        assert!(verify_no_table(&file, len, 512).is_err());
        let primary = 2 * 512 + GPT_ENTRY_BYTES;
        let backup = len - 512 - GPT_ENTRY_BYTES;
        let mut middle = vec![0; usize::try_from(backup - primary).unwrap()];
        file.read_exact_at(&mut middle, primary).unwrap();
        assert!(middle.iter().all(|byte| *byte == 0));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_installed_table_is_read_through_the_gpt_reader() {
        use std::os::unix::fs::FileExt;
        let len = 1u64 << 30;
        let (dir, path) = image("table", len);
        let sector = 512;
        let align = td_boot_protocol::PARTITION_ALIGN_BYTES / sector;
        let esp_end = align + td_boot_protocol::ESP_BYTES / sector - 1;
        let last = td_engine::gpt::last_usable_lba(sector, len / sector).unwrap();
        let volume_end = (last + 1) / 8 * 8 - 1;
        let partition =
            |type_guid, unique: u8, start_lba, end_lba, name: &str| td_engine::gpt::Partition {
                type_guid,
                unique_guid: td_engine::gpt::Guid([unique; 16]),
                start_lba,
                end_lba,
                attributes: 0,
                name: name.into(),
            };
        let table = td_engine::gpt::build(&td_engine::gpt::Layout {
            sector_size: sector,
            disk_sectors: len / sector,
            disk_guid: td_engine::gpt::Guid([1; 16]),
            align_sectors: align,
            partitions: vec![
                partition(
                    td_engine::gpt::TYPE_ESP,
                    2,
                    align,
                    esp_end,
                    td_boot_protocol::ESP_PARTITION_NAME,
                ),
                partition(
                    td_engine::gpt::TYPE_LINUX_FS,
                    3,
                    esp_end + 1,
                    volume_end,
                    td_boot_protocol::VOLUME_PARTITION_NAME,
                ),
            ],
        })
        .unwrap();
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        file.write_all_at(&table.primary, table.primary_offset)
            .unwrap();
        file.write_all_at(&table.backup, table.backup_offset)
            .unwrap();
        let volume = (esp_end + 1) * sector;
        // No FAT boot sector in the ESP yet.
        assert!(verify_table(&file, len, sector, volume).is_err());
        file.write_all_at(&[0x55, 0xaa], td_boot_protocol::PARTITION_ALIGN_BYTES + 510)
            .unwrap();
        let end = verify_table(&file, len, sector, volume).unwrap();
        assert_eq!(end, (volume_end + 1) * sector);
        assert!(whole_sectors(volume, end).is_ok());
        assert!(verify_table(&file, len, sector, volume + 4096).is_err());
        // A flipped entry byte breaks the primary array's CRC.
        let at = 2 * sector + 130;
        let mut byte = [0];
        file.read_exact_at(&mut byte, at).unwrap();
        file.write_all_at(&[byte[0] ^ 1], at).unwrap();
        assert!(verify_table(&file, len, sector, volume).is_err());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_volume_partition_must_be_whole_encryption_sectors() {
        let start = 34 << 20;
        assert_eq!(whole_sectors(start, start + (6 << 30)), Ok(6 << 30));
        // The red the layout fix closed: a 512-byte disk's last usable LBA.
        assert!(whole_sectors(start, start + (6 << 30) + 3584).is_err());
        assert!(whole_sectors(start + 512, start + 4096 + 512).is_err());
        assert!(whole_sectors(start, start - 4096).is_err());
    }

    #[test]
    fn a_table_less_image_has_both_ranges_zero() {
        let len = 1u64 << 24;
        let (dir, path) = image("zero", len);
        verify_no_table(&File::open(&path).unwrap(), len, 512).unwrap();
        for offset in [0, 34 * 512 - 1, len - 33 * 512, len - 1] {
            use std::os::unix::fs::FileExt;
            let file = OpenOptions::new().write(true).open(&path).unwrap();
            file.write_all_at(&[1], offset).unwrap();
            assert!(verify_no_table(&File::open(&path).unwrap(), len, 512).is_err());
            file.write_all_at(&[0], offset).unwrap();
        }
        fs::remove_dir_all(&dir).unwrap();
    }
}

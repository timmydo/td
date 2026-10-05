//! The guest legs of `qemu-install-encrypted` (td-install/ENCRYPTION.md
//! increment 5): the service started with its device-bound storage operand,
//! driven as both of its peers through the recovery-key phase, and the
//! installed volume then opened with the key typed back. The key's digits
//! stay in this process and the service: no record, argv or environment
//! carries them, and every copy here is zeroed when dropped.

use super::*;
use installation_protocol::{
    Phase, RecoveryDigits, Refusal, Reply, Request, ReviewNonce, State, RECOVERY_DIGITS,
};
use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// The verified root's static cryptsetup: the binary the service runs.
const CRYPTSETUP: &str = "/root-image/bin/cryptsetup";
/// This fixture's own mapping of the installed volume.
const MAPPING: &str = "td-oracle";
/// cryptsetup's exit when no keyslot accepts the passphrase (`-EPERM`).
const BAD_PASSPHRASE: i32 = 2;
/// cryptsetup 2.8.8's exit for `status` of an inactive mapping.
const INACTIVE: i32 = 4;
/// Digits per group of the key's display form (td-protector "Recovery key").
const GROUP_DIGITS: usize = 6;
/// The display form: eight groups of six digits joined by hyphens.
const DISPLAY_BYTES: usize = RECOVERY_DIGITS + RECOVERY_DIGITS / GROUP_DIGITS - 1;
/// A GPT entry array of 128 entries of 128 bytes. The primary table is the
/// protective MBR, the header and the array; the backup, the array and the
/// header that end the disk.
const GPT_ENTRY_BYTES: u64 = 128 * 128;
/// The bounds of the command-line sampler: distinct lines kept, and the
/// bytes read of one line or environment.
const MAX_SAMPLES: usize = 4096;
const MAX_SAMPLE_BYTES: u64 = 4096;

/// The device-bound installation onto `device`. With `cut`, the leg stops
/// in the recovery-key phase, the key sent and not typed back, for the
/// host's power cut.
pub(super) fn install(device: &str, cut: bool) -> Result<(), String> {
    let media = mount_source(device)?;
    let (deployment, id) = source_deployment()?;
    mount_system_root()?;
    let loops = bound_loops()?;
    bind_root_store()?;
    record_deployment(&id)?;
    // The host seeded a GPT, so the service is what clears it.
    require_table(device, true)?;
    let sampler = Sampler::start()?;
    let mut service = Service::start(installation_plan::Storage::DeviceBound, Stdio::inherit())?;
    let driven = drive(&mut service, device, media, deployment, cut);
    let (uuid, key) = service.finish(driven)?;
    let samples = sampler.finish()?;
    let needles = Needles::new(&key);
    let invocations = require_key_unsampled(&samples, &needles, &uuid)?;
    report(
        std::io::stdout(),
        format_args!(
            "{ENCRYPTED_CMDLINES_MARKER} {} {} {invocations}",
            samples.cmdlines.len(),
            samples.cryptsetup.len()
        ),
    )?;
    if bound_loops()? != loops {
        return Err("the installation left a loop device bound".into());
    }
    report(
        std::io::stdout(),
        format_args!("{SERVED_MARKER} {uuid} {device}"),
    )?;
    drop_page_cache()?;
    let scanned = require_key_absent(device, &needles)?;
    report(
        std::io::stdout(),
        format_args!("{ENCRYPTED_KEY_ABSENT_MARKER} {scanned}"),
    )?;
    let (volume, mapped) = open_installed(device, &key, &needles, &id)?;
    drop(needles);
    drop(key);
    report(
        std::io::stdout(),
        format_args!("{ENCRYPTED_VOLUME_MARKER} {uuid} {volume} {id} {mapped}"),
    )?;
    command("/bin/umount", &["/td/store"])?;
    applet(&["sync"])?;
    report(
        std::io::stdout(),
        format_args!("{ENCRYPTED_MARKER} {uuid} {device}"),
    )?;
    report(std::io::stdout(), format_args!("{ENCRYPTED_END_MARKER}"))
}

/// Writes back and then drops the clean page cache, so the scan that
/// follows reads `/dev/vda`'s blocks from the disk rather than from pages
/// a writer left: `drop_caches` drops only clean pages, hence the sync.
fn drop_page_cache() -> Result<(), String> {
    applet(&["sync"])?;
    fs::write("/proc/sys/vm/drop_caches", b"1\n")
        .map_err(|error| format!("drop the page cache: {error}"))
}

/// One device-bound installation through the service's protocols: the
/// recovery key fetched once, a second ask refused, a mistyped type-back
/// refused with the phase continuing, the key typed back, and the finished
/// report. Returns the plan's UUID and the key.
fn drive(
    service: &mut Service,
    device: &str,
    media: &str,
    deployment: [u8; 32],
    cut: bool,
) -> Result<(String, RecoveryDigits), String> {
    let plan = start_installation(
        service,
        device,
        media,
        deployment,
        installation_plan::Storage::DeviceBound,
    )?;
    let nonce = *plan.nonce();
    let review = ReviewNonce::new(nonce)?;
    let uuid = plan_uuid(&plan)?;
    let mut phases = Vec::new();
    let installer = &mut service.installer;
    poll_installation(installer, nonce, &mut phases, Some(Phase::RecoveryKey))?;
    require_phase_order(&phases, installation_plan::Storage::DeviceBound)?;
    // Written and verified, the service holding the key: nothing yet makes
    // the disk bootable.
    require_table(device, false)?;
    report(
        std::io::stdout(),
        format_args!("{ENCRYPTED_NO_TABLE_MARKER} {uuid} {device}"),
    )?;
    let key = match exchange(installer, &Request::RecoveryKey(review))? {
        Reply::RecoveryKey(sent, digits) if *sent.as_bytes() == nonce => digits,
        other => return Err(format!("the recovery key was not sent: {other:?}")),
    };
    match exchange(installer, &Request::RecoveryKey(review))? {
        Reply::Refused(Refusal::RecoveryKeySent) => {}
        other => return Err(format!("a second ask was not refused as sent: {other:?}")),
    }
    if cut {
        report(
            std::io::stdout(),
            format_args!("{ENCRYPTED_CUT_MARKER} {uuid} {device}"),
        )?;
        report(std::io::stdout(), format_args!("{ENCRYPTED_END_MARKER}"))?;
        // The host cuts power here: both channels open, the key unconfirmed.
        loop {
            std::thread::sleep(Duration::from_secs(1));
        }
    }
    match exchange(
        installer,
        &Request::ConfirmRecovery(review, mistyped(&key)?),
    )? {
        Reply::Refused(Refusal::RecoveryKeyMismatch) => {}
        other => {
            return Err(format!(
                "a mistyped key was not refused as a mismatch: {other:?}"
            ))
        }
    }
    match exchange(installer, &Request::Status)? {
        Reply::Status(State::Running(running, Phase::RecoveryKey))
            if *running.as_bytes() == nonce => {}
        other => return Err(format!("a mistyped key ended the phase: {other:?}")),
    }
    match exchange(installer, &Request::ConfirmRecovery(review, key.clone()))? {
        Reply::Status(State::Running(running, Phase::RecoveryKey))
            if *running.as_bytes() == nonce => {}
        other => return Err(format!("the key typed back was not confirmed: {other:?}")),
    }
    report(
        std::io::stdout(),
        format_args!("{RECOVERY_TYPED_BACK_MARKER}"),
    )?;
    poll_installation(installer, nonce, &mut phases, None)?;
    require_phase_order(&phases, installation_plan::Storage::DeviceBound)?;
    require_finished(&mut service.authority, nonce)?;
    Ok((uuid, key))
}

/// The key with its last digit changed: well formed, and not the key.
fn mistyped(key: &RecoveryDigits) -> Result<RecoveryDigits, String> {
    let mut digits = *key.as_bytes();
    let wrong = match digits.last_mut() {
        Some(last) => {
            *last = b'0' + last.wrapping_sub(b'0').wrapping_add(1) % 10;
            RecoveryDigits::new(&digits)
        }
        None => Err("an empty recovery key".into()),
    };
    installation_protocol::scrub(&mut digits);
    wrong
}

/// With `present`, both of `device`'s GPT ranges hold bytes (the host's
/// seeded table); without, neither holds a byte. Read without a claim: the
/// service holds the disk's.
fn require_table(device: &str, present: bool) -> Result<(), String> {
    let name = device.strip_prefix("/dev/").ok_or("invalid target path")?;
    let sector = required_attribute(
        &Path::new("/sys/class/block")
            .join(name)
            .join("queue/logical_block_size"),
    )?
    .parse::<u64>()
    .map_err(|_| "invalid target sector size")?;
    let mut file = File::open(device).map_err(|error| format!("open {device}: {error}"))?;
    let len = file
        .seek(SeekFrom::End(0))
        .map_err(|error| format!("size {device}: {error}"))?;
    for (copy, offset, bytes) in table_ranges(len, sector)? {
        let mut range = vec![0; usize::try_from(bytes).map_err(|_| "GPT range overflow")?];
        file.seek(SeekFrom::Start(offset))
            .and_then(|_| file.read_exact(&mut range))
            .map_err(|error| format!("read the {copy} table range of {device}: {error}"))?;
        let written = range.iter().any(|byte| *byte != 0);
        if written != present {
            return Err(if present {
                format!("the {copy} table range of {device} holds no seeded table")
            } else {
                format!(
                    "the {copy} table range of {device} is written before the recovery key \
                     is confirmed"
                )
            });
        }
    }
    Ok(())
}

/// A disk's two GPT ranges at `sector` bytes per sector: the primary's
/// protective MBR, header and entry array, and the backup's array and
/// header ending the disk.
fn table_ranges(len: u64, sector: u64) -> Result<[(&'static str, u64, u64); 2], String> {
    let primary = sector
        .checked_mul(2)
        .and_then(|bytes| bytes.checked_add(GPT_ENTRY_BYTES))
        .ok_or("GPT range overflow")?;
    let backup = sector
        .checked_add(GPT_ENTRY_BYTES)
        .ok_or("GPT range overflow")?;
    let start = len
        .checked_sub(backup)
        .filter(|start| *start >= primary)
        .ok_or("the disk is smaller than its two GPT ranges")?;
    Ok([("primary", 0, primary), ("backup", start, backup)])
}

/// The recovery key as a disk or a command line could carry it: its
/// passphrase digits and its hyphen-grouped display form, zeroed on drop.
struct Needles {
    passphrase: Box<[u8; RECOVERY_DIGITS]>,
    display: Box<[u8; DISPLAY_BYTES]>,
}

impl Needles {
    fn new(key: &RecoveryDigits) -> Self {
        let mut passphrase = Box::new([0; RECOVERY_DIGITS]);
        passphrase.copy_from_slice(key.as_bytes());
        let mut display = Box::new([b'-'; DISPLAY_BYTES]);
        for (index, digit) in key.as_bytes().iter().enumerate() {
            if let Some(slot) = display.get_mut(index + index / GROUP_DIGITS) {
                *slot = *digit;
            }
        }
        Self {
            passphrase,
            display,
        }
    }

    fn found_in(&self, haystack: &[u8]) -> bool {
        contains(haystack, &self.passphrase[..]) || contains(haystack, &self.display[..])
    }
}

impl Drop for Needles {
    fn drop(&mut self) {
        installation_protocol::scrub(&mut self.passphrase[..]);
        installation_protocol::scrub(&mut self.display[..]);
    }
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    let Some(&first) = needle.first() else {
        return true;
    };
    let mut at = 0;
    while let Some(found) = haystack
        .get(at..)
        .and_then(|rest| rest.iter().position(|byte| *byte == first))
    {
        let start = at.saturating_add(found);
        if haystack.get(start..start.saturating_add(needle.len())) == Some(needle) {
            return true;
        }
        at = start.saturating_add(1);
    }
    false
}

/// Every byte of `device` (the whole disk: the ESP, the LUKS2 header and
/// the ciphertext alike; or the opened mapping's plaintext), read in order with an overlap no form of the key fits in; returns
/// the bytes read.
fn require_key_absent(device: &str, needles: &Needles) -> Result<u64, String> {
    const CHUNK: usize = 4 << 20;
    let keep = DISPLAY_BYTES - 1;
    let mut file = File::open(device).map_err(|error| format!("open {device}: {error}"))?;
    let mut buffer = vec![0; keep + CHUNK];
    let mut carried = 0;
    let mut total = 0u64;
    loop {
        let free = buffer.get_mut(carried..).ok_or("scan buffer overflow")?;
        let read = file
            .read(free)
            .map_err(|error| format!("read {device}: {error}"))?;
        if read == 0 {
            return Ok(total);
        }
        total = total.saturating_add(read as u64);
        let filled = carried + read;
        let window = buffer.get(..filled).ok_or("scan buffer overflow")?;
        if needles.found_in(window) {
            return Err(format!("the recovery key is on {device} near byte {total}"));
        }
        let start = filled.saturating_sub(keep);
        buffer.copy_within(start..filled, 0);
        carried = filled - start;
    }
}

/// The verified root's cryptsetup with `input` on standard input and an
/// empty environment, its output on the console; its exit code.
fn cryptsetup(args: &[&str], input: &[u8]) -> Result<Option<i32>, String> {
    let mut child = Command::new(CRYPTSETUP)
        .args(args)
        .env_clear()
        .current_dir("/")
        .stdin(Stdio::piped())
        .spawn()
        .map_err(|error| format!("start cryptsetup {}: {error}", args.join(" ")))?;
    // Dropped at the end of the statement, which closes the child's input.
    let fed = child
        .stdin
        .take()
        .ok_or("cryptsetup stdin is unavailable")
        .and_then(|mut stdin| stdin.write_all(input).map_err(|_| "feed cryptsetup"));
    let status = child
        .wait()
        .map_err(|error| format!("wait for cryptsetup: {error}"))?;
    fed?;
    Ok(status.code())
}

/// The installed volume, found among the disk's partitions in the kernel's
/// block inventory: keyslot 0 opens with the key typed back and no keyslot
/// with a mistyped one; opened, it holds the published deployment and the
/// settings, and no byte of it holds the key. The mapping is closed, as
/// device-mapper answers, whatever happened. Returns the volume partition
/// and the mapping's bytes read.
fn open_installed(
    device: &str,
    key: &RecoveryDigits,
    needles: &Needles,
    id: &str,
) -> Result<(String, u64), String> {
    applet(&["reread-partitions", device])?;
    let (_, volume) = partitions(device)?;
    let test = |slot: Option<&'static str>, input: &[u8]| {
        let mut args = vec!["open", "--test-passphrase", "--type", "luks2"];
        if let Some(slot) = slot {
            args.extend(["--key-slot", slot]);
        }
        args.extend(["--key-file=-", &volume]);
        cryptsetup(&args, input)
    };
    match test(Some("0"), key.as_bytes())? {
        Some(0) => {}
        other => {
            return Err(format!(
                "the recovery key does not open keyslot 0: {other:?}"
            ))
        }
    }
    let wrong = mistyped(key)?;
    match test(None, wrong.as_bytes())? {
        Some(BAD_PASSPHRASE) => {}
        other => return Err(format!("a mistyped key was not refused: {other:?}")),
    }
    drop(wrong);
    match cryptsetup(
        &["open", "--type", "luks2", "--key-file=-", &volume, MAPPING],
        key.as_bytes(),
    )? {
        Some(0) => {}
        other => {
            return Err(format!(
                "the recovery key does not open {volume}: {other:?}"
            ))
        }
    }
    let mapped = format!("/dev/mapper/{MAPPING}");
    let inspected = require_key_absent(&mapped, needles)
        .and_then(|bytes| inspect_volume(&mapped, id).map(|()| bytes));
    let closed = cryptsetup(&["close", MAPPING], &[])
        .and_then(|_| cryptsetup(&["status", MAPPING], &[]))
        .and_then(|code| match code {
            Some(INACTIVE) => Ok(()),
            other => Err(format!(
                "the mapping {MAPPING} survives its close: {other:?}"
            )),
        });
    let mapped = inspected?;
    closed?;
    Ok((volume, mapped))
}

/// The destination's partitions as the kernel lists them under the disk:
/// exactly 1 and 2, as `/dev` paths.
fn partitions(device: &str) -> Result<(String, String), String> {
    let name = device.strip_prefix("/dev/").ok_or("invalid target path")?;
    let disk = Path::new("/sys/class/block").join(name);
    let mut found = Vec::new();
    for entry in fs::read_dir(&disk).map_err(|error| format!("list {}: {error}", disk.display()))? {
        let entry = entry.map_err(|error| format!("read {}: {error}", disk.display()))?;
        // A partition is a directory named after its disk; the disk's own
        // attributes are files beside it.
        let child = entry.file_name();
        let directory = entry
            .file_type()
            .map_err(|error| format!("inspect {}: {error}", entry.path().display()))?
            .is_dir();
        if !directory || !child.as_bytes().starts_with(name.as_bytes()) {
            continue;
        }
        let Some(number) = attribute(&entry.path().join("partition"), true)? else {
            continue;
        };
        let number = number
            .parse::<u32>()
            .map_err(|_| format!("invalid partition number under {name}"))?;
        let child = entry
            .file_name()
            .into_string()
            .map_err(|_| format!("non-UTF-8 partition under {name}"))?;
        found.push((number, child));
    }
    found.sort();
    match found.as_slice() {
        [(1, esp), (2, volume)] => Ok((format!("/dev/{esp}"), format!("/dev/{volume}"))),
        _ => Err(format!(
            "{device} does not hold exactly partitions 1 and 2: {found:?}"
        )),
    }
}

/// Inside the opened volume: td-boot verifies the current deployment as
/// the one validated, and `@var` holds the reviewed settings.
fn inspect_volume(mapped: &str, id: &str) -> Result<(), String> {
    command("/bin/td-boot", &["mount-root", mapped, "/volume"])?;
    let verified = Command::new("/bin/td-boot")
        .args(["verify", "/volume"])
        .output()
        .map_err(|error| format!("run td-boot verify: {error}"));
    let unmounted = applet(&["umount", "/volume"]);
    let verified = verified?;
    unmounted?;
    let line = String::from_utf8_lossy(&verified.stdout);
    if !verified.status.success() || !line.starts_with(&format!("current {id} ")) {
        return Err(format!(
            "the installed volume does not verify the deployment {id}: {} {line:?} {}",
            verified.status,
            String::from_utf8_lossy(&verified.stderr).trim_end()
        ));
    }
    command("/bin/td-boot", &["mount-var", mapped, "/state"])?;
    let settings = check_timezone(Path::new("/state"))
        .and_then(|()| check_hostname(Path::new("/state")))
        .and_then(|()| check_username(Path::new("/state")));
    let unmounted = applet(&["umount", "/state"]);
    settings?;
    unmounted
}

/// What the sampler saw of `/proc` while the service ran.
#[derive(Clone, Default)]
struct Samples {
    cmdlines: BTreeSet<Vec<u8>>,
    /// The distinct command lines whose program is named cryptsetup.
    cryptsetup: BTreeSet<Vec<u8>>,
    /// cryptsetup processes seen with a nonempty environment.
    environments: usize,
}

/// Samples every process's command line, and each cryptsetup's
/// environment, every two milliseconds on its own thread until finished.
struct Sampler {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<Result<Samples, String>>>,
}

impl Sampler {
    fn start() -> Result<Self, String> {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        let thread = std::thread::Builder::new()
            .name("cmdline-sampler".into())
            .spawn(move || sample(&flag))
            .map_err(|error| format!("start the command-line sampler: {error}"))?;
        Ok(Self {
            stop,
            thread: Some(thread),
        })
    }

    fn finish(mut self) -> Result<Samples, String> {
        self.stop.store(true, Ordering::Relaxed);
        self.thread
            .take()
            .ok_or("the command-line sampler is gone")?
            .join()
            .map_err(|_| "the command-line sampler failed")?
    }
}

impl Drop for Sampler {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// A `/proc` file of at most `MAX_SAMPLE_BYTES`; `None` when the process
/// is gone. A longer file refuses, so no line escapes the check unread.
fn sampled(path: &Path) -> Result<Option<Vec<u8>>, String> {
    let Ok(file) = File::open(path) else {
        return Ok(None);
    };
    let mut bytes = Vec::new();
    if file
        .take(MAX_SAMPLE_BYTES + 1)
        .read_to_end(&mut bytes)
        .is_err()
    {
        return Ok(None);
    }
    if bytes.len() as u64 > MAX_SAMPLE_BYTES {
        return Err(format!("{} exceeds the sampler's bound", path.display()));
    }
    Ok(Some(bytes))
}

fn sample(stop: &AtomicBool) -> Result<Samples, String> {
    let mut samples = Samples::default();
    while !stop.load(Ordering::Relaxed) {
        for entry in fs::read_dir("/proc").map_err(|error| format!("list /proc: {error}"))? {
            let Ok(entry) = entry else {
                continue;
            };
            if !entry.file_name().as_bytes().iter().all(u8::is_ascii_digit) {
                continue;
            }
            let path = entry.path();
            // Empty for a kernel thread or a process exiting.
            let Some(cmdline) = sampled(&path.join("cmdline"))?.filter(|line| !line.is_empty())
            else {
                continue;
            };
            let program = cmdline.split(|byte| *byte == 0).next().unwrap_or_default();
            if Path::new(OsStr::from_bytes(program)).file_name() == Some(OsStr::new("cryptsetup")) {
                if sampled(&path.join("environ"))?.is_some_and(|environ| !environ.is_empty()) {
                    samples.environments += 1;
                }
                samples.cryptsetup.insert(cmdline.clone());
            }
            if samples.cmdlines.len() >= MAX_SAMPLES && !samples.cmdlines.contains(&cmdline) {
                return Err("the sampler saw too many distinct command lines".into());
            }
            samples.cmdlines.insert(cmdline);
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    Ok(samples)
}

/// No sampled command line holds the key, cryptsetup was seen running,
/// each time as exactly one of td-install's argument lists and always
/// with an empty environment. Returns the invocations seen, comma-joined.
fn require_key_unsampled(
    samples: &Samples,
    needles: &Needles,
    uuid: &str,
) -> Result<String, String> {
    if samples.cmdlines.iter().any(|line| needles.found_in(line)) {
        return Err("a command line sampled during the installation holds the recovery key".into());
    }
    if samples.cryptsetup.is_empty() {
        return Err("no cryptsetup command line was sampled while the service ran".into());
    }
    let mut seen = BTreeSet::new();
    for line in &samples.cryptsetup {
        // The program, then its arguments.
        let words: Vec<&[u8]> = line
            .strip_suffix(&[0])
            .unwrap_or(line)
            .split(|byte| *byte == 0)
            .skip(1)
            .collect();
        let Some(name) = invocation(&words, uuid) else {
            return Err(format!(
                "a sampled cryptsetup command line is none of td-install's: {} words",
                words.len()
            ));
        };
        seen.insert(name);
    }
    if samples.environments != 0 {
        return Err(format!(
            "{} sampled cryptsetup processes had an environment",
            samples.environments
        ));
    }
    Ok(seen.into_iter().collect::<Vec<_>>().join(","))
}

/// One word of a documented cryptsetup argument list.
#[derive(Clone, Copy)]
enum Word {
    /// Exactly these bytes.
    Is(&'static str),
    /// The loop over the volume: `/dev/loopN`.
    Loop,
    /// The service's mapping: `td-install-` and 16 lowercase hex digits.
    Mapping,
    /// The new key's descriptor: `/proc/PID/fd/N`.
    Descriptor,
    /// The plan's volume UUID.
    Uuid,
    /// Keyslot 0 or 1.
    Slot,
}

use Word::{Descriptor, Is, Loop, Mapping, Slot, Uuid};

/// td-install's cryptsetup argument lists after the program (td-install
/// `device_bound`: `format_args`, `add_key_args`, `token_import_args`,
/// `open_args`, `test_args`, `close_args`, `status_args`), by name. A
/// secret in any encoding matches none of their words.
const INVOCATIONS: &[(&str, &[Word])] = &[
    (
        "luksFormat",
        &[
            Is("luksFormat"),
            Is("--batch-mode"),
            Is("--type"),
            Is("luks2"),
            Is("--cipher"),
            Is("aes-xts-plain64"),
            Is("--key-size"),
            Is("512"),
            Is("--sector-size"),
            Is("4096"),
            Is("--hash"),
            Is("sha256"),
            Is("--pbkdf"),
            Is("pbkdf2"),
            Is("--pbkdf-force-iterations"),
            Is("1000"),
            Is("--use-random"),
            Is("--luks2-metadata-size"),
            Is("16384"),
            Is("--luks2-keyslots-size"),
            Is("16744448"),
            Is("--offset"),
            Is("32768"),
            Is("--uuid"),
            Uuid,
            Is("--label"),
            Is("td-system"),
            Is("--key-slot"),
            Is("0"),
            Is("--key-file=-"),
            Loop,
        ],
    ),
    (
        "luksAddKey",
        &[
            Is("luksAddKey"),
            Is("--batch-mode"),
            Is("--pbkdf"),
            Is("pbkdf2"),
            Is("--pbkdf-force-iterations"),
            Is("1000"),
            Is("--hash"),
            Is("sha256"),
            Is("--key-slot"),
            Is("0"),
            Is("--new-key-slot"),
            Is("1"),
            Is("--key-file=-"),
            Loop,
            Descriptor,
        ],
    ),
    (
        "token-import",
        &[
            Is("token"),
            Is("import"),
            Is("--token-id"),
            Is("0"),
            Is("--json-file=-"),
            Loop,
        ],
    ),
    (
        "open",
        &[
            Is("open"),
            Is("--type"),
            Is("luks2"),
            Is("--key-file=-"),
            Loop,
            Mapping,
        ],
    ),
    (
        "test-passphrase",
        &[
            Is("open"),
            Is("--test-passphrase"),
            Is("--type"),
            Is("luks2"),
            Is("--key-slot"),
            Slot,
            Is("--key-file=-"),
            Loop,
        ],
    ),
    ("close", &[Is("close"), Mapping]),
    ("status", &[Is("status"), Mapping]),
];

/// The name of the documented argument list `words` is exactly, if any.
fn invocation(words: &[&[u8]], uuid: &str) -> Option<&'static str> {
    INVOCATIONS.iter().find_map(|(name, list)| {
        (list.len() == words.len()
            && list
                .iter()
                .zip(words)
                .all(|(expected, word)| matches(*expected, word, uuid)))
        .then_some(*name)
    })
}

fn matches(expected: Word, word: &[u8], uuid: &str) -> bool {
    fn number(digits: &[u8]) -> bool {
        (1..=7).contains(&digits.len()) && digits.iter().all(u8::is_ascii_digit)
    }
    match expected {
        Is(text) => word == text.as_bytes(),
        Loop => word.strip_prefix(b"/dev/loop").is_some_and(number),
        Mapping => word.strip_prefix(b"td-install-").is_some_and(|tag| {
            tag.len() == 16
                && tag
                    .iter()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(byte))
        }),
        Descriptor => word.strip_prefix(b"/proc/").is_some_and(|rest| {
            rest.iter()
                .position(|byte| *byte == b'/')
                .is_some_and(|slash| {
                    let (pid, fd) = rest.split_at(slash);
                    number(pid) && fd.strip_prefix(b"/fd/").is_some_and(number)
                })
        }),
        Uuid => word == uuid.as_bytes(),
        Slot => word == b"0" || word == b"1",
    }
}

/// Without a TPM, `serve --storage device-bound` refuses to start: it exits
/// unsuccessfully naming the missing TPM, sends neither channel a byte and
/// leaves the disk as it was.
pub(super) fn refuse_without_tpm(device: &str) -> Result<(), String> {
    for node in ["/dev/tpm0", "/dev/tpmrm0"] {
        match fs::symlink_metadata(node) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Ok(_) => return Err(format!("the no-TPM leg has {node}")),
            Err(error) => return Err(format!("inspect {node}: {error}")),
        }
    }
    let before = canaries(device)?;
    mount_source(device)?;
    let (_, id) = source_deployment()?;
    mount_system_root()?;
    bind_root_store()?;
    record_deployment(&id)?;
    let Service {
        mut child,
        mut installer,
        mut authority,
    } = Service::start(installation_plan::Storage::DeviceBound, Stdio::piped())?;
    let mut diagnostic = Vec::new();
    // The host's deadline bounds a service that does not exit.
    let read = child
        .stderr
        .take()
        .ok_or("the service's stderr is not piped")
        .and_then(|stderr| {
            stderr
                .take(64 * 1024)
                .read_to_end(&mut diagnostic)
                .map_err(|_| "read the service's stderr")
        });
    let status = child.wait();
    read?;
    let status = status.map_err(|error| format!("wait for the service: {error}"))?;
    let diagnostic = String::from_utf8_lossy(&diagnostic);
    report(std::io::stderr(), format_args!("{}", diagnostic.trim_end()))?;
    if status.success() || !diagnostic.contains(NO_TPM_DIAGNOSTIC) {
        return Err(format!(
            "the device-bound service did not refuse for want of a TPM: {status}"
        ));
    }
    for (label, stream) in [("installer", &mut installer), ("consent", &mut authority)] {
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .map_err(|error| format!("bound the {label} channel: {error}"))?;
        let mut byte = [0; 1];
        match stream.read(&mut byte) {
            Ok(0) => {}
            Ok(_) => return Err(format!("the refusing service wrote to its {label} channel")),
            Err(error) => return Err(format!("read the {label} channel: {error}")),
        }
    }
    if canaries(device)? != before {
        return Err("the refusing service changed the disk".into());
    }
    command("/bin/umount", &["/td/store"])?;
    report(std::io::stdout(), format_args!("{NO_TPM_MARKER}"))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]
    use super::*;

    const KEY: &[u8; RECOVERY_DIGITS] = b"123456789012345678901234567890123456789012345678";

    #[test]
    fn a_mistyped_key_differs_only_in_its_last_digit() {
        let key = RecoveryDigits::new(KEY).unwrap();
        let wrong = mistyped(&key).unwrap();
        assert_ne!(wrong, key);
        assert_eq!(wrong.as_bytes()[..47], KEY[..47]);
        assert_eq!(wrong.as_bytes()[47], b'9');
        let nine = RecoveryDigits::new(&[b'9'; RECOVERY_DIGITS]).unwrap();
        assert_eq!(mistyped(&nine).unwrap().as_bytes()[47], b'0');
    }

    #[test]
    fn needles_find_the_passphrase_and_the_display_form_only() {
        let needles = Needles::new(&RecoveryDigits::new(KEY).unwrap());
        assert_eq!(
            &needles.display[..],
            b"123456-789012-345678-901234-567890-123456-789012-345678"
        );
        let mut haystack = b"prefix ".to_vec();
        haystack.extend_from_slice(KEY);
        assert!(needles.found_in(&haystack));
        assert!(needles.found_in(b"x123456-789012-345678-901234-567890-123456-789012-345678y"));
        assert!(!needles.found_in(&KEY[1..]));
        assert!(!needles.found_in(b"123456 789012 345678 901234 567890 123456 789012 345678"));
        assert!(!needles.found_in(b""));
    }

    #[test]
    fn contains_finds_a_needle_at_every_position() {
        assert!(contains(b"abc", b"abc"));
        assert!(contains(b"aabc", b"abc"));
        assert!(contains(b"xxab", b"ab"));
        assert!(!contains(b"xxa", b"ab"));
        assert!(!contains(b"", b"a"));
        assert!(contains(b"a", b""));
    }

    #[test]
    fn table_ranges_cover_both_gpt_copies_at_either_sector_size() {
        assert_eq!(
            table_ranges(1 << 30, 512).unwrap(),
            [
                ("primary", 0, 34 * 512),
                ("backup", (1 << 30) - 33 * 512, 33 * 512)
            ]
        );
        assert_eq!(
            table_ranges(1 << 30, 4096).unwrap(),
            [
                ("primary", 0, 6 * 4096),
                ("backup", (1 << 30) - 5 * 4096, 5 * 4096)
            ]
        );
        assert!(table_ranges(67 * 512 - 1, 512).is_err());
        assert!(table_ranges(67 * 512, 512).is_ok());
    }

    #[test]
    fn the_key_scan_finds_a_key_split_across_reads() {
        let dir =
            std::env::temp_dir().join(format!("td-install-qemu-test-scan-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let needles = Needles::new(&RecoveryDigits::new(KEY).unwrap());
        let clean = dir.join("clean");
        let mut bytes = vec![0x5a; (4 << 20) + 100];
        fs::write(&clean, &bytes).unwrap();
        assert_eq!(
            require_key_absent(clean.to_str().unwrap(), &needles).unwrap(),
            bytes.len() as u64
        );
        // Straddling the first read's end, and in display form.
        let at = (4 << 20) - 20;
        bytes[at..at + DISPLAY_BYTES].copy_from_slice(&needles.display[..]);
        let dirty = dir.join("dirty");
        fs::write(&dirty, &bytes).unwrap();
        assert!(require_key_absent(dirty.to_str().unwrap(), &needles).is_err());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn sampled_lines_holding_the_key_or_no_cryptsetup_refuse() {
        const UUID: &str = "5a5a5a5a-5a5a-405a-805a-5a5a5a5a5a5a";
        let needles = Needles::new(&RecoveryDigits::new(KEY).unwrap());
        let mut samples = Samples::default();
        samples.cmdlines.insert(b"/bin/td-install\0serve".to_vec());
        assert!(require_key_unsampled(&samples, &needles, UUID).is_err());
        let program = "/root-image/bin/cryptsetup";
        let format = format!(
            "luksFormat --batch-mode --type luks2 --cipher aes-xts-plain64 --key-size 512 \
             --sector-size 4096 --hash sha256 --pbkdf pbkdf2 --pbkdf-force-iterations 1000 \
             --use-random --luks2-metadata-size 16384 --luks2-keyslots-size 16744448 \
             --offset 32768 --uuid {UUID} --label td-system --key-slot 0 --key-file=- \
             /dev/loop0"
        );
        let lines = [
            format.as_str(),
            "luksAddKey --batch-mode --pbkdf pbkdf2 --pbkdf-force-iterations 1000 --hash \
             sha256 --key-slot 0 --new-key-slot 1 --key-file=- /dev/loop0 /proc/812/fd/5",
            "token import --token-id 0 --json-file=- /dev/loop0",
            "open --type luks2 --key-file=- /dev/loop0 td-install-0123456789abcdef",
            "open --test-passphrase --type luks2 --key-slot 1 --key-file=- /dev/loop12",
            "close td-install-0123456789abcdef",
            "status td-install-0123456789abcdef",
        ];
        let argv = |line: &str| {
            let mut bytes = program.as_bytes().to_vec();
            for word in line.split(' ') {
                bytes.push(0);
                bytes.extend_from_slice(word.as_bytes());
            }
            bytes.push(0);
            bytes
        };
        for line in lines {
            samples.cryptsetup.insert(argv(line));
        }
        assert_eq!(
            require_key_unsampled(&samples, &needles, UUID).unwrap(),
            "close,luksAddKey,luksFormat,open,status,test-passphrase,token-import"
        );
        samples.environments = 1;
        assert!(require_key_unsampled(&samples, &needles, UUID).is_err());
        samples.environments = 0;
        // Each documented list with one word changed, dropped or added.
        for line in lines {
            let words: Vec<&str> = line.split(' ').collect();
            for index in 0..words.len() {
                for replacement in [
                    "6a5a5a5a-5a5a-405a-805a-5a5a5a5a5a5a",
                    "/dev/shm/aGVsbG8td29ybGQtc2VjcmV0",
                    "/proc/self/environ",
                    "--key=0123456789abcdef",
                    "/dev/loop",
                    "td-install-0123",
                    "2",
                ] {
                    if words.get(index) == Some(&replacement) {
                        continue;
                    }
                    let mut changed = words.clone();
                    changed[index] = replacement;
                    let mut odd = samples.clone();
                    odd.cryptsetup.insert(argv(&changed.join(" ")));
                    assert!(
                        require_key_unsampled(&odd, &needles, UUID).is_err(),
                        "{changed:?}"
                    );
                }
                let mut dropped = words.clone();
                dropped.remove(index);
                let mut odd = samples.clone();
                odd.cryptsetup.insert(argv(&dropped.join(" ")));
                assert!(require_key_unsampled(&odd, &needles, UUID).is_err());
            }
            let mut odd = samples.clone();
            odd.cryptsetup.insert(argv(&format!("{line} --debug")));
            assert!(require_key_unsampled(&odd, &needles, UUID).is_err());
        }
        let mut line = b"/root-image/bin/cryptsetup\0".to_vec();
        line.extend_from_slice(KEY);
        samples.cmdlines.insert(line);
        assert!(require_key_unsampled(&samples, &needles, UUID).is_err());
    }
}

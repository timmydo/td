//! `td-install storage-operation --uid 1000`: td-authd's root storage
//! worker (DESIGN.md "Storage operation worker", td-authd/DESIGN.md
//! amendment 10). In increment 8e it carries the selector update in both
//! forms; td-authd starts it only in test builds until ENCRYPTION.md's
//! item 9, so nothing in production runs it.
//!
//! The worker reads everything an update needs before it presents the
//! description, so the approval key's time holds no I/O, and after root's
//! commit acknowledgement it re-reads the header, the deployment's
//! standing and the ESP's `EFI/BOOT` sector before its first write: a
//! change there is `28`, definite because nothing was written, and the
//! worker exits successfully after sending it.

use crate::consent::{Operation, Request, SelectorForm, SELECTOR_UPDATE_CEILING};
use crate::frames::{self, remaining, Wire};
use crate::{
    esp_region, inventory, logical_sector_size, paths, protocol, read_trusted_key, sha256, td_fs,
    MAX_BOOT_FILE,
};
use std::fs::File;
use std::io::{self, Read as _, Seek, SeekFrom};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use td_protector::luks2::{self, Header, Kind};
use td_protector::protected::{FidoRole, Shape, Token};
use td_protector::protected_plan;

const PRESENT: u8 = 0x10;
const PRESENTED: u8 = 0x11;
const COMMIT: u8 = 0x12;
const COMMITTED: u8 = 0x13;
const SUCCESS: u8 = 0x14;
const FAILURE: u8 = 0x15;
const BASELINE: u8 = 0x18;
/// What the commit invitation must leave of the deadline: the writes'.
pub(crate) const COMMIT_MARGIN: Duration = Duration::from_secs(120);

/// Why an operation failed, as its `15` frame carries it. Root ends the
/// operation with `20` to `22` from the baseline itself, so the worker
/// never sends them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Failure {
    Timeout,
    Failed,
    Internal,
    Deployment,
    Esp,
    /// After root's `13` and before any write.
    Changed,
}

impl Failure {
    pub(crate) fn kind(self) -> u8 {
        match self {
            Self::Timeout => 0x08,
            Self::Failed => 0x0f,
            Self::Internal => 0x10,
            Self::Deployment => 0x23,
            Self::Esp => 0x24,
            Self::Changed => 0x28,
        }
    }
}

/// The baseline's volume byte.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Volume {
    None,
    DeviceBound,
    Upgrading,
    TpmPrimary,
    Fido2Primary,
    Refused,
}

impl Volume {
    fn byte(self) -> u8 {
        match self {
            Self::None => 0,
            Self::DeviceBound => 1,
            Self::Upgrading => 2,
            Self::TpmPrimary => 3,
            Self::Fido2Primary => 4,
            Self::Refused => 5,
        }
    }

    /// The selector update's form on this volume; none where it takes none.
    fn form(self) -> Option<SelectorForm> {
        match self {
            Self::DeviceBound => Some(SelectorForm::DeviceBound),
            Self::TpmPrimary => Some(SelectorForm::Staged),
            Self::Fido2Primary => Some(SelectorForm::Fido2),
            Self::None | Self::Upgrading | Self::Refused => None,
        }
    }

    /// A protected volume's selector carries the deployment measurement
    /// policy (DESIGN.md "Selector deployment measurement prerequisite").
    fn measured(self) -> bool {
        matches!(self, Self::TpmPrimary | Self::Fido2Primary)
    }
}

/// What discovery's held partition gave: no encrypted td volume, or its
/// LUKS2 header as td-protector's reader read it.
pub(crate) type Read = Option<Result<Header, String>>;

/// The baseline from a header read and the running deployment's standing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Baseline {
    pub(crate) volume: Volume,
    recovery: bool,
    tokens: Vec<(FidoRole, [u8; 4])>,
    pub(crate) current: Option<[u8; 32]>,
}

impl Baseline {
    pub(crate) fn new(read: &Read, current: Option<[u8; 32]>) -> Self {
        let mut baseline = Self {
            volume: Volume::None,
            recovery: false,
            tokens: Vec::new(),
            current,
        };
        let header = match read {
            None => return baseline,
            Some(Err(_)) => {
                baseline.volume = Volume::Refused;
                return baseline;
            }
            Some(Ok(header)) => header,
        };
        baseline.volume = match header.kind() {
            Kind::DeviceBound => Volume::DeviceBound,
            Kind::Upgrading => Volume::Upgrading,
            Kind::Protected => match protected_plan::check(header) {
                Ok(Shape::Tpm) => Volume::TpmPrimary,
                Ok(Shape::Fido2) => Volume::Fido2Primary,
                Err(_) => Volume::Refused,
            },
        };
        baseline.recovery = protected_plan::marker(header).is_some();
        // The reader bounds fido2 tokens at four, in token-number order.
        baseline.tokens = header
            .protected
            .iter()
            .filter_map(|(_, token)| match token {
                Token::Fido2(fido) => Some((fido.role, fingerprint(&fido.credential))),
                _ => None,
            })
            .take(luks2::MAX_FIDO2_TOKENS)
            .collect();
        baseline
    }

    /// `18`, the volume byte, the recovery-key byte, the tokens with their
    /// roles and fingerprints, then `00` and the manifest ID, or `01`.
    pub(crate) fn frame(&self) -> Vec<u8> {
        let mut frame = vec![
            BASELINE,
            self.volume.byte(),
            u8::from(self.recovery),
            // At most four.
            self.tokens.len() as u8,
        ];
        for (role, fingerprint) in &self.tokens {
            frame.push(match role {
                FidoRole::Primary => 1,
                FidoRole::Recovery => 2,
            });
            frame.extend_from_slice(fingerprint);
        }
        match self.current {
            Some(id) => {
                frame.push(0);
                frame.extend_from_slice(&id);
            }
            None => frame.push(1),
        }
        frame
    }
}

/// A credential's public short name, td-fido's `fingerprint`: the first
/// four bytes of its SHA-256 (td-authd/DESIGN.md amendment 10). Restated,
/// not imported: td-install is not one of td-fido's dependents
/// (td-fido/DESIGN.md, "Consumers"; `FIDO_DEPENDENTS`).
pub(crate) fn fingerprint(credential: &[u8]) -> [u8; 4] {
    let mut hasher = crate::sha256::Sha256::new();
    hasher.update(credential);
    let [a, b, c, d, ..] = hasher.finalize();
    [a, b, c, d]
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut text, byte| {
            // Writing to a String cannot fail.
            let _ = write!(text, "{byte:02x}");
            text
        })
}

/// What the worker reads and writes, the machine's in production and a
/// test's built header, deployment and ESP image in host tests.
pub(crate) trait Host {
    type Disk: td_fat::Device;
    /// The held partition's header, read afresh each call.
    fn header(&mut self) -> Read;
    /// The volume UUID `td.volume=` names.
    fn uuid(&self) -> &str;
    /// The running deployment's manifest ID when it is the volume's
    /// `current`, acknowledged, with no pending attempt.
    fn standing(&mut self) -> Option<[u8; 32]>;
    /// `current`'s `bzImage`, after its manifest is verified under the
    /// trusted key and the kernel hashed against it.
    fn kernel(&mut self, id: &[u8; 32]) -> Result<Vec<u8>, String>;
    /// The running root's selector template.
    fn template(&mut self) -> Result<Vec<u8>, String>;
    /// The trusted key as the volume holds it.
    fn trusted_key(&mut self) -> Result<Vec<u8>, String>;
    /// The held disk's ESP, admitted by td-fat.
    fn esp(&mut self) -> Result<td_fat::Esp<Self::Disk>, String>;
}

/// The selector `prepare` builds from the template, key and identity.
pub(crate) type Prepare<'a> = &'a dyn Fn(&[u8], &[u8], &str, bool) -> Result<Vec<u8>, String>;

/// The operation, then its failure frame, best effort. A definite `28`
/// is the one failure the worker exits successfully after.
pub(crate) fn operate<H: Host>(
    wire: &mut Wire,
    host: &mut H,
    uid: u32,
    started: Instant,
    margin: Duration,
    prepare: Prepare<'_>,
    entropy: &mut dyn FnMut() -> Result<[u8; 32], String>,
) -> Result<(), Failure> {
    let result = perform(wire, host, uid, started, margin, prepare, entropy);
    wire.session(None);
    match result {
        Ok(()) => Ok(()),
        Err(failure) => {
            let _ = wire.send(&[FAILURE, failure.kind()]);
            if failure == Failure::Changed {
                Ok(())
            } else {
                Err(failure)
            }
        }
    }
}

fn perform<H: Host>(
    wire: &mut Wire,
    host: &mut H,
    uid: u32,
    started: Instant,
    margin: Duration,
    prepare: Prepare<'_>,
    entropy: &mut dyn FnMut() -> Result<[u8; 32], String>,
) -> Result<(), Failure> {
    let read = host.header();
    let baseline = Baseline::new(&read, host.standing());
    wire.send(&baseline.frame()).map_err(|_| framing(wire))?;
    let frame = wire.receive_cleared().map_err(|_| framing(wire))?;
    let request = Request::decode(frame.bytes()).map_err(|_| Failure::Internal)?;
    drop(frame);
    // Root derives the description from the baseline, as this does.
    let (form, id) = match (
        request.operation(),
        baseline.volume.form(),
        baseline.current,
    ) {
        (Operation::SelectorUpdate { form, current, .. }, Some(expected), Some(id))
            if request.owner() == uid && *form == expected && *current == hex(&id) =>
        {
            (*form, id)
        }
        _ => return Err(Failure::Internal),
    };
    wire.limit(
        started
            .checked_add(SELECTOR_UPDATE_CEILING)
            .ok_or(Failure::Internal)?,
    );
    // Every read before the presentation.
    let kernel = host.kernel(&id).map_err(|_| Failure::Deployment)?;
    let template = host.template().map_err(|_| Failure::Deployment)?;
    let key = host.trusted_key().map_err(|_| Failure::Internal)?;
    let selector = prepare(&template, &key, host.uuid(), baseline.volume.measured())
        .map_err(|_| Failure::Deployment)?;
    drop(template);
    let esp = host.esp().map_err(|_| Failure::Esp)?;
    let staged = form == SelectorForm::Staged;
    esp.fits(kernel.len() as u64, selector.len() as u64, staged)
        .map_err(|_| Failure::Esp)?;
    let admitted = esp.boot().clone();
    drop(esp);
    // The description, presented once; its typed key is its receipt.
    let description = request.encode();
    let round = entropy().map_err(|_| Failure::Internal)?;
    let bound = wire.bound();
    wire.exchange(PRESENT, PRESENTED, round, &description, Some(bound))
        .map_err(|_| framing(wire))?;
    if remaining(wire.deadline()).map_err(|_| Failure::Timeout)? < margin {
        return Err(Failure::Timeout);
    }
    let round = entropy().map_err(|_| Failure::Internal)?;
    wire.exchange(COMMIT, COMMITTED, round, &description, None)
        .map_err(|_| framing(wire))?;
    // Root may now count a write as begun; nothing is written until every
    // read below agrees with what was presented.
    if host.header() != read || host.standing() != baseline.current {
        return Err(Failure::Changed);
    }
    let mut esp = host.esp().map_err(|_| Failure::Changed)?;
    if *esp.boot() != admitted {
        return Err(Failure::Changed);
    }
    // `fits` writes nothing, so a refusal now is as definite as the reads':
    // the ESP's free space or slots changed since the presentation.
    esp.fits(kernel.len() as u64, selector.len() as u64, staged)
        .map_err(|_| Failure::Changed)?;
    let input = |bytes: &[u8]| bytes.len() as u64;
    let (mut k, mut i) = (kernel.as_slice(), selector.as_slice());
    let kernel = td_fat::Input {
        len: input(k),
        reader: &mut k,
    };
    let initrd = td_fat::Input {
        len: input(i),
        reader: &mut i,
    };
    if staged {
        esp.stage(kernel, initrd)
    } else {
        esp.write_direct(kernel, initrd)
    }
    .map_err(|_| Failure::Failed)?;
    wire.send(&[SUCCESS]).map_err(|_| Failure::Internal)
}

/// A channel failure: TIMEOUT when a deadline ran out, else INTERNAL.
fn framing(wire: &Wire) -> Failure {
    if wire.expired() {
        Failure::Timeout
    } else {
        Failure::Internal
    }
}

/// The verb: startup's checks, the memory state, discovery, then the
/// operation. Its exit is success for `14` and for a definite `28`.
pub(crate) fn run(uid: u32) -> Result<(), String> {
    let stream = startup()?;
    let started = Instant::now();
    let deadline = started
        .checked_add(SELECTOR_UPDATE_CEILING)
        .ok_or("storage operation deadline overflow")?;
    let mut wire = Wire::new(stream, deadline)?;
    // Before the baseline, as td-secret's workers do: a disk proof holds a
    // recovery key, a PIN or a derived passphrase.
    let memory = crate::memory::with_memory_state(
        paths::read_to_string(Path::new("/proc/swaps")),
        paths::read_to_string(Path::new("/proc/self/limits")),
        || paths::open_read(Path::new("/dev/urandom")).map_err(|e| e.to_string()),
    );
    let mut random = match memory {
        Ok(random) => random,
        Err(error) => {
            let _ = wire.send(&[FAILURE, Failure::Internal.kind()]);
            return Err(format!("storage operation refused its start: {error}"));
        }
    };
    let mut host = Machine::discover();
    operate(
        &mut wire,
        &mut host,
        uid,
        started,
        COMMIT_MARGIN,
        &prepare,
        &mut || {
            let mut round = [0; 32];
            random
                .read_exact(&mut round)
                .map_err(|_| "read kernel entropy".to_string())?;
            Ok(round)
        },
    )
    .map_err(|failure| format!("storage operation failed: {failure:?}"))
}

/// What the worker's process files say, read through this crate's path
/// rules, checked by the shared endpoint rules.
fn startup() -> Result<UnixStream, String> {
    let status = paths::read_bounded(Path::new("/proc/self/status"), 8192)
        .ok()
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .ok_or("read operation process identity")?;
    let metadata = |fd: u8| {
        paths::metadata(Path::new(&format!("/proc/self/fd/{fd}")))
            .map_err(|_| "missing operation descriptor".to_string())
    };
    let descriptors = [metadata(0)?, metadata(1)?, metadata(2)?];
    let open = paths::read_dir_bounded(Path::new("/proc/self/fd"), 5)
        .map_err(|_| "inspect operation descriptors")?
        .iter()
        .filter_map(|path| path.file_name().map(std::ffi::OsStr::to_os_string))
        .collect();
    frames::endpoint(frames::Process {
        status,
        descriptors,
        open,
    })
}

/// `prepare-selector`'s rules in memory: the template, its padding and the
/// appended identity, with the measurement policy on a protected volume.
fn prepare(template: &[u8], key: &[u8], uuid: &str, measured: bool) -> Result<Vec<u8>, String> {
    let uuid = crate::VolumeUuid::parse(uuid).map_err(|e| e.to_string())?;
    crate::selector_bytes(template, key, &uuid, measured).map_err(|e| e.to_string())
}

/// Where the running system holds the volume, and what it booted, under
/// the machine's root.
const VOLUME_MOUNT: &str = "run/td-volume";
const DEPLOYMENT: &str = "run/td-deployment";
const CMDLINE: &str = "/proc/cmdline";
const TD_BOOT: &str = "/bin/td-boot";
const MAX_CMDLINE_BYTES: usize = 4096;

/// The running machine: the volume `td.volume=` names, as td-boot's
/// discovery finds it, its LUKS2 partition held for the operation, and the
/// disk that partition is on.
pub(crate) struct Machine {
    uuid: String,
    cmdline: String,
    partition: Option<File>,
    /// `/`; a scratch directory in host tests.
    root: PathBuf,
}

impl Machine {
    /// Discovery through `td-boot volume`, td-boot's own scan, whose line
    /// names the UUID, the partition and, for an encrypted volume, its
    /// active `td-system` mapping. Anything else leaves no held partition,
    /// which the baseline reads as no encrypted td volume.
    pub(crate) fn discover() -> Self {
        let cmdline = paths::read_bounded(Path::new(CMDLINE), MAX_CMDLINE_BYTES)
            .ok()
            .and_then(|bytes| String::from_utf8(bytes).ok())
            .unwrap_or_default();
        let mut uuids = cmdline
            .split_ascii_whitespace()
            .filter_map(|token| token.strip_prefix(protocol::VOLUME_CMDLINE_PREFIX));
        let uuid = match (uuids.next(), uuids.next()) {
            (Some(uuid), None) => uuid.to_owned(),
            _ => String::new(),
        };
        let partition = (!uuid.is_empty())
            .then(|| volume_partition(&uuid))
            .flatten();
        Self {
            uuid,
            cmdline,
            partition,
            root: PathBuf::from("/"),
        }
    }

    fn volume(&self) -> PathBuf {
        self.root.join(VOLUME_MOUNT)
    }
}

/// The LUKS2 partition `td-boot volume UUID` names, opened, when its line
/// is exactly the UUID, a device and a mapping node.
fn volume_partition(uuid: &str) -> Option<File> {
    let output = Command::new(TD_BOOT)
        .args(["volume", uuid])
        .env_clear()
        .current_dir("/")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let line = std::str::from_utf8(&output.stdout)
        .ok()?
        .strip_suffix('\n')?;
    match line.split(' ').collect::<Vec<_>>().as_slice() {
        [found, device, mapping]
            if *found == uuid && device.starts_with("/dev/") && mapping.starts_with("/dev/") =>
        {
            paths::open_read(Path::new(device)).ok()
        }
        _ => None,
    }
}

fn digest(bytes: &[u8]) -> [u8; 32] {
    let mut hasher = sha256::Sha256::new();
    hasher.update(bytes);
    hasher.finalize()
}

/// The 64 lowercase hexadecimal digits of `text` as bytes.
fn id_bytes(text: &[u8]) -> Option<[u8; 32]> {
    if text.len() != 64 {
        return None;
    }
    let digit = |byte: u8| match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    };
    let (pairs, []) = text.as_chunks::<2>() else {
        return None;
    };
    let mut id = [0u8; 32];
    for (slot, [high, low]) in id.iter_mut().zip(pairs) {
        *slot = digit(*high)? << 4 | digit(*low)?;
    }
    Some(id)
}

/// The kernel of the deployment td-boot authenticate's line names: that
/// line is the ID's, the manifest hashes to the ID, and `bzImage` is the
/// file the manifest names, hashed here.
fn authenticated_kernel(
    directory: &Path,
    id: &[u8; 32],
    authenticated: &[u8],
) -> Result<Vec<u8>, String> {
    if authenticated != format!("{}\n", hex(id)).as_bytes() {
        return Err("current's deployment does not authenticate".into());
    }
    let manifest = paths::read_bounded(
        &directory.join(protocol::MANIFEST_NAME),
        protocol::MAX_MANIFEST_BYTES as usize,
    )
    .map_err(|e| e.to_string())?;
    if digest(&manifest) != *id {
        return Err("current's manifest is not its ID".into());
    }
    let expected = manifest_digest(&manifest, "bzImage").ok_or("the manifest names no bzImage")?;
    let kernel = td_fs::read_bounded_real_file(
        &directory.join("bzImage"),
        "deployment kernel",
        MAX_BOOT_FILE,
    )
    .map_err(|e| e.to_string())?;
    if digest(&kernel) != expected {
        return Err("bzImage does not match current's manifest".into());
    }
    Ok(kernel)
}

/// The SHA-256 the manifest gives `name`: its line `<64 hex>  <name>`.
fn manifest_digest(manifest: &[u8], name: &str) -> Option<[u8; 32]> {
    let text = std::str::from_utf8(manifest).ok()?;
    let mut lines = text.split_terminator('\n');
    if lines.next()?.as_bytes() != protocol::MANIFEST_HEADER {
        return None;
    }
    let mut found = None;
    for line in lines {
        let (digest, entry) = line.split_once("  ")?;
        if entry == name {
            if found.is_some() {
                return None;
            }
            found = Some(id_bytes(digest.as_bytes())?);
        }
    }
    found
}

impl Host for Machine {
    type Disk = File;

    fn header(&mut self) -> Read {
        let partition = self.partition.as_mut()?;
        Some(luks2::read(partition).and_then(|header| {
            if header.uuid == self.uuid {
                Ok(header)
            } else {
                Err("the held partition's header names another volume".into())
            }
        }))
    }

    fn uuid(&self) -> &str {
        &self.uuid
    }

    /// The deployment the selector booted (`/run/td-deployment`) when the
    /// volume's `current` names it, no attempt is pending for it and the
    /// selector could keep its bookkeeping.
    fn standing(&mut self) -> Option<[u8; 32]> {
        let running = paths::read_bounded(&self.root.join(DEPLOYMENT), 80).ok()?;
        let id = id_bytes(running.strip_suffix(b"\n")?)?;
        let name = hex(&id);
        let volume = self.volume();
        let current =
            paths::read_link(&volume.join(protocol::BOOT_DIR).join(protocol::CURRENT_SLOT)).ok()?;
        if current != Path::new(&format!("{}{name}", protocol::SELECTOR_PREFIX)) {
            return None;
        }
        match paths::symlink_metadata(&volume.join(protocol::ATTEMPTS_DIR).join(&name)) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            _ => return None,
        }
        let bookkeeping = self
            .cmdline
            .split_ascii_whitespace()
            .any(|token| token == protocol::BOOKKEEPING_UNAVAILABLE_CMDLINE_TOKEN);
        (!bookkeeping).then_some(id)
    }

    /// `td-boot authenticate` checks the deployment's manifest and
    /// signature under the volume's trusted key: a bounded read and no
    /// payload hash, so a worker root kills leaves no long child behind.
    fn kernel(&mut self, id: &[u8; 32]) -> Result<Vec<u8>, String> {
        let volume = self.volume();
        let directory = volume.join(protocol::DEPLOYMENTS_DIR).join(hex(id));
        let output = Command::new(TD_BOOT)
            .arg("authenticate")
            .arg(&directory)
            .arg(volume.join(protocol::VOLUME_TRUSTED_KEY))
            .env_clear()
            .current_dir("/")
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .map_err(|e| format!("run td-boot authenticate: {e}"))?;
        if !output.status.success() {
            return Err("current's deployment does not authenticate".into());
        }
        authenticated_kernel(&directory, id, &output.stdout)
    }

    fn template(&mut self) -> Result<Vec<u8>, String> {
        td_fs::read_bounded_real_file(
            &self.root.join(protocol::SELECTOR_TEMPLATE_PATH),
            "selector template",
            MAX_BOOT_FILE,
        )
        .map_err(|e| e.to_string())
    }

    fn trusted_key(&mut self) -> Result<Vec<u8>, String> {
        read_trusted_key(&self.volume().join(protocol::VOLUME_TRUSTED_KEY))
            .map_err(|e| e.to_string())
    }

    /// The held partition's disk, read-write, and its `td-esp` partition,
    /// which td's layout makes the first.
    fn esp(&mut self) -> Result<td_fat::Esp<File>, String> {
        use std::os::linux::fs::MetadataExt;
        let partition = self.partition.as_ref().ok_or("no held partition")?;
        let rdev = partition.metadata().map_err(|e| e.to_string())?.st_rdev();
        let disk = inventory::backing_disk(rdev).map_err(|e| e.to_string())?;
        let mut file =
            paths::open_read_write(&Path::new("/dev").join(&disk)).map_err(|e| e.to_string())?;
        let sector = logical_sector_size(&file).map_err(|e| e.to_string())?;
        let bytes = file.seek(SeekFrom::End(0)).map_err(|e| e.to_string())?;
        let (offset, len) =
            esp_region(&mut file, sector, bytes / sector).map_err(|e| e.to_string())?;
        td_fat::Esp::open(file, offset, len).map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::panic
    )]
    use super::*;
    use crate::consent::ApprovalKey;
    use std::cell::RefCell;
    use std::io::Write as _;
    use std::os::unix::net::UnixStream;
    use std::rc::Rc;
    use td_protector::luks2::HeaderCopy;
    use td_protector::protected::Fido2;

    const UUID: &str = "5a5a5a5a-5a5a-405a-805a-5a5a5a5a5a5a";
    const ID: [u8; 32] = [0xab; 32];
    const NONCE: [u8; 32] = [42; 32];
    const KEY: &[u8] = b"0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef\n";
    /// The ESP's first sector on the test disk.
    const BASE: u64 = 1 << 20;
    const SECTORS: u64 = 67_000;

    /// A disk image in memory that every `esp()` call reopens.
    #[derive(Clone)]
    struct Disk(Rc<RefCell<Vec<u8>>>);

    impl td_fat::Device for Disk {
        fn read_at(&self, offset: u64, buf: &mut [u8]) -> std::io::Result<()> {
            let bytes = self.0.borrow();
            let at = usize::try_from(offset).unwrap();
            let slice = bytes
                .get(at..at + buf.len())
                .ok_or_else(|| std::io::Error::other("read past the disk"))?;
            buf.copy_from_slice(slice);
            Ok(())
        }
        fn write_at(&mut self, offset: u64, buf: &[u8]) -> std::io::Result<()> {
            let mut bytes = self.0.borrow_mut();
            let at = usize::try_from(offset).unwrap();
            bytes
                .get_mut(at..at + buf.len())
                .ok_or_else(|| std::io::Error::other("write past the disk"))?
                .copy_from_slice(buf);
            Ok(())
        }
        fn sync(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// td's layout's ESP holding `kernel` and `initrd` as the pair.
    fn esp_image(kernel: &[u8], initrd: &[u8], sectors: u64) -> Disk {
        use crate::fat::Node;
        let volume = crate::fat::Volume {
            bytes_per_sector: 512,
            total_sectors: sectors,
            hidden_sectors: (BASE / 512) as u32,
            volume_id: 0x7d1e_9a30,
            label: "TD-ESP".into(),
            sectors_per_cluster: Some(1),
            root: vec![(
                "EFI".into(),
                Node::Dir(vec![(
                    "BOOT".into(),
                    Node::Dir(vec![
                        ("BOOTX64.EFI".into(), Node::File(kernel)),
                        ("INITRD".into(), Node::File(initrd)),
                    ]),
                )]),
            )],
        };
        let image = crate::fat::build(&volume).unwrap();
        let mut bytes = vec![0u8; usize::try_from(BASE + sectors * 512).unwrap()];
        for extent in &image.extents {
            let at = usize::try_from(BASE + extent.offset).unwrap();
            bytes[at..at + extent.bytes.len()].copy_from_slice(&extent.bytes);
        }
        Disk(Rc::new(RefCell::new(bytes)))
    }

    fn header(protected: Vec<(u8, Token)>) -> Header {
        Header {
            copy: HeaderCopy::Primary,
            seqid: 1,
            hdr_size: 16384,
            uuid: UUID.into(),
            label: "td-system".into(),
            keyslots: vec![0, 1],
            tokens: Vec::new(),
            orphans: Vec::new(),
            foreign: Vec::new(),
            protected,
        }
    }

    fn fido(role: FidoRole, keyslot: u8, credential: u8) -> Token {
        Token::Fido2(Fido2 {
            keyslot: Some(keyslot),
            role,
            credential: vec![credential; 16],
            salt: [3; 32],
            x: [4; 32],
            y: [5; 32],
        })
    }

    /// The FIDO2-primary shape: the marker and a primary and recovery key.
    fn fido_header() -> Header {
        header(vec![
            (0, Token::RecoveryKey { keyslot: Some(0) }),
            (1, fido(FidoRole::Primary, 1, 7)),
            (2, fido(FidoRole::Recovery, 2, 8)),
        ])
    }

    fn tpm_pin(keyslot: u8) -> Token {
        use td_protector::pin::PinPcrs;
        use td_protector::protected::TpmPin;
        use td_tpm::SealedObject;
        Token::TpmPin(TpmPin {
            keyslot: Some(keyslot),
            pcrs: PinPcrs::Selector,
            primary: [0x0b; 34],
            salt: [9; 32],
            sealed: SealedObject {
                public: vec![1; 8],
                private: vec![2; 8],
            },
        })
    }

    /// The TPM-primary shape: the marker and a tpm-pin token.
    fn tpm_header() -> Header {
        header(vec![
            (0, Token::RecoveryKey { keyslot: Some(0) }),
            (1, tpm_pin(1)),
        ])
    }

    struct Fake {
        reads: Vec<Read>,
        standings: Vec<Option<[u8; 32]>>,
        kernel: Result<Vec<u8>, String>,
        template: Result<Vec<u8>, String>,
        disk: Disk,
        /// Applied to the disk at the second `esp()`, the re-read after `13`.
        poke: Option<fn(&mut [u8])>,
        esp_calls: usize,
        /// What the worker asked for, in order.
        log: Vec<&'static str>,
    }

    impl Fake {
        fn new(read: Read) -> Self {
            Self {
                reads: vec![read],
                standings: vec![Some(ID)],
                kernel: Ok(vec![0x4b; 3000]),
                template: Ok(vec![0x54; 2000]),
                disk: esp_image(&[1; 5000], &[2; 4000], SECTORS),
                poke: None,
                esp_calls: 0,
                log: Vec::new(),
            }
        }

        /// The `n`th of `values`, the last one repeating.
        fn nth<T: Clone>(values: &[T], n: usize) -> T {
            values.get(n).or(values.last()).cloned().unwrap()
        }
    }

    impl Host for Fake {
        type Disk = Disk;
        fn header(&mut self) -> Read {
            let n = self.log.iter().filter(|&&what| what == "header").count();
            self.log.push("header");
            Self::nth(&self.reads, n)
        }
        fn uuid(&self) -> &str {
            UUID
        }
        fn standing(&mut self) -> Option<[u8; 32]> {
            let n = self.log.iter().filter(|&&what| what == "standing").count();
            self.log.push("standing");
            Self::nth(&self.standings, n)
        }
        fn kernel(&mut self, id: &[u8; 32]) -> Result<Vec<u8>, String> {
            assert_eq!(id, &ID);
            self.log.push("kernel");
            self.kernel.clone()
        }
        fn template(&mut self) -> Result<Vec<u8>, String> {
            self.log.push("template");
            self.template.clone()
        }
        fn trusted_key(&mut self) -> Result<Vec<u8>, String> {
            self.log.push("key");
            Ok(KEY.to_vec())
        }
        fn esp(&mut self) -> Result<td_fat::Esp<Disk>, String> {
            self.log.push("esp");
            self.esp_calls += 1;
            if self.esp_calls == 2 {
                if let Some(poke) = self.poke {
                    poke(&mut self.disk.0.borrow_mut());
                }
            }
            td_fat::Esp::open(self.disk.clone(), BASE, SECTORS * 512).map_err(|e| e.to_string())
        }
    }

    /// The template, a marker, the key and the identity: what the test's
    /// `prepare` returns, so the staged initrd can be checked.
    fn prepare(template: &[u8], key: &[u8], uuid: &str, measured: bool) -> Result<Vec<u8>, String> {
        let mut out = template.to_vec();
        out.extend_from_slice(b"|");
        out.extend_from_slice(key);
        out.extend_from_slice(uuid.as_bytes());
        out.push(u8::from(measured));
        Ok(out)
    }

    fn update(form: SelectorForm, id: &[u8; 32]) -> Request {
        Request::new(
            NONCE,
            1000,
            Operation::SelectorUpdate {
                key: ApprovalKey::new(*b"36").unwrap(),
                form,
                current: hex(id),
            },
        )
        .unwrap()
    }

    /// Root's side: everything the worker sent, framed, after root sends
    /// `description` and acknowledges each invitation.
    fn root(stream: UnixStream, description: Option<Vec<u8>>) -> Vec<Vec<u8>> {
        let mut stream = stream;
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let mut seen = Vec::new();
        let read = |stream: &mut UnixStream| -> Option<Vec<u8>> {
            let mut header = [0; 2];
            stream.read_exact(&mut header).ok()?;
            let mut frame = vec![0; usize::from(u16::from_be_bytes(header))];
            stream.read_exact(&mut frame).ok()?;
            Some(frame)
        };
        let send = |stream: &mut UnixStream, bytes: &[u8]| {
            let _ = stream.write_all(&(bytes.len() as u16).to_be_bytes());
            let _ = stream.write_all(bytes);
        };
        let Some(baseline) = read(&mut stream) else {
            return seen;
        };
        seen.push(baseline);
        if let Some(description) = description {
            send(&mut stream, &description);
        }
        while let Some(frame) = read(&mut stream) {
            if let Some(tag @ (PRESENT | COMMIT)) = frame.first().copied() {
                let mut answer = frame.clone();
                answer[0] = tag + 1;
                send(&mut stream, &answer);
            }
            seen.push(frame);
        }
        seen
    }

    struct Run {
        result: Result<(), Failure>,
        frames: Vec<Vec<u8>>,
        log: Vec<&'static str>,
        disk: Disk,
    }

    fn run_with(mut host: Fake, description: Option<Vec<u8>>, margin: Duration) -> Run {
        let (worker, parent) = UnixStream::pair().unwrap();
        let root = std::thread::spawn(move || root(parent, description));
        let started = Instant::now();
        let mut wire = Wire::new(worker, started + SELECTOR_UPDATE_CEILING).unwrap();
        let mut rounds = 0u8;
        let result = operate(
            &mut wire,
            &mut host,
            1000,
            started,
            margin,
            &prepare,
            &mut || {
                rounds += 1;
                Ok([rounds; 32])
            },
        );
        drop(wire);
        Run {
            result,
            frames: root.join().unwrap(),
            log: host.log,
            disk: host.disk,
        }
    }

    fn run(host: Fake, form: SelectorForm) -> Run {
        run_with(
            host,
            Some(update(form, &ID).encode()),
            Duration::from_secs(5),
        )
    }

    /// The pair, and the staged pair, the disk now names.
    fn files(disk: &Disk) -> (Vec<u8>, Vec<u8>, Option<(Vec<u8>, Vec<u8>)>) {
        let esp = td_fat::Esp::open(disk.clone(), BASE, SECTORS * 512).unwrap();
        let read = |file: &td_fat::FileEntry| {
            let mut out = Vec::new();
            esp.read(file, &mut |chunk| {
                out.extend_from_slice(chunk);
                Ok(())
            })
            .unwrap();
            out
        };
        let boot = esp.boot().clone();
        (
            read(&boot.pair.kernel),
            read(&boot.pair.initrd),
            boot.staged
                .map(|staged| (read(&staged.kernel), read(&staged.initrd))),
        )
    }

    fn selector(measured: bool) -> Vec<u8> {
        prepare(&[0x54; 2000], KEY, UUID, measured).unwrap()
    }

    #[test]
    fn a_tpm_primary_volume_stages_the_running_pair_and_says_so_last() {
        let host = Fake::new(Some(Ok(tpm_header())));
        let run = run(host, SelectorForm::Staged);
        assert_eq!(run.result, Ok(()));
        let description = update(SelectorForm::Staged, &ID).encode();
        let mut baseline = vec![0x18, 3, 1, 0, 0];
        baseline.extend_from_slice(&ID);
        assert_eq!(run.frames[0], baseline);
        // One presentation, one commit round, both of exactly the description,
        // then success.
        assert_eq!(run.frames.len(), 4);
        assert_eq!(run.frames[1][0], PRESENT);
        assert_eq!(&run.frames[1][1..33], &[1; 32]);
        assert_eq!(&run.frames[1][33..], &description[..]);
        assert_eq!(run.frames[2][0], COMMIT);
        assert_eq!(&run.frames[2][1..33], &[2; 32]);
        assert_eq!(&run.frames[2][33..], &description[..]);
        assert_eq!(run.frames[3], [SUCCESS]);
        // Every read before the presentation; the re-reads after the commit.
        assert_eq!(
            run.log,
            [
                "header", "standing", "kernel", "template", "key", "esp", "header", "standing",
                "esp"
            ]
        );
        let (kernel, initrd, staged) = files(&run.disk);
        assert_eq!((kernel, initrd), (vec![1; 5000], vec![2; 4000]));
        assert_eq!(staged, Some((vec![0x4b; 3000], selector(true))));
    }

    #[test]
    fn the_direct_forms_replace_the_pair() {
        for (header, form, measured, volume) in [
            (header(Vec::new()), SelectorForm::DeviceBound, false, 1),
            (fido_header(), SelectorForm::Fido2, true, 4),
        ] {
            let run = run(Fake::new(Some(Ok(header))), form);
            assert_eq!(run.result, Ok(()), "{form:?}");
            assert_eq!(run.frames[0][1], volume);
            assert_eq!(run.frames.last().unwrap(), &[SUCCESS]);
            let (kernel, initrd, staged) = files(&run.disk);
            assert_eq!(kernel, vec![0x4b; 3000]);
            assert_eq!(initrd, selector(measured));
            assert_eq!(staged, None);
        }
    }

    #[test]
    fn the_baseline_names_the_volume_its_keys_and_the_running_standing() {
        let fingerprint = |byte| super::fingerprint(&[byte; 16]);
        let fido = Baseline::new(&Some(Ok(fido_header())), Some(ID));
        let mut frame = vec![0x18, 4, 1, 2, 1];
        frame.extend_from_slice(&fingerprint(7));
        frame.push(2);
        frame.extend_from_slice(&fingerprint(8));
        frame.push(0);
        frame.extend_from_slice(&ID);
        assert_eq!(fido.frame(), frame);
        let mut upgrading = header(vec![(0, Token::RecoveryKey { keyslot: Some(0) })]);
        upgrading.protected.push((
            3,
            Token::Upgrade {
                shape: Shape::Tpm,
                from: [1; 32],
            },
        ));
        for (read, current, expected) in [
            (None, Some(ID), vec![0x18, 0, 0, 0, 0]),
            (Some(Err("refused".into())), None, vec![0x18, 5, 0, 0, 1]),
            (Some(Ok(header(Vec::new()))), None, vec![0x18, 1, 0, 0, 1]),
            // A protected header outside both shapes is one td's planner refuses.
            (
                Some(Ok(header(vec![(1, tpm_pin(1))]))),
                None,
                vec![0x18, 5, 0, 0, 1],
            ),
        ] {
            let frame = Baseline::new(&read, current).frame();
            assert_eq!(frame[..expected.len()], expected[..], "{read:?}");
        }
        assert_eq!(
            Baseline::new(&Some(Ok(upgrading)), None).volume,
            Volume::Upgrading
        );
    }

    /// Root's first frame must be the description the baseline admits: any
    /// other ends the worker as INTERNAL before it reads anything else.
    #[test]
    fn a_description_the_baseline_does_not_admit_reads_nothing() {
        let other = [0xcd; 32];
        let owner = Request::new(
            NONCE,
            1001,
            Operation::SelectorUpdate {
                key: ApprovalKey::new(*b"36").unwrap(),
                form: SelectorForm::Staged,
                current: hex(&ID),
            },
        )
        .unwrap();
        for (read, standing, description) in [
            (
                tpm_header(),
                Some(ID),
                update(SelectorForm::DeviceBound, &ID).encode(),
            ),
            (
                tpm_header(),
                Some(ID),
                update(SelectorForm::Staged, &other).encode(),
            ),
            (
                tpm_header(),
                None,
                update(SelectorForm::Staged, &ID).encode(),
            ),
            (tpm_header(), Some(ID), owner.encode()),
            (
                header(Vec::new()),
                Some(ID),
                update(SelectorForm::Staged, &ID).encode(),
            ),
            (tpm_header(), Some(ID), b"TDCONS01".to_vec()),
        ] {
            let mut host = Fake::new(Some(Ok(read)));
            host.standings = vec![standing];
            let run = run_with(host, Some(description), Duration::from_secs(5));
            assert_eq!(run.result, Err(Failure::Internal));
            assert_eq!(run.log, ["header", "standing"]);
            assert_eq!(run.frames.last().unwrap(), &[FAILURE, 0x10]);
        }
    }

    #[test]
    fn each_read_before_the_presentation_has_its_kind() {
        let staged = || Fake::new(Some(Ok(tpm_header())));
        let mut kernel = staged();
        kernel.kernel = Err("bzImage does not match the manifest".into());
        let mut template = staged();
        template.template = Err("unreadable".into());
        let mut foreign = staged();
        foreign.disk = Disk(Rc::new(RefCell::new(vec![0; 4 << 20])));
        // More than the volume has free.
        let mut small = staged();
        small.kernel = Ok(vec![0x4b; 34 << 20]);
        for (host, failure) in [
            (kernel, Failure::Deployment),
            (template, Failure::Deployment),
            (foreign, Failure::Esp),
            (small, Failure::Esp),
        ] {
            let run = run(host, SelectorForm::Staged);
            assert_eq!(run.result, Err(failure));
            // No presentation, no commit: the baseline and the failure.
            assert_eq!(run.frames.len(), 2, "{failure:?}");
            assert_eq!(run.frames[1], [FAILURE, failure.kind()]);
        }
    }

    /// The commit invitation needs the writes' time left of the deadline.
    #[test]
    fn the_commit_margin_is_checked_before_the_invitation() {
        let host = Fake::new(Some(Ok(tpm_header())));
        let before = files(&host.disk);
        let run = run_with(
            host,
            Some(update(SelectorForm::Staged, &ID).encode()),
            SELECTOR_UPDATE_CEILING,
        );
        assert_eq!(run.result, Err(Failure::Timeout));
        assert_eq!(run.frames.len(), 3);
        assert_eq!(run.frames[1][0], PRESENT);
        assert_eq!(run.frames[2], [FAILURE, 0x08]);
        assert_eq!(files(&run.disk), before);
    }

    /// After root's `13` the header, the standing and the sector are read
    /// again; any change is `28` with nothing written, and the worker exits
    /// successfully after it.
    #[test]
    fn a_change_after_the_commit_is_definite_and_writes_nothing() {
        let changed_header = {
            let mut changed = tpm_header();
            changed.seqid = 2;
            changed
        };
        let mut header = Fake::new(Some(Ok(tpm_header())));
        header.reads.push(Some(Ok(changed_header)));
        let mut unreadable = Fake::new(Some(Ok(tpm_header())));
        unreadable.reads.push(Some(Err("refused".into())));
        let mut standing = Fake::new(Some(Ok(tpm_header())));
        standing.standings.push(None);
        let mut sector = Fake::new(Some(Ok(tpm_header())));
        // BOOTX64.EFI's creation-time tenths in EFI/BOOT's first sector, which
        // td-fat admits either way: the root's cluster is 2, EFI's 3, BOOT's
        // 4, one sector each after the reserved sectors and the two FATs.
        sector.poke = Some(|disk| {
            let (reserved, fat, _) = layout(disk);
            disk[BASE as usize + (reserved + 2 * fat + 2) * 512 + 2 * 32 + 13] = 1;
        });
        // Every free cluster marked bad in both FATs, EFI/BOOT untouched:
        // td-fat's space check, which writes nothing, now refuses.
        let mut space = Fake::new(Some(Ok(tpm_header())));
        space.poke = Some(|disk| {
            let (reserved, fat, clusters) = layout(disk);
            for copy in 0..2 {
                let base = BASE as usize + (reserved + copy * fat) * 512;
                for cluster in 2..clusters + 2 {
                    let at = base + cluster * 4;
                    if disk[at..at + 4] == [0; 4] {
                        disk[at..at + 4].copy_from_slice(&0x0fff_fff7u32.to_le_bytes());
                    }
                }
            }
        });
        for host in [header, unreadable, standing, sector, space] {
            let before = files(&host.disk);
            let run = run(host, SelectorForm::Staged);
            assert_eq!(run.result, Ok(()));
            assert_eq!(run.frames.len(), 4);
            assert_eq!(run.frames[3], [FAILURE, 0x28]);
            assert_eq!(files(&run.disk), before);
        }
    }

    /// The test ESP's reserved and FAT sectors and its cluster count, one
    /// sector a cluster.
    fn layout(disk: &[u8]) -> (usize, usize, usize) {
        let bpb = &disk[BASE as usize..][..512];
        let reserved = usize::from(u16::from_le_bytes([bpb[14], bpb[15]]));
        let fat = u32::from_le_bytes(bpb[36..40].try_into().unwrap()) as usize;
        let total = u32::from_le_bytes(bpb[32..36].try_into().unwrap()) as usize;
        (reserved, fat, total - reserved - 2 * fat)
    }

    /// A machine rooted at a scratch directory holding the volume's
    /// `current` link to `ID` and the running deployment's record.
    struct Rooted {
        machine: Machine,
        root: PathBuf,
    }

    impl Drop for Rooted {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn rooted(cmdline: &str) -> Rooted {
        let root = crate::scratch::path("storage-machine");
        let volume = root.join(VOLUME_MOUNT);
        std::fs::create_dir_all(volume.join(protocol::ATTEMPTS_DIR)).unwrap();
        std::fs::create_dir_all(volume.join(protocol::DEPLOYMENTS_DIR).join(hex(&ID))).unwrap();
        std::os::unix::fs::symlink(
            format!("{}{}", protocol::SELECTOR_PREFIX, hex(&ID)),
            volume.join(protocol::BOOT_DIR).join(protocol::CURRENT_SLOT),
        )
        .unwrap();
        std::fs::write(root.join(DEPLOYMENT), format!("{}\n", hex(&ID))).unwrap();
        Rooted {
            machine: Machine {
                uuid: UUID.into(),
                cmdline: cmdline.into(),
                partition: None,
                root: root.clone(),
            },
            root,
        }
    }

    /// The running deployment stands when it is `current`, with no pending
    /// attempt and with bookkeeping; a record in another form, another
    /// `current`, a pending attempt or a bookkeeping failure leaves none.
    #[test]
    fn the_machine_stands_on_current_alone() {
        let mut host = rooted("quiet td.volume=x");
        assert_eq!(host.machine.standing(), Some(ID));
        assert!(host.machine.header().is_none());

        let mut host = rooted(&format!(
            "quiet {}",
            protocol::BOOKKEEPING_UNAVAILABLE_CMDLINE_TOKEN
        ));
        assert_eq!(host.machine.standing(), None);

        let host = rooted("");
        let attempt = host
            .root
            .join(VOLUME_MOUNT)
            .join(protocol::ATTEMPTS_DIR)
            .join(hex(&ID));
        std::fs::write(&attempt, b"1\n").unwrap();
        let mut host = host;
        assert_eq!(host.machine.standing(), None);

        let mut host = rooted("");
        let current = host
            .root
            .join(VOLUME_MOUNT)
            .join(protocol::BOOT_DIR)
            .join(protocol::CURRENT_SLOT);
        std::fs::remove_file(&current).unwrap();
        std::os::unix::fs::symlink(
            format!("{}{}", protocol::SELECTOR_PREFIX, "cd".repeat(32)),
            &current,
        )
        .unwrap();
        assert_eq!(host.machine.standing(), None);

        for record in [
            hex(&ID),
            format!("{}\n", hex(&ID).to_uppercase()),
            format!("{}\n", &hex(&ID)[2..]),
            format!("{}\n\n", hex(&ID)),
        ] {
            let mut host = rooted("");
            std::fs::write(host.root.join(DEPLOYMENT), record).unwrap();
            assert_eq!(host.machine.standing(), None);
        }
        let mut host = rooted("");
        std::fs::remove_file(host.root.join(DEPLOYMENT)).unwrap();
        assert_eq!(host.machine.standing(), None);
    }

    /// The template is the running root's; the trusted key the volume's.
    #[test]
    fn the_machine_reads_the_template_and_key_under_its_root() {
        let mut host = rooted("");
        assert!(host.machine.template().is_err());
        assert!(host.machine.trusted_key().is_err());
        let template = host.root.join(protocol::SELECTOR_TEMPLATE_PATH);
        std::fs::create_dir_all(template.parent().unwrap()).unwrap();
        std::fs::write(&template, b"template").unwrap();
        std::fs::write(
            host.root
                .join(VOLUME_MOUNT)
                .join(protocol::VOLUME_TRUSTED_KEY),
            KEY,
        )
        .unwrap();
        assert_eq!(host.machine.template().unwrap(), b"template");
        assert_eq!(host.machine.trusted_key().unwrap(), KEY);
    }

    /// After `td-boot authenticate`, the kernel is read only when its line
    /// is the ID's, the manifest hashes to the ID and the kernel to the
    /// manifest's one `bzImage` line.
    #[test]
    fn the_authenticated_kernel_is_current_manifests() {
        let host = rooted("");
        let kernel = b"kernel bytes".to_vec();
        let directory = host
            .root
            .join(VOLUME_MOUNT)
            .join(protocol::DEPLOYMENTS_DIR)
            .join(hex(&ID));
        let manifest = format!(
            "td-deployment-v1\n{}  bzImage\n{}  initramfs.cpio\n",
            hex(&digest(&kernel)),
            "00".repeat(32)
        );
        std::fs::write(directory.join(protocol::MANIFEST_NAME), &manifest).unwrap();
        std::fs::write(directory.join("bzImage"), &kernel).unwrap();
        let id = digest(manifest.as_bytes());
        let line = format!("{}\n", hex(&id));
        assert_eq!(
            authenticated_kernel(&directory, &id, line.as_bytes()).unwrap(),
            kernel
        );
        // Another line, or none.
        for other in [String::new(), hex(&id), format!("{}\n", hex(&ID))] {
            assert!(authenticated_kernel(&directory, &id, other.as_bytes()).is_err());
        }
        // A manifest that is not the ID.
        assert!(
            authenticated_kernel(&directory, &ID, format!("{}\n", hex(&ID)).as_bytes()).is_err()
        );
        // A kernel the manifest does not name.
        std::fs::write(directory.join("bzImage"), b"other kernel").unwrap();
        assert!(authenticated_kernel(&directory, &id, line.as_bytes())
            .is_err_and(|e| e.contains("does not match")));
    }

    /// The worker's td-boot children read a bounded amount: discovery and
    /// the manifest's authentication, never a whole deployment's hash, which
    /// a worker root killed would leave running.
    #[test]
    fn the_worker_runs_only_bounded_td_boot_verbs() {
        let source = include_str!("storage_operation.rs");
        let production = source.split("#[cfg(test)]").next().unwrap();
        assert_eq!(production.matches("Command::new(TD_BOOT)").count(), 2);
        assert_eq!(production.matches("Command::new(").count(), 2);
        assert!(production.contains(".args([\"volume\", uuid])"));
        assert!(production.contains(".arg(\"authenticate\")"));
        for verb in ["validate-source", "verify", "boot", "install"] {
            assert!(!production.contains(&format!("\"{verb}\"")), "{verb}");
        }
    }

    /// The kernel's digest comes from the manifest's own form, once.
    #[test]
    fn the_manifest_names_its_kernel_once_in_its_own_form() {
        let manifest = format!(
            "td-deployment-v1\n{}  bzImage\n{}  initramfs.cpio\n{}  root.erofs\n",
            "ab".repeat(32),
            "cd".repeat(32),
            "ef".repeat(32)
        );
        assert_eq!(
            manifest_digest(manifest.as_bytes(), "bzImage"),
            Some([0xab; 32])
        );
        assert_eq!(manifest_digest(manifest.as_bytes(), "selector"), None);
        let other = manifest.replace("td-deployment-v1", "td-deployment-v2");
        assert_eq!(manifest_digest(other.as_bytes(), "bzImage"), None);
        let twice = format!("{manifest}{}  bzImage\n", "ab".repeat(32));
        assert_eq!(manifest_digest(twice.as_bytes(), "bzImage"), None);
        assert_eq!(id_bytes("AB".repeat(32).as_bytes()), None);
        assert_eq!(id_bytes("ab".repeat(31).as_bytes()), None);
        assert_eq!(id_bytes(&[b'a'; 65]), None);
    }

    #[test]
    fn failure_kinds_are_the_documented_bytes() {
        for (failure, byte) in [
            (Failure::Timeout, 0x08),
            (Failure::Failed, 0x0f),
            (Failure::Internal, 0x10),
            (Failure::Deployment, 0x23),
            (Failure::Esp, 0x24),
            (Failure::Changed, 0x28),
        ] {
            assert_eq!(failure.kind(), byte);
        }
    }

    /// td-fido's fingerprint, the first four bytes of SHA-256, over FIPS
    /// 180-2's "abc" vector.
    #[test]
    fn the_fingerprint_is_the_first_four_bytes_of_sha256() {
        assert_eq!(fingerprint(b"abc"), [0xba, 0x78, 0x16, 0xbf]);
    }
}

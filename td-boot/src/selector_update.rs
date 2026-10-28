//! A staged selector update's pre-seal and commit in the installed
//! selector (td-install/ENCRYPTION.md "Selector update"). After a TPM PIN
//! release and before the cap, `preseal` reads the staged pair once off
//! the ESP, holding it for the commit, and predicts its chain's PCRs 4 and
//! 9 from the event log; td-protector seals the released secret to that
//! prediction. After the cap, with the volume mounted, `commit`
//! checks the staged pair against the volume's verified `current`
//! deployment and then imports the predicted object and commits, deletes
//! the staged update, or keeps it under an `update-retry` token. The
//! machine is behind `Machine`, so the decisions are tested over an ESP,
//! a header and a deployment the tests build.
use std::fs::{self, File, OpenOptions};
use std::io::{Seek, SeekFrom};
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use td_fat::{BootDir, Esp, FileEntry};
use td_protector::cryptsetup::Resilience;
use td_protector::luks2::{self, Header};
use td_protector::protected::TpmPin;
use td_protector::protected_plan;
use td_protector::protected_release::{self as protected, Entered, Predicted, Prompt};
use td_protector::release::{Runner, Tpm};
use td_protector::update_retry;
use td_protector::upgrade;

use crate::eventlog::{self, Chain, Pcrs};
use crate::sha256::Sha256;
use crate::{authenticode, gpt, protocol, selector_appendix, td_fs, volume};

/// The console's offer after a retry's prediction failed again with the
/// staged pair authentic and its retry token verifying.
pub(crate) const UPDATE_WARNING: &str = "td: the staged selector update is authentic, but td \
     could not predict its boot chain at this boot or the last. Completing it anyway restarts \
     the machine at once; the next boot shows the boot-chain warning once and asks for a \
     security key or the recovery key, which is expected. Any other answer deletes the staged \
     update.";
pub(crate) const UPDATE_PROMPT: &str = "td: type update to complete it: ";
pub(crate) const UPDATE_WORD: &[u8] = b"update";

/// Where the commit binds `current`'s root and reads its template: the
/// selector initramfs's empty `/sysroot`, which it never pivots to.
const LOOP_DEVICE: &str = "/dev/loop0";
const TEMPLATE_ROOT: &str = "/sysroot";
const SECURITYFS: &str = "/sys/kernel/security";
/// The largest staged initramfs or template read, as the largest kernel.
const MAX_FILE_BYTES: usize = authenticode::MAX_IMAGE_BYTES;
const MAX_SYSFS_BYTES: u64 = 4096;

/// The staged pair's digests, from the pre-seal's one read of each file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Digests {
    pub(crate) kernel: [u8; 32],
    pub(crate) authenticode: [u8; 32],
    pub(crate) initrd: [u8; 32],
}

/// What the pre-seal read and holds to the commit: the ESP, `EFI/BOOT` as
/// it was read and hashed, the staged digests, and the prediction or why
/// there is none.
pub(crate) struct Staged<D: td_fat::Device> {
    esp: Esp<D>,
    read: BootDir,
    digests: Digests,
    prediction: Result<Pcrs, String>,
}

impl<D: td_fat::Device> Staged<D> {
    /// The predicted PCRs, for td-protector's seal.
    pub(crate) fn predicted(&self) -> Option<Predicted> {
        self.prediction.as_ref().ok().map(|pcrs| Predicted {
            pcr4: pcrs.pcr4,
            pcr7: pcrs.pcr7,
            pcr9: pcrs.pcr9,
        })
    }
}

/// `current`'s kernel digest, as its verified manifest names it, and
/// the selector template its hashed `root.erofs` holds.
pub(crate) struct Current {
    pub(crate) kernel: [u8; 32],
    pub(crate) template: Vec<u8>,
}

/// Why the commit cannot match the staged pair to `current`.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Unverified {
    /// It is not `current`'s: the staged update is deleted.
    Refused(String),
    /// `current` could not be read: the staged update waits.
    Unavailable(String),
}

/// What the selector reads and runs for an update.
pub(crate) trait Machine {
    type Disk: td_fat::Device;
    /// The held partition's disk's ESP, admitted by td-fat.
    fn esp(&mut self) -> Result<Esp<Self::Disk>, String>;
    /// PCRs 4, 7 and 9 as the event log replays them with `staged` in
    /// place of `running`, the log read through securityfs and checked
    /// against the PCRs as read.
    fn predict(&mut self, running: &Chain, staged: &Chain) -> Result<Pcrs, String>;
    /// The held partition's LUKS2 header, read afresh.
    fn header(&mut self) -> Result<Header, String>;
    /// `current` on the volume mounted at `root`, its manifest hashing to
    /// its id and its root to the manifest.
    fn current(&mut self, root: &Path) -> Result<Current, Unverified>;
    /// The archive `prepare-selector` appends with this selector's own
    /// volume UUID and measurement policy.
    fn appendix(&mut self) -> Result<Vec<u8>, String>;
}

fn digest(bytes: &[u8]) -> [u8; 32] {
    let mut hash = Sha256::new();
    hash.update(bytes);
    hash.finalize()
}

/// A manifest's 64 lowercase hexadecimal digits as bytes.
fn parse_digest(text: &str) -> Option<[u8; 32]> {
    let digit = |byte: u8| match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    };
    let (pairs, []) = text.as_bytes().as_chunks::<2>() else {
        return None;
    };
    if pairs.len() != 32 {
        return None;
    }
    let mut out = [0u8; 32];
    for (slot, [high, low]) in out.iter_mut().zip(pairs) {
        *slot = digit(*high)? << 4 | digit(*low)?;
    }
    Some(out)
}

/// One file of the pair, whole, at most `MAX_FILE_BYTES`.
fn read_file<D: td_fat::Device>(esp: &Esp<D>, file: &FileEntry) -> Result<Vec<u8>, String> {
    let size = usize::try_from(file.size).map_err(|_| "a file exceeds memory".to_string())?;
    if size > MAX_FILE_BYTES {
        return Err(format!("a {size}-byte file is over td's {MAX_FILE_BYTES}"));
    }
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(size)
        .map_err(|_| "a file exceeds memory".to_string())?;
    esp.read(file, &mut |chunk| {
        bytes.extend_from_slice(chunk);
        Ok(())
    })
    .map_err(|error| error.to_string())?;
    Ok(bytes)
}

/// The kernel's Authenticode SHA-256 and the initramfs's SHA-256, from one
/// read of each, with the kernel's SHA-256 beside them. The kernel is held
/// whole for Authenticode; the initramfs is hashed as it is read.
fn hash_pair<D: td_fat::Device>(esp: &Esp<D>, pair: &td_fat::Pair) -> Result<Digests, String> {
    let kernel = read_file(esp, &pair.kernel)?;
    let initrd = hash_file(esp, &pair.initrd)?;
    Ok(Digests {
        kernel: digest(&kernel),
        authenticode: authenticode::sha256(&kernel).map_err(|why| format!("the kernel: {why}"))?,
        initrd,
    })
}

/// One file's SHA-256, read through once, at most `MAX_FILE_BYTES`.
fn hash_file<D: td_fat::Device>(esp: &Esp<D>, file: &FileEntry) -> Result<[u8; 32], String> {
    let size = usize::try_from(file.size).map_err(|_| "a file exceeds memory".to_string())?;
    if size > MAX_FILE_BYTES {
        return Err(format!("a {size}-byte file is over td's {MAX_FILE_BYTES}"));
    }
    let mut hash = Sha256::new();
    esp.read(file, &mut |chunk| {
        hash.update(chunk);
        Ok(())
    })
    .map_err(|error| error.to_string())?;
    Ok(hash.finalize())
}

/// The pre-seal's reads (ENCRYPTION.md "Selector update", phase 2): the
/// ESP and its staged pair, each staged file read once for every digest
/// the commit uses, and the prediction. `None` when no update is staged,
/// or when the staged pair cannot be read, which it says; nothing is
/// written.
pub(crate) fn preseal<M: Machine>(
    machine: &mut M,
    console: &mut dyn FnMut(&str),
) -> Option<Staged<M::Disk>> {
    // An ESP the selector cannot locate or td-fat refuses shows no staged
    // pair, and says nothing: the storage worker stages only through the
    // same admission, and the line would repeat at every PIN boot.
    let esp = machine.esp().ok()?;
    let read = esp.boot().clone();
    let staged = read.staged?;
    console("a selector update is staged: reading it before the release cap");
    let digests = match hash_pair(&esp, &staged) {
        Ok(digests) => digests,
        Err(error) => {
            console(&format!(
                "the staged selector update cannot be read ({error}); it waits"
            ));
            return None;
        }
    };
    let running = hash_pair(&esp, &read.pair).map(|running| Chain {
        application: running.authenticode,
        initrd: running.initrd,
    });
    let prediction = running.and_then(|running| {
        machine.predict(
            &running,
            &Chain {
                application: digests.authenticode,
                initrd: digests.initrd,
            },
        )
    });
    match &prediction {
        Ok(_) => console("the staged update's boot chain is predicted"),
        Err(why) => console(&format!(
            "td cannot predict the staged update's boot chain: {why}"
        )),
    }
    Some(Staged {
        esp,
        read,
        digests,
        prediction,
    })
}

/// What the commit did.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// The predicted object is imported and the new pair committed.
    Committed,
    /// The staged update is deleted, or it was not `current`'s.
    Deleted,
    /// It waits for the next PIN boot, with or without a new retry token.
    Kept,
    /// `update` committed it without a predicted object: restart now.
    Restart,
}

/// The staged pair is `current`'s: `BOOTX64.NEW` is its kernel, and
/// `INITRD.NEW` its root's template followed by exactly this selector's
/// appended archive.
fn matches_current<M: Machine>(
    machine: &mut M,
    root: &Path,
    digests: &Digests,
) -> Result<(), Unverified> {
    let current = machine.current(root)?;
    if current.kernel != digests.kernel {
        return Err(Unverified::Refused(
            "BOOTX64.NEW is not the current deployment's kernel".into(),
        ));
    }
    let appendix = machine.appendix().map_err(Unverified::Unavailable)?;
    let padding = selector_appendix::padding(current.template.len() as u64)
        .map_err(Unverified::Unavailable)?;
    let zeros = [0u8; 3];
    let zeros = zeros
        .get(..padding)
        .ok_or_else(|| Unverified::Unavailable("invalid selector padding".into()))?;
    let mut hash = Sha256::new();
    hash.update(&current.template);
    hash.update(zeros);
    hash.update(&appendix);
    if hash.finalize() != digests.initrd {
        return Err(Unverified::Refused(
            "INITRD.NEW is not the current deployment's selector template followed by this \
             selector's own archive"
                .into(),
        ));
    }
    Ok(())
}

/// The retry token's removal. A token left behind verifies only for this
/// same pair staged again, so a commit removes it first and goes no
/// further without that; a deletion removes it first as well, but deletes
/// whatever the removal did.
fn remove_retry<M: Machine, C: Runner>(
    machine: &mut M,
    cryptsetup: &mut C,
    device: &Path,
    console: &mut dyn FnMut(&str),
) -> Result<(), String> {
    let header = machine.header()?;
    match protected_plan::retry_remove(&header) {
        Some(step) => protected::write_tokens(&header, device, &[step], cryptsetup, console),
        None => Ok(()),
    }
}

/// The commit's directory write, by where a failure fell. `Before` leaves
/// the staged update as it was; `Uncertain` may have made the new pair
/// `EFI/BOOT`'s; `After` did, leaving the old chains for the next
/// reclaim.
fn commit_pair<D: td_fat::Device>(
    esp: &mut Esp<D>,
    read: &BootDir,
    console: &mut dyn FnMut(&str),
) -> Option<bool> {
    match esp.commit_staged(read) {
        Ok(()) => Some(true),
        Err(error @ td_fat::CommitError::Before(_)) => {
            console(&format!("the selector update's commit failed: {error}"));
            None
        }
        Err(error @ td_fat::CommitError::Uncertain(_)) => {
            console(&format!("the selector update's commit failed: {error}"));
            Some(false)
        }
        Err(error @ td_fat::CommitError::After(_)) => {
            console(&format!("the selector update's commit: {error}"));
            Some(true)
        }
    }
}

/// The commit (ENCRYPTION.md "Selector update", phase 3), after the cap
/// and `finish`, with the volume mounted at `root`, which it only reads.
/// An authentic pair with `predicted`: the object imported as a second
/// tpm-pin token naming the keyslot that released, the retry token
/// removed, then one sector write pointing `BOOTX64.EFI` and `INITRD` at
/// the clusters the pre-seal read and hashed. One that is not authentic:
/// deleted. An authentic pair whose prediction failed this boot: kept
/// under a fresh `update-retry` token, unless its token already verifies
/// for this pair, when exactly `update` commits it without an object and
/// restarts, and anything else deletes it. One predicted this boot whose
/// object was not sealed or not kept: kept as it is, for the next PIN
/// boot.
#[allow(clippy::too_many_arguments)]
pub(crate) fn commit<M: Machine, C: Runner>(
    staged: Staged<M::Disk>,
    predicted: Option<TpmPin>,
    root: &Path,
    machine: &mut M,
    cryptsetup: &mut C,
    device: &Path,
    volume_key: &[u8],
    prompt: &mut dyn Prompt,
    console: &mut dyn FnMut(&str),
) -> Outcome {
    let Staged {
        mut esp,
        read,
        digests,
        prediction,
    } = staged;
    let delete = |esp: &mut Esp<M::Disk>,
                  machine: &mut M,
                  cryptsetup: &mut C,
                  console: &mut dyn FnMut(&str)| {
        if let Err(error) = remove_retry(machine, cryptsetup, device, console) {
            console(&format!("the update-retry token is not removed: {error}"));
        }
        match esp.delete_staged(&read) {
            Ok(()) => console("the staged selector update is deleted"),
            Err(error) => console(&format!(
                "the staged selector update could not be deleted: {error}"
            )),
        }
        Outcome::Deleted
    };
    match matches_current(machine, root, &digests) {
        Ok(()) => {}
        Err(Unverified::Refused(why)) => {
            console(&format!("the staged selector update is refused: {why}"));
            drop(predicted);
            return delete(&mut esp, machine, cryptsetup, console);
        }
        Err(Unverified::Unavailable(why)) => {
            console(&format!(
                "the staged selector update cannot be checked ({why}); it waits"
            ));
            return Outcome::Kept;
        }
    }
    let unremoved = |console: &mut dyn FnMut(&str), error: String| {
        console(&format!(
            "the staged selector update waits: its update-retry token is not removed: {error}"
        ));
        Outcome::Kept
    };
    if let Some(token) = predicted {
        let header = match machine.header() {
            Ok(header) => header,
            Err(error) => {
                console(&format!(
                    "the predicted protector is not imported ({error}); the staged update waits"
                ));
                return Outcome::Kept;
            }
        };
        let imported = token
            .keyslot
            .ok_or_else(|| "it names no keyslot".to_string())
            .and_then(|slot| {
                protected_plan::predicted_import(&header, slot, &token, volume_key)
                    .map_err(|refusal| refusal.to_string())
            })
            .and_then(|step| {
                protected::write_tokens(&header, device, &[step], cryptsetup, console)
            });
        if let Err(error) = imported {
            console(&format!(
                "the predicted protector is not imported ({error}); the staged update waits"
            ));
            return Outcome::Kept;
        }
        if let Err(error) = remove_retry(machine, cryptsetup, device, console) {
            return unremoved(console, error);
        }
        return match commit_pair(&mut esp, &read, console) {
            None => Outcome::Kept,
            Some(written) => {
                console(if written {
                    "the selector update is committed: the next boot runs the new selector and \
                     asks for the TPM PIN"
                } else {
                    "the next boot runs the old or the new selector, and either asks for the TPM \
                     PIN"
                });
                Outcome::Committed
            }
        };
    }
    let why = match &prediction {
        Err(why) => why.clone(),
        Ok(_) => {
            console(
                "the staged selector update waits: its boot chain was predicted, but no \
                 predicted protector was sealed and kept this boot; the next TPM PIN boot \
                 predicts it again",
            );
            return Outcome::Kept;
        }
    };
    let header = match machine.header() {
        Ok(header) => header,
        Err(error) => {
            console(&format!(
                "the staged selector update waits: the header cannot be read: {error}"
            ));
            return Outcome::Kept;
        }
    };
    let Some(uuid) = td_protector::lockout::volume_uuid(&header.uuid) else {
        console("the staged selector update waits: the header's UUID is not canonical");
        return Outcome::Kept;
    };
    let retried = protected_plan::update_retry(&header).is_some_and(|(_, token)| {
        update_retry::verifies(volume_key, &uuid, token, &digests.kernel, &digests.initrd)
    });
    if retried {
        console(UPDATE_WARNING);
        let answer = match prompt.read(UPDATE_PROMPT) {
            Ok(Entered::Line(answer)) => answer.as_bytes() == UPDATE_WORD,
            Ok(Entered::Eof | Entered::Refused) => false,
            Err(error) => {
                console(&format!("the update question failed: {error}"));
                false
            }
        };
        if !answer {
            return delete(&mut esp, machine, cryptsetup, console);
        }
        if let Err(error) = remove_retry(machine, cryptsetup, device, console) {
            return unremoved(console, error);
        }
        return match commit_pair(&mut esp, &read, console) {
            None => Outcome::Kept,
            Some(_) => {
                console(
                    "the selector update is committed without a predicted protector: restarting",
                );
                Outcome::Restart
            }
        };
    }
    let written = update_retry::token(volume_key, &uuid, &digests.kernel, &digests.initrd)
        .and_then(|token| {
            protected_plan::retry_write(&header, &token).map_err(|refusal| refusal.to_string())
        })
        .and_then(|step| protected::write_tokens(&header, device, &[step], cryptsetup, console));
    match written {
        Ok(()) => console(&format!(
            "the staged selector update is authentic but td could not predict its boot chain \
             ({why}); it waits for the next TPM PIN boot, which predicts it again"
        )),
        Err(error) => console(&format!(
            "the staged selector update waits; its update-retry token is not written: {error}"
        )),
    }
    Outcome::Kept
}

/// The installed selector's machine: the pinned partition, its disk's
/// ESP, securityfs's event log, the TPM's PCRs, `current` on the volume
/// and this selector's own files.
pub(crate) struct Selector<'a> {
    pub(crate) partition: &'a volume::Pinned,
    /// The volume UUID this selector boots, as its archive names it.
    pub(crate) uuid: String,
}

/// glibc's `makedev` split of a device number.
fn major_minor(rdev: u64) -> (u64, u64) {
    (
        ((rdev >> 8) & 0xfff) | ((rdev >> 32) & !0xfff),
        (rdev & 0xff) | ((rdev >> 12) & !0xff),
    )
}

fn sysfs(path: &Path) -> Result<String, String> {
    let bytes = td_fs::read_bounded_real_file(path, "sysfs attribute", MAX_SYSFS_BYTES)
        .map_err(|error| error.to_string())?;
    String::from_utf8(bytes).map_err(|_| format!("{} is not text", path.display()))
}

fn run(command: &mut Command, what: &str) -> Result<(), String> {
    crate::run_command(command, what).map_err(|error| error.to_string())
}

/// The sysfs directory of the whole disk `partition` is on.
fn disk_sysfs(partition: &volume::Pinned) -> Result<PathBuf, String> {
    let rdev = partition
        .file()
        .metadata()
        .map_err(|error| error.to_string())?
        .rdev();
    let (major, minor) = major_minor(rdev);
    let entry = fs::canonicalize(format!("/sys/dev/block/{major}:{minor}"))
        .map_err(|error| format!("the partition's sysfs entry: {error}"))?;
    if !entry.join("partition").exists() {
        return Err("the volume is not on a partition".into());
    }
    entry
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| "the partition has no disk".into())
}

/// The reencryption's resilience for the disk `partition` is on, by its
/// `queue/atomic_write_unit_max_bytes` (td-install/ENCRYPTION.md
/// "Re-encrypting upgrade"); a disk that states none, or whose entry
/// cannot be read, takes journal resilience.
pub(crate) fn resilience(partition: &volume::Pinned) -> Resilience {
    let unit = disk_sysfs(partition)
        .and_then(|disk| sysfs(&disk.join("queue/atomic_write_unit_max_bytes")))
        .ok()
        .and_then(|text| text.trim_end().parse::<u64>().ok());
    upgrade::resilience(unit)
}

/// Whether the machine is on mains power for the upgrade's start: each
/// `class` entry's `type`, and `online` reading `1`, through
/// `upgrade::on_mains`. An entry whose `type` cannot be read is no supply;
/// a supply whose `online` cannot be read is offline.
pub(crate) fn on_mains(class: &Path) -> bool {
    let mut supplies: Vec<(String, bool)> = Vec::new();
    if let Ok(entries) = fs::read_dir(class) {
        for entry in entries.flatten() {
            let Ok(dir) = fs::canonicalize(entry.path()) else {
                continue;
            };
            let Ok(kind) = sysfs(&dir.join("type")) else {
                continue;
            };
            let online = sysfs(&dir.join("online")).is_ok_and(|text| text.trim_end() == "1");
            supplies.push((kind.trim_end().to_owned(), online));
        }
    }
    let supplies: Vec<(&str, bool)> = supplies
        .iter()
        .map(|(kind, online)| (kind.as_str(), *online))
        .collect();
    upgrade::on_mains(&supplies)
}

impl Selector<'_> {
    /// The whole disk the pinned partition is on, opened read-write by its
    /// sysfs name and required to be that device, and its logical sector.
    fn disk(&self) -> Result<(File, u64), String> {
        let disk = disk_sysfs(self.partition)?;
        let disk = disk.as_path();
        let name = disk
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or("the disk has no name")?;
        let number = sysfs(&disk.join("dev"))?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(0o400000) // O_NOFOLLOW
            .open(Path::new("/dev").join(name))
            .map_err(|error| format!("/dev/{name}: {error}"))?;
        let metadata = file.metadata().map_err(|error| error.to_string())?;
        let (disk_major, disk_minor) = major_minor(metadata.rdev());
        if !metadata.file_type().is_block_device()
            || number.trim_end() != format!("{disk_major}:{disk_minor}")
        {
            return Err(format!("/dev/{name} is not the partition's disk"));
        }
        let sector = sysfs(&disk.join("queue/logical_block_size"))?
            .trim_end()
            .parse::<u64>()
            .map_err(|_| "the disk's logical sector size is not a number".to_string())?;
        Ok((file, sector))
    }
}

impl Machine for Selector<'_> {
    type Disk = File;

    fn esp(&mut self) -> Result<Esp<File>, String> {
        let (mut file, sector) = self.disk()?;
        if sector == 0 {
            return Err("the disk reports no logical sector size".into());
        }
        let bytes = file
            .seek(SeekFrom::End(0))
            .map_err(|error| error.to_string())?;
        let sectors = bytes / sector;
        let [primary, backup] = gpt::table_ranges(sector, sectors)?;
        let read = |file: &File, (offset, len): (u64, u64)| -> Result<Vec<u8>, String> {
            let mut bytes = vec![0u8; usize::try_from(len).map_err(|_| "a table overflows")?];
            std::os::unix::fs::FileExt::read_exact_at(file, &mut bytes, offset)
                .map_err(|error| error.to_string())?;
            Ok(bytes)
        };
        let primary = read(&file, primary)?;
        let backup = read(&file, backup)?;
        let (offset, len) = gpt::locate(
            &primary,
            &backup,
            sector,
            sectors,
            protocol::ESP_PARTITION_NAME,
            gpt::TYPE_ESP,
            true,
        )?
        .ok_or("the disk has no td-esp partition first")?;
        Esp::open(file, offset, len).map_err(|error| error.to_string())
    }

    fn predict(&mut self, running: &Chain, staged: &Chain) -> Result<Pcrs, String> {
        // Already mounted is no failure: the log's read says what matters.
        let _ = run(
            Command::new(crate::TD_MOUNT).stdin(Stdio::null()).args([
                "-t",
                "securityfs",
                "-o",
                "ro,nodev,nosuid,noexec",
                "securityfs",
                SECURITYFS,
            ]),
            "securityfs mount",
        );
        let log = eventlog::read_log(&Path::new("/").join(eventlog::LOG_PATH))
            .map_err(|refusal| refusal.to_string())?;
        let selection = td_tpm::PcrSelection::new((1 << 4) | (1 << 7) | (1 << 9))?;
        let values = td_protector::release::DeviceTpm
            .open()?
            .read_pcrs(selection)?;
        let [pcr4, pcr7, pcr9] = values.as_slice() else {
            return Err("the TPM read back other than PCRs 4, 7 and 9".into());
        };
        let read = Pcrs {
            pcr4: *pcr4,
            pcr7: *pcr7,
            pcr9: *pcr9,
        };
        eventlog::predict(&log, &read, running, staged).map_err(|refusal| refusal.to_string())
    }

    fn header(&mut self) -> Result<Header, String> {
        luks2::read(&mut self.partition.file())
    }

    fn current(&mut self, root: &Path) -> Result<Current, Unverified> {
        let refused = |why: String| Unverified::Refused(why);
        let id = crate::read_selector(root, "current")
            .map_err(|error| refused(format!("the volume's current deployment: {error}")))?;
        let (directory, manifest) = crate::verified_manifest(root, &id)
            .map_err(|error| refused(format!("the current deployment: {error}")))?;
        let kernel = parse_digest(&manifest.kernel)
            .ok_or_else(|| refused("the current manifest's kernel digest".into()))?;
        let erofs = crate::verify_payload(&directory, "root.erofs", &manifest.root)
            .map_err(|error| refused(format!("the current deployment: {error}")))?;
        let template = template(erofs)?;
        Ok(Current { kernel, template })
    }

    fn appendix(&mut self) -> Result<Vec<u8>, String> {
        let measured =
            crate::measurement::enabled(Path::new("/")).map_err(|error| error.to_string())?;
        selector_appendix::appendix(&self.uuid, measured)
    }
}

/// `current`'s root, hashed already, bound read-only on the loop device
/// with td-init's `losetup` and mounted read-only at `/sysroot` for its
/// selector template; then unmounted. The loop stays bound to the end of
/// the selector, whose kexec ends it: td-init has no detach.
fn template(erofs: File) -> Result<Vec<u8>, Unverified> {
    let unavailable = |why: String| Unverified::Unavailable(why);
    if fs::symlink_metadata(LOOP_DEVICE).is_err() {
        run(
            Command::new("/bin/mknod")
                .stdin(Stdio::null())
                .args([LOOP_DEVICE, "b", "7", "0"]),
            "loop device node",
        )
        .map_err(unavailable)?;
    }
    run(
        &mut crate::loop_command(erofs, Path::new(LOOP_DEVICE)),
        "read-only root loop",
    )
    .map_err(unavailable)?;
    run(
        Command::new(crate::TD_MOUNT).stdin(Stdio::null()).args([
            "-t",
            "erofs",
            "-o",
            "ro,nodev,nosuid,noexec",
            LOOP_DEVICE,
            TEMPLATE_ROOT,
        ]),
        "the current root's mount",
    )
    .map_err(unavailable)?;
    let read = td_fs::read_bounded_real_file(
        &Path::new(TEMPLATE_ROOT).join(protocol::SELECTOR_TEMPLATE_PATH),
        "selector template",
        MAX_FILE_BYTES as u64,
    )
    .map_err(|error| Unverified::Refused(format!("the current root's selector template: {error}")));
    let _ = run(
        Command::new(crate::TD_UMOUNT)
            .stdin(Stdio::null())
            .arg(TEMPLATE_ROOT),
        "the current root's unmount",
    );
    read
}

/// td-init's `reboot`, which syncs first. It returns only on failure.
pub(crate) fn restart() -> String {
    match Command::new(Path::new("/bin").join(protocol::REBOOT_APPLET))
        .stdin(Stdio::null())
        .status()
    {
        Ok(status) => format!("reboot exited with {status}"),
        Err(error) => format!("reboot could not run: {error}"),
    }
}

#[cfg(test)]
#[path = "selector_update_tests.rs"]
mod tests;

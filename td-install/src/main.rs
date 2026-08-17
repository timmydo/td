//! td-install lays out a td disk: a protective-MBR GPT carrying a FAT32 EFI
//! System Partition and the td volume td-boot selects from.
//!
//! One code path, two destinations — a block device or a regular file (D9).
//! The file case is not a convenience: it is what makes the installer testable
//! headlessly, and an installer whose tested path differs from its shipped one
//! is an installer nobody has tested. Neither the size nor the sector size is
//! assumed from which of the two it is; both are asked of the destination.
//!
//! `td-install/DESIGN.md` is the normative specification for this path.
#![forbid(unsafe_code)]

#[path = "../../td-boot/src/protocol.rs"]
#[allow(dead_code)]
mod protocol;
// The real-regular-bounded file rule, td-boot's and now shared rather than
// reimplemented here — DESIGN §10 item 10b. A rule spelled in both crates is
// one they can come to disagree about, and this one did, three ways, on the
// day the second copy was written.
#[path = "../../td-boot/src/realfile.rs"]
#[allow(dead_code)]
mod realfile;
// `gpt.rs` reaches its checksum as `crate::crc32`, the spelling that resolves
// identically inside the engine lib and here, so the two are declared as a PAIR.
#[path = "../../engine/src/crc32.rs"]
#[allow(dead_code)]
mod crc32;
#[path = "../../engine/src/gpt.rs"]
#[allow(dead_code)]
mod gpt;
#[path = "../../engine/src/fat.rs"]
#[allow(dead_code)]
mod fat;

use std::ffi::OsString;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

/// The sector size assumed for a regular file, and the smallest a disk may
/// report. A 4Kn device says so itself — see `logical_sector_size`.
const FILE_SECTOR_BYTES: u64 = 512;

fn invalid(message: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

const USAGE: &str = "usage: td-install layout <destination>\n       \
                     td-install volume <destination> <mkfs.btrfs> <scratch-dir> \
                     [<td-boot> <deployment> <trusted-key>]";

#[derive(Debug, Eq, PartialEq)]
enum Mode {
    Layout {
        destination: PathBuf,
    },
    /// `mkfs` is passed rather than looked up: this crate execs exactly what it
    /// is told to and never resolves a program through an ambient `PATH`, which
    /// is what makes the one third-party program on the install path (D7) a
    /// declared input of whoever calls it. `scratch` is the caller's too — the
    /// image needs room the size of the volume's real contents, and only the
    /// caller knows where there is any. With a publish that is the deployment
    /// TWICE over: td-boot copies the bundle into the staging tree and mkfs
    /// then copies the tree into the image, so a scratch sized from the volume
    /// alone runs out inside mkfs rather than here.
    Volume {
        destination: PathBuf,
        mkfs: PathBuf,
        scratch: PathBuf,
        /// The deployment to publish into the volume as it is made, if any.
        ///
        /// All THREE together or none: the publish is `td-boot`'s (D1), it
        /// needs a bundle, and the key is what makes it check that bundle
        /// rather than take it on trust — so a caller that named two of them
        /// asked for something this cannot do, and defaulting the third is
        /// how a fail-open gets in. An install with nothing to publish is
        /// this command without them.
        publish: Option<Publish>,
    },
}

#[derive(Debug, Eq, PartialEq)]
struct Publish {
    /// Where `td-boot` is. Passed, never resolved: this crate execs what it is
    /// told to, as it does for `mkfs.btrfs`.
    td_boot: PathBuf,
    deployment: PathBuf,
    /// Named EXPLICITLY rather than left to td-boot's probe, and DESIGN §10
    /// item 7c says why: absence is what the probe reads as "no trust root",
    /// and absence is indistinguishable from a key provisioned under the wrong
    /// name or behind a dangling symlink. Naming it means a wrong path is an
    /// error instead of an unverified publish.
    trusted_key: PathBuf,
}

fn parse_args(mut args: impl Iterator<Item = OsString>) -> io::Result<Mode> {
    let verb = args.next().ok_or_else(|| invalid(USAGE.to_string()))?;
    let rest: Vec<PathBuf> = args.map(PathBuf::from).collect();
    match (verb.to_str(), rest.as_slice()) {
        (Some("layout"), [destination]) => Ok(Mode::Layout {
            destination: destination.clone(),
        }),
        (Some("volume"), [destination, mkfs, scratch, td_boot, deployment, trusted_key]) => {
            Ok(Mode::Volume {
                destination: destination.clone(),
                mkfs: mkfs.clone(),
                scratch: scratch.clone(),
                publish: Some(Publish {
                    td_boot: td_boot.clone(),
                    deployment: deployment.clone(),
                    trusted_key: trusted_key.clone(),
                }),
            })
        }
        (Some("volume"), [destination, mkfs, scratch]) => Ok(Mode::Volume {
            destination: destination.clone(),
            mkfs: mkfs.clone(),
            scratch: scratch.clone(),
            publish: None,
        }),
        _ => Err(invalid(USAGE.to_string())),
    }
}

/// The destination's size in bytes, asked of the destination itself.
///
/// `seek` to the end rather than `metadata().len()`, which reports 0 for a
/// block device: seeking answers for both destinations, in safe `std`, and D8
/// keeps this crate's syscall surface empty — a `BLKGETSIZE64` ioctl would be
/// an amendment to `UNSAFE.md` for something ordinary file I/O already does.
fn destination_bytes(file: &mut File) -> io::Result<u64> {
    let size = file.seek(SeekFrom::End(0))?;
    file.rewind()?;
    Ok(size)
}

/// Split a 64-bit `st_rdev` into its major and minor numbers.
///
/// glibc's `gnu_dev_major`/`gnu_dev_minor` in `<sys/sysmacros.h>`, which is the
/// encoding the kernel writes and `/sys/dev/block/<major>:<minor>/` is named
/// for: minor is bits 0..=7 and 20..=43, major is bits 8..=19 and 44..=63, so
/// the two INTERLEAVE and neither is a contiguous field.
///
/// Each half is masked to its own width rather than by clearing the other's low
/// bits, which is the mistake this had: shifting the extended minor down puts
/// the extended MAJOR just above it, and a mask that only clears the bottom
/// leaves it there. Dormant while block majors stay under 4096 — they all do
/// today — and a sysfs path naming a device that does not exist when one does
/// not, which reads as "cannot read", not as a wrong sector size.
fn device_numbers(rdev: u64) -> (u64, u64) {
    let major = ((rdev >> 8) & 0xfff) | ((rdev >> 32) & 0xffff_f000);
    let minor = (rdev & 0xff) | ((rdev >> 12) & 0xffff_ff00);
    (major, minor)
}

/// The destination's logical sector size.
///
/// A regular file has none, so it takes `FILE_SECTOR_BYTES`. A block device is
/// asked through sysfs, by the device NUMBER off the opened file rather than by
/// its path — the same argument that makes `td-init`'s `losetup` read its
/// read-only flag out of `/sys/dev/block/<major>:<minor>/`: a path can name a
/// different device than the one the descriptor is open on. Getting this wrong
/// on a 4Kn disk writes a table whose every LBA is off by a factor of eight,
/// which firmware reads as no table at all.
fn logical_sector_size(file: &File) -> io::Result<u64> {
    use std::os::linux::fs::MetadataExt;
    use std::os::unix::fs::FileTypeExt;

    let metadata = file.metadata()?;
    if metadata.is_file() {
        return Ok(FILE_SECTOR_BYTES);
    }
    if !metadata.file_type().is_block_device() {
        return Err(invalid(
            "a destination must be a regular file or a block device".to_string(),
        ));
    }
    let (major, minor) = device_numbers(metadata.st_rdev());
    let path = PathBuf::from(format!(
        "/sys/dev/block/{major}:{minor}/queue/logical_block_size"
    ));
    let text = paths::read_to_string(&path)?;
    let trimmed = text.trim();
    let size = trimmed.parse::<u64>().map_err(|_| {
        invalid(format!(
            "{} reads {trimmed:?}, which is not a sector size",
            path.display()
        ))
    })?;
    // Refused HERE, where the number arrives from outside, rather than at each
    // division downstream. A device with no media reports 0 bytes and can read 0
    // here, and `0.is_multiple_of(0)` is TRUE — so a sector check written the
    // obvious way passes and the division after it aborts the process, this
    // crate being `panic = "abort"`.
    if size == 0 {
        return Err(invalid(format!(
            "{} reads a sector size of 0",
            path.display()
        )));
    }
    Ok(size)
}

/// Where the two partitions go, in sectors.
#[derive(Debug, Eq, PartialEq)]
struct Plan {
    sector_size: u64,
    /// Partition alignment in SECTORS, computed and validated once. Carried
    /// rather than recomputed at the call site for the reason the layout
    /// constants live in `protocol.rs`: two expressions of one value can
    /// disagree.
    align_sectors: u64,
    disk_sectors: u64,
    esp_start: u64,
    esp_end: u64,
    volume_start: u64,
    volume_end: u64,
}

impl Plan {
    fn esp_offset(&self) -> Option<u64> {
        self.esp_start.checked_mul(self.sector_size)
    }

    fn esp_sectors(&self) -> Option<u64> {
        self.esp_end.checked_sub(self.esp_start)?.checked_add(1)
    }
}

/// Round `sectors` up to the next multiple of the alignment.
fn align_up(sectors: u64, align: u64) -> Option<u64> {
    if align == 0 {
        return None;
    }
    let remainder = sectors % align;
    if remainder == 0 {
        return Some(sectors);
    }
    sectors.checked_add(align - remainder)
}

/// Compute the layout, or say why the disk cannot hold one.
///
/// Every partition boundary is INCLUSIVE, because that is how GPT stores it and
/// an exclusive end written into that field is an off-by-one no reader detects.
fn plan(sector_size: u64, disk_bytes: u64) -> Result<Plan, String> {
    if sector_size == 0 {
        return Err("the destination reports a sector size of 0".to_string());
    }
    if !disk_bytes.is_multiple_of(sector_size) {
        return Err(format!(
            "destination is {disk_bytes} bytes, not a whole number of \
             {sector_size}-byte sectors"
        ));
    }
    let disk_sectors = disk_bytes / sector_size;
    let minimum = gpt::minimum_disk_sectors(sector_size)?;
    if disk_sectors < minimum {
        return Err(format!(
            "destination holds {disk_sectors} sectors, and a GPT alone \
             needs {minimum}"
        ));
    }
    let align = protocol::PARTITION_ALIGN_BYTES / sector_size;
    if align == 0 {
        return Err(format!(
            "a {sector_size}-byte sector is larger than the \
             {}-byte partition alignment",
            protocol::PARTITION_ALIGN_BYTES
        ));
    }
    let first_usable = gpt::first_usable_lba(sector_size)?;
    let last_usable = gpt::last_usable_lba(sector_size, disk_sectors)?;

    let esp_start = align_up(first_usable, align)
        .ok_or_else(|| "aligning the ESP start overflowed".to_string())?;
    let esp_sectors = protocol::ESP_BYTES / sector_size;
    let esp_end = esp_start
        .checked_add(esp_sectors)
        .and_then(|end| end.checked_sub(1))
        .ok_or_else(|| "the ESP does not fit in an LBA".to_string())?;

    let volume_start = align_up(
        esp_end
            .checked_add(1)
            .ok_or_else(|| "the volume start overflowed".to_string())?,
        align,
    )
    .ok_or_else(|| "aligning the volume start overflowed".to_string())?;
    if volume_start > last_usable {
        return Err(format!(
            "destination is too small — the ESP alone reaches LBA \
             {esp_end} and the last usable LBA is {last_usable}"
        ));
    }
    let volume_sectors = last_usable
        .checked_sub(volume_start)
        .and_then(|s| s.checked_add(1))
        .unwrap_or(0);
    let volume_bytes = volume_sectors.saturating_mul(sector_size);
    if volume_bytes < protocol::MIN_VOLUME_BYTES {
        return Err(format!(
            "the td volume would be {volume_bytes} bytes and needs at \
             least {} — a disk this size cannot hold two deployments",
            protocol::MIN_VOLUME_BYTES
        ));
    }
    Ok(Plan {
        sector_size,
        align_sectors: align,
        disk_sectors,
        esp_start,
        esp_end,
        volume_start,
        volume_end: last_usable,
    })
}

/// 16 bytes from `/dev/urandom`, as an RFC 4122 version-4 GUID.
///
/// A disk and its partitions are identified by these, so they are per-install
/// rather than derived from anything: two disks laid out by the same build must
/// not claim the same GUID, or firmware and udev each pick one of them.
/// `/dev/urandom` is an ordinary file, which is what keeps D8 intact.
fn random_guid() -> io::Result<gpt::Guid> {
    let mut bytes = [0u8; 16];
    let urandom = Path::new("/dev/urandom");
    let mut file = paths::open_read(urandom)?;
    file.read_exact(&mut bytes)?;
    // Version 4 and the RFC 4122 variant, in the on-disk mixed-endian layout:
    // the version nibble is the high nibble of byte 7's field, which little-endian
    // encoding of the third group puts at index 7, and the variant at index 8.
    if let Some(byte) = bytes.get_mut(7) {
        *byte = (*byte & 0x0f) | 0x40;
    }
    if let Some(byte) = bytes.get_mut(8) {
        *byte = (*byte & 0x3f) | 0x80;
    }
    Ok(gpt::Guid(bytes))
}

/// EVERY filesystem call this program makes, and the only place a path is
/// paired with the error that names it.
///
/// `io::Error` carries an errno and nothing else, so a destination that is not
/// there refuses with a bare `No such file or directory` on a command line
/// that names up to five paths — and an operator has no way to tell which one
/// it meant. Same argument `td-boot`'s `read_trusted_key` makes, on the other
/// half of the deployment path.
///
/// The property is STRUCTURAL rather than checked. Each wrapper takes its path
/// as a parameter and holds no other, so `.at()` cannot be handed a file the
/// operation never touched: there is no second path in scope to hand it. What
/// the test outside enforces is only that nothing else in the crate opens
/// anything, which is a question about where a call IS rather than about what
/// it means — and four rounds of review are why it is put that way round.
///
/// Scoped to operations that take a PATH. A call on an already-open `File` — a
/// write, a `sync_all`, a `set_len` — is deliberately not here: it is about a
/// descriptor rather than a name, and the name it would be given is the one
/// the open already reported.
///
/// The wrap costs `raw_os_error()` and any `source()` chain, neither of which
/// `io::Error::new` can carry. `kind()` survives, which is what the callers'
/// `!= NotFound` tests read.
#[allow(clippy::disallowed_methods)]
mod paths {
    use std::fs::{DirBuilder, File, Metadata, OpenOptions, Permissions};
    use std::io;
    use std::path::{Path, PathBuf};

    /// Name the file an IO failure was about.
    fn named(error: io::Error, path: &Path) -> io::Error {
        io::Error::new(error.kind(), format!("{}: {error}", path.display()))
    }

    /// `named` where the result is being propagated, which is most of them.
    trait NamePath<T> {
        fn at(self, path: &Path) -> io::Result<T>;
    }

    impl<T> NamePath<T> for io::Result<T> {
        fn at(self, path: &Path) -> io::Result<T> {
            self.map_err(|error| named(error, path))
        }
    }

    pub fn open_read(path: &Path) -> io::Result<File> {
        File::open(path).at(path)
    }

    pub fn open_read_write(path: &Path) -> io::Result<File> {
        OpenOptions::new().read(true).write(true).open(path).at(path)
    }

    /// Create, refusing anything already there — which is what keeps a symlink
    /// left at the path from being followed and truncated.
    pub fn create_new(path: &Path) -> io::Result<File> {
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .at(path)
    }

    /// The same, at a chosen creation mode.
    pub fn create_new_with_mode(path: &Path, mode: u32) -> io::Result<File> {
        use std::os::unix::fs::OpenOptionsExt;
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .open(path)
            .at(path)
    }

    pub fn create_dir_all(path: &Path) -> io::Result<()> {
        std::fs::create_dir_all(path).at(path)
    }

    /// A directory created AT `mode` rather than widened to it, so there is no
    /// window in which it is more permissive than asked for.
    pub fn create_dir_with_mode(path: &Path, mode: u32) -> io::Result<()> {
        use std::os::unix::fs::DirBuilderExt;
        DirBuilder::new().mode(mode).create(path).at(path)
    }

    pub fn set_mode(path: &Path, mode: u32) -> io::Result<()> {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, Permissions::from_mode(mode)).at(path)
    }

    pub fn canonicalize(path: &Path) -> io::Result<PathBuf> {
        std::fs::canonicalize(path).at(path)
    }

    pub fn symlink_metadata(path: &Path) -> io::Result<Metadata> {
        std::fs::symlink_metadata(path).at(path)
    }

    /// What is at `path`, or nothing — the one call here whose error is
    /// DISCARDED, because the caller asks only whether two files are the same
    /// and an unreadable path is not one of them.
    pub fn metadata_if_present(path: &Path) -> Option<Metadata> {
        std::fs::metadata(path).ok()
    }

    /// Whether `path` is a directory, and an ERROR rather than `false` where
    /// the question cannot be answered.
    ///
    /// `Path::is_dir` is the obvious spelling and is wrong twice over: it is a
    /// filesystem call outside this module, and it reports a directory it was
    /// REFUSED as one that is not there. Its caller turns that answer into
    /// "td-boot published nothing", which would be a false accusation against
    /// the program that had just done the work.
    pub fn is_dir(path: &Path) -> io::Result<bool> {
        match std::fs::metadata(path) {
            Ok(metadata) => Ok(metadata.is_dir()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(named(error, path)),
        }
    }

    /// A rename names BOTH paths: either can be the one at fault, and the
    /// errno alone does not say which.
    pub fn rename(from: &Path, to: &Path) -> io::Result<()> {
        std::fs::rename(from, to).map_err(|error| {
            io::Error::new(
                error.kind(),
                format!(
                    "cannot rename {} to {}: {error}",
                    from.display(),
                    to.display()
                ),
            )
        })
    }

    /// A whole small file, named as a READ failure so it cannot be mistaken
    /// for the parse that follows it — `device_numbers` turns on the
    /// difference between a sysfs path that is not there and a sector size
    /// that does not parse.
    pub fn read_to_string(path: &Path) -> io::Result<String> {
        std::fs::read_to_string(path).map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("cannot read {}: {error}", path.display()),
            )
        })
    }

    /// Absent is the state wanted, so an absent path is not a failure.
    pub fn remove_file_if_present(path: &Path) -> io::Result<()> {
        match std::fs::remove_file(path) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => Err(named(error, path)),
            _ => Ok(()),
        }
    }

    /// The same for a tree, which is how both verbs empty a staging root.
    pub fn remove_dir_all_if_present(path: &Path) -> io::Result<()> {
        match std::fs::remove_dir_all(path) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => Err(named(error, path)),
            _ => Ok(()),
        }
    }
}

/// Write `bytes` at `offset`, seeking first.
fn write_at(file: &mut File, offset: u64, bytes: &[u8]) -> io::Result<()> {
    file.seek(SeekFrom::Start(offset))?;
    file.write_all(bytes)
}

/// Zero `len` bytes at `offset`, in bounded chunks.
fn zero_at(file: &mut File, offset: u64, len: u64) -> io::Result<()> {
    const CHUNK: usize = 1024 * 1024;
    let span = usize::try_from(len).unwrap_or(CHUNK).min(CHUNK);
    let zeros = vec![0u8; span];
    file.seek(SeekFrom::Start(offset))?;
    let mut remaining = len;
    while remaining > 0 {
        let take = usize::try_from(remaining.min(span as u64)).unwrap_or(span);
        let chunk = zeros
            .get(..take)
            .ok_or_else(|| invalid("zero chunk out of range".to_string()))?;
        file.write_all(chunk)?;
        remaining -= take as u64;
    }
    Ok(())
}

/// How much of the ESP must be zeroed before `fat::build`'s extents land on it.
///
/// `fat.rs` states the precondition and cannot check it: it emits only what must
/// be non-zero, so the FAT is written as a live PREFIX and everything past it is
/// whatever the destination already held. Over a device with a previous
/// filesystem on it those bytes read as ALLOCATED clusters — lost chains, a free
/// count that disagrees with the table, and a later write handing out a cluster
/// that already holds something.
///
/// Zeroing the whole ESP would satisfy it and cost half a gigabyte of writes on
/// every install. The METADATA region is enough: reserved sectors, both FATs,
/// and the root directory's cluster. Past that the FATs read as all clusters
/// free, so no directory entry and no chain reaches the stale data — and the
/// first write to a cluster overwrites what was there.
///
/// The root cluster is in the region even though `fat::build` currently emits it
/// whole, so this does not depend on it continuing to: one cluster of zeroing is
/// cheaper than a precondition that holds only while an extent happens to cover
/// it.
fn metadata_bytes(image: &fat::Image) -> Option<u64> {
    let sector = u64::from(image.bytes_per_sector);
    let reserved = u64::from(fat::RESERVED_SECTORS);
    let fats = u64::from(fat::NUM_FATS).checked_mul(u64::from(image.sectors_per_fat))?;
    let root = u64::from(image.sectors_per_cluster);
    reserved
        .checked_add(fats)?
        .checked_add(root)?
        .checked_mul(sector)
}

/// Zero whatever table the destination already carries.
///
/// Over exactly the two ranges the new one will occupy, which is what makes
/// this complete without reading the old table: a GPT's primary and backup live
/// at fixed positions for a given disk size and sector size, so the ranges are
/// the same ones regardless of what wrote them. The protective MBR is inside
/// the primary range and goes with it — a disk carrying that and no header is
/// one every tool reads as unpartitioned.
fn invalidate_table(file: &mut File, table: &gpt::Image) -> io::Result<()> {
    for (offset, len) in [
        (table.primary_offset, table.primary.len()),
        (table.backup_offset, table.backup.len()),
    ] {
        zero_at(file, offset, len as u64)?;
    }
    Ok(())
}

fn run_layout(destination: &Path, out: &mut dyn Write) -> io::Result<()> {
    let mut file = paths::open_read_write(destination)?;
    let disk_bytes = destination_bytes(&mut file)?;
    let sector_size = logical_sector_size(&file)?;
    let plan = plan(sector_size, disk_bytes).map_err(invalid)?;

    let layout = gpt::Layout {
        sector_size,
        disk_sectors: plan.disk_sectors,
        disk_guid: random_guid()?,
        align_sectors: plan.align_sectors,
        partitions: vec![
            gpt::Partition {
                type_guid: gpt::TYPE_ESP,
                unique_guid: random_guid()?,
                start_lba: plan.esp_start,
                end_lba: plan.esp_end,
                attributes: 0,
                name: protocol::ESP_PARTITION_NAME.to_string(),
            },
            gpt::Partition {
                type_guid: gpt::TYPE_LINUX_FS,
                unique_guid: random_guid()?,
                start_lba: plan.volume_start,
                end_lba: plan.volume_end,
                attributes: 0,
                name: protocol::VOLUME_PARTITION_NAME.to_string(),
            },
        ],
    };
    let table = gpt::build(&layout).map_err(invalid)?;

    let esp_offset = plan
        .esp_offset()
        .ok_or_else(|| invalid("the ESP offset overflowed".to_string()))?;
    let esp_sectors = plan
        .esp_sectors()
        .ok_or_else(|| invalid("the ESP length overflowed".to_string()))?;
    let esp_start_lba = u32::try_from(plan.esp_start).map_err(|_| {
        invalid("the ESP starts past what a FAT32 BPB can record".to_string())
    })?;
    let volume = fat::Volume {
        bytes_per_sector: u32::try_from(sector_size)
            .map_err(|_| invalid("sector size exceeds a FAT32 BPB".to_string()))?,
        total_sectors: esp_sectors,
        hidden_sectors: esp_start_lba,
        // Derived from the ESP's own GUID rather than from a clock, so the same
        // disk laid out twice differs only where GPT already says it does.
        volume_id: volume_serial(&layout)?,
        label: protocol::ESP_VOLUME_LABEL.to_string(),
        sectors_per_cluster: None,
        root: Vec::new(),
    };
    let esp = fat::build(&volume).map_err(invalid)?;
    let metadata = metadata_bytes(&esp)
        .ok_or_else(|| invalid("the ESP metadata region overflowed".to_string()))?;
    // The region is exactly tight — today's last extent ENDS on it — so this is
    // the invariant the comment on `metadata_bytes` is really claiming, checked
    // rather than reasoned. A root directory needing a second cluster (item 8
    // puts `\EFI\BOOT\BOOTX64.EFI` on this volume) moves that end past the
    // zeroed region, and the result would be a volume with lost chains that
    // nothing reports. Failing the install is the right answer to that.
    let written = esp
        .extents
        .iter()
        .try_fold(0u64, |high, extent| {
            let end = extent.offset.checked_add(extent.bytes.len() as u64)?;
            Some(high.max(end))
        })
        .ok_or_else(|| invalid("an ESP extent overflowed".to_string()))?;
    if written > metadata {
        return Err(invalid(format!(
            "the ESP zeroes {metadata} bytes but fat::build writes up \
             to {written} — the zeroed region no longer covers the metadata it \
             must (see metadata_bytes)"
        )));
    }

    // A REINSTALL is the case this order exists for. On a disk that already
    // carries a table, that table stays valid while the ESP beneath it is being
    // rewritten, so an install that dies part way leaves a table pointing at a
    // filesystem that is half replaced — which is worse than no table, because
    // firmware will try it. So the old table goes FIRST and the disk spends the
    // install carrying none.
    //
    // Each stage is flushed before the next. Nothing else orders one write
    // against another across a power cut: without the barriers the table can
    // reach the platter before the filesystem it describes. The primary table
    // is written LAST because it is the commit point — it is what firmware
    // reads first, and it is only correct once everything it points at is
    // durable.
    invalidate_table(&mut file, &table)?;
    file.sync_all()?;

    zero_at(&mut file, esp_offset, metadata)?;
    for extent in &esp.extents {
        let at = esp_offset
            .checked_add(extent.offset)
            .ok_or_else(|| invalid("an ESP extent overflowed".to_string()))?;
        write_at(&mut file, at, &extent.bytes)?;
    }
    file.sync_all()?;

    write_at(&mut file, table.backup_offset, &table.backup)?;
    file.sync_all()?;
    write_at(&mut file, table.primary_offset, &table.primary)?;
    file.sync_all()?;

    // NUMBERS ONLY, whitespace-separated, and every one a BYTE OFFSET. The
    // destination is deliberately not echoed back: a caller already knows what
    // it passed, and a path is the one field here that can contain a space —
    // which shifts every field a caller reads by position — or a newline, which
    // would break the one-line promise outright. Nothing that can carry either
    // goes on this channel.
    //
    // Bytes rather than the LBAs this function works in, because `volume`
    // reports bytes and two verbs of one program reporting the same-shaped line
    // in different units is a caller reading 2048 where the ESP is at 1048576 —
    // with nothing on either line to say which it got.
    let esp = plan
        .esp_offset()
        .ok_or_else(|| invalid("the ESP offset overflowed".to_string()))?;
    let volume = plan
        .volume_start
        .checked_mul(plan.sector_size)
        .ok_or_else(|| invalid("the volume offset overflowed".to_string()))?;
    writeln!(out, "{esp} {volume}")
}

/// The two byte ranges a table occupies, as `(offset, len)` pairs.
///
/// The same positions `gpt::build` writes to, derived rather than remembered:
/// the primary runs from LBA 0 through the end of its entry array (which is
/// `first_usable_lba`), and the backup is the entry array plus the header on
/// the LAST sector.
fn table_ranges(sector_size: u64, disk_sectors: u64) -> Result<[(u64, u64); 2], String> {
    let entries = gpt::entry_array_sectors(sector_size)?;
    let primary_sectors = gpt::first_usable_lba(sector_size)?;
    let backup_sectors = entries
        .checked_add(1)
        .ok_or_else(|| "the backup table length overflowed".to_string())?;
    let backup_start = disk_sectors
        .checked_sub(backup_sectors)
        .ok_or_else(|| "the disk is too small to hold a backup table".to_string())?;
    let bytes = |sectors: u64| {
        sectors
            .checked_mul(sector_size)
            .ok_or_else(|| "a table range overflowed".to_string())
    };
    Ok([
        (0, bytes(primary_sectors)?),
        (bytes(backup_start)?, bytes(backup_sectors)?),
    ])
}

fn read_at(file: &mut File, offset: u64, len: u64) -> io::Result<Vec<u8>> {
    let len = usize::try_from(len)
        .map_err(|_| invalid("a table range exceeds this address space".to_string()))?;
    let mut bytes = vec![0u8; len];
    file.seek(SeekFrom::Start(offset))?;
    file.read_exact(&mut bytes)?;
    Ok(bytes)
}

/// Where the td volume is, read back off the disk rather than recomputed.
///
/// `plan()` would answer the same for a disk this installer laid out, and that
/// is exactly why it is not asked: the partition the filesystem goes in must be
/// the one the TABLE describes, or a `plan` that changed between the layout and
/// the volume would write a filesystem outside its own partition. So the table
/// is parsed — which also refuses a destination that was never laid out, and a
/// disk whose two copies of the table disagree.
fn volume_region(file: &mut File, sector_size: u64, disk_sectors: u64) -> io::Result<(u64, u64)> {
    let [primary, backup] = table_ranges(sector_size, disk_sectors).map_err(invalid)?;
    let primary = read_at(file, primary.0, primary.1)?;
    let backup = read_at(file, backup.0, backup.1)?;
    let table = gpt::parse(&primary, &backup, sector_size).map_err(invalid)?;
    // The table must be a table OF THIS DISK. `gpt::parse` is handed two byte
    // slices and never learns where they came from, so it cannot tell; here the
    // real count is known, and a header describing a different-sized disk is a
    // table that was copied from one rather than written on this one.
    if table.disk_sectors != disk_sectors {
        return Err(invalid(format!(
            "the table describes a {}-sector disk, not the {disk_sectors} sectors \
             this destination has",
            table.disk_sectors
        )));
    }
    // A loop rather than the searching iterator adaptor, whose name this file
    // may not spell: it is staged into a recipe as a `WriteFile` body, and the
    // ladder's host-tool guard tokenises those bodies and reads that name as an
    // invocation of the GNU tool it shares
    // (`no_bootstrap_step_invokes_host_find_or_xargs`).
    // A NAME is not an identity: GPT does not require partition names to be
    // unique, so a table carrying two of them is one this program cannot choose
    // between and must not guess at — the wrong choice formats a partition
    // somebody else's data is in. The TYPE is checked with it for the same
    // reason, since a name is 36 characters anyone can write and the type GUID
    // is what says what the partition is FOR.
    let mut found = None;
    let mut matches = 0usize;
    for part in &table.partitions {
        if part.name == protocol::VOLUME_PARTITION_NAME && part.type_guid == gpt::TYPE_LINUX_FS {
            matches += 1;
            if found.is_none() {
                found = Some(part);
            }
        }
    }
    if matches > 1 {
        return Err(invalid(format!(
            "this disk has {matches} partitions named {}",
            protocol::VOLUME_PARTITION_NAME
        )));
    }
    let part = found.ok_or_else(|| {
        invalid(format!(
            "no {} partition on this disk — run `layout` first",
            protocol::VOLUME_PARTITION_NAME
        ))
    })?;
    let offset = part
        .start_lba
        .checked_mul(sector_size)
        .ok_or_else(|| invalid("the volume offset overflowed".to_string()))?;
    // INCLUSIVE end, as GPT stores it.
    let len = part
        .end_lba
        .checked_sub(part.start_lba)
        .and_then(|span| span.checked_add(1))
        .and_then(|sectors| sectors.checked_mul(sector_size))
        .ok_or_else(|| invalid("the volume length overflowed".to_string()))?;
    // ...and the region must not overlap either copy of the TABLE that named
    // it. `gpt::parse` bounds partitions by the header's OWN `first_usable` and
    // `last_usable`, which are fields in the same table — so a table declaring
    // a usable range over its own entry array is self-consistent, and this is
    // the only place the REAL positions are known. A volume overlapping them is
    // an install that destroys the table on its way to using it.
    let [primary, backup] = table_ranges(sector_size, disk_sectors).map_err(invalid)?;
    let primary_end = primary.0.saturating_add(primary.1);
    let end = offset.saturating_add(len);
    if offset < primary_end || end > backup.0 {
        return Err(invalid(format!(
            "the volume at {offset}..{end} overlaps a partition table \
             ({}..{primary_end} and {}..)",
            primary.0, backup.0
        )));
    }
    Ok((offset, len))
}

/// The unit both the sparse copy and the edge zeroing work in. Named once
/// because `run_volume` orders the copy by it: the chunk it defers has to be
/// the one holding the superblock, and two spellings of a megabyte could
/// disagree about which that is.
const COPY_CHUNK: u64 = 1024 * 1024;

/// Copy the parts of `image` that are not all zero to `offset` in `file`.
///
/// A freshly made Btrfs is nearly all hole, so copying the holes would be a
/// write of the whole volume — on a 100 GB partition, minutes of writes to say
/// nothing. Reading them costs no I/O: a hole reads from the zero page.
///
/// What that skip gives up is stated in DESIGN §10 item 7 and is why the
/// caller zeroes the region's first bytes: a chunk the image leaves as a hole
/// is a chunk the destination KEEPS, so a signature mkfs erased by writing
/// zeros would survive here, being indistinguishable from the holes around it.
///
/// Returns the bytes actually written, which the caller reports — an install
/// that copied nothing is one whose mkfs wrote nothing.
///
/// `from`..`to` is a range WITHIN the image, so the caller can order the copy;
/// see `run_volume` for why the first chunk goes last.
fn copy_sparse(
    image: &mut File,
    file: &mut File,
    offset: u64,
    from: u64,
    to: u64,
) -> io::Result<u64> {
    let span = usize::try_from(COPY_CHUNK).unwrap_or(1).max(1);
    let mut buffer = vec![0u8; span];
    let mut at = from;
    let mut written = 0u64;
    image.seek(SeekFrom::Start(from))?;
    while at < to {
        let take = usize::try_from(to.saturating_sub(at).min(span as u64)).unwrap_or(span);
        let chunk = buffer
            .get_mut(..take)
            .ok_or_else(|| invalid("copy chunk out of range".to_string()))?;
        image.read_exact(chunk)?;
        if chunk.iter().any(|byte| *byte != 0) {
            let dest = offset
                .checked_add(at)
                .ok_or_else(|| invalid("a copy offset overflowed".to_string()))?;
            write_at(file, dest, chunk)?;
            written = written.saturating_add(take as u64);
        }
        at = at.saturating_add(take as u64);
    }
    Ok(written)
}

/// Zero both ENDS of the region before the copy lands on it.
///
/// This is the whole of what the sparse copy gives up. mkfs erases a previous
/// filesystem's signature by WRITING ZEROS, and zeros in a fresh sparse image
/// are holes the copy skips — so a signature the new filesystem believes it
/// erased survives underneath it, and a prober that finds two says the disk is
/// ambiguous or, worse, assembles the older one.
///
/// BOTH ends, because "the first megabyte covers every signature" is false: XFS
/// at 0, ext* at 1 KiB and Btrfs at 64 KiB are all at the front, but MD RAID
/// 0.90 and 1.0 metadata and ZFS's L2/L3 labels sit in the LAST few hundred
/// kilobytes of the device. An alignment's worth at each end covers both sets,
/// and costs two megabytes against a partition measured in gigabytes.
///
/// Btrfs's own superblock mirrors need no such care: they are at fixed offsets
/// and the new mkfs writes every one this volume is large enough to hold.
fn zero_edges(file: &mut File, offset: u64, len: u64) -> io::Result<()> {
    let edge = protocol::PARTITION_ALIGN_BYTES.min(len);
    // A region too small to hold two disjoint edges is zeroed once, whole,
    // rather than twice over its own middle.
    if len <= edge.saturating_mul(2) {
        return zero_at(file, offset, len);
    }
    zero_at(file, offset, edge)?;
    let tail = offset
        .checked_add(len)
        .and_then(|end| end.checked_sub(edge))
        .ok_or_else(|| invalid("the volume tail overflowed".to_string()))?;
    zero_at(file, tail, edge)
}

/// Read the trusted key under td-boot's rule, which is now literally
/// td-boot's: `realfile.rs` is one implementation both crates include.
///
/// Applied here rather than left to td-boot because this program SNAPSHOTS
/// the key and hands td-boot the copy — a copy is a small regular file
/// whatever the original was, so without this the snapshot would launder a
/// key past every refusal the real reader makes.
fn read_trusted_key(path: &Path) -> io::Result<Vec<u8>> {
    realfile::read_bounded_real_file(path, "trusted deployment key", protocol::MAX_PUBLIC_KEY_BYTES)
}

/// Publish `deployment` into the staging tree, through `td-boot`.
///
/// D1: this crate does not learn to write a deployment directory, update a
/// selector, or account for attempts — it hands the whole transaction to the
/// one writer. What it does here is make the directories that writer requires,
/// which are the LAYOUT rather than the transaction. Everything this function
/// shares with `td-boot` — the two nested directory names, the VERB, and the
/// shape of a deployment id — is `protocol.rs`'s, for that file's own stated
/// reason: a thing spelled in both crates is a thing they can come to disagree
/// about, at the first boot after an install rather than at build time.
///
/// The child's stdout is the deployment id, and it is replayed on OUR stderr
/// for `mkfs.btrfs`'s reason: this program's stdout is a machine-readable line
/// of byte offsets, and an id is neither a byte offset nor something a caller
/// reading by position expects to see there. It is also READ, which is the
/// whole of what stops a successful exit standing in for a publish — see below.
fn publish_into(staging: &Path, publish: &Publish, key: &[u8]) -> io::Result<()> {
    // The four `install_deployment` requires, in its order and its spelling —
    // `td` is a literal there too, and the two nested constants make it
    // redundant only for as long as they stay under it. Mirroring the check
    // rather than deriving from it is what makes a moved constant a missing
    // directory td-boot names, instead of one this loop silently stopped
    // creating.
    //
    // MODE PINNED rather than left to the ambient umask, because `--rootdir` copies a
    // staging directory's mode into the filesystem verbatim: these are baked
    // onto a machine's disk, not scratch. Under `umask 000` the selector
    // directory — which holds the `current`/`previous` symlinks the boot path
    // follows — shipped as 0777, world-writable on the installed system, and
    // nothing downstream pins it: td-boot pins the mode of everything it
    // writes ITSELF, and `require_real_directory` asks only whether these are
    // directories. It also makes two installs of the same inputs produce the
    // same image, which is the oracle's comparison.
    //
    // What this does NOT fix is OWNERSHIP: the tree is owned by whoever ran
    // the installer, and changing that needs `chown`, which is a syscall this
    // crate deliberately does not have (DESIGN D8). An installer runs as root
    // on the path that matters, where the answer is already right.
    // SET rather than passed to `mkdir`, because a creation mode is masked by
    // the umask and a `chmod` is not: `DirBuilder::mode(0o755)` still yields
    // 0700 under `umask 077`, which is a different image for the same inputs.
    // `VOLUME_CHANNEL_DIR` is here because `td-boot update` treats a MISSING
    // channel as a configuration fault rather than as nothing to do — which is
    // right, and which means an installed machine whose channel was never
    // created fails every timer tick. Empty is the correct initial state: the
    // machine has an update channel and nothing has been offered in it yet.
    for directory in [
        "td",
        protocol::BOOT_DIR,
        protocol::DEPLOYMENTS_DIR,
        protocol::VOLUME_CHANNEL_DIR,
    ] {
        let path = staging.join(directory);
        paths::create_dir_all(&path)?;
        paths::set_mode(&path, 0o755)?;
    }
    // The key is SNAPSHOT and td-boot is handed the snapshot, so the file that
    // authenticates the bundle is the file the volume keeps — two reads of one
    // path are two chances for it to say different things, and nothing would
    // report the disagreement. Beside the staging tree rather than in it, so
    // td-boot never reads a trust root out of the volume root it is writing.
    // `staging` is canonical, so its parent is the scratch directory.
    //
    // In a directory of its own at 0700, because the scratch directory is not
    // private and `td-trusted.pub` is a guessable name: everything below
    // narrows the window in which the snapshot can be swapped, and a directory
    // nothing else may write into is what removes it rather than narrowing it.
    // Restricting needs no chmod after the fact — a umask can only take mode
    // bits away, and 0700 is already the fewest this needs — which is the
    // reverse of the widening two blocks down.
    let private = staging
        .parent()
        .ok_or_else(|| invalid(format!("the staging tree {} has no parent", staging.display())))?
        .join("td-install-key");
    paths::remove_dir_all_if_present(&private)?;
    paths::create_dir_with_mode(&private, 0o700)?;
    let snapshot = private.join("td-trusted.pub");
    let identity;
    {
        use std::os::unix::fs::PermissionsExt;
        // Created 0600 and WIDENED, not created 0644: a creation mode is
        // masked by the umask, so 0644 asked for directly is 0600 under `umask
        // 077` with nothing to correct it — and the other order would leave a
        // window where a machine's trust root is world-writable.
        //
        // Widened through the open DESCRIPTOR, not the path: a path-based
        // chmod follows a symlink, so one swapped in after the write would
        // take the 0644 instead — the same race `create_new` closes at the
        // other end, and closing only one end closes neither.
        let mut file = paths::create_new_with_mode(&snapshot, 0o600)?;
        file.write_all(key)?;
        file.set_permissions(std::fs::Permissions::from_mode(0o644))?;
        use std::os::linux::fs::MetadataExt;
        let written = file.metadata()?;
        identity = (written.st_dev(), written.st_ino());
    }
    let output = std::process::Command::new(&publish.td_boot)
        .arg(protocol::PUBLISH_VERB)
        .arg(staging)
        .arg(&publish.deployment)
        .arg(&snapshot)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .output()
        // A failure to SPAWN reports the errno and no path, so a mistyped or
        // unbuilt `td-boot` says only `No such file or directory` — on a
        // command line that names four other paths any of which a reader would
        // suspect first.
        .map_err(|error| {
            invalid(format!(
                "cannot run {}: {error}",
                publish.td_boot.display()
            ))
        })?;
    let _ = io::stderr().write_all(&output.stdout);
    if !output.status.success() {
        return Err(invalid(format!(
            "{} publish failed ({})",
            publish.td_boot.display(),
            output.status
        )));
    }
    // A SUCCESSFUL EXIT IS NOT A PUBLISH. Nothing about a zero status says a
    // deployment landed, and the failure that hides behind one is the worst
    // this verb has: a complete, correct, mountable volume with an empty
    // `td/deployments` — a disk that installs, formats, reports its offsets and
    // then cannot boot, discovered by the machine rather than by the installer.
    // So the id the child prints is READ BACK against the tree it claims to
    // have written, which is a fact this crate already has: it made the
    // directory the id has to appear in.
    //
    // This is not the crate learning the transaction (D1) — it does not know
    // what a deployment CONTAINS, only that the writer named one and that the
    // name resolves. `valid_digest` is `protocol.rs`'s so the shape is stated
    // once, and it is checked BEFORE the join rather than after: an id is
    // otherwise a path component out of a program's stdout, and `..` in it
    // would answer this question with a directory outside the staging tree.
    let id = std::str::from_utf8(&output.stdout)
        .map_err(|_| {
            invalid(format!(
                "{} printed a deployment id that is not ASCII",
                publish.td_boot.display()
            ))
        })?
        .trim();
    if !protocol::valid_digest(id.as_bytes()) {
        return Err(invalid(format!(
            "{} published no deployment id ({id:?})",
            publish.td_boot.display()
        )));
    }
    let published = staging.join(protocol::DEPLOYMENTS_DIR).join(id);
    if !paths::is_dir(&published)? {
        return Err(invalid(format!(
            "{} reported {id} but {} is not there",
            publish.td_boot.display(),
            published.display()
        )));
    }
    // The volume keeps the key, so the machine this installs can authenticate
    // its own updates: it has none otherwise, since `TRUSTED_KEY_PATH` is the
    // SELECTOR initramfs's copy and `switch_root` replaces that rootfs
    // (DESIGN §10 item 10a).
    //
    // Promoted only now, so a key that authenticated nothing never reaches a
    // disk — and by RENAME, which neither copies the bytes again nor follows a
    // symlink standing at the destination.
    //
    // A rename moves whatever the path names AT RENAME TIME, though, not the
    // file td-boot just read, so the inode is checked against the one written
    // above — the `st_dev`/`st_ino` comparison the scratch image already makes
    // one screen down. The 0700 directory is what makes this a check nothing
    // is expected to trip; it is here because the alternative to tripping it
    // is a volume carrying a key that authenticated nothing, which is the one
    // outcome this whole path exists to prevent.
    {
        use std::os::linux::fs::MetadataExt;
        let now = paths::symlink_metadata(&snapshot)?;
        if !now.is_file() || (now.st_dev(), now.st_ino()) != identity {
            return Err(invalid(format!(
                "the trusted key {} was replaced while the deployment was published",
                snapshot.display()
            )));
        }
    }
    let destination = staging.join(protocol::VOLUME_TRUSTED_KEY);
    paths::rename(&snapshot, &destination)?;
    Ok(())
}

fn run_volume(
    destination: &Path,
    mkfs: &Path,
    scratch: &Path,
    publish: Option<&Publish>,
    out: &mut dyn Write,
) -> io::Result<()> {
    // `Command::new` SEARCHES `PATH` for a name with no separator in it, which
    // is the ambient resolution the declared-input contract above rules out —
    // and the one form of it a caller cannot see they asked for.
    //
    // BOTH programs, and both here, before anything is opened or removed. An
    // argv-shaped mistake should cost nothing, and the td-boot check began life
    // inside `publish_into` — which runs after the destination is open and,
    // worse, after the caller's staging tree has been emptied. A bare name in
    // the fourth argument therefore destroyed a directory before saying it did
    // not like the fourth argument.
    for (label, program) in [
        ("mkfs.btrfs", Some(mkfs)),
        ("td-boot", publish.map(|publish| publish.td_boot.as_path())),
    ] {
        if let Some(program) = program {
            if !program.is_absolute() {
                // The value is QUOTED because the label and it collide in the
                // likely mistake: passing the conventional bare name gives
                // `mkfs.btrfs mkfs.btrfs is not an absolute path`, which reads
                // as a typo in the diagnostic rather than as the argument.
                return Err(invalid(format!(
                    "{label} {:?} is not an absolute path, and a bare name resolves \
                     through PATH",
                    program.display()
                )));
            }
        }
    }
    // The KEY is read here for the same reason and in the same place. It is an
    // argv-shaped mistake like the two above — a path that is not there, or is
    // not a key — and reading it inside `publish_into` put the refusal after
    // the staging tree had been emptied, so a mistyped fifth argument
    // destroyed a directory before saying it did not like the fifth argument.
    // That is verbatim the failure the paragraph above records for the fourth.
    let key = publish
        .map(|publish| read_trusted_key(&publish.trusted_key))
        .transpose()?;
    let mut file = paths::open_read_write(destination)?;
    let disk_bytes = destination_bytes(&mut file)?;
    let sector_size = logical_sector_size(&file)?;
    if !disk_bytes.is_multiple_of(sector_size) {
        return Err(invalid(format!(
            "destination is {disk_bytes} bytes, not a whole number of \
             {sector_size}-byte sectors"
        )));
    }
    let (offset, len) = volume_region(&mut file, sector_size, disk_bytes / sector_size)?;
    // The partition the TABLE describes must fit in the destination the table
    // is on. `gpt::parse` cannot check this — it is handed two byte slices and
    // never learns where they came from — so a header claiming a larger disk
    // than it sits on passes every checksum and puts `td-volume` past the end.
    // On a regular file the copy would then EXTEND it; on a block device it
    // would write over the real backup table before running out of room.
    let end = offset
        .checked_add(len)
        .ok_or_else(|| invalid("the volume region overflowed".to_string()))?;
    if end > disk_bytes {
        return Err(invalid(format!(
            "the table puts the volume at {offset}..{end} on a {disk_bytes}-byte \
             destination"
        )));
    }

    // The image is the volume's own size, because `--byte-count` is what the
    // filesystem records as the device it lives on: a smaller one would make a
    // volume that reports less space than the partition it is copied into, and
    // a larger one a volume whose tail is off the end of the partition.
    let image_path = scratch.join("td-volume.img");
    let staging = scratch.join("td-volume-root");
    let subvol = staging.join(protocol::VOLUME_SUBVOL);
    // The staging tree is not a working directory but the volume's CONTENTS:
    // `--rootdir` copies whatever is under it into the filesystem. So it is
    // emptied rather than merely ensured — a scratch directory a previous run
    // or another program left something in would otherwise put that something
    // on a machine's /var, with nothing about the install saying so.
    paths::remove_dir_all_if_present(&staging)?;
    paths::create_dir_all(&subvol)?;
    // BEFORE the mkfs that bakes this tree into the image, which is the whole
    // of why the publish can happen without a mount: `--rootdir` is what puts
    // it in the filesystem.
    //
    // CANONICALIZED first, because `td-boot` requires an absolute volume root
    // and a relative `<scratch-dir>` is otherwise accepted by every other part
    // of this verb — a `volume ./scratch` that worked would start failing the
    // moment a deployment was passed to it, which is a difference between the
    // two forms that nothing about either says. Resolved once and used for
    // `--rootdir` too, so the publish and the filesystem cannot be given two
    // different names for one directory.
    let staging = paths::canonicalize(&staging)?;
    if let Some(publish) = publish {
        // `key` is `Some` exactly when `publish` is — both come from the one
        // `Option` above. Asked for rather than matched alongside, because a
        // pattern over the pair would SKIP the publish if they ever
        // disagreed, and a skipped publish reports success and ships a volume
        // with nothing on it: the fail-open this verb's whole read-back
        // exists to refuse.
        let key = key.ok_or_else(|| invalid("the trusted key was not read".to_string()))?;
        publish_into(&staging, publish, &key)?;
    }
    // `File::create` TRUNCATES, so a scratch directory that puts the image on
    // top of the DESTINATION destroys the disk whose table was just read — and
    // reports success, since everything after this writes a filesystem into
    // what is left. Compared by device and inode rather than by name: a symlink
    // or a hard link is the same file under a different path, and `metadata`
    // follows the one while a string comparison sees neither.
    {
        use std::os::linux::fs::MetadataExt;
        if let Some(existing) = paths::metadata_if_present(&image_path) {
            let target = file.metadata()?;
            if (existing.st_dev(), existing.st_ino()) == (target.st_dev(), target.st_ino()) {
                return Err(invalid(format!(
                    "the scratch image {} is the destination itself",
                    image_path.display()
                )));
            }
        }
    }
    // UNLINK then CREATE NEW, rather than `File::create`, which opens what is
    // there — following a symlink to wherever it points and truncating THAT.
    // The inode check above covers the destination and nothing else, so a
    // `td-volume.img` pointing at some other file or a block device would be
    // truncated and grown to the partition's size, under an installer that is
    // usually root. Removing the ENTRY affects only the link, and `create_new`
    // then refuses anything that appeared in between rather than opening it.
    paths::remove_file_if_present(&image_path)?;
    let image = paths::create_new(&image_path)?;
    image.set_len(len)?;
    let got = image.metadata()?.len();
    if got != len {
        return Err(invalid(format!(
            "the scratch image is {got} bytes, not the {len} the volume needs"
        )));
    }
    drop(image);

    let uuid = random_guid()?.to_string();
    // The child's stdout is CAPTURED and replayed on ours, because this
    // program's stdout is a machine-readable line and mkfs.btrfs opens with a
    // banner. Inherited, that banner is the first line of what a caller parses
    // — which is exactly how this was found, the recipe check reading `v7.0`
    // where it wanted an offset. stderr is inherited, so a failure still says
    // what went wrong as it happens.
    let output = std::process::Command::new(mkfs)
        .arg("--byte-count")
        .arg(len.to_string())
        .arg("--uuid")
        .arg(&uuid)
        .arg("--label")
        .arg(protocol::VOLUME_LABEL)
        // The one directory in the staged root becomes the read-write subvolume
        // the boot path mounts on /var. An empty volume without it is a disk
        // that lays out, formats, and then cannot boot.
        .arg("--rootdir")
        .arg(&staging)
        .arg("--subvol")
        .arg(format!("rw:{}", protocol::VOLUME_SUBVOL))
        .arg(&image_path)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .output()
        .map_err(|error| invalid(format!("cannot run {}: {error}", mkfs.display())))?;
    // `let _`, as every other write to the diagnostic channel in this crate is:
    // a closed or full stderr is not a reason to abandon an install half done,
    // and reporting its ENOSPC would name the wrong disk entirely.
    let _ = io::stderr().write_all(&output.stdout);
    if !output.status.success() {
        return Err(invalid(format!(
            "{} failed on the scratch image ({})",
            mkfs.display(),
            output.status
        )));
    }

    zero_edges(&mut file, offset, len)?;
    // Durable BEFORE the copy starts, or the ordering below buys nothing: a
    // power loss could otherwise persist new filesystem blocks while the zero
    // over the old superblock is still only in page cache, which is exactly the
    // mixed, apparently-valid volume the deferral exists to prevent.
    file.sync_all()?;
    let mut image = paths::open_read(&image_path)?;
    // The copy is ORDERED, for `run_layout`'s reason one level down: the
    // superblock is this filesystem's commit point, as the primary table is the
    // disk's, and it is only true once everything it points at is durable.
    // Btrfs puts it 64 KiB in, so the FIRST chunk is the commit point — write it
    // last, behind a barrier, and an interrupted `volume` leaves nothing at the
    // offset a mount reads. Written first, the same interruption leaves a
    // superblock a prober calls valid over chunks that are still the PREVIOUS
    // install's bytes, which is a disk that reports a good btrfs and fails to
    // mount.
    //
    // The PRIMARY only. A mirror 64 MiB in is written during the first pass,
    // and deferring it too would not buy the same thing: the zeroing does not
    // reach that far, so what stands there meanwhile is the previous install's
    // mirror rather than nothing. `btrfs rescue super-recover` can promote a
    // mirror, so an interrupted install is recoverable-into-nonsense by a tool
    // asked to try; every path that MOUNTS reads the primary.
    //
    // This is also what makes the head half of `zero_edges` load-bearing rather
    // than merely tidy: with the chunk deferred, those zeros are what stands in
    // the superblock's place for the length of the copy.
    let head = COPY_CHUNK.min(len);
    let rest = copy_sparse(&mut image, &mut file, offset, head, len)?;
    file.sync_all()?;
    let first = copy_sparse(&mut image, &mut file, offset, 0, head)?;
    file.sync_all()?;
    let written = rest.saturating_add(first);

    // The scratch directory is the CALLER's, and so is what is left in it. Not
    // tidiness deferred: the image is the only artifact anything can check the
    // filesystem itself against — `btrfs check` wants a device or a file, and
    // the copy on the destination begins half a gigabyte in, where no tool can
    // be pointed at it. Deleting it here would leave the strongest available
    // check with nothing to run on.
    writeln!(out, "{offset} {len} {written}")
}

/// The FAT volume serial, taken from the ESP partition's own GUID.
///
/// Not a clock: an installer that stamped the time would make two otherwise
/// identical installs differ in a field nothing needs to differ in, and the
/// oracle compares images. The GUID is already per-install and already random.
fn volume_serial(layout: &gpt::Layout) -> io::Result<u32> {
    let esp = layout
        .partitions
        .first()
        .ok_or_else(|| invalid("the layout has no ESP".to_string()))?;
    let bytes = esp
        .unique_guid
        .0
        .get(..4)
        .and_then(|slice| <[u8; 4]>::try_from(slice).ok())
        .ok_or_else(|| invalid("the ESP GUID is too short".to_string()))?;
    Ok(u32::from_le_bytes(bytes))
}

fn main() -> ExitCode {
    let mode = match parse_args(std::env::args_os().skip(1)) {
        Ok(mode) => mode,
        Err(error) => {
            let _ = writeln!(io::stderr(), "td-install: {error}");
            return ExitCode::FAILURE;
        }
    };
    let result = match mode {
        Mode::Layout { destination } => run_layout(&destination, &mut io::stdout()),
        Mode::Volume {
            destination,
            mkfs,
            scratch,
            publish,
        } => run_volume(
            &destination,
            &mkfs,
            &scratch,
            publish.as_ref(),
            &mut io::stdout(),
        ),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            let _ = writeln!(io::stderr(), "td-install: {error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // The shipped half reaches the filesystem only through `mod paths` now, so
    // this is the test half's own — a test opening a fixture is not a path the
    // installer takes from an operator, and the scan reads only the half above.
    use std::fs::OpenOptions;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);

    const MIB: u64 = 1024 * 1024;
    const GIB: u64 = 1024 * MIB;
    /// Big enough for the ESP plus the smallest volume td-install accepts, and
    /// no bigger: these are sparse files, but every byte a test READS is real.
    const DISK: u64 = 4 * GIB;

    struct Scratch {
        path: PathBuf,
    }

    impl Scratch {
        /// A sparse file of `bytes`, which is a destination `td-install` treats
        /// exactly as it treats a disk (D9).
        fn disk(bytes: u64) -> Scratch {
            let sequence = NEXT.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir()
                .join(format!("td-install-test-{}-{sequence}", std::process::id()));
            let file = File::create(&path).unwrap();
            file.set_len(bytes).unwrap();
            Scratch { path }
        }

        /// Read a RANGE. Never the whole file: these destinations are sparse
        /// and gigabytes wide, and `fs::read` would materialize every zero.
        fn read_at(&self, offset: u64, len: usize) -> Vec<u8> {
            let mut file = File::open(&self.path).unwrap();
            file.seek(SeekFrom::Start(offset)).unwrap();
            let mut bytes = vec![0u8; len];
            file.read_exact(&mut bytes).unwrap();
            bytes
        }

        fn table(&self, disk_bytes: u64) -> gpt::Table {
            let sectors = disk_bytes / 512;
            let primary = self.read_at(0, 34 * 512);
            let backup = self.read_at((sectors - 33) * 512, 33 * 512);
            gpt::parse(&primary, &backup, 512).unwrap()
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }

    /// A stand-in that RECORDS the argv it was given, one word a line, beside
    /// itself. Every check in this file and in the recipe reads what mkfs
    /// produced, and the arguments are invisible to all of them: `--byte-count`
    /// half the real size still puts the superblock at 64 KiB, the label at
    /// 64 KiB+299 and a mirror at 64 MiB, so a volume sized wrongly passes every
    /// one. The only place the request itself can be seen is here.
    const RECORDING_MKFS: &str = "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$(dirname \"$0\")/argv\"\n";

    /// A directory holding an executable `mkfs.btrfs` stand-in with `body`.
    fn fake_mkfs(body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!(
            "td-install-mkfs-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let fake = dir.join("mkfs.btrfs");
        std::fs::write(&fake, body).unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        dir
    }

    fn args(values: &[&str]) -> std::vec::IntoIter<OsString> {
        values
            .iter()
            .map(|value| OsString::from(*value))
            .collect::<Vec<_>>()
            .into_iter()
    }

    #[test]
    fn the_verb_and_its_arity_are_exact() {
        assert_eq!(
            parse_args(args(&["layout", "/dev/sda"])).unwrap(),
            Mode::Layout {
                destination: PathBuf::from("/dev/sda")
            }
        );
        assert!(parse_args(args(&["layout"])).is_err(), "missing destination");
        assert!(
            parse_args(args(&["layout", "/dev/sda", "extra"])).is_err(),
            "a third argument is not silently ignored"
        );
        assert!(parse_args(args(&["format", "/dev/sda"])).is_err(), "verb");
        assert!(parse_args(args(&[])).is_err(), "no arguments");
        assert_eq!(
            parse_args(args(&["volume", "/dev/sda", "/bin/mkfs.btrfs", "/tmp"])).unwrap(),
            Mode::Volume {
                destination: PathBuf::from("/dev/sda"),
                mkfs: PathBuf::from("/bin/mkfs.btrfs"),
                scratch: PathBuf::from("/tmp"),
                publish: None,
            }
        );
        // Each of the three is REQUIRED, and none is defaulted: a `volume` that
        // guessed where mkfs.btrfs is would resolve it out of an ambient PATH,
        // and one that guessed a scratch directory would put a
        // partition-sized image somewhere nobody chose.
        for short in [
            vec!["volume", "/dev/sda"],
            vec!["volume", "/dev/sda", "/bin/mkfs.btrfs"],
        ] {
            assert!(parse_args(args(&short)).is_err(), "{short:?} is incomplete");
        }
        assert!(
            parse_args(args(&["volume", "/dev/sda", "/bin/mkfs.btrfs", "/tmp", "x"])).is_err(),
            "a fourth argument is not silently ignored"
        );
    }

    /// The ranges a table is read back from are the ones `gpt::build` wrote to.
    /// Derived rather than remembered, so this pins them at BOTH sector sizes —
    /// the 4Kn arithmetic is where a hardcoded 34/33 would be wrong by eight.
    #[test]
    fn the_table_ranges_are_where_the_table_was_written() {
        for (sector, disk) in [(512u64, DISK), (4096, DISK)] {
            let sectors = disk / sector;
            let [primary, backup] = table_ranges(sector, sectors).unwrap();
            let layout = gpt::Layout {
                sector_size: sector,
                disk_sectors: sectors,
                disk_guid: gpt::Guid::parse("12345678-1234-4234-8234-123456789abc").unwrap(),
                align_sectors: protocol::PARTITION_ALIGN_BYTES / sector,
                partitions: Vec::new(),
            };
            let image = gpt::build(&layout).unwrap();
            assert_eq!(primary, (image.primary_offset, image.primary.len() as u64));
            assert_eq!(backup, (image.backup_offset, image.backup.len() as u64));
        }
    }

    /// The volume region comes off the TABLE, so it is the partition the disk
    /// describes and not a recomputation that could have drifted from it.
    #[test]
    fn the_volume_region_is_the_partition_the_table_describes() {
        let scratch = Scratch::disk(DISK);
        run_layout(&scratch.path, &mut Vec::new()).unwrap();
        let plan = plan(512, DISK).unwrap();
        let mut file = File::open(&scratch.path).unwrap();
        let (offset, len) = volume_region(&mut file, 512, DISK / 512).unwrap();
        assert_eq!(offset, plan.volume_start * 512);
        assert_eq!(len, (plan.volume_end - plan.volume_start + 1) * 512);
        // ...and it ends on the last usable sector, so the partition the
        // filesystem is sized for is the whole of what the table set aside.
        assert_eq!(offset + len, (plan.volume_end + 1) * 512);
    }

    /// A table that is INTERNALLY consistent but describes a different disk is
    /// refused.
    ///
    /// `gpt::parse` is handed two byte slices and never learns where they came
    /// from, so it cannot catch this: every checksum is over the bytes, and a
    /// backup written somewhere other than the LBA its own header names is
    /// still a backup that verifies. Built here exactly that way — a table for
    /// a disk twice this size, with its backup placed where THIS disk's backup
    /// goes — because the consequence is a `td-volume` past the end of the
    /// destination, which a regular file silently EXTENDS to fit.
    #[test]
    fn a_table_describing_a_different_disk_is_refused() {
        let scratch = Scratch::disk(DISK);
        let claimed = (DISK * 2) / 512;
        let layout = gpt::Layout {
            sector_size: 512,
            disk_sectors: claimed,
            disk_guid: gpt::Guid::parse("12345678-1234-4234-8234-123456789abc").unwrap(),
            align_sectors: protocol::PARTITION_ALIGN_BYTES / 512,
            partitions: Vec::new(),
        };
        let forged = gpt::build(&layout).unwrap();
        {
            let mut file = OpenOptions::new().write(true).open(&scratch.path).unwrap();
            write_at(&mut file, forged.primary_offset, &forged.primary).unwrap();
            // ...at THIS disk's backup position, not the one the header names.
            let [_, backup] = table_ranges(512, DISK / 512).unwrap();
            write_at(&mut file, backup.0, &forged.backup).unwrap();
        }
        let mut file = File::open(&scratch.path).unwrap();
        let error = volume_region(&mut file, 512, DISK / 512).unwrap_err();
        assert!(
            format!("{error}").contains("-sector disk, not the"),
            "a table for another disk must be refused: {error}"
        );
    }

    /// The volume mkfs is ASKED for is the partition's own size.
    ///
    /// Nothing downstream can see this. `--byte-count` half the real length
    /// still leaves the superblock at 64 KiB, the label beside it and a mirror
    /// at 64 MiB, so every offset check in this crate and in the recipe passes
    /// while the filesystem reports less space than the partition it lives in.
    /// The argv is the only place the request itself is visible.
    #[test]
    fn mkfs_is_asked_for_the_partitions_own_size() {
        let scratch = Scratch::disk(DISK);
        run_layout(&scratch.path, &mut Vec::new()).unwrap();
        let dir = fake_mkfs(RECORDING_MKFS);
        run_volume(
            &scratch.path,
            &dir.join("mkfs.btrfs"),
            &dir,
            None,
            &mut Vec::new(),
        )
        .unwrap();
        let argv = std::fs::read_to_string(dir.join("argv")).unwrap();
        let words: Vec<&str> = argv.lines().collect();
        let plan = plan(512, DISK).unwrap();
        let expected = (plan.volume_end - plan.volume_start + 1) * 512;
        let after = |flag: &str| {
            words
                .iter()
                .position(|w| *w == flag)
                .and_then(|i| words.get(i + 1))
                .map(|s| s.to_string())
        };
        assert_eq!(
            after("--byte-count"),
            Some(expected.to_string()),
            "mkfs was sized for something other than the partition: {argv:?}"
        );
        assert_eq!(
            after("--label"),
            Some(protocol::VOLUME_LABEL.to_string()),
            "the label is the one td-boot looks for: {argv:?}"
        );
        assert_eq!(
            after("--subvol"),
            Some(format!("rw:{}", protocol::VOLUME_SUBVOL)),
            "the subvolume is read-write and named: {argv:?}"
        );
        // The UUID is per-install and random, so what is pinned is that one was
        // asked for and that it PARSES — a malformed one mkfs would reject.
        let uuid = after("--uuid").unwrap_or_default();
        assert!(
            gpt::Guid::parse(&uuid).is_ok(),
            "the uuid is not a GUID: {uuid:?}"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Two installs draw different volume UUIDs, so two td disks are not one
    /// filesystem as far as anything resolving by UUID is concerned.
    #[test]
    fn each_volume_gets_its_own_uuid() {
        let mut seen = Vec::new();
        for _ in 0..2 {
            let scratch = Scratch::disk(DISK);
            run_layout(&scratch.path, &mut Vec::new()).unwrap();
            let dir = fake_mkfs(RECORDING_MKFS);
            run_volume(
                &scratch.path,
                &dir.join("mkfs.btrfs"),
                &dir,
                None,
                &mut Vec::new(),
            )
            .unwrap();
            let argv = std::fs::read_to_string(dir.join("argv")).unwrap();
            let words: Vec<&str> = argv.lines().collect();
            let uuid = words
                .iter()
                .position(|w| *w == "--uuid")
                .and_then(|i| words.get(i + 1))
                .map(|s| s.to_string())
                .unwrap();
            seen.push(uuid);
            std::fs::remove_dir_all(&dir).unwrap();
        }
        assert_ne!(seen.first(), seen.get(1), "two installs shared a volume UUID");
    }

    /// A table with TWO partitions of the volume's name is refused rather than
    /// resolved by position: GPT does not make names unique, and picking the
    /// first formats whichever partition happens to be listed earlier.
    ///
    /// A partition of the right name and the WRONG TYPE is not a match at all —
    /// a name is 36 characters anyone can write, and the type GUID is what says
    /// what the partition is for.
    #[test]
    fn an_ambiguous_or_wrongly_typed_volume_is_refused() {
        let make = |parts: Vec<gpt::Partition>| {
            let scratch = Scratch::disk(DISK);
            let layout = gpt::Layout {
                sector_size: 512,
                disk_sectors: DISK / 512,
                disk_guid: gpt::Guid::parse("12345678-1234-4234-8234-123456789abc").unwrap(),
                align_sectors: protocol::PARTITION_ALIGN_BYTES / 512,
                partitions: parts,
            };
            let image = gpt::build(&layout).unwrap();
            {
                let mut file = OpenOptions::new().write(true).open(&scratch.path).unwrap();
                write_at(&mut file, image.primary_offset, &image.primary).unwrap();
                write_at(&mut file, image.backup_offset, &image.backup).unwrap();
            }
            let mut file = File::open(&scratch.path).unwrap();
            let error = volume_region(&mut file, 512, DISK / 512).unwrap_err();
            format!("{error}")
        };
        // Distinct unique GUIDs, because `gpt::build` refuses two of one — the
        // ambiguity under test here is of the NAME, which nothing refuses.
        let volume = |start: u64, end: u64, type_guid: gpt::Guid, tag: u8| gpt::Partition {
            type_guid,
            unique_guid: gpt::Guid::parse(&format!("00000000-0000-4000-8000-00000000000{tag}"))
                .unwrap(),
            start_lba: start,
            end_lba: end,
            attributes: 0,
            name: protocol::VOLUME_PARTITION_NAME.to_string(),
        };
        let two = make(vec![
            volume(2048, 4095, gpt::TYPE_LINUX_FS, 1),
            volume(4096, 8191, gpt::TYPE_LINUX_FS, 2),
        ]);
        assert!(
            two.contains("has 2 partitions named"),
            "two of the name must be refused: {two}"
        );
        let wrong_type = make(vec![volume(2048, 4095, gpt::TYPE_ESP, 3)]);
        assert!(
            wrong_type.contains("no td-volume partition on this disk"),
            "the name alone is not a match: {wrong_type}"
        );
    }

    /// The ENGINE refuses to place a partition over the table, which is the
    /// first of the two answers to a volume that would overwrite one.
    ///
    /// The second is `volume_region`'s own `overlaps a partition table` bound,
    /// and NO TEST HERE REACHES IT — deliberately recorded rather than left to
    /// be discovered. `gpt::parse` bounds partitions by the header's own
    /// `first_usable`, a field in the same table, so a hand-forged table
    /// declaring a usable range over its own entry array would be
    /// self-consistent and would reach it. Building one means re-sealing two
    /// headers and an entry-array CRC by hand, which is `gpt.rs`'s job
    /// reimplemented in a test that would then pass while the real sealing
    /// changed underneath it. So the bound stays as what it is — a check on the
    /// last value before a raw write to somebody's disk — and this test pins
    /// the reachable half.
    #[test]
    fn a_volume_over_the_table_cannot_even_be_built() {
        let layout = gpt::Layout {
            sector_size: 512,
            disk_sectors: DISK / 512,
            disk_guid: gpt::Guid::parse("12345678-1234-4234-8234-123456789abc").unwrap(),
            align_sectors: protocol::PARTITION_ALIGN_BYTES / 512,
            partitions: vec![gpt::Partition {
                type_guid: gpt::TYPE_LINUX_FS,
                unique_guid: gpt::Guid::parse("00000000-0000-4000-8000-000000000002").unwrap(),
                // LBA 2 is inside the primary entry array at any sector size.
                start_lba: 2,
                end_lba: 2047,
                attributes: 0,
                name: protocol::VOLUME_PARTITION_NAME.to_string(),
            }],
        };
        let error = gpt::build(&layout).unwrap_err();
        assert!(
            error.contains("the table itself occupies through"),
            "the engine must refuse a partition over its own table: {error}"
        );
    }

    /// A scratch image that is a SYMLINK is replaced, not followed — otherwise
    /// `File::create` truncates whatever it points at and grows it to the
    /// partition's size, under an installer that is usually root.
    #[test]
    fn a_symlinked_scratch_image_is_replaced_rather_than_followed() {
        let dir = fake_mkfs(RECORDING_MKFS);
        let scratch = Scratch::disk(DISK);
        run_layout(&scratch.path, &mut Vec::new()).unwrap();
        let bystander = dir.join("someone-elses-file");
        std::fs::write(&bystander, b"do not truncate me").unwrap();
        std::os::unix::fs::symlink(&bystander, dir.join("td-volume.img")).unwrap();
        run_volume(
            &scratch.path,
            &dir.join("mkfs.btrfs"),
            &dir,
            None,
            &mut Vec::new(),
        )
        .unwrap();
        assert_eq!(
            std::fs::read(&bystander).unwrap(),
            b"do not truncate me",
            "the symlink's target was written through"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A destination with no table at all is refused, and SO IS one with a
    /// valid table that has no `td-volume` in it — a different branch, and the
    /// one the diagnostic is written for.
    #[test]
    fn a_volume_needs_a_layout_first() {
        let scratch = Scratch::disk(DISK);
        let mut file = File::open(&scratch.path).unwrap();
        let error = volume_region(&mut file, 512, DISK / 512).unwrap_err();
        assert!(
            format!("{error}").contains("gpt:"),
            "an unlaid-out disk is refused by the parser: {error}"
        );

        // A well-formed table for this disk, carrying no partitions.
        let layout = gpt::Layout {
            sector_size: 512,
            disk_sectors: DISK / 512,
            disk_guid: gpt::Guid::parse("12345678-1234-4234-8234-123456789abc").unwrap(),
            align_sectors: protocol::PARTITION_ALIGN_BYTES / 512,
            partitions: Vec::new(),
        };
        let empty = gpt::build(&layout).unwrap();
        {
            let mut file = OpenOptions::new().write(true).open(&scratch.path).unwrap();
            write_at(&mut file, empty.primary_offset, &empty.primary).unwrap();
            write_at(&mut file, empty.backup_offset, &empty.backup).unwrap();
        }
        let mut file = File::open(&scratch.path).unwrap();
        let error = volume_region(&mut file, 512, DISK / 512).unwrap_err();
        assert!(
            format!("{error}").contains("no td-volume partition on this disk"),
            "a table without the volume must say so: {error}"
        );
    }

    /// The line `run_volume` reports is the volume's own geometry, and a
    /// `mkfs` that wrote nothing copies nothing.
    ///
    /// The stand-in is a shell script rather than the real thing: no host is
    /// required to have `mkfs.btrfs`, and what is under test here is the
    /// arithmetic and the reporting, not the filesystem. It leaves the image
    /// all zeros, so `written` is 0 — which is pinned too, since a copy that
    /// wrote something out of an empty image would be inventing it.
    ///
    /// What this canNOT cover is the child's stdout staying out of the
    /// process's own, which is what the parent commit's recipe check caught:
    /// `out` here is a `Vec`, so a child inheriting fd 1 is invisible to it.
    /// `tests/stdout_is_a_data_channel.rs` runs the real binary for that.
    #[test]
    fn the_reported_line_is_the_volumes_geometry() {
        let scratch = Scratch::disk(DISK);
        run_layout(&scratch.path, &mut Vec::new()).unwrap();
        let dir = fake_mkfs("#!/bin/sh\necho 'btrfs-progs v7.0'\n");
        let mut out = Vec::new();
        run_volume(&scratch.path, &dir.join("mkfs.btrfs"), &dir, None, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        let fields: Vec<&str> = text.split_whitespace().collect();
        assert_eq!(fields.len(), 3, "the line is <off> <len> <written>: {text:?}");
        let plan = plan(512, DISK).unwrap();
        assert_eq!(
            fields.first().map(|f| f.parse::<u64>().unwrap()),
            Some(plan.volume_start * 512)
        );
        assert_eq!(
            fields.get(1).map(|f| f.parse::<u64>().unwrap()),
            Some((plan.volume_end - plan.volume_start + 1) * 512)
        );
        assert_eq!(fields.get(2).map(|f| f.parse::<u64>().unwrap()), Some(0));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A destination whose PATH has a space in it does not shift the fields a
    /// caller reads by position. Nothing but numbers goes on that channel, so
    /// this holds by construction rather than by escaping — which is the point:
    /// escaping is a rule every future field has to remember.
    #[test]
    fn a_destination_with_a_space_in_its_name_does_not_shift_the_fields() {
        let dir = std::env::temp_dir().join(format!(
            "td-install-space {}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a disk.img");
        File::create(&path).unwrap().set_len(DISK).unwrap();
        let mut out = Vec::new();
        run_layout(&path, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert_eq!(text.lines().count(), 1, "one line: {text:?}");
        let fields: Vec<&str> = text.split_whitespace().collect();
        assert_eq!(fields.len(), 2, "the line is <esp> <volume>: {text:?}");
        for field in &fields {
            assert!(field.parse::<u64>().is_ok(), "{field:?} is not a number");
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The publish arguments are three or none, never a defaulted subset.
    #[test]
    fn a_publish_is_all_three_arguments_or_none() {
        assert_eq!(
            parse_args(args(&[
                "volume",
                "/dev/sda",
                "/bin/mkfs.btrfs",
                "/tmp",
                "/bin/td-boot",
                "/media/deployment",
                "/media/key.pub",
            ]))
            .unwrap(),
            Mode::Volume {
                destination: PathBuf::from("/dev/sda"),
                mkfs: PathBuf::from("/bin/mkfs.btrfs"),
                scratch: PathBuf::from("/tmp"),
                publish: Some(Publish {
                    td_boot: PathBuf::from("/bin/td-boot"),
                    deployment: PathBuf::from("/media/deployment"),
                    trusted_key: PathBuf::from("/media/key.pub"),
                }),
            }
        );
        // Four and five arguments are a caller who asked for something this
        // cannot do. Defaulting the missing one is how a fail-open gets in —
        // the key most of all, whose absence is what td-boot reads as "publish
        // without checking".
        for short in [
            vec![
                "volume",
                "/dev/sda",
                "/bin/mkfs.btrfs",
                "/tmp",
                "/bin/td-boot",
            ],
            vec![
                "volume",
                "/dev/sda",
                "/bin/mkfs.btrfs",
                "/tmp",
                "/bin/td-boot",
                "/media/deployment",
            ],
        ] {
            assert!(parse_args(args(&short)).is_err(), "{short:?} is incomplete");
        }
        assert!(
            parse_args(args(&[
                "volume",
                "/dev/sda",
                "/bin/mkfs.btrfs",
                "/tmp",
                "/bin/td-boot",
                "/media/deployment",
                "/media/key.pub",
                "extra",
            ]))
            .is_err(),
            "a seventh argument is not silently ignored"
        );
    }

    /// 64 lowercase hex, because that is what a deployment id is: `td-install`
    /// checks the shape before joining it onto a path, so a stand-in printing
    /// anything else is exercising the refusal.
    const STAND_IN_ID: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    /// A trusted key on disk. Real bytes and a real file, because the key is
    /// carried onto the volume and so has to be readable — a stand-in td-boot
    /// ignores it, but `publish_into` does not.
    fn key_file(dir: &Path) -> PathBuf {
        let path = dir.join("key.pub");
        std::fs::write(&path, format!("{}\n", "ab".repeat(32))).unwrap();
        path
    }

    /// A `td-boot` stand-in that publishes: runs `body`, then does the two
    /// things `publish_into` reads back — creates `td/deployments/<id>` and
    /// prints that id.
    fn publishing_td_boot(path: &Path, body: &str) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::write(
            path,
            format!(
                "#!/bin/sh\n{body}mkdir -p \"$2/{}/{STAND_IN_ID}\"\necho {STAND_IN_ID}\n",
                protocol::DEPLOYMENTS_DIR
            ),
        )
        .unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// The publish runs BEFORE mkfs, into the tree `--rootdir` bakes in, and
    /// hands td-boot exactly what it was told to — the three directories the
    /// one writer requires already made.
    #[test]
    fn the_publish_reaches_td_boot_before_the_filesystem_is_made() {
        let scratch = Scratch::disk(DISK);
        run_layout(&scratch.path, &mut Vec::new()).unwrap();
        // Both stand-ins APPEND their name to one file, so the order is read off
        // that file rather than inferred. Nothing else here can see the order:
        // a publish that ran after mkfs would still see the directories made
        // and still write its witness, which is how this test passed the
        // mutation it is named for until the log existed.
        let dir = fake_mkfs(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$(dirname \"$0\")/argv\"\n\
             echo mkfs >> \"$(dirname \"$0\")/order\"\n",
        );
        // A td-boot stand-in that records its argv and proves the staging tree
        // was ready when it ran: it writes into the deployments directory,
        // which only exists if `publish_into` made it first. It also DOES what
        // a publish does — makes `td/deployments/<id>` and names it on stdout —
        // because that is now the contract, and a stand-in that did less would
        // be testing the refusal rather than the path.
        let td_boot = dir.join("td-boot");
        publishing_td_boot(
            &td_boot,
            "printf '%s\\n' \"$@\" > \"$(dirname \"$0\")/publish-argv\"\n\
             echo publish >> \"$(dirname \"$0\")/order\"\n",
        );
        let key = key_file(&dir);
        let publish = Publish {
            td_boot: td_boot.clone(),
            deployment: PathBuf::from("/media/deployment"),
            trusted_key: key.clone(),
        };
        let mut out = Vec::new();
        run_volume(
            &scratch.path,
            &dir.join("mkfs.btrfs"),
            &dir,
            Some(&publish),
            &mut out,
        )
        .unwrap();

        let argv = std::fs::read_to_string(dir.join("publish-argv")).unwrap();
        let words: Vec<&str> = argv.lines().collect();
        // Canonical on BOTH sides: the argument is resolved, so a host whose
        // temporary directory is itself a symlink would otherwise fail this on
        // the spelling rather than on anything it is about.
        let staging = std::fs::canonicalize(dir.join("td-volume-root")).unwrap();
        // The key argument is the SNAPSHOT, not the path this program was
        // given: handing td-boot the caller's path would be a second read of
        // it, which is the race the snapshot exists to close. Asserted as the
        // exact path rather than merely "not the original", so a snapshot
        // written somewhere unexpected is a failure too.
        let snapshot = std::fs::canonicalize(&dir)
            .unwrap()
            .join("td-install-key")
            .join("td-trusted.pub");
        assert_ne!(snapshot, key, "the snapshot and the given key are one file");
        assert_eq!(
            words,
            vec![
                "publish",
                staging.to_str().unwrap(),
                "/media/deployment",
                snapshot.to_str().unwrap(),
            ],
            "td-boot was not asked to publish into the staging tree: {argv:?}"
        );
        // ...and it is BESIDE the volume root, never inside it: td-boot must
        // not read its trust root out of the tree it is writing.
        assert!(
            !snapshot.starts_with(&staging),
            "the trust root snapshot is inside the volume root"
        );
        // The snapshot is gone once promoted — it was RENAMED, so the volume's
        // key is the very file td-boot authenticated under.
        assert!(!snapshot.exists(), "the snapshot was copied rather than renamed");
        assert!(
            staging.join(protocol::DEPLOYMENTS_DIR).join(STAND_IN_ID).is_dir(),
            "the deployments directory was not there when td-boot ran"
        );
        let order = std::fs::read_to_string(dir.join("order")).unwrap();
        assert_eq!(
            order.lines().collect::<Vec<_>>(),
            vec!["publish", "mkfs"],
            "the deployment must be in the tree before `--rootdir` reads it"
        );
        // The staging tree still carries @var, so the publish did not displace
        // what the volume already needed.
        assert!(staging.join(protocol::VOLUME_SUBVOL).is_dir());
        // The reported line is still three fields. That the id did not reach fd
        // 1 is NOT checkable here — `out` is a `Vec`, so a child handed
        // `Stdio::inherit()` writes past it and this stays green; that mutation
        // is caught by the subprocess test instead.
        let text = String::from_utf8(out).unwrap();
        assert_eq!(text.split_whitespace().count(), 3, "{text:?}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The volume keeps the trust root, so the installed machine can
    /// authenticate its own updates — and keeps it only once the publish that
    /// used those bytes has succeeded.
    #[test]
    fn the_volume_carries_the_key_that_authenticated_it() {
        let scratch = Scratch::disk(DISK);
        run_layout(&scratch.path, &mut Vec::new()).unwrap();
        let dir = fake_mkfs(RECORDING_MKFS);
        let td_boot = dir.join("td-boot");
        publishing_td_boot(&td_boot, "");
        let key = key_file(&dir);
        let publish = Publish {
            td_boot,
            deployment: PathBuf::from("/media/deployment"),
            trusted_key: key.clone(),
        };
        run_volume(
            &scratch.path,
            &dir.join("mkfs.btrfs"),
            &dir,
            Some(&publish),
            &mut Vec::new(),
        )
        .unwrap();

        let staging = dir.join("td-volume-root");
        let carried = staging.join(protocol::VOLUME_TRUSTED_KEY);
        assert_eq!(
            std::fs::read(&carried).unwrap(),
            std::fs::read(&key).unwrap(),
            "the volume's key is not the one the install was given"
        );
        // Where that path is, and that it is relative, are properties of the
        // constant and are pinned in their own test rather than asserted here
        // against themselves.
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&carried).unwrap().permissions().mode() & 0o777,
            0o644
        );

        // The update CHANNEL, empty and 0755. `td-boot update` treats a missing
        // channel as a configuration fault rather than as nothing to do, so a
        // machine installed without one fails every timer tick — and the qemu
        // harness stages its own, so no oracle would ever show it. Asserted on
        // the staging tree because that is what `--rootdir` reads, and empty
        // directories do survive into the image.
        let channel = staging.join(protocol::VOLUME_CHANNEL_DIR);
        assert!(
            channel.is_dir(),
            "the volume must carry an update channel: {}",
            channel.display()
        );
        assert_eq!(
            std::fs::read_dir(&channel).unwrap().count(),
            0,
            "a freshly installed channel has been offered nothing"
        );
        assert_eq!(
            std::fs::metadata(&channel).unwrap().permissions().mode() & 0o777,
            0o755
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The snapshot must not LAUNDER a key past td-boot's own reader.
    ///
    /// td-boot refuses a symlink, a non-regular file, and anything over
    /// `MAX_PUBLIC_KEY_BYTES`. Handing it a copy would turn every one of those
    /// refusals into a successful install, because the copy is a small regular
    /// file whatever the original was — so the same rule is applied here, and
    /// each refusal is checked by its REASON rather than by failing at all: a
    /// non-zero result is satisfied by any error, including one from a later
    /// step that would mean the rule never ran.
    #[test]
    fn a_key_td_boot_would_refuse_is_refused_here_too() {
        let dir = fake_mkfs(RECORDING_MKFS);
        let real = key_file(&dir);

        let link = dir.join("link.pub");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let refused = read_trusted_key(&link).unwrap_err().to_string();
        assert!(refused.contains("must be a real regular file"), "{refused}");

        let big = dir.join("big.pub");
        std::fs::write(&big, vec![b'a'; protocol::MAX_PUBLIC_KEY_BYTES as usize + 1]).unwrap();
        let refused = read_trusted_key(&big).unwrap_err().to_string();
        // The whole phrase: the message carries the path, which holds a pid,
        // and a pid containing the bound would satisfy a bare digit check.
        assert!(
            refused.contains(&format!(
                "trusted deployment key exceeds {} bytes",
                protocol::MAX_PUBLIC_KEY_BYTES
            )),
            "{refused}"
        );

        let refused = read_trusted_key(&dir).unwrap_err().to_string();
        assert!(refused.contains("must be a real regular file"), "{refused}");

        // A FIFO is the case the type check must catch BEFORE the open rather
        // than after it: `File::open` on one blocks until a writer appears, so
        // a version that opened first hung here with no diagnostic instead of
        // failing. Nothing writes to this one, so a regression does not fail
        // this test — it hangs it, which is the honest signal.
        let fifo = dir.join("fifo.pub");
        let made = std::process::Command::new("mkfifo").arg(&fifo).status();
        if made.map(|status| status.success()).unwrap_or(false) {
            let refused = read_trusted_key(&fifo).unwrap_err().to_string();
            assert!(refused.contains("must be a real regular file"), "{refused}");
        }

        // The bound is applied to what is READ and not only to what `stat`
        // claimed, since the two can disagree — a file that grew between them
        // would otherwise be cut to a valid length and laundered past td-boot's
        // refusal. `/proc/self/status` is that disagreement without a race to
        // arrange: a regular file whose reported length is 0 and whose
        // contents are not.
        let proc_status = Path::new("/proc/self/status");
        assert_eq!(std::fs::metadata(proc_status).unwrap().len(), 0);
        let refused = read_trusted_key(proc_status).unwrap_err().to_string();
        assert!(refused.contains("changed while reading"), "{refused}");

        // ...and the bound is not so tight that a key AT it is refused, nor so
        // loose that a real key trips it.
        let edge = dir.join("edge.pub");
        std::fs::write(&edge, vec![b'a'; protocol::MAX_PUBLIC_KEY_BYTES as usize]).unwrap();
        assert_eq!(
            read_trusted_key(&edge).unwrap().len(),
            protocol::MAX_PUBLIC_KEY_BYTES as usize
        );
        assert_eq!(read_trusted_key(&real).unwrap(), std::fs::read(&real).unwrap());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A path this program cannot open NAMES ITSELF.
    ///
    /// `io::Error` carries an errno and nothing else, so the destination — the
    /// one argument an operator is most likely to mistype, and the one that is
    /// a device node on a real install — refused with a bare `No such file or
    /// directory` on a command line naming up to five paths. The key already
    /// named itself, through `realfile`; this is the rest of them.
    ///
    /// Both verbs, because they open the destination independently and only
    /// `layout` was ever driven with a bad one.
    #[test]
    fn a_path_that_cannot_be_opened_names_itself() {
        let dir = std::env::temp_dir().join(format!(
            "td-install-named-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();

        let absent = dir.join("no-such-disk");
        let refused = run_layout(&absent, &mut Vec::new()).unwrap_err().to_string();
        assert!(
            refused.contains(&absent.display().to_string()),
            "layout must name the destination it could not open, got {refused:?}"
        );

        let refused = run_volume(&absent, &dir.join("mkfs.btrfs"), &dir, None, &mut Vec::new())
            .unwrap_err()
            .to_string();
        assert!(
            refused.contains(&absent.display().to_string()),
            "volume must name the destination it could not open, got {refused:?}"
        );

        // A DIRECTORY is the other shape a mistyped destination takes, and it
        // fails at a different call than an absent one.
        let refused = run_layout(&dir, &mut Vec::new()).unwrap_err().to_string();
        assert!(
            refused.contains(&dir.display().to_string()),
            "layout must name a destination that is not a file, got {refused:?}"
        );

        // The key's own refusal is `realfile`'s and predates this; asserted
        // here so the property is stated over every path the verbs take.
        let refused = read_trusted_key(&absent).unwrap_err().to_string();
        assert!(
            refused.contains(&absent.display().to_string()),
            "the trusted key must name itself, got {refused:?}"
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// NOTHING OUTSIDE THE CHOKE POINTS TOUCHES THE FILESYSTEM, AND THE
    /// COMPILER IS WHAT SAYS SO.
    ///
    /// `clippy.toml` disallows every path-taking entry point into the
    /// filesystem and `Cargo.toml` denies the lint, so such a call outside
    /// the two choke points is a BUILD failure and not a test failure. That
    /// pair IS the roster. What is left for a test is the attribute that
    /// opens a hole in it, and an attribute is text.
    ///
    /// This replaced a source scan over six files, and the scan is the reason
    /// the roster moved. Nine rounds of review walked out of it, every time
    /// through a spelling it did not model: an import, an alias, a turbofish,
    /// a qualified path, a raw identifier, a macro-assembled callee, a line
    /// break between two tokens, a string holding `fn `. Clippy resolves a
    /// PATH, so every one of those is the same call to it. The last round
    /// found the one that ended the argument rather than extending it: a
    /// wrapper taking `sidecar: &str` opens a file, because `File::open`
    /// takes `impl AsRef<Path>` — no text can tell that a parameter is a path
    /// when its own type does not say so, and a compiler never had to ask.
    #[test]
    fn the_lint_is_allowed_only_at_the_choke_points() {
        for (label, source, expected) in compiled_files() {
            // Every ATTRIBUTE that suppresses ANY lint, rather than every
            // mention of this one. Review reached the shipped code through
            // five spellings that never name it: `#[allow(clippy::all)]`,
            // `#[allow(clippy::style)]` — the group it is IN —
            // `#[expect(clippy::style)]`, a crate-level `#![allow(clippy::all)]`
            // and `#[allow(clippy :: disallowed_methods)]`, which rustc accepts
            // and a search for the name walks past. So the roster is the
            // ATTRIBUTES this crate permits, spaces removed, and every other
            // one is refused whatever lint it names.
            //
            // Over the UNCOMMENTED source, or the likeliest author of the next
            // false red is a td-boot developer explaining the allow this crate
            // put in their file.
            let text = uncommented(source);
            let lines: Vec<&str> = text.lines().collect();
            let mut seen = Vec::new();
            for (unit, item) in units(&lines) {
                if !suppresses_a_lint(&unit) {
                    continue;
                }
                assert!(
                    unit == unspaced(ALLOW),
                    "{label} before line {item}: a lint suppression this crate \
                     does not permit: {unit}"
                );
                seen.push(lines.get(item).copied().unwrap_or_default().trim());
            }
            assert_eq!(
                seen, expected,
                "{label}: the allow sits on an item this test does not know — \
                 either an allow moved, or a choke point's signature changed \
                 under MAIN_CHOKE/REALFILE_CHOKE"
            );
        }
    }

    /// Whether `squeezed` — one line with its whitespace removed — is an
    /// ATTRIBUTE that turns a lint off.
    ///
    /// The source as ATTRIBUTE UNITS, whitespace removed, each paired with the
    /// index of the line AFTER it — the item it sits on.
    ///
    /// A line that opens an attribute and does not close it takes the lines
    /// that finish it. Review spread a suppression over four:
    ///
    /// ```ignore
    /// #[cfg_attr(
    ///     all(),
    ///     allow(clippy::all)
    /// )]
    /// ```
    ///
    /// where the opener carries no `allow(` and the `allow(` carries no
    /// opener, so a line-at-a-time scan saw neither half.
    fn units(lines: &[&str]) -> Vec<(String, usize)> {
        // Brackets OUTSIDE strings, or a `reason = "…[…"` joins the rest of
        // the file to itself.
        let unbalanced = |text: &str| {
            let plain = unstringed(text);
            plain.matches('[').count() > plain.matches(']').count()
        };
        let mut out = Vec::new();
        let mut index = 0usize;
        while let Some(line) = lines.get(index) {
            let mut unit = (*line).to_string();
            let mut end = index;
            // A line that BEGINS one, not one that merely mentions `#[` — this
            // scan reads its own source, where `"#["` appears in a string, and
            // joining from there swallowed the file. A multiline attribute
            // sharing a line with code is therefore not joined; that is one
            // coincidence past what review demonstrated and is a known limit.
            // UNSPACED, since `# [cfg_attr(` is an attribute to rustc and was
            // not one to this — the same raw-versus-unspaced split the scan
            // below had already been taught, one layer up. A line merely
            // MENTIONING `#[` in a string still does not open one, because
            // its unspaced form does not start with it.
            let opener = unspaced(&unit);
            if opener.starts_with("#[") || opener.starts_with("#![") {
                while unbalanced(&unit) {
                    end = end.saturating_add(1);
                    match lines.get(end) {
                        None => break,
                        Some(next) => unit.push_str(next),
                    }
                }
            }
            index = end.saturating_add(1);
            // The ITEM an attribute sits on is the next line with anything on
            // it. A blank line or a comment between the two is legal Rust and
            // rustc applies the attribute across either, so reporting the
            // line immediately below reds a file that is correct — and
            // `uncommented` has already blanked the comments, which makes the
            // two cases one skip.
            let mut item = index;
            while lines.get(item).is_some_and(|line| line.trim().is_empty()) {
                item = item.saturating_add(1);
            }
            out.push((unspaced(&unit), item));
        }
        out
    }

    /// The three words that lower a lint's level. `warn(` is here because
    /// `#[warn(clippy::disallowed_methods)]` turns the deny into a warning and
    /// the preflight passes no `-D warnings` — review found it, an eighth
    /// spelling after the seven the scan was built for.
    const LOWERS: [&str; 3] = ["allow(", "expect(", "warn("];

    /// The four spellings that can reach THIS lint: itself, the group it is
    /// in, the group that holds every clippy lint, and rustc's group that
    /// holds every lint at all. A suppression naming none of them cannot
    /// touch it.
    ///
    /// Named rather than refused wholesale, because the wholesale rule was a
    /// cross-crate landmine: `#[allow(clippy::too_many_arguments)]` in
    /// `engine/src/gpt.rs` reds td-install, and its author is an engine
    /// developer with no reason to know td-install compiles that file.
    ///
    /// The first is the SINGULAR, which is a prefix of the plural and so
    /// covers both in one entry. `clippy::disallowed_method` is the lint's
    /// pre-1.55 name and still a RENAME ALIAS: rustc resolves it and the
    /// allow takes effect, which review measured — with the rename note
    /// itself removable on the same line by
    /// `#[allow(renamed_and_removed_lints, …)]`, leaving no diagnostic at
    /// all. So an alias reaches this lint today, and the residual risk is
    /// not only the clippy release that moves it into another group.
    const REACHES: [&str; 4] = [
        concat!("clippy::disallowed", "_method"),
        "clippy::style",
        "clippy::all",
        "warnings",
    ];

    /// An attribute OPENER anywhere in `squeezed`, a word that LOWERS a level,
    /// and a name that REACHES this lint. All three, because any two of them
    /// is a rule about something else: an opener and a word is every
    /// `#[allow(dead_code)]` in these files, and a word alone is every
    /// `.expect(` call in the test half.
    ///
    /// Anywhere rather than at the start, because an attribute may sit
    /// mid-line beside code — review wrote `let x = 1; #[cfg_attr(all(),
    /// allow(…))] File::open(p)`, which begins with neither `#` nor the
    /// attribute's own opener. The opener is `#[` or `#![` and not the whole
    /// `#[allow(`, so a suppression NESTED in a `cfg_attr` is caught by the
    /// same rule rather than by an entry naming it.
    fn suppresses_a_lint(squeezed: &str) -> bool {
        let attribute =
            squeezed.starts_with('#') || squeezed.contains("#[") || squeezed.contains("#![");
        attribute
            && LOWERS.iter().any(|word| squeezed.contains(word))
            && REACHES.iter().any(|name| squeezed.contains(name))
    }

    /// THE OTHER TWO PARTS OF THE MECHANISM, which live outside this file and
    /// would each stop it dead without a word.
    ///
    /// `Cargo.toml` is what turns the roster into an error; without the deny
    /// every entry in `clippy.toml` is advice. And the roster itself is the
    /// roster — its length is pinned because a deleted line refuses nothing
    /// and looks like nothing.
    #[test]
    fn the_roster_and_the_deny_are_both_still_there() {
        // The deny must be a real ENTRY and nothing may countermand it, which
        // are two separate ways the same line stops being in force. A
        // `manifest.contains` is satisfied by a COMMENT, so the key is looked
        // for among a table's entries; and review added `all = { level =
        // "allow", priority = 1 }` beside it, which leaves every assertion
        // here green and silences the deny for the whole crate, a higher
        // priority outranking a plain entry whatever it says. BOTH tables,
        // since `[lints.rust]` can allow the `warnings` group the same way
        // and reaches clippy's lints too. Every entry in either is a bare
        // deny or forbid today, so the check is that their shape has not
        // changed rather than a rule about which levels are permitted.
        let manifest = include_str!("../Cargo.toml");
        // A `#` starts a TOML comment wherever it sits, so it is cut from
        // every line rather than only skipped when it opens one: review wrote
        // `all = { level = "allow", priority = 1 } # = "deny"`, which ends
        // with the deny's own text and passed the shape check below while
        // silencing the lint crate-wide.
        //
        // And a HEADER is a line of its own, not the first text that spells
        // one. `split_once` took a header out of a COMMENT, so a decoy
        // section of denies read as the lint table while the real one below
        // it allowed the group — review's, and green.
        let table = |name: &str| -> Vec<&str> {
            let mut rows = Vec::new();
            let mut inside = false;
            for line in manifest.lines() {
                let bare = line.split('#').next().unwrap_or_default().trim();
                if bare.starts_with('[') {
                    inside = bare == name;
                } else if inside && !bare.is_empty() {
                    rows.push(bare);
                }
            }
            rows
        };
        let clippy = table("[lints.clippy]");
        let rust = table("[lints.rust]");
        assert!(!clippy.is_empty(), "Cargo.toml has no [lints.clippy] table");
        assert!(!rust.is_empty(), "Cargo.toml has no [lints.rust] table");
        assert!(
            clippy.contains(&concat!("disallowed", "_methods = \"deny\"")),
            "Cargo.toml no longer denies the lint, so the roster is advice"
        );
        for line in clippy.iter().chain(rust.iter()) {
            assert!(
                line.ends_with("= \"deny\"") || line.ends_with("= \"forbid\""),
                "Cargo.toml's lint tables gained an entry that is not a plain \
                 deny or forbid, which can outrank the deny above: {line}"
            );
        }
        let roster = include_str!("../clippy.toml");
        assert_eq!(
            roster.matches("{ path = ").count(),
            53,
            "the disallowed-path roster is not the length it was"
        );
        // The ones that are not `std::fs` at all are the easiest to lose to a
        // tidy-up, since they do not look like filesystem calls — and the two
        // `DirEntry` methods are easier still, because nothing in this crate
        // calls them yet and a roster entry for an absent call looks dead.
        for entry in [
            "std::env::set_current_dir",
            "std::path::absolute",
            "std::os::unix::net::UnixListener::bind",
            "std::os::unix::net::UnixDatagram::connect",
            "std::os::unix::net::SocketAddr::from_pathname",
            "std::os::unix::net::UnixDatagram::send_to_addr",
            "std::path::Path::try_exists",
            "std::fs::DirEntry::metadata",
            "std::fs::DirEntry::file_type",
        ] {
            assert!(roster.contains(entry), "the roster no longer holds {entry}");
        }
    }

    /// The one attribute that may open a hole in the roster.
    ///
    /// Spelled in two pieces so this file does not match itself:
    /// `include_str!("main.rs")` reads the whole of it, test half included.
    /// The split is at `#[all`/`ow(` rather than inside the lint name,
    /// because that is the half the scan below keys on.
    const ALLOW: &str = concat!("#[all", "ow(clippy::disallowed_methods)]");

    /// The six files this binary compiles, with the item each allow in them
    /// must sit on — none, for the four that reach no filesystem.
    type Compiled = (&'static str, &'static str, &'static [&'static str]);

    fn compiled_files() -> [Compiled; 6] {
        [
            ("main.rs", include_str!("main.rs"), MAIN_CHOKE.as_slice()),
            (
                "realfile.rs",
                include_str!("../../td-boot/src/realfile.rs"),
                REALFILE_CHOKE.as_slice(),
            ),
            ("gpt.rs", include_str!("../../engine/src/gpt.rs"), [].as_slice()),
            ("fat.rs", include_str!("../../engine/src/fat.rs"), [].as_slice()),
            ("crc32.rs", include_str!("../../engine/src/crc32.rs"), [].as_slice()),
            (
                "protocol.rs",
                include_str!("../../td-boot/src/protocol.rs"),
                [].as_slice(),
            ),
        ]
    }

    /// THE LIST ABOVE IS HAND-KEPT, and a seventh file compiled into this
    /// binary would be read by neither guard — silently, since both count only
    /// what they were handed. Nothing but this relates it to the `#[path]`
    /// declarations it mirrors. The marker is split so this file does not
    /// match itself, as `ALLOW` is: the scan reads its own source.
    ///
    /// Over the UNCOMMENTED source, for the reason the allow scan is: a
    /// comment explaining a `#[path]` declaration is prose, and reading one as
    /// a declaration reds a file that compiles exactly six.
    #[test]
    fn every_compiled_file_is_one_the_guards_read() {
        // WHITESPACE-INSENSITIVE from the marker on: `#[path="x.rs"]` with no
        // spaces is the same attribute to rustc and was invisible to a search
        // for the spelling this file happens to use.
        const MARKER: &str = concat!("#[pa", "th");
        // The marker scan is the one view that keeps STRINGS: it reads the
        // attribute's own path out of one. Everything below reads code
        // instead, and reads it off `plain_source`, which drops both.
        let text = uncommented(include_str!("main.rs"));
        let source = unspaced(&text);
        let source = source.as_str();
        let labels: Vec<&str> = compiled_files().iter().map(|(label, _, _)| *label).collect();
        // The TABLE's own body, because `include_str!` below has to bind a
        // declaration to the file the guards read and a `contains` over the
        // whole source binds it to any TEXT: review spelled the missing
        // include inside a `stringify!`, which satisfied the search while
        // `compiled_files` went on reading the original.
        let table_body = {
            const HEAD: &str = "fn compiled_files() -> [Compiled; 6] {";
            let Some(at) = index_of(&text, HEAD) else {
                panic!("the compiled-file table is not where this scan looks for it")
            };
            let tail = text.get(at..).unwrap_or_default();
            let mut depth = 0usize;
            let mut end = tail.len();
            for (n, c) in tail.char_indices() {
                match c {
                    '{' => depth = depth.saturating_add(1),
                    '}' => {
                        depth = depth.saturating_sub(1);
                        if depth == 0 {
                            end = n;
                            break;
                        }
                    }
                    _ => {}
                }
            }
            unspaced(tail.get(..end).unwrap_or_default())
        };
        let mut declared = 0usize;
        let mut from = 0usize;
        while let Some(at) = source.get(from..).and_then(|rest| index_of(rest, MARKER)) {
            let start = from.saturating_add(at).saturating_add(MARKER.len());
            let rest = source.get(start..).unwrap_or_default();
            // Advanced BEFORE the shape test below, which may skip this
            // occurrence — a `continue` past the cursor is a loop that never
            // ends, and this one hung the suite until it was moved up.
            from = start;
            // The attribute's own shape, not merely its name: unspaced, a
            // declaration is `#[path="…"]` and this test's own prose about
            // `#[path]` is not. Written as a prefix test rather than by
            // trimming, or the prose matches and the path read is garbage —
            // which is how this was caught.
            let Some(quoted) = rest.strip_prefix("=\"") else {
                continue;
            };
            let path = quoted.split('"').next().unwrap_or_default();
            let name = path.rsplit('/').next().unwrap_or_default();
            assert!(
                labels.contains(&name),
                "{path} is compiled into this binary and scanned by neither guard"
            );
            // …and the guard reads THAT file rather than one with the same
            // basename. `compiled_files` names each by its `include_str!`
            // argument, so requiring the declared path to appear as one is
            // what ties the two together: review pointed out that moving a
            // declaration to `../../alternate/gpt.rs` still matches the
            // `gpt.rs` label while the guards go on reading the original.
            assert!(
                table_body.contains(&format!("include_str!(\"{path}\")")),
                "{path} is compiled in, but the guards read a different file \
                 of that name"
            );
            declared = declared.saturating_add(1);
        }
        // …and the list holds nothing ELSE, or a file could be dropped from
        // the crate while its entry went on standing in for it.
        assert_eq!(
            declared.saturating_add(1),
            labels.len(),
            "{declared} `#[path]` declarations against {} scanned files",
            labels.len()
        );
        // …and no file reaches this binary any OTHER way. A `#[path]` is not
        // how Rust normally names a second file: `mod escape;` compiles
        // `src/escape.rs` with no attribute to count, and `include!` splices
        // one into this file outright. Either is a seventh compiled source
        // carrying a crate-level `#![allow]` and any filesystem call it
        // likes, with both guards passing — review's, and the reason the loop
        // below counts DECLARATIONS rather than trusting the attribute.
        // EVERY compiled file, not this one alone. A seventh file arrives
        // through whichever of the six declares it, and until review measured
        // it the three checks below ran over `main.rs` only: an
        // `include!("spliced.rs")` in `gpt.rs` — which resolves to the same
        // file for both crates, so it is the realistic shape rather than a
        // contrivance — spliced in a crate-level allow and a raw
        // `File::open`, clippy exit 0 and all four guards green. `pub mod
        // extra;` there did the same. The comment this replaces claimed "no
        // file reaches this binary any OTHER way"; it did.
        for (label, text, _) in compiled_files() {
            // ONE pass, and one that refuses what it cannot lex. A raw string
            // holding an odd quote desynchronises a composed strip for the
            // rest of the file, and review measured the silent pass that
            // follows: `mod escape;` below one is invisible, the module count
            // still agrees, and the module compiles with an allow of its own.
            // A BLOCK comment is the other refusal, and it is a twelfth
            // suppression rather than a lexing nicety —
            // `#[allow(clippy::/*x*/all)]` resolves for rustc, does not match
            // the roster, and was measured green over a raw `File::open`.
            let Some(plain_file) = plain_source(text) else {
                panic!(
                    "{label} holds a raw string or a block comment, either of \
                     which can spell a declaration or a lint path the guards \
                     below cannot read"
                )
            };
            let unspaced_file = unspaced(&plain_file);
            // Split like `MARKER` and `ALLOW`: written whole, the assertion is
            // an instance of what it refuses, since the scan reads its own
            // source. The bare macro NAME, not one delimiter of it: Rust takes
            // `(`, `[` and `{` alike, and a check naming the first walks past
            // the other two. `include_str!` does not match, its `!` sitting
            // elsewhere.
            assert!(
                !unspaced_file.contains(concat!("inclu", "de!")),
                "{label} splices a file into itself, where neither guard can \
                 tell it apart from that file's own source"
            );
            // A MACRO can assemble an attribute out of pieces no unit holds: a
            // `#[$attr]` in a definition and `allow(clippy::all)` at the call
            // site are two texts, and the scan reads text. Nothing here
            // defines one, and these six files are compiled with no external
            // crate to import one from, so refusing the DEFINITION closes it.
            //
            // Over the UNSPACED, UNSTRINGED file rather than at a line start:
            // `macro_rules ! wrapped` compiles, defines the macro, and matched
            // neither the line-start test nor the keyword roster — as did the
            // same name anywhere but a line start. Dropping the strings is
            // what keeps the roster naming it above from matching itself, the
            // line start having been doing that job.
            assert!(
                !unspaced_file.contains("macro_rules!"),
                "{label} defines a macro, which can assemble a lint \
                 suppression the allow scan cannot read"
            );
            // …and no module of its own. `main.rs` declares the five; any
            // other file declaring one compiles a seventh source.
            let mods = declarations(&plain_file)
                .into_iter()
                .filter(|(keyword, head)| *keyword == "mod" && head.trim_end().ends_with(';'))
                .count();
            let want = if label == "main.rs" { declared } else { 0 };
            assert_eq!(
                mods, want,
                "{label} declares {mods} file modules against {want} `#[path]` \
                 declarations — a module without one compiles a file neither \
                 guard reads"
            );
        }
    }

    /// `main.rs`'s one choke point, as the line under its allow reads.
    const MAIN_CHOKE: [&str; 1] = ["mod paths {"];

    /// `realfile.rs`'s two, which are functions rather than a module: td-boot
    /// compiles that file too and has no `clippy.toml`, so the allow there is
    /// inert for it and load-bearing here.
    const REALFILE_CHOKE: [&str; 2] = [
        "pub fn open_real_file(path: &Path, label: &str) -> io::Result<(File, Metadata)> {",
        "fn open_checked(path: &Path, expected: &Metadata, label: &str) -> io::Result<(File, Metadata)> {",
    ];

    /// EVERY WRAPPER IN A CHOKE POINT NAMES EVERY PATH IT TAKES.
    ///
    /// The half the compiler cannot check. Clippy says WHERE a call may be;
    /// whether the wrapper holding it puts the path in the message is a
    /// question about the wrapper's own text, and the wrappers are short
    /// enough for text to answer it.
    ///
    /// It earns its place: review wrote a `rename` that named only `from`,
    /// and the destination is the argument an operator is likelier to have
    /// got wrong.
    #[test]
    fn every_choke_wrapper_names_every_path_it_takes() {
        let mut checked = 0;
        for (label, source, markers) in compiled_files() {
            for marker in markers {
                let body = uncommented(region(label, source, marker));
                // Plainness FIRST: the two below read the region with its
                // strings dropped, and that strip is only sound once the
                // constructs it cannot lex have been refused.
                reads_as_plain_code(label, marker, &body);
                hands_out_no_capability(label, marker, &body);
                checked += names_every_path(label, marker, &body);
            }
        }
        // A naming test that found nothing to check would pass whatever the
        // wrappers did.
        assert_eq!(checked, 19, "{checked} wrappers were checked");
    }

    /// The text of the item opened at `marker`, up to the next line that is a
    /// lone `}`.
    ///
    /// A brace WALK would have to know which braces are inside a string, and
    /// that lexer is what this commit deleted. Every item here closes at
    /// column 0 — `mod paths` and both `realfile.rs` functions are top-level
    /// — which is a property of the file's layout that a reader can check and
    /// `rustfmt` keeps.
    fn region<'a>(label: &str, source: &'a str, marker: &str) -> &'a str {
        let Some(at) = index_of(source, marker) else {
            panic!("{label}: the choke point {marker} is not there any more");
        };
        let tail = source.get(at..).unwrap_or_default();
        let Some(end) = index_of(tail, "\n}") else {
            panic!("{label}: the choke point {marker} never closes");
        };
        tail.get(..end).unwrap_or_default()
    }

    /// `code` without its line comments, which discuss `.at(path)` and
    /// `File::open` in prose and would otherwise answer for the code.
    fn uncommented(code: &str) -> String {
        let mut out = String::with_capacity(code.len());
        for line in code.lines() {
            match comment_at(line) {
                Some(at) => out.push_str(line.get(..at).unwrap_or_default()),
                None => out.push_str(line),
            }
            out.push('\n');
        }
        out
    }

    /// Where `line`'s comment starts, if it has one — the first `//` OUTSIDE a
    /// string literal.
    ///
    /// Outside, because review hid an attribute behind one: in
    /// `const _: &str = "//"; #[allow(clippy::all)]` the slashes are data, and
    /// a strip that cut there deleted a suppression rustc sees. The toggle is
    /// per LINE, so a quote char literal earlier on the same line still fools
    /// it; that is two deliberate coincidences deep and is a known limit
    /// rather than a closed one.
    fn comment_at(line: &str) -> Option<usize> {
        let mut in_str = false;
        let mut esc = false;
        let mut slash = false;
        for (at, ch) in line.char_indices() {
            if in_str {
                match ch {
                    _ if esc => esc = false,
                    '\\' => esc = true,
                    '"' => in_str = false,
                    _ => {}
                }
                slash = false;
                continue;
            }
            if ch == '"' {
                in_str = true;
                slash = false;
                continue;
            }
            if ch == '/' && slash {
                return at.checked_sub(1);
            }
            slash = ch == '/';
        }
        None
    }

    /// A CHOKE POINT MAY NOT HAND ITS CAPABILITY OUT.
    ///
    /// The allow makes one region able to open any path. Nothing stopped that
    /// region EXPORTING the ability, and review wrote both spellings green:
    ///
    /// ```ignore
    /// pub fn opener() -> impl Fn(&Path) -> io::Result<File> { |p| File::open(p) }
    /// pub const OPENER: fn(&Path) -> io::Result<File> = |p| File::open(p);
    /// ```
    ///
    /// Either lets any call site in the crate open any path with the bare
    /// `io::Error` this whole mechanism exists to replace, and neither
    /// declares a `Path` PARAMETER — so the naming loop below skips both and
    /// the wrapper count stays right. A closure can only be NAMED with `Fn`,
    /// and a function pointer is the only other way to carry one, so refusing
    /// those spellings refuses the shape rather than two examples of it.
    ///
    /// Over the region with its WHITESPACE REMOVED, and the visibility read
    /// the same way. `fn (&Path)` is the same type as `fn(&Path)`, and
    /// `pub(crate)` is `pub ` with no space after it — review wrote both and
    /// walked past a check reading raw text. Every export here is therefore a
    /// plain `pub fn`, which also refuses `pub(crate)`, `pub unsafe fn` and
    /// `pub static`: fifteen short wrappers need none of them, and a shape
    /// that arrives is a decision rather than an accident.
    fn hands_out_no_capability(label: &str, region: &str, body: &str) {
        // With STRINGS dropped, so a message naming a shape is data rather
        // than an export — and read as DECLARATIONS rather than as lines. A
        // line is not the unit: review put a newline after `impl`, a tab
        // after it, and an attribute before it on the same line, and all
        // three walked past a rule anchored at a line start. `rustfmt` writes
        // the same shape by itself once a signature outgrows the width.
        let plain = unstringed(body);
        let squeezed = unspaced(&plain);
        for shape in ["Fn(", "Fn<", "FnMut(", "FnOnce(", "fn("] {
            assert!(
                !squeezed.contains(shape),
                "{label}: {region} names `{shape}`, so it can hand the \
                 capability out"
            );
        }
        // A head ends at the `{` or `;` that CLOSES it, not at the first one
        // in it. `[u8; 4]` supplies a `;` inside brackets, and cutting there
        // handed the return-type read half a signature — a false red naming
        // the wrong problem, which review measured on a shape this crate has
        // every reason to write.
        // …and a parenthesised parameter TYPE does not end the list, for the
        // same reason and with a worse consequence: every parameter after it
        // went unread, so a wrapper could take a path it was never asked to
        // name.
        assert_eq!(
            path_parameters("fn f(pair: (u32, u32), path: &Path) -> io::Result<()> {"),
            vec!["path".to_string()],
            "a parenthesised parameter type cuts the argument list short"
        );
        let arrayed = "pub fn read_first(path: &Path, buf: &mut [u8; 4]) -> io::Result<()> {";
        assert!(
            declarations(arrayed)
                .iter()
                .any(|(keyword, head)| *keyword == "pub" && *head == arrayed),
            "an array type in a parameter cuts a signature short"
        );
        for (keyword, head) in declarations(&plain) {
            let head = unspaced(head);
            // An ITEM a caller outside can reach carries a capability whether
            // or not it spells `Fn`, which is what the shapes above missed:
            // review wrote `impl crate::OpenAnything for crate::AnyOpener`
            // into the region, with the trait and the type declared outside
            // it, and every rule here walked past — a trait-impl method has
            // no visibility of its own, and `&self` plus a `&str` is no
            // `Path` parameter to check. So the item kinds that can carry one
            // are pinned WHOLE, and a region declares exactly what it
            // declares today.
            if keyword != "pub" {
                assert!(
                    PINNED_ITEMS.iter().any(|item| unspaced(item) == head),
                    "{label}: {region} declares an item this test does not \
                     know, which may carry a capability with no `Fn` in it: \
                     {head}"
                );
                continue;
            }
            assert!(
                head.starts_with("pubfn"),
                "{label}: {region} exports something that is not a plain \
                 `pub fn`: {head}"
            );
            // …and every one takes a PATH. `File::open` accepts an
            // `impl AsRef<Path>`, so review's `pub fn open_named(name: &str)`
            // is the whole capability with the bare error, declares no path
            // to name, and is a plain `pub fn` — three checks walked past it.
            // A wrapper with no `Path` parameter could not name one anyway,
            // which is why refusing it costs nothing.
            assert!(
                head.contains("&Path"),
                "{label}: {region} exports a wrapper that takes no `&Path`, \
                 so it has no path to name: {head}"
            );
            // …and hands back one of two shapes. An allowlist rather than
            // more refused spellings, because a capability can be named by
            // an ALIAS declared outside the region — `type Cap = fn(&Path)
            // -> io::Result<File>` and a wrapper returning `Cap` spells no
            // shape above — and no text in the region can tell what a name
            // means. Read after the LAST `->`, which is the return arrow: a
            // parameter cannot carry one, since the types that do are the
            // `Fn`/`fn(` shapes refused above. Over the whole head it was a
            // check a PARAMETER could satisfy — review wrote `_seed:
            // io::Result<()>` beside a return type naming an alias. Matched
            // as a SUFFIX of the type rather than straight after the arrow,
            // or `-> std::io::Result<File>` reds. What this cannot close is
            // the same capability smuggled INSIDE an allowed return type;
            // that is the `AsRef<Path>` limit again, and the compiler is the
            // thing that could tell.
            let returns = index_of_last(&head, "->")
                .and_then(|at| head.get(at..))
                .unwrap_or_default();
            assert!(
                returns.contains("io::Result<") || returns.contains("Option<"),
                "{label}: {region} exports a wrapper returning something \
                 other than `io::Result` or `Option`: {head}"
            );
            // A `ReadDir` handed back is the one wrapper shape DESIGN.md's
            // D10 forbids in prose: reading a directory fails once per
            // ENTRY, and the roster cannot hold `ReadDir::next` because that
            // path does not resolve. So the rule lands here — a wrapper must
            // consume the iterator and name the directory itself.
            assert!(
                !returns.contains("ReadDir"),
                "{label}: {region} hands back a `ReadDir`, whose per-entry \
                 errors name nothing: {head}"
            );
        }
    }

    /// The DECLARATIONS `plain` opens: each keyword at an identifier
    /// boundary, paired with its head — the text through to the `{` or `;`
    /// that ends it.
    ///
    /// A boundary on BOTH sides, since `mod` is a prefix of `mode`, `impl` of
    /// `implementation` and `pub` of `public`; and a head rather than a line,
    /// since a declaration may be written across as many as it likes.
    fn declarations(plain: &str) -> Vec<(&'static str, &str)> {
        const KEYWORDS: [&str; 5] = ["impl", "trait", "mod", "macro_rules!", "pub"];
        let ident = |c: char| c.is_alphanumeric() || c == '_';
        let mut out = Vec::new();
        for keyword in KEYWORDS {
            let mut from = 0usize;
            while let Some(at) = plain.get(from..).and_then(|rest| index_of(rest, keyword)) {
                let start = from.saturating_add(at);
                from = start.saturating_add(keyword.len());
                let before = plain.get(..start).and_then(|t| t.chars().next_back());
                let after = plain.get(from..).and_then(|t| t.chars().next());
                if before.is_some_and(ident) || after.is_some_and(ident) {
                    continue;
                }
                let tail = plain.get(start..).unwrap_or_default();
                // Walked rather than searched: this file is staged into a
                // recipe, and the recipe scan refuses the bare token an
                // iterator search would spell — which is why `index_of` and
                // `index_of_last` exist here at all.
                // At BRACKET DEPTH ZERO, or an array type ends the head: the
                // `;` in `buf: &mut [u8; 4]` cut a signature in half and the
                // truncated text then failed the return-type read, reporting
                // a wrapper as exporting the wrong thing. A false red with
                // the wrong diagnosis, which review measured.
                let mut end = tail.len();
                let mut depth = 0usize;
                for (at, c) in tail.char_indices() {
                    match c {
                        '[' | '(' => depth = depth.saturating_add(1),
                        ']' | ')' => depth = depth.saturating_sub(1),
                        '{' | ';' if depth == 0 => {
                            end = at.saturating_add(c.len_utf8());
                            break;
                        }
                        _ => {}
                    }
                }
                out.push((keyword, tail.get(..end).unwrap_or_default()));
            }
        }
        out
    }

    /// The three declarations the two choke points open, pinned whole. A
    /// fourth is a decision someone makes on purpose.
    ///
    /// A `use` is not among the keywords that reach this, because `pub use`
    /// is already refused as a non-`pub fn` export and a private one
    /// re-exports nothing.
    const PINNED_ITEMS: [&str; 3] = [
        "mod paths {",
        "trait NamePath<T> {",
        "impl<T> NamePath<T> for io::Result<T> {",
    ];

    /// The char literals a choke point may not hold, written as escapes so
    /// this file is not itself an instance of what it refuses.
    ///
    /// Each would desynchronise a COUNTER this test runs on the region: the
    /// two quotes are a delimiter to `unstringed`'s toggle, and the two braces
    /// are an item boundary to the brace count below. A sibling scan in
    /// `builder` NEUTRALISES the same literals instead, because it reads whole
    /// files and cannot refuse one; nineteen short wrappers can afford the
    /// refusal, and refusing is the smaller thing to be right about.
    const REFUSED_CHARS: [&str; 4] = ["'\u{22}'", "'\\\u{22}'", "'{'", "'}'"];

    /// `unstringed`'S TOGGLE IS ONLY RIGHT IF THE REGION IS PLAIN, AND THE
    /// REGION IS ONLY THE ITEM IF NOTHING CUT IT SHORT.
    ///
    /// Four constructs break one or the other, and each is refused rather than
    /// lexed. A RAW string holding a quote ends the literal early, so its
    /// content is read as code — at ANY hash count, which review found the
    /// two-spelling check missing: `r##"\"##;` desynchronises the toggle for
    /// the rest of the region. A `'"'` char literal opens a string that
    /// swallows the rest of the function. A BLOCK comment can hold a line that
    /// begins `}`, which is where `region` stops. And a string SPANNING LINES
    /// can hold one too — that is the truncation, and it is why every line
    /// must close the strings it opens.
    ///
    /// The brace count is what proves the region is the whole item, and on its
    /// own it does NOT: review appended
    ///
    /// ```ignore
    /// const NOTE: &str = "usage:
    /// }";
    /// pub const OPENER: fn(&Path) -> io::Result<File> = |p| File::open(p);
    /// ```
    ///
    /// at MODULE level, where every wrapper before it is balanced, so the
    /// module's own brace is still the only one left open and the count read
    /// 1 — with the handout sitting after the cut, scanned by nothing. It
    /// takes all four refusals for the count to mean anything: with no
    /// line-spanning string, no block comment and no brace char literal, a
    /// `}` at column 0 is code closing the one brace the count says is open,
    /// which is the item's own.
    fn reads_as_plain_code(label: &str, region: &str, body: &str) {
        // Over the STRING-STRIPPED body, or every message ending in the
        // letter `r` is a raw string to this: review found `concat!("cannot
        // open dir")` refused, and `"for"`, `"error"`, `"other"` and `"/usr"`
        // with it. The strip is sound for the question — a real raw string
        // still shows its `r` and its opening quote, since the strip keeps
        // both delimiters and only drops what is between them.
        assert!(
            !opens_a_raw_string(&unstringed(body)),
            "{label}: {region} holds a raw string, which the string strip \
             cannot lex"
        );
        assert!(
            !body.contains("/*"),
            "{label}: {region} holds a block comment, which can hold the line \
             that ends the item"
        );
        for spelling in REFUSED_CHARS {
            assert!(
                !body.contains(spelling),
                "{label}: {region} holds the char literal {spelling}, which \
                 the string strip or the brace count reads as punctuation"
            );
        }
        // A string that does not close on its own line is what cuts a region
        // short, and it is visible from the CUT side: the truncated body ends
        // inside the literal. `unstringed` keeps both delimiters and drops the
        // content, so an odd count of them is an unclosed one.
        for (n, line) in body.lines().enumerate() {
            assert_eq!(
                unstringed(line).matches('"').count() % 2,
                0,
                "{label}: {region} line {} holds a string literal that does \
                 not close on its own line: {line}",
                n.saturating_add(1)
            );
        }
        // …and the region is the WHOLE item. `region` cuts at the first line
        // that is a lone `}`; counting braces outside strings is what notices
        // a cut anywhere inside a wrapper, and the refusals above are what
        // make the count sound at module level too.
        let plain = unstringed(body);
        let opened = plain.matches('{').count();
        let closed = plain.matches('}').count();
        assert_eq!(
            opened.saturating_sub(closed),
            1,
            "{label}: {region} was cut short — {opened} `{{` and {closed} `}}` \
             where the item's own opening brace should be the only one left"
        );
    }

    /// Whether `body` opens a RAW string, at any hash count.
    ///
    /// `r"` and `r#"` were checked by name, and `r##"` — which
    /// `builder`'s own gate bodies use — walked past both.
    /// `text` with its comments gone and the CONTENT of its strings and char
    /// literals blanked, in ONE pass — or `None` where it meets a construct it
    /// does not lex.
    ///
    /// Two passes cannot do this soundly and this file had two: stripping
    /// comments first reads a `//` INSIDE a string as a comment, and stripping
    /// strings first reads a `"` inside a COMMENT as a string. Both are live
    /// here — main.rs continues a string across lines — so the composed view
    /// really was desynchronised, and review measured what that buys: a `mod
    /// escape;` below the desynchronisation is invisible to the module count,
    /// which still agrees, while the module compiles with a lint suppression
    /// of its own.
    ///
    /// The region scans still REFUSE rather than lex, and that stays the rule
    /// for a region: nineteen short wrappers can afford it. A whole FILE
    /// cannot — main.rs holds sixteen char literals a counter would
    /// desynchronise on, and refusing them would refuse this file.
    ///
    /// What it does not lex is a raw string and a block comment, and neither
    /// is a caveat: it refuses both, none of the six compiled files holds
    /// either (measured), and the refusal is what the raw-string check this
    /// replaces could not state soundly.
    fn plain_source(text: &str) -> Option<String> {
        let chars: Vec<char> = text.chars().collect();
        let mut out = String::with_capacity(text.len());
        let mut i = 0usize;
        while let Some(&c) = chars.get(i) {
            let next = chars.get(i.saturating_add(1)).copied();
            if c == '/' && next == Some('/') {
                while chars.get(i).is_some_and(|ch| *ch != '\n') {
                    i = i.saturating_add(1);
                }
                continue;
            }
            if c == '/' && next == Some('*') {
                return None;
            }
            // A raw string, at any hash count and under any prefix. The `r`
            // must open one rather than end an identifier, or `for r"` — an
            // ordinary string after a variable named `r` — refuses the file.
            if c == 'r' || (matches!(c, 'b' | 'c') && next == Some('r')) {
                let at = if c == 'r' { i } else { i.saturating_add(1) };
                let ident = |ch: char| ch.is_alphanumeric() || ch == '_';
                let opens = !at
                    .checked_sub(1)
                    .and_then(|p| chars.get(p))
                    .is_some_and(|ch| ident(*ch));
                let mut j = at.saturating_add(1);
                while chars.get(j) == Some(&'#') {
                    j = j.saturating_add(1);
                }
                if opens && chars.get(j) == Some(&'"') {
                    return None;
                }
            }
            if c == '\'' {
                if let Some(len) = char_literal_len(&chars, i) {
                    out.push_str("'x'");
                    i = i.saturating_add(len);
                    continue;
                }
            }
            if c == '"' {
                out.push('"');
                i = i.saturating_add(1);
                let mut esc = false;
                while let Some(&ch) = chars.get(i) {
                    i = i.saturating_add(1);
                    if esc {
                        esc = false;
                    } else if ch == '\\' {
                        esc = true;
                    } else if ch == '"' {
                        out.push('"');
                        break;
                    }
                    // Newlines survive so what is left has the line structure
                    // the file had, a continued string included.
                    if ch == '\n' {
                        out.push('\n');
                    }
                }
                continue;
            }
            out.push(c);
            i = i.saturating_add(1);
        }
        Some(out)
    }

    /// The length in CHARS of the char literal at `at`, or `None` where that
    /// `'` opens a LIFETIME or a label.
    ///
    /// Decided by LOOKAHEAD, as rustc decides it: a literal is one character
    /// or one escape between two quotes, and a lifetime has no closing quote.
    /// `builder`'s `lex` carries the same function for the same reason —
    /// neither crate may depend on the other, both being their own workspace.
    fn char_literal_len(chars: &[char], at: usize) -> Option<usize> {
        let get = |n: usize| chars.get(at.saturating_add(n)).copied();
        let hex = |n: usize| get(n).is_some_and(|c| c.is_ascii_hexdigit());
        if get(0) != Some('\'') {
            return None;
        }
        let close = match get(1)? {
            '\\' => match get(2)? {
                'x' if hex(3) && hex(4) => 5,
                'x' => return None,
                'u' => {
                    if get(3) != Some('{') {
                        return None;
                    }
                    let digits = (4..10).take_while(|n| hex(*n)).count();
                    let end = 4usize.saturating_add(digits);
                    if get(end) != Some('}') {
                        return None;
                    }
                    end.saturating_add(1)
                }
                _ => 3,
            },
            '\'' | '\n' | '\r' | '\t' => return None,
            _ => 2,
        };
        (get(close) == Some('\'')).then(|| close.saturating_add(1))
    }

    fn opens_a_raw_string(body: &str) -> bool {
        let chars: Vec<char> = body.chars().collect();
        chars.iter().enumerate().any(|(at, c)| {
            if *c != 'r' {
                return false;
            }
            let mut j = at.saturating_add(1);
            while chars.get(j) == Some(&'#') {
                j = j.saturating_add(1);
            }
            chars.get(j) == Some(&'"')
        })
    }

    /// How many wrappers were checked, so the caller can refuse a run that
    /// checked none.
    ///
    /// A marker's PRESENCE is what this reads, not that the error carries it.
    /// `let _ = path.display(); File::open(path)` names the path and returns
    /// the bare error, and review wrote it green. Closing that needs the
    /// naming tied to the value returned, which is a question about
    /// expressions rather than about text; the limit is recorded rather than
    /// papered over.
    fn names_every_path(label: &str, region: &str, body: &str) -> usize {
        let pieces = functions(body);
        // EVERY `fn` in the region is one of them. Without this the
        // enumeration counts only what it could parse, so a spelling it stops
        // at is a wrapper that vanishes rather than one that reds — the same
        // "count only what they were handed" failure the compiled-file list
        // has, one layer down, and the shape three separate rounds of review
        // walked out through (`fn  x`, a name on the next line, `x (`,
        // `x<T>(`). Pinned against the KEYWORDS rather than against a number,
        // so it holds however many wrappers there come to be.
        let keywords = {
            let plain = unstringed(body);
            let mut count = 0usize;
            let mut from = 0usize;
            while let Some(hit) = plain.get(from..).and_then(|rest| index_of(rest, "fn")) {
                let at = from.saturating_add(hit);
                from = at.saturating_add("fn".len());
                let before = plain.get(..at).and_then(|t| t.chars().next_back());
                let after = plain.get(from..).and_then(|t| t.chars().next());
                let ident = |c: char| c.is_alphanumeric() || c == '_';
                if !before.is_some_and(ident) && !after.is_some_and(ident) {
                    count = count.saturating_add(1);
                }
            }
            count
        };
        assert_eq!(
            pieces.len(),
            keywords,
            "{label}: {region} declares {keywords} functions and this read \
             {} — a spelling this enumeration stops at is a wrapper nothing \
             below ever asks to name its path",
            pieces.len()
        );
        let mut checked = 0;
        for piece in pieces {
            // A trait's method SIGNATURE has no body to name anything in.
            // Its piece runs on to the next declaration, so holding a `{` does
            // not tell the two apart — which comes FIRST does.
            let opens = index_of(piece, "{");
            let ends = index_of(piece, ";");
            if opens.is_none() || (ends.is_some() && ends < opens) {
                continue;
            }
            // A path parameter is spelled as a `Path` here, so the signature
            // says which arguments are ones. Taken GENERICALLY it does not:
            // review wrote `open_any(path: impl AsRef<Path>)` naming
            // `/not-the-file` for every failure, and the loop below found no
            // parameter to require a naming for.
            assert!(
                !piece.contains("AsRef<") && !piece.contains("Into<"),
                "{label}: {region} takes a path generically, where this cannot \
                 read it off the signature: {piece}"
            );
            let names = path_parameters(piece);
            if names.is_empty() {
                continue;
            }
            checked += 1;
            let body = unspaced(&unstringed(piece));
            // The one wrapper whose error never becomes a message is exempt
            // BY NAME, because a shape was tried twice and broken twice: a
            // bare `.ok()` anywhere excused every path in the function, and
            // narrowing that to `(path).ok()` still let `let _ =
            // std::fs::metadata(path).ok();` sit beside an unnamed
            // `File::open(path)`. The `(` is part of the name, or
            // `metadata_if_present_and_open` inherits the exemption.
            //
            // And the RETURN TYPE, which is what actually makes it safe:
            // nothing that hands back an `Option` can propagate an error at
            // all, so there is none to leave unnamed. A wrapper of this name
            // that started returning `io::Result` would keep the exemption
            // without it and could then propagate one raw.
            let discarded = piece.trim_start().starts_with(DISCARDS_ITS_ERROR)
                && piece.contains("-> Option<")
                && names
                    .iter()
                    .any(|name| body.contains(&format!("({name}).ok()")));
            for name in names {
                // The three ways a name reaches a message, spelled out rather
                // than counted. Counting is what review broke: `p` occurring
                // twice in `File::open(p).at(Path::new("/wrong"))` is once as
                // the argument and once inside the word `open`, so a wrapper
                // reporting an unrelated path passed. A wrapper that names its
                // path some fourth way reds here and adds its spelling.
                let spellings = [
                    format!(".at({name})"),
                    format!("named(error,{name})"),
                    format!("{name}.display()"),
                ];
                assert!(
                    spellings.iter().any(|spelling| body.contains(spelling)) || discarded,
                    "{label}: {region} takes `{name}` and does not name it: {piece}"
                );
            }
        }
        checked
    }

    /// The functions `body` declares, cut at each `fn NAME(` rather than at
    /// each `fn `, so a string or a name ending in `fn` opens nothing.
    ///
    /// A RAW identifier is a name too. `pub fn r#open_raw(path: &Path)` is a
    /// wrapper the enumeration stopped reading at the `#`, so it declared no
    /// path, was never counted, and could return the bare error while the
    /// pinned count stayed right — review's, and the same class as every
    /// spelling that drove the roster to clippy.
    fn functions(body: &str) -> Vec<&str> {
        let mut heads = Vec::new();
        let mut from = 0;
        while let Some(hit) = body.get(from..).and_then(|rest| index_of(rest, "fn")) {
            let at = from.saturating_add(hit);
            let after = at.saturating_add("fn".len());
            from = after;
            // `fn` as a WORD, and whatever whitespace follows it. The literal
            // `fn ` was the keyword plus exactly one space, so `pub fn  name`
            // and a `fn` with its name on the next line — both valid, and the
            // second is what `rustfmt` writes for a long one — declared a
            // wrapper this enumeration never saw: no name, no path, never
            // counted, and the pinned nineteen still right. Review's.
            let before = body.get(..at).and_then(|t| t.chars().next_back());
            if before.is_some_and(|c| c.is_alphanumeric() || c == '_') {
                continue;
            }
            let spaced = body.get(after..).unwrap_or_default();
            if !spaced.starts_with(char::is_whitespace) {
                continue;
            }
            let tail = spaced.trim_start();
            // The piece begins at the NAME, as it did when the keyword and
            // its one space were matched together.
            let name_at = after.saturating_add(spaced.len().saturating_sub(tail.len()));
            let tail = tail.strip_prefix("r#").unwrap_or(tail);
            let name: String = tail
                .chars()
                .take_while(|ch| ch.is_alphanumeric() || *ch == '_')
                .collect();
            // WHITESPACE and a GENERIC LIST both sit between a name and its
            // arguments, and requiring the `(` to touch the name stopped this
            // enumeration at either: `open_spaced (path: &Path)` and
            // `open_generic<T>(path: &Path, _t: T)` are wrappers it never
            // saw, so neither declared a path, neither was counted, and both
            // could hand back the bare error with the pinned count still
            // right. Review's, and the mirror of the two spellings the round
            // before — which is why the count below is now pinned to the
            // `fn`s in the region rather than to what this returns.
            let rest = tail.get(name.len()..).unwrap_or_default().trim_start();
            let rest = match rest.strip_prefix('<') {
                Some(generics) => {
                    let mut depth = 1usize;
                    let mut end = generics.len();
                    let mut prev = '<';
                    for (i, c) in generics.char_indices() {
                        match c {
                            '<' => depth = depth.saturating_add(1),
                            // Not the `>` of a `->`, which closes nothing.
                            '>' if prev != '-' => {
                                depth = depth.saturating_sub(1);
                                if depth == 0 {
                                    end = i.saturating_add(1);
                                    break;
                                }
                            }
                            _ => {}
                        }
                        prev = c;
                    }
                    generics.get(end..).unwrap_or_default().trim_start()
                }
                None => rest,
            };
            if !name.is_empty() && rest.starts_with('(') {
                heads.push(name_at);
            }
        }
        let mut out = Vec::new();
        for (index, start) in heads.iter().enumerate() {
            let end = heads.get(index + 1).copied().unwrap_or(body.len());
            out.push(body.get(*start..end).unwrap_or_default());
        }
        out
    }

    /// `code` with every space taken out, so the spellings above match a
    /// wrapper however it is wrapped across lines.
    fn unspaced(code: &str) -> String {
        code.chars().filter(|ch| !ch.is_whitespace()).collect()
    }

    /// `code` with the CONTENTS of its string literals removed.
    ///
    /// A marker inside a MESSAGE answers for the code otherwise. Review wrote
    /// `format!("open failed, see .at(path) below: {error}")` into a wrapper
    /// that named nothing and watched it pass: `uncommented` takes comments
    /// and nothing took strings, so the prose satisfied a check the code did
    /// not. Every naming this looks for is CODE — `.at(path)`,
    /// `named(error, path)`, `path.display()` — so dropping string content can
    /// only lose a fake one.
    ///
    /// A plain quote TOGGLE, which is sound only because
    /// `reads_as_plain_code` has refused the two constructs that break one.
    fn unstringed(code: &str) -> String {
        let mut out = String::with_capacity(code.len());
        let mut in_str = false;
        let mut esc = false;
        for ch in code.chars() {
            if in_str {
                match ch {
                    _ if esc => esc = false,
                    '\\' => esc = true,
                    '"' => {
                        in_str = false;
                        out.push(ch);
                    }
                    _ => {}
                }
                continue;
            }
            if ch == '"' {
                in_str = true;
            }
            out.push(ch);
        }
        out
    }

    /// The path parameters `piece` declares, read off its signature.
    ///
    /// By the TYPE naming `Path`, which is what a wrapper here writes and not
    /// what every path-taking parameter must be: `File::open` accepts an
    /// `impl AsRef<Path>`, so a `&str` — or an `&OsStr`, the same class and
    /// the same blind spot — is a path too and this cannot tell. The generic
    /// form is refused above; the concrete one is a limit, and a small one now
    /// that it applies to nineteen short functions the compiler has already
    /// fenced rather than to a whole crate.
    fn path_parameters(piece: &str) -> Vec<String> {
        let Some((_, after)) = piece.split_once('(') else {
            return Vec::new();
        };
        // To the paren that CLOSES the list, not the first one inside it. A
        // parenthesised parameter type — a tuple, or a function type — closes
        // one of its own, and cutting there dropped every parameter after it,
        // the `&Path` a wrapper must name included. Review measured a wrapper
        // that took a path and was never asked to name it.
        let mut depth = 0usize;
        let mut end = after.len();
        for (at, c) in after.char_indices() {
            match c {
                '(' => depth = depth.saturating_add(1),
                ')' if depth == 0 => {
                    end = at;
                    break;
                }
                ')' => depth = depth.saturating_sub(1),
                _ => {}
            }
        }
        let arguments = after.get(..end).unwrap_or_default();
        arguments
            .split(',')
            .filter(|argument| argument.contains("Path"))
            .filter_map(|argument| argument.split(':').next())
            .map(|name| name.trim().trim_start_matches("mut ").trim().to_string())
            .filter(|name| !name.is_empty())
            .collect()
    }

    /// The ONE wrapper whose error never becomes a message, named rather than
    /// recognised.
    const DISCARDS_ITS_ERROR: &str = "metadata_if_present(";

    /// The first index of `needle`, spelled without the method that names it.
    ///
    /// This file is `include_str!`'d into its recipe, and the catalog's
    /// command-surface scan reds on the bare token that method would leave in
    /// the text — the same reason a comment here cannot spell it either.
    fn index_of(haystack: &str, needle: &str) -> Option<usize> {
        haystack.match_indices(needle).next().map(|(index, _)| index)
    }

    /// The LAST index of `needle`, spelled without its method for the same
    /// reason — the reverse one leaves a token the catalog scan reds on too.
    ///
    /// Walked rather than reversed: `MatchIndices` is only double-ended for a
    /// pattern whose searcher can run backwards, which a `&str`'s cannot.
    fn index_of_last(haystack: &str, needle: &str) -> Option<usize> {
        let mut last = None;
        for (index, _) in haystack.match_indices(needle) {
            last = Some(index);
        }
        last
    }

    /// A KEY THAT IS NOT THERE COSTS NOTHING.
    ///
    /// The refusal has to come before the staging tree is emptied, or a
    /// mistyped fifth argument destroys a directory before saying it did not
    /// like the fifth argument. That is the same failure `run_volume`'s own
    /// comment records for the fourth, which is why the read sits beside those
    /// checks rather than in `publish_into` where it started: there it ran
    /// after `remove_dir_all`, and this test reds.
    #[test]
    fn a_key_that_is_not_there_does_not_cost_the_staging_tree() {
        let scratch = Scratch::disk(DISK);
        run_layout(&scratch.path, &mut Vec::new()).unwrap();
        let dir = fake_mkfs(RECORDING_MKFS);
        let td_boot = dir.join("td-boot");
        publishing_td_boot(&td_boot, "");
        // Something in the staging tree that a premature refusal would take.
        let keep = dir.join("td-volume-root").join("keepme");
        std::fs::create_dir_all(&keep).unwrap();
        std::fs::write(keep.join("data"), b"not yours to remove").unwrap();
        let publish = Publish {
            td_boot,
            deployment: PathBuf::from("/media/deployment"),
            trusted_key: dir.join("no-such-key.pub"),
        };
        let error = run_volume(
            &scratch.path,
            &dir.join("mkfs.btrfs"),
            &dir,
            Some(&publish),
            &mut Vec::new(),
        )
        .unwrap_err();
        assert!(
            format!("{error}").contains("trusted deployment key"),
            "the refusal must name the key: {error}"
        );
        assert_eq!(
            std::fs::read(keep.join("data")).unwrap(),
            b"not yours to remove",
            "a missing key emptied the caller's staging tree"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The trust root's path is RELATIVE and outside the deployments
    /// directory.
    ///
    /// Both are properties of the constant rather than of any run, and both
    /// are pinned here because `staging.join(…)` is where they bite: an
    /// absolute constant would discard the staging tree silently and put a
    /// machine's trust root at `/td/trusted.pub` on the INSTALLER, and one
    /// under `td/deployments` would be replaced by the first update. The
    /// literal is pinned too, as `TRUSTED_KEY_PATH` is, since a rename is a
    /// key the reader never finds.
    #[test]
    fn the_trust_roots_path_is_relative_and_outside_the_deployments() {
        assert_eq!(protocol::VOLUME_TRUSTED_KEY, "td/trusted.pub");
        let path = Path::new(protocol::VOLUME_TRUSTED_KEY);
        assert!(path.is_relative(), "an absolute key path discards the volume");
        assert!(!path.starts_with(protocol::DEPLOYMENTS_DIR));
        assert_eq!(
            Path::new("/vol").join(protocol::VOLUME_TRUSTED_KEY),
            Path::new("/vol/td/trusted.pub")
        );
    }

    /// Nothing pre-placed where the snapshot goes is ever WRITTEN THROUGH.
    ///
    /// The scratch directory is not private and both names here are guessable,
    /// so each is tried as a symlink pointing at a file that must survive:
    /// following either would truncate it under an installer that is usually
    /// root. Whether the install then succeeds or refuses is not the property
    /// — the victim is — so both outcomes are accepted and only the bytes are
    /// asserted.
    #[test]
    fn nothing_pre_placed_at_the_snapshot_is_written_through() {
        for name in ["td-install-key", "td-install-key/td-trusted.pub"] {
            let scratch = Scratch::disk(DISK);
            run_layout(&scratch.path, &mut Vec::new()).unwrap();
            let dir = fake_mkfs(RECORDING_MKFS);
            let td_boot = dir.join("td-boot");
            publishing_td_boot(&td_boot, "");
            let victim = dir.join("victim");
            std::fs::write(&victim, b"do not truncate me").unwrap();
            let planted = dir.join(name);
            if let Some(parent) = planted.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::os::unix::fs::symlink(&victim, &planted).unwrap();
            let publish = Publish {
                td_boot,
                deployment: PathBuf::from("/media/deployment"),
                trusted_key: key_file(&dir),
            };
            let _ = run_volume(
                &scratch.path,
                &dir.join("mkfs.btrfs"),
                &dir,
                Some(&publish),
                &mut Vec::new(),
            );
            assert_eq!(
                std::fs::read(&victim).unwrap(),
                b"do not truncate me",
                "a symlink at {name} was followed and its target truncated"
            );
            std::fs::remove_dir_all(&dir).unwrap();
        }
    }

    /// No publish, no key.
    ///
    /// Honestly labelled: with no publish `publish_into` never runs, so no
    /// mutation of the code this names can red it. It is a GUARD against a
    /// later change that writes the key somewhere earlier or unconditionally
    /// — a trust root with no deployment beside it is one nothing has ever
    /// checked — and not evidence about the code as it stands.
    #[test]
    fn a_volume_with_no_deployment_carries_no_key() {
        let scratch = Scratch::disk(DISK);
        run_layout(&scratch.path, &mut Vec::new()).unwrap();
        let dir = fake_mkfs(RECORDING_MKFS);
        run_volume(
            &scratch.path,
            &dir.join("mkfs.btrfs"),
            &dir,
            None,
            &mut Vec::new(),
        )
        .unwrap();
        assert!(
            !dir
                .join("td-volume-root")
                .join(protocol::VOLUME_TRUSTED_KEY)
                .exists(),
            "a volume with no deployment carries a trust root"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A SUCCESSFUL EXIT IS NOT A PUBLISH.
    ///
    /// The shape this exists for: a td-boot that exits 0 having written
    /// nothing produced a complete, correct, mountable volume with an empty
    /// `td/deployments`, and `volume` printed its byte offsets and returned
    /// success. The disk installs and cannot boot, and the first thing to
    /// notice is the machine. Each of the three answers is refused separately,
    /// because they fail for different reasons: nothing printed, a malformed
    /// id, and — the one that matters — a well-formed id naming a directory
    /// that is not there.
    #[test]
    fn a_publish_that_writes_nothing_is_not_a_publish() {
        for (name, body) in [
            ("silent", "#!/bin/sh\nexit 0\n"),
            ("malformed", "#!/bin/sh\necho not-a-digest\n"),
            // The id is REAL and the directory is not: this is the only one of
            // the three a check on the child's output alone would pass.
            (
                "absent",
                "#!/bin/sh\necho 0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef\n",
            ),
        ] {
            let scratch = Scratch::disk(DISK);
            run_layout(&scratch.path, &mut Vec::new()).unwrap();
            let dir = fake_mkfs(RECORDING_MKFS);
            let td_boot = dir.join("td-boot");
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::write(&td_boot, body).unwrap();
                std::fs::set_permissions(&td_boot, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
            let publish = Publish {
                td_boot,
                deployment: PathBuf::from("/media/deployment"),
                trusted_key: key_file(&dir),
            };
            let error = run_volume(
                &scratch.path,
                &dir.join("mkfs.btrfs"),
                &dir,
                Some(&publish),
                &mut Vec::new(),
            )
            .unwrap_err();
            let text = format!("{error}");
            assert!(
                text.contains("published no deployment id") || text.contains("is not there"),
                "the {name} publish was taken for a real one: {text}"
            );
            assert!(
                !dir.join("argv").exists(),
                "the {name} publish still made a filesystem"
            );
            std::fs::remove_dir_all(&dir).unwrap();
        }
    }

    /// A DEPLOYMENTS DIRECTORY THAT CANNOT BE READ is not a publish that
    /// wrote nothing, and says so.
    ///
    /// This is the one behaviour `paths::is_dir` changed rather than merely
    /// named. `Path::is_dir` answers `false` for every failure it meets, so a
    /// `td/deployments` that is not a directory at all — or that this process
    /// may not traverse — read as an id td-boot never wrote, and the operator
    /// was told the program that had just done the work published nothing.
    /// The errno is the whole difference between "look at td-boot" and "look
    /// at the disk", so it is the errno that has to reach the message.
    #[test]
    fn a_deployments_directory_that_cannot_be_read_is_not_a_missing_publish() {
        let scratch = Scratch::disk(DISK);
        run_layout(&scratch.path, &mut Vec::new()).unwrap();
        let dir = fake_mkfs(RECORDING_MKFS);
        let td_boot = dir.join("td-boot");
        {
            use std::os::unix::fs::PermissionsExt;
            // The staging root is `$2`. td-install created `td/deployments` as
            // a directory before this ran; putting a FILE there is ENOTDIR on
            // the join below, which needs no ownership games to arrange and so
            // behaves the same for a test run as root.
            std::fs::write(
                &td_boot,
                "#!/bin/sh\nrmdir \"$2/td/deployments\"\n: > \"$2/td/deployments\"\n\
                 echo 0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef\n",
            )
            .unwrap();
            std::fs::set_permissions(&td_boot, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let publish = Publish {
            td_boot,
            deployment: PathBuf::from("/media/deployment"),
            trusted_key: key_file(&dir),
        };
        let error = run_volume(
            &scratch.path,
            &dir.join("mkfs.btrfs"),
            &dir,
            Some(&publish),
            &mut Vec::new(),
        )
        .unwrap_err();
        let text = format!("{error}");
        assert!(
            !text.contains("is not there"),
            "an unreadable deployments directory was reported as a publish \
             that never happened: {text}"
        );
        assert!(
            text.contains("td/deployments"),
            "the refusal does not name the path it could not read: {text}"
        );
        assert!(
            !dir.join("argv").exists(),
            "the refusal still made a filesystem"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// An id that is well-formed but points OUT of the staging tree is refused
    /// on its shape, before it is joined onto a path.
    #[test]
    fn a_traversing_deployment_id_is_refused_before_it_is_joined() {
        let scratch = Scratch::disk(DISK);
        run_layout(&scratch.path, &mut Vec::new()).unwrap();
        let dir = fake_mkfs(RECORDING_MKFS);
        let td_boot = dir.join("td-boot");
        {
            use std::os::unix::fs::PermissionsExt;
            // `..` resolves to the staging tree itself, which IS a directory —
            // so a readback that joined first and asked afterwards would accept
            // this and report a deployment that was never written.
            std::fs::write(&td_boot, "#!/bin/sh\necho ../..\n").unwrap();
            std::fs::set_permissions(&td_boot, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let publish = Publish {
            td_boot,
            deployment: PathBuf::from("/media/deployment"),
            trusted_key: key_file(&dir),
        };
        let error = run_volume(
            &scratch.path,
            &dir.join("mkfs.btrfs"),
            &dir,
            Some(&publish),
            &mut Vec::new(),
        )
        .unwrap_err();
        assert!(
            format!("{error}").contains("published no deployment id"),
            "a traversing id must be refused on its shape: {error}"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A td-boot that FAILS fails the install, rather than making a volume
    /// with no deployment in it and reporting success.
    #[test]
    fn a_failing_publish_fails_the_volume() {
        let scratch = Scratch::disk(DISK);
        run_layout(&scratch.path, &mut Vec::new()).unwrap();
        let dir = fake_mkfs(RECORDING_MKFS);
        let td_boot = dir.join("td-boot");
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::write(&td_boot, "#!/bin/sh\nexit 7\n").unwrap();
            std::fs::set_permissions(&td_boot, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let publish = Publish {
            td_boot,
            deployment: PathBuf::from("/media/deployment"),
            trusted_key: key_file(&dir),
        };
        let error = run_volume(
            &scratch.path,
            &dir.join("mkfs.btrfs"),
            &dir,
            Some(&publish),
            &mut Vec::new(),
        )
        .unwrap_err();
        assert!(
            format!("{error}").contains("publish failed"),
            "a failing publish must be reported: {error}"
        );
        // ...and mkfs never ran, so no volume was made to hold nothing.
        assert!(
            !dir.join("argv").exists(),
            "the filesystem was made despite the publish failing"
        );
        // Nor is the trust root there. The key is READ before the publish and
        // written after it, so a staging tree that failed carries no key — one
        // written first would outlive the publish it belonged to and be the
        // root a later, unrelated publish into the same scratch inherited.
        assert!(
            !dir
                .join("td-volume-root")
                .join(protocol::VOLUME_TRUSTED_KEY)
                .exists(),
            "a failed publish left its trust root behind"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A relative `td-boot` is refused rather than resolved through `PATH`,
    /// for the reason the mkfs one is.
    #[test]
    fn a_relative_td_boot_is_refused() {
        let scratch = Scratch::disk(DISK);
        run_layout(&scratch.path, &mut Vec::new()).unwrap();
        let dir = fake_mkfs(RECORDING_MKFS);
        let publish = Publish {
            td_boot: PathBuf::from("td-boot"),
            deployment: PathBuf::from("/media/deployment"),
            trusted_key: key_file(&dir),
        };
        let error = run_volume(
            &scratch.path,
            &dir.join("mkfs.btrfs"),
            &dir,
            Some(&publish),
            &mut Vec::new(),
        )
        .unwrap_err();
        // Named, not just refused: the mkfs refusal emits the same sentence, so
        // an assertion on it alone would pass whichever of the two fired.
        let text = format!("{error}");
        assert!(
            text.contains("resolves through PATH") && text.contains("td-boot"),
            "a bare td-boot name must be refused, and said to be td-boot's: {text}"
        );
        // ...and it costs nothing: the caller's staging tree is untouched,
        // which is what moving this check ahead of the wipe bought.
        assert!(
            !dir.join("td-volume-root").exists(),
            "an argv mistake destroyed the staging tree before reporting itself"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A relative `mkfs` is refused rather than resolved through `PATH`.
    #[test]
    fn only_main_writes_the_program_name_into_a_diagnostic() {
        // The subprocess guard can only reach diagnostics some invocation
        // produces, and most of these are overflow arms, block-device-only
        // paths and post-mkfs failures nothing drives — so a fifty-first
        // message written with the prefix would be invisible to it. This is a
        // scan over the crate's own SOURCE, the shape the confinement tests
        // elsewhere in td use for exactly this reason.
        //
        // The needle is COMPOSED rather than written as a literal, or this
        // assertion would count itself and be off by one forever.
        let needle = format!("{}td-install: ", '"');
        let hits: Vec<&str> = include_str!("main.rs")
            .lines()
            .filter(|line| line.contains(&needle))
            .collect();
        assert_eq!(
            hits.len(),
            2,
            "the program's name belongs at the one place that PRINTS an \
             error, not in the message: {hits:#?}"
        );
        for line in &hits {
            assert!(
                line.contains("writeln!(io::stderr()"),
                "this names the program somewhere other than main's printer: {line}"
            );
        }
    }

    #[test]
    fn a_relative_mkfs_is_refused_before_it_can_be_searched_for() {
        let scratch = Scratch::disk(DISK);
        run_layout(&scratch.path, &mut Vec::new()).unwrap();
        let error = run_volume(
            &scratch.path,
            Path::new("mkfs.btrfs"),
            &std::env::temp_dir(),
            None,
            &mut Vec::new(),
        )
        .unwrap_err();
        assert!(
            format!("{error}").contains("resolves through PATH"),
            "a bare name must be refused: {error}"
        );
    }

    /// A scratch directory that would put the image ON the destination is
    /// refused, rather than truncating the disk whose table was just parsed.
    #[test]
    fn a_scratch_image_that_is_the_destination_is_refused() {
        let dir = std::env::temp_dir().join(format!(
            "td-install-alias-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("td-volume.img");
        File::create(&path).unwrap().set_len(DISK).unwrap();
        run_layout(&path, &mut Vec::new()).unwrap();
        let fake = fake_mkfs("#!/bin/sh\nexit 0\n");
        let error =
            run_volume(&path, &fake.join("mkfs.btrfs"), &dir, None, &mut Vec::new()).unwrap_err();
        assert!(
            format!("{error}").contains("is the destination itself"),
            "the alias must be refused: {error}"
        );
        // ...and the disk it would have truncated is still the size it was.
        assert_eq!(std::fs::metadata(&path).unwrap().len(), DISK);
        std::fs::remove_dir_all(&dir).unwrap();
        std::fs::remove_dir_all(&fake).unwrap();
    }

    /// Both ENDS of the region are cleared before the copy, because a signature
    /// the sparse copy would skip can be at either.
    #[test]
    fn the_copy_clears_both_ends_of_the_region_first() {
        const MIB: u64 = 1024 * 1024;
        let dest = Scratch::disk(8 * MIB);
        let (offset, len) = (MIB, 6 * MIB);
        {
            let mut file = OpenOptions::new().write(true).open(&dest.path).unwrap();
            // A signature at each end of the region, and one in its middle that
            // the copy is expected to leave alone.
            write_at(&mut file, offset, &[0xaa; 512]).unwrap();
            write_at(&mut file, offset + len - 512, &[0xbb; 512]).unwrap();
            write_at(&mut file, offset + 3 * MIB, &[0xcc; 512]).unwrap();
        }
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&dest.path)
            .unwrap();
        zero_edges(&mut file, offset, len).unwrap();
        assert_eq!(dest.read_at(offset, 4), [0; 4], "the head was cleared");
        assert_eq!(
            dest.read_at(offset + len - 512, 4),
            [0; 4],
            "the tail was cleared — MD RAID and ZFS put metadata there"
        );
        assert_eq!(
            dest.read_at(offset + 3 * MIB, 4),
            [0xcc; 4],
            "the middle is the copy's to write, not this function's"
        );
    }

    /// The copy honours its RANGE, which is what lets `run_volume` order it.
    ///
    /// The ordering itself — everything but the first chunk, a barrier, then the
    /// first chunk — cannot be observed from the final state, and an interrupted
    /// install is not something a test can stage. What can be pinned is the
    /// mechanism the ordering rests on: a range that starts past a live chunk
    /// must leave that chunk's destination untouched, or "deferred" would mean
    /// "written twice" and the barrier would guarantee nothing.
    #[test]
    fn the_copy_writes_only_the_range_it_is_given() {
        const CHUNK: u64 = 1024 * 1024;
        let source = Scratch::disk(3 * CHUNK);
        let dest = Scratch::disk(4 * CHUNK);
        {
            let mut file = OpenOptions::new().write(true).open(&source.path).unwrap();
            write_at(&mut file, 0, &[0xab; 4096]).unwrap();
            write_at(&mut file, 2 * CHUNK, &[0xcd; 4096]).unwrap();
        }
        {
            let mut file = OpenOptions::new().write(true).open(&dest.path).unwrap();
            write_at(&mut file, CHUNK, &[0xee; 512]).unwrap();
        }
        let mut image = File::open(&source.path).unwrap();
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&dest.path)
            .unwrap();
        // Everything BUT the first chunk, exactly as `run_volume` does it.
        let written = copy_sparse(&mut image, &mut file, CHUNK, CHUNK, 3 * CHUNK).unwrap();
        assert_eq!(written, CHUNK, "only the one live chunk in range");
        assert_eq!(
            dest.read_at(CHUNK, 4),
            [0xee; 4],
            "the deferred chunk's destination was not touched"
        );
        assert_eq!(dest.read_at(3 * CHUNK, 4), [0xcd; 4], "the in-range chunk landed");
        // ...and the deferred pass then lands it, at its own offset.
        let first = copy_sparse(&mut image, &mut file, CHUNK, 0, CHUNK).unwrap();
        assert_eq!(first, CHUNK);
        assert_eq!(dest.read_at(CHUNK, 4), [0xab; 4]);
    }

    /// The staging tree is EMPTIED, not merely ensured: `--rootdir` copies what
    /// is under it into the filesystem, so anything left there by a previous run
    /// would land on a machine's /var.
    #[test]
    fn a_stale_staging_tree_does_not_reach_the_volume() {
        let scratch = Scratch::disk(DISK);
        run_layout(&scratch.path, &mut Vec::new()).unwrap();
        let dir = fake_mkfs(RECORDING_MKFS);
        let stale = dir.join("td-volume-root").join("junk");
        std::fs::create_dir_all(stale.parent().unwrap()).unwrap();
        std::fs::write(&stale, b"not mine").unwrap();
        run_volume(
            &scratch.path,
            &dir.join("mkfs.btrfs"),
            &dir,
            None,
            &mut Vec::new(),
        )
        .unwrap();
        assert!(!stale.exists(), "a stale staging file survived into the volume");
        assert!(
            dir.join("td-volume-root").join(protocol::VOLUME_SUBVOL).is_dir(),
            "the subvolume directory is still staged"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A region too small for two disjoint edges is cleared once, whole,
    /// rather than twice over its own middle.
    #[test]
    fn a_region_smaller_than_two_edges_is_cleared_once_and_entirely() {
        const MIB: u64 = 1024 * 1024;
        let dest = Scratch::disk(4 * MIB);
        {
            let mut file = OpenOptions::new().write(true).open(&dest.path).unwrap();
            write_at(&mut file, MIB, &vec![0xff; MIB as usize]).unwrap();
        }
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&dest.path)
            .unwrap();
        zero_edges(&mut file, MIB, MIB).unwrap();
        assert_eq!(dest.read_at(MIB, 4), [0; 4]);
        assert_eq!(dest.read_at(2 * MIB - 4, 4), [0; 4], "to its very end");
    }

    /// A `mkfs` that FAILS fails the install, rather than leaving a partition
    /// with whatever was in it before and a zero exit status.
    #[test]
    fn a_failing_mkfs_fails_the_volume() {
        let scratch = Scratch::disk(DISK);
        run_layout(&scratch.path, &mut Vec::new()).unwrap();
        let dir = fake_mkfs("#!/bin/sh\nexit 3\n");
        let error = run_volume(
            &scratch.path,
            &dir.join("mkfs.btrfs"),
            &dir,
            None,
            &mut Vec::new(),
        )
        .unwrap_err();
        assert!(
            format!("{error}").contains("failed on the scratch image"),
            "a failing mkfs must be reported: {error}"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The sparse copy writes the chunks that hold something and skips the
    /// holes — and the skip is not free, which is the half worth pinning: a
    /// byte under a hole SURVIVES. That is why `run_volume` zeroes the region's
    /// first megabyte before copying, and this is the test that would notice if
    /// the skip ever silently became a full copy (`written` would jump).
    #[test]
    fn the_sparse_copy_skips_holes_and_keeps_what_is_under_them() {
        const CHUNK: u64 = 1024 * 1024;
        let source = Scratch::disk(3 * CHUNK);
        let dest = Scratch::disk(4 * CHUNK);
        {
            let mut file = OpenOptions::new().write(true).open(&source.path).unwrap();
            // First chunk holds data, second is a hole, third holds data.
            write_at(&mut file, 0, &[0xab; 4096]).unwrap();
            write_at(&mut file, 2 * CHUNK, &[0xcd; 4096]).unwrap();
        }
        {
            // Something already on the destination, under what will be the hole.
            let mut file = OpenOptions::new().write(true).open(&dest.path).unwrap();
            write_at(&mut file, CHUNK + CHUNK, &[0xee; 512]).unwrap();
        }
        let mut image = File::open(&source.path).unwrap();
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&dest.path)
            .unwrap();
        let written = copy_sparse(&mut image, &mut file, CHUNK, 0, 3 * CHUNK).unwrap();
        assert_eq!(written, 2 * CHUNK, "only the two live chunks are written");
        assert_eq!(dest.read_at(CHUNK, 4), [0xab; 4], "the first chunk arrived");
        assert_eq!(
            dest.read_at(3 * CHUNK, 4),
            [0xcd; 4],
            "the third chunk arrived at its own offset, not packed after the first"
        );
        assert_eq!(
            dest.read_at(2 * CHUNK, 4),
            [0xee; 4],
            "the skipped hole left the destination's own bytes in place"
        );
    }

    #[test]
    fn a_plan_puts_both_partitions_where_gpt_allows_and_alignment_requires() {
        let p = plan(512, DISK).unwrap();
        let align = protocol::PARTITION_ALIGN_BYTES / 512;

        assert_eq!(p.esp_start % align, 0, "ESP start is 1 MiB aligned");
        assert_eq!(p.volume_start % align, 0, "volume start is 1 MiB aligned");
        assert!(p.esp_start >= gpt::first_usable_lba(512).unwrap());
        assert_eq!(
            p.volume_end,
            gpt::last_usable_lba(512, DISK / 512).unwrap(),
            "the volume takes the REMAINDER of the disk; stopping short is \
             capacity lost with nothing reporting it"
        );
        assert!(p.esp_end < p.volume_start, "the partitions do not overlap");
        assert_eq!(
            p.volume_start,
            align_up(p.esp_end + 1, align).unwrap(),
            "the gap after the ESP is the alignment and nothing more"
        );
        assert_eq!(
            (p.esp_end - p.esp_start + 1) * 512,
            protocol::ESP_BYTES,
            "the ESP is exactly its declared size"
        );
    }

    /// The end LBA is INCLUSIVE. An exclusive end is an off-by-one that no
    /// reader can detect and that overlaps the next partition by one sector.
    /// Both ends are INCLUSIVE, which is how GPT stores them: an exclusive end
    /// is an off-by-one no reader detects and one sector of overlap with the
    /// next partition.
    #[test]
    fn partition_ends_are_inclusive() {
        let p = plan(512, DISK).unwrap();
        // The ESP spans esp_end - esp_start + 1 sectors, so its declared size
        // is only exact if the end is the LAST sector rather than one past it.
        assert_eq!(p.esp_sectors().unwrap(), protocol::ESP_BYTES / 512);
        // Same for the volume, whose end is the last usable LBA and not the
        // first unusable one — a disk's final sector is addressable.
        let last = gpt::last_usable_lba(512, DISK / 512).unwrap();
        assert_eq!(p.volume_end, last);
        assert!(p.volume_end < p.disk_sectors, "the end is an LBA, not a count");
    }

    #[test]
    fn a_disk_too_small_for_two_deployments_is_refused_by_name() {
        let error = plan(512, protocol::ESP_BYTES + 64 * MIB).unwrap_err();
        assert!(error.contains("td volume"), "{error}");
        // Smaller than the ESP itself: the volume never starts.
        let error = plan(512, 16 * MIB).unwrap_err();
        assert!(error.contains("too small"), "{error}");
        // Smaller than a GPT, which is refused before any partition is placed.
        let error = plan(512, 8 * 512).unwrap_err();
        assert!(error.contains("GPT alone"), "{error}");
    }

    #[test]
    fn a_size_that_is_not_whole_sectors_is_refused() {
        let error = plan(512, 8 * GIB + 1).unwrap_err();
        assert!(error.contains("whole number"), "{error}");
    }

    /// 4Kn disks are laid out in their OWN sectors, not in 512-byte ones.
    #[test]
    fn a_4kn_disk_plans_in_its_own_sectors() {
        let p = plan(4096, DISK).unwrap();
        assert_eq!(p.sector_size, 4096);
        assert_eq!(p.disk_sectors, DISK / 4096);
        assert_eq!((p.esp_end - p.esp_start + 1) * 4096, protocol::ESP_BYTES);
        assert_eq!(p.esp_start % (protocol::PARTITION_ALIGN_BYTES / 4096), 0);
    }

    #[test]
    fn align_up_rounds_and_refuses_a_zero_alignment() {
        assert_eq!(align_up(0, 2048), Some(0));
        assert_eq!(align_up(1, 2048), Some(2048));
        assert_eq!(align_up(2048, 2048), Some(2048));
        assert_eq!(align_up(2049, 2048), Some(4096));
        assert_eq!(align_up(1, 0), None, "a zero alignment is not a no-op");
        assert_eq!(align_up(u64::MAX, 2048), None, "overflow is not wrapped");
    }

    #[test]
    fn a_laid_out_disk_carries_a_table_gpt_reads_back() {
        let scratch = Scratch::disk(DISK);
        run_layout(&scratch.path, &mut Vec::new()).unwrap();
        let table = scratch.table(DISK);

        let p = plan(512, DISK).unwrap();
        assert_eq!(table.partitions.len(), 2);
        let esp = table.partitions.first().unwrap();
        let volume = table.partitions.get(1).unwrap();
        assert_eq!(esp.type_guid, gpt::TYPE_ESP);
        assert_eq!(esp.name, protocol::ESP_PARTITION_NAME);
        assert_eq!(volume.type_guid, gpt::TYPE_LINUX_FS);
        assert_eq!(volume.name, protocol::VOLUME_PARTITION_NAME);
        // Against the PLAN, which is the divergence this crate can have: the
        // FAT geometry is derived from `plan` and the GPT entries from
        // `layout`, so a table written for a different layout than the ESP was
        // formatted against is well-formed and wrong.
        assert_eq!(table.disk_sectors, p.disk_sectors);
        assert_eq!((esp.start_lba, esp.end_lba), (p.esp_start, p.esp_end));
        assert_eq!(
            (volume.start_lba, volume.end_lba),
            (p.volume_start, p.volume_end)
        );
        assert_ne!(esp.unique_guid, volume.unique_guid, "distinct partitions");
    }

    /// The ESP is a FAT32 volume firmware will mount: signature, the label, and
    /// a `total_sectors` that matches the partition the table declares.
    #[test]
    fn the_esp_is_a_fat32_volume_of_the_partitions_size() {
        let scratch = Scratch::disk(DISK);
        run_layout(&scratch.path, &mut Vec::new()).unwrap();
        let p = plan(512, DISK).unwrap();

        let boot = scratch.read_at(p.esp_offset().unwrap(), 512);
        assert_eq!(boot.get(510..512).unwrap(), &[0x55, 0xaa], "boot signature");
        assert_eq!(
            boot.get(0x47..0x52).unwrap(),
            b"TD-ESP     ",
            "the label is space padded to 11"
        );
        let total = u32::from_le_bytes(boot.get(0x20..0x24).unwrap().try_into().unwrap());
        assert_eq!(
            u64::from(total),
            p.esp_sectors().unwrap(),
            "the volume fills its partition"
        );
        let hidden = u32::from_le_bytes(boot.get(0x1c..0x20).unwrap().try_into().unwrap());
        assert_eq!(u64::from(hidden), p.esp_start, "HiddSec is the start LBA");
    }

    /// `fat.rs` requires zeroed space and cannot check it. A destination whose
    /// ESP already holds a filesystem must come out as though it never did:
    /// stale bytes in the FAT read as ALLOCATED clusters, which is a volume
    /// whose free count disagrees with its own table.
    #[test]
    fn a_destination_with_a_stale_filesystem_is_zeroed_before_it_is_formatted() {
        let scratch = Scratch::disk(DISK);
        let p = plan(512, DISK).unwrap();
        let at = p.esp_offset().unwrap();
        {
            let mut file = OpenOptions::new().write(true).open(&scratch.path).unwrap();
            // Every byte of the metadata region set, which is the worst case:
            // every FAT slot reads as a cluster chain that goes nowhere.
            file.seek(SeekFrom::Start(at)).unwrap();
            file.write_all(&vec![0xffu8; 8 * MIB as usize]).unwrap();
        }
        run_layout(&scratch.path, &mut Vec::new()).unwrap();

        let esp = fat::build(&fat::Volume {
            bytes_per_sector: 512,
            total_sectors: p.esp_sectors().unwrap(),
            hidden_sectors: p.esp_start as u32,
            volume_id: 0,
            label: protocol::ESP_VOLUME_LABEL.to_string(),
            sectors_per_cluster: None,
            root: Vec::new(),
        })
        .unwrap();
        let fat_bytes = u64::from(esp.sectors_per_fat) * 512;
        let first_fat = at + u64::from(fat::RESERVED_SECTORS) * 512;

        // BOTH copies, each past its own live prefix — an empty volume's FAT is
        // three entries long (media, end-of-chain, the root's), and every slot
        // after them must read as FREE. Checking one copy would miss the second,
        // which is the one a repair tool falls back to.
        // The live prefix is whatever `fat::build` emitted for this FAT, not a
        // literal: everything past it must read as free.
        for copy in 0..u64::from(fat::NUM_FATS) {
            let start = first_fat + copy * fat_bytes;
            let rel = start - at;
            let live = esp
                .extents
                .iter()
                .filter(|e| e.offset == rel)
                .map(|e| e.bytes.len() as u64)
                .max()
                .unwrap();
            let tail = scratch.read_at(start + live, usize::try_from(fat_bytes - live).unwrap());
            assert!(
                tail.iter().all(|byte| *byte == 0),
                "FAT copy {copy} still holds stale bytes past its live prefix"
            );
        }
        // The root directory's cluster is WRITTEN whole rather than merely
        // zeroed — an empty labelled volume still has one entry, the label — so
        // what it must equal is the extent, not zero. A stale entry surviving
        // here would be a file firmware lists on a volume that never had one.
        let root = first_fat + u64::from(fat::NUM_FATS) * fat_bytes;
        let expected = esp
            .extents
            .iter()
            .filter(|e| e.offset + at == root)
            .map(|e| e.bytes.as_ref())
            .next()
            .unwrap();
        assert_eq!(
            scratch.read_at(root, expected.len()),
            expected,
            "the root directory cluster is not what fat::build laid down"
        );
    }

    /// A REINSTALL replaces the table rather than layering on it, and the old
    /// one is gone before the ESP under it is touched.
    ///
    /// The second half is what the ordering is for and cannot be observed from
    /// the finished disk, so it is checked where it IS observable: after
    /// invalidation the destination carries no table at all, which is what a
    /// failed install would leave behind. A disk carrying a stale table over a
    /// half-written filesystem is worse, because firmware tries it.
    #[test]
    fn a_reinstall_clears_the_old_table_before_the_esp_beneath_it() {
        let scratch = Scratch::disk(DISK);
        run_layout(&scratch.path, &mut Vec::new()).unwrap();
        let first = scratch.table(DISK);

        {
            let mut file = OpenOptions::new().write(true).open(&scratch.path).unwrap();
            let p = plan(512, DISK).unwrap();
            let image = gpt::build(&gpt::Layout {
                sector_size: 512,
                disk_sectors: p.disk_sectors,
                disk_guid: random_guid().unwrap(),
                align_sectors: 2048,
                partitions: Vec::new(),
            })
            .unwrap();
            invalidate_table(&mut file, &image).unwrap();
        }
        let primary = scratch.read_at(0, 34 * 512);
        assert!(
            primary.iter().all(|byte| *byte == 0),
            "an invalidated disk still carries a header or a protective MBR"
        );
        assert!(
            gpt::parse(&primary, &scratch.read_at(DISK - 33 * 512, 33 * 512), 512).is_err(),
            "an invalidated disk must not parse as partitioned"
        );

        run_layout(&scratch.path, &mut Vec::new()).unwrap();
        let second = scratch.table(DISK);
        assert_eq!(second.partitions.len(), 2, "the reinstall wrote a table");
        assert_ne!(
            first.disk_guid, second.disk_guid,
            "a reinstall is a new disk identity, not the old one recovered"
        );
    }

    /// Two installs of the same size differ in their GUIDs. A fixed one would
    /// have every td disk claim the same identity, which udev and firmware each
    /// resolve by picking one.
    #[test]
    fn each_install_gets_its_own_guids() {
        let first = Scratch::disk(DISK);
        let second = Scratch::disk(DISK);
        run_layout(&first.path, &mut Vec::new()).unwrap();
        run_layout(&second.path, &mut Vec::new()).unwrap();

        let (a, b) = (first.table(DISK), second.table(DISK));
        assert_ne!(a.disk_guid, b.disk_guid);
        assert_ne!(
            a.partitions.first().unwrap().unique_guid,
            b.partitions.first().unwrap().unique_guid
        );
    }

    /// A GUID from `/dev/urandom` still has to BE a GUID: RFC 4122 version 4
    /// and the variant bits, which is what a reader uses to tell a random one
    /// from a structured one.
    #[test]
    fn a_generated_guid_carries_version_four_and_the_variant() {
        for _ in 0..8 {
            let guid = random_guid().unwrap();
            assert_eq!(guid.0.get(7).unwrap() & 0xf0, 0x40, "version 4");
            assert_eq!(guid.0.get(8).unwrap() & 0xc0, 0x80, "RFC 4122 variant");
        }
    }

    /// A regular file has no logical block size to ask for, and must not be
    /// asked: the sysfs path for a regular file's device number is the
    /// FILESYSTEM's, whose block size is not the sector size of anything.
    /// The interleaved device encoding, against glibc's own masks.
    ///
    /// The extended-major cases are the point. Shifting the extended minor down
    /// puts the extended major directly above it, so a mask that only clears
    /// the low bits carries it along — which is what this did, and what stays
    /// invisible while every block major is under 4096.
    #[test]
    fn device_numbers_match_the_sysmacros_encoding() {
        /// `makedev`: major over bits 8..=19 and 44..=63, minor over 0..=7 and
        /// 20..=43. Written out here rather than reusing the decoder, so the
        /// test does not agree with the code by construction.
        fn makedev(major: u64, minor: u64) -> u64 {
            ((major & 0xfff) << 8) | ((major >> 12) << 44) | (minor & 0xff) | ((minor >> 8) << 20)
        }

        for (major, minor) in [
            (8, 0),               // /dev/sda
            (8, 1),               // /dev/sda1
            (259, 5),             // an NVMe namespace, major past one byte
            (0xfff, 0xff_ffff),   // every bit of both low fields
            (0x1000, 3),          // the first EXTENDED major
            (0xf_ffff, 0xff_ffff),// every bit of both
            (0x1234, 0x9_abcd),
        ] {
            let rdev = makedev(major, minor);
            assert_eq!(
                device_numbers(rdev),
                (major, minor),
                "rdev {rdev:#x} decodes wrong"
            );
        }
    }

    #[test]
    fn a_regular_file_takes_the_default_sector_size() {
        let scratch = Scratch::disk(DISK);
        let file = File::open(&scratch.path).unwrap();
        assert_eq!(logical_sector_size(&file).unwrap(), FILE_SECTOR_BYTES);
    }

    #[test]
    fn a_destinations_size_is_asked_of_the_destination() {
        let scratch = Scratch::disk(3 * GIB);
        let mut file = File::open(&scratch.path).unwrap();
        assert_eq!(destination_bytes(&mut file).unwrap(), 3 * GIB);
        assert_eq!(
            file.stream_position().unwrap(),
            0,
            "the descriptor is left where it was found"
        );
    }

    /// The metadata region is what the zeroing covers, and it must be the
    /// reserved sectors plus BOTH FATs plus the root cluster — a region short
    /// by one FAT leaves the second copy holding whatever was there.
    #[test]
    fn the_metadata_region_covers_both_fats_and_the_root_cluster() {
        let p = plan(512, DISK).unwrap();
        let esp = fat::build(&fat::Volume {
            bytes_per_sector: 512,
            total_sectors: p.esp_sectors().unwrap(),
            hidden_sectors: p.esp_start as u32,
            volume_id: 0,
            label: protocol::ESP_VOLUME_LABEL.to_string(),
            sectors_per_cluster: None,
            root: Vec::new(),
        })
        .unwrap();

        let expected = (u64::from(fat::RESERVED_SECTORS)
            + 2 * u64::from(esp.sectors_per_fat)
            + u64::from(esp.sectors_per_cluster))
            * 512;
        assert_eq!(metadata_bytes(&esp).unwrap(), expected);
        assert_eq!(u64::from(fat::NUM_FATS), 2, "two FATs is what that 2 is");
    }
}

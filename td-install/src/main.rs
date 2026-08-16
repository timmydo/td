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
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

/// The sector size assumed for a regular file, and the smallest a disk may
/// report. A 4Kn device says so itself — see `logical_sector_size`.
const FILE_SECTOR_BYTES: u64 = 512;

fn invalid(message: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[derive(Debug, Eq, PartialEq)]
enum Mode {
    Layout { destination: PathBuf },
}

fn parse_args(mut args: impl Iterator<Item = OsString>) -> io::Result<Mode> {
    let verb = args
        .next()
        .ok_or_else(|| invalid("usage: td-install layout <destination>".to_string()))?;
    let destination = args.next().map(PathBuf::from);
    if args.next().is_some() {
        return Err(invalid(
            "usage: td-install layout <destination>".to_string(),
        ));
    }
    match (verb.to_str(), destination) {
        (Some("layout"), Some(destination)) => Ok(Mode::Layout { destination }),
        _ => Err(invalid(
            "usage: td-install layout <destination>".to_string(),
        )),
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
            "td-install: a destination must be a regular file or a block device".to_string(),
        ));
    }
    let (major, minor) = device_numbers(metadata.st_rdev());
    let path = PathBuf::from(format!(
        "/sys/dev/block/{major}:{minor}/queue/logical_block_size"
    ));
    let mut text = String::new();
    File::open(&path)
        .and_then(|mut f| f.read_to_string(&mut text))
        .map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("cannot read {}: {error}", path.display()),
            )
        })?;
    let trimmed = text.trim();
    trimmed.parse::<u64>().map_err(|_| {
        invalid(format!(
            "{} reads {trimmed:?}, which is not a sector size",
            path.display()
        ))
    })
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
        return Err("td-install: the destination reports a sector size of 0".to_string());
    }
    if !disk_bytes.is_multiple_of(sector_size) {
        return Err(format!(
            "td-install: destination is {disk_bytes} bytes, not a whole number of \
             {sector_size}-byte sectors"
        ));
    }
    let disk_sectors = disk_bytes / sector_size;
    let minimum = gpt::minimum_disk_sectors(sector_size)?;
    if disk_sectors < minimum {
        return Err(format!(
            "td-install: destination holds {disk_sectors} sectors, and a GPT alone \
             needs {minimum}"
        ));
    }
    let align = protocol::PARTITION_ALIGN_BYTES / sector_size;
    if align == 0 {
        return Err(format!(
            "td-install: a {sector_size}-byte sector is larger than the \
             {}-byte partition alignment",
            protocol::PARTITION_ALIGN_BYTES
        ));
    }
    let first_usable = gpt::first_usable_lba(sector_size)?;
    let last_usable = gpt::last_usable_lba(sector_size, disk_sectors)?;

    let esp_start = align_up(first_usable, align)
        .ok_or_else(|| "td-install: aligning the ESP start overflowed".to_string())?;
    let esp_sectors = protocol::ESP_BYTES / sector_size;
    let esp_end = esp_start
        .checked_add(esp_sectors)
        .and_then(|end| end.checked_sub(1))
        .ok_or_else(|| "td-install: the ESP does not fit in an LBA".to_string())?;

    let volume_start = align_up(
        esp_end
            .checked_add(1)
            .ok_or_else(|| "td-install: the volume start overflowed".to_string())?,
        align,
    )
    .ok_or_else(|| "td-install: aligning the volume start overflowed".to_string())?;
    if volume_start > last_usable {
        return Err(format!(
            "td-install: destination is too small — the ESP alone reaches LBA \
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
            "td-install: the td volume would be {volume_bytes} bytes and needs at \
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
    File::open("/dev/urandom")?.read_exact(&mut bytes)?;
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
            .ok_or_else(|| invalid("td-install: zero chunk out of range".to_string()))?;
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

fn run_layout(destination: &Path) -> io::Result<()> {
    let mut file = OpenOptions::new().read(true).write(true).open(destination)?;
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
        .ok_or_else(|| invalid("td-install: the ESP offset overflowed".to_string()))?;
    let esp_sectors = plan
        .esp_sectors()
        .ok_or_else(|| invalid("td-install: the ESP length overflowed".to_string()))?;
    let esp_start_lba = u32::try_from(plan.esp_start).map_err(|_| {
        invalid("td-install: the ESP starts past what a FAT32 BPB can record".to_string())
    })?;
    let volume = fat::Volume {
        bytes_per_sector: u32::try_from(sector_size)
            .map_err(|_| invalid("td-install: sector size exceeds a FAT32 BPB".to_string()))?,
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
        .ok_or_else(|| invalid("td-install: the ESP metadata region overflowed".to_string()))?;
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
        .ok_or_else(|| invalid("td-install: an ESP extent overflowed".to_string()))?;
    if written > metadata {
        return Err(invalid(format!(
            "td-install: the ESP zeroes {metadata} bytes but fat::build writes up \
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
            .ok_or_else(|| invalid("td-install: an ESP extent overflowed".to_string()))?;
        write_at(&mut file, at, &extent.bytes)?;
    }
    file.sync_all()?;

    write_at(&mut file, table.backup_offset, &table.backup)?;
    file.sync_all()?;
    write_at(&mut file, table.primary_offset, &table.primary)?;
    file.sync_all()?;

    writeln!(
        io::stdout(),
        "{} {} {}",
        destination.display(),
        plan.esp_start,
        plan.volume_start
    )
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
        .ok_or_else(|| invalid("td-install: the layout has no ESP".to_string()))?;
    let bytes = esp
        .unique_guid
        .0
        .get(..4)
        .and_then(|slice| <[u8; 4]>::try_from(slice).ok())
        .ok_or_else(|| invalid("td-install: the ESP GUID is too short".to_string()))?;
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
        Mode::Layout { destination } => run_layout(&destination),
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
        run_layout(&scratch.path).unwrap();
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
        run_layout(&scratch.path).unwrap();
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
        run_layout(&scratch.path).unwrap();

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
        run_layout(&scratch.path).unwrap();
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

        run_layout(&scratch.path).unwrap();
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
        run_layout(&first.path).unwrap();
        run_layout(&second.path).unwrap();

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

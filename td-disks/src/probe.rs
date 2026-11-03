//! Which block devices carry which Btrfs or ext4 filesystem.
//!
//! Read-only: each candidate is opened `O_NOFOLLOW`, checked against its
//! sysfs device number before and after, and its superblock read. Nothing
//! is written, scanned into the kernel or repaired.

use crate::setting::{Kind, Uuid};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read};
use std::os::unix::fs::{FileExt, FileTypeExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

const EXT4_OFFSET: u64 = 1024;
const EXT4_BYTES: usize = 1024;
const EXT4_MAGIC: u16 = 0xef53;
/// s_feature_incompat's JOURNAL_DEV: an external journal, not a filesystem.
const EXT4_JOURNAL_DEV: u32 = 0x8;
const BTRFS_OFFSET: u64 = 65536;
const BTRFS_SUPER: usize = 4096;
/// incompat METADATA_UUID: device items carry `metadata_uuid`, not fsid.
const BTRFS_METADATA_UUID: u64 = 1 << 10;
// x86-64 Linux O_NOFOLLOW | O_NONBLOCK.
const OPEN_FLAGS: i32 = 0o400000 | 0o4000;
const MAX_DEVICES: usize = 4096;
const MAX_SYSFS: u64 = 128;

/// One filesystem member device.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Found {
    pub device: PathBuf,
    pub uuid: Uuid,
    pub kind: Kind,
    /// Btrfs only: this member's devid and the filesystem's device count.
    pub member: Option<(u64, u64)>,
}

fn invalid(why: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, why.into())
}

/// An ext4 superblock's UUID, from the 1 KiB at byte 1024.
pub fn ext4(block: &[u8]) -> Option<Uuid> {
    let magic = u16::from_le_bytes(block.get(56..58)?.try_into().ok()?);
    let incompat = u32::from_le_bytes(block.get(96..100)?.try_into().ok()?);
    if magic != EXT4_MAGIC || incompat & EXT4_JOURNAL_DEV != 0 {
        return None;
    }
    let uuid: [u8; 16] = block.get(104..120)?.try_into().ok()?;
    (uuid != [0; 16]).then_some(Uuid(uuid))
}

fn le64(block: &[u8], at: usize) -> io::Result<u64> {
    block
        .get(at..at + 8)
        .and_then(|b| b.try_into().ok())
        .map(u64::from_le_bytes)
        .ok_or_else(|| invalid("short Btrfs superblock"))
}

fn uuid_at(block: &[u8], at: usize) -> io::Result<[u8; 16]> {
    block
        .get(at..at + 16)
        .and_then(|b| b.try_into().ok())
        .ok_or_else(|| invalid("short Btrfs superblock"))
}

/// A Btrfs member's filesystem UUID, devid and device count, from the
/// primary superblock. Identity only: the magic, the recorded offset, the
/// crc32c checksum and the device item's filesystem, never td-btrfs's
/// mkfs geometry or feature set, since whether it mounts is the kernel's.
pub fn btrfs(block: &[u8]) -> io::Result<Option<(Uuid, u64, u64)>> {
    if block.len() != BTRFS_SUPER || block.get(64..72) != Some(b"_BHRfS_M".as_slice()) {
        return Ok(None);
    }
    if le64(block, 48)? != BTRFS_OFFSET {
        return Err(invalid("Btrfs superblock records another offset"));
    }
    let csum_type = block
        .get(196..198)
        .and_then(|b| b.try_into().ok())
        .map(u16::from_le_bytes)
        .ok_or_else(|| invalid("short Btrfs superblock"))?;
    if csum_type != 0 {
        return Err(invalid(format!(
            "Btrfs checksum type {csum_type} is not crc32c"
        )));
    }
    let stored = block
        .get(0..4)
        .and_then(|b| b.try_into().ok())
        .map(u32::from_le_bytes)
        .ok_or_else(|| invalid("short Btrfs superblock"))?;
    let body = block
        .get(32..)
        .ok_or_else(|| invalid("short Btrfs superblock"))?;
    if stored != td_btrfs::crc32c::checksum(body) {
        return Err(invalid("Btrfs superblock fails its checksum"));
    }
    let fsid = uuid_at(block, 32)?;
    let incompat = le64(block, 188)?;
    // With METADATA_UUID the device items carry the metadata UUID, while
    // fsid stays the UUID the filesystem is known and mounted by.
    let owner = if incompat & BTRFS_METADATA_UUID != 0 {
        uuid_at(block, 571)?
    } else {
        fsid
    };
    if fsid == [0; 16] || uuid_at(block, 283)? != owner {
        return Err(invalid("Btrfs device item names another filesystem"));
    }
    let count = le64(block, 136)?;
    let devid = le64(block, 201)?;
    // devid 0 is a running replace's target, not a member.
    if count == 0 || devid == 0 {
        return Err(invalid("Btrfs member numbering is inconsistent"));
    }
    Ok(Some((Uuid(fsid), devid, count)))
}

/// A filesystem UUID, its kind and, for Btrfs, `(devid, device count)`.
pub type Identity = (Uuid, Kind, Option<(u64, u64)>);

/// What one opened device holds, if it is either filesystem.
pub fn identify(device: &File) -> io::Result<Option<Identity>> {
    let mut block = vec![0u8; BTRFS_SUPER];
    if device.read_exact_at(&mut block, BTRFS_OFFSET).is_ok() {
        if let Some((uuid, devid, count)) = btrfs(&block)? {
            return Ok(Some((uuid, Kind::Btrfs, Some((devid, count)))));
        }
    }
    let mut block = vec![0u8; EXT4_BYTES];
    if device.read_exact_at(&mut block, EXT4_OFFSET).is_err() {
        return Ok(None);
    }
    Ok(ext4(&block).map(|uuid| (uuid, Kind::Ext4, None)))
}

/// Whole disks and partitions of the kinds td installs to and reads
/// from; never device-mapper, loop, RAM or optical nodes.
pub fn supported_name(name: &str) -> bool {
    if let Some(rest) = name.strip_prefix("vd").or_else(|| name.strip_prefix("sd")) {
        let letters = rest.bytes().take_while(u8::is_ascii_lowercase).count();
        return letters > 0
            && rest
                .get(letters..)
                .is_some_and(|tail| tail.bytes().all(|b| b.is_ascii_digit()));
    }
    let Some(rest) = name.strip_prefix("nvme") else {
        return false;
    };
    let Some((controller, rest)) = rest.split_once('n') else {
        return false;
    };
    let (namespace, partition) = rest
        .split_once('p')
        .map_or((rest, None), |(n, p)| (n, Some(p)));
    let number = |value: &str| !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit());
    number(controller) && number(namespace) && partition.is_none_or(number)
}

fn sysfs(path: &Path) -> io::Result<String> {
    let mut text = String::new();
    File::open(path)?
        .take(MAX_SYSFS + 1)
        .read_to_string(&mut text)?;
    if text.len() as u64 > MAX_SYSFS {
        return Err(invalid(format!("oversized {}", path.display())));
    }
    Ok(text.trim_end().into())
}

pub fn device_number(text: &str) -> Option<(u64, u64)> {
    let (major, minor) = text.split_once(':')?;
    let parse = |part: &str| {
        (!part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))
            .then(|| part.parse().ok())
            .flatten()
    };
    Some((parse(major)?, parse(minor)?))
}

fn numbers(meta: &fs::Metadata) -> (u64, u64) {
    let dev = meta.rdev();
    (
        ((dev >> 8) & 0xfff) | ((dev >> 32) & 0xffff_f000),
        (dev & 0xff) | ((dev >> 12) & 0xffff_ff00),
    )
}

/// Whether `meta` is a block device numbered `expected`.
pub fn same(meta: &fs::Metadata, expected: (u64, u64)) -> bool {
    meta.file_type().is_block_device() && numbers(meta) == expected
}

/// Whether the sysfs device behind `entry` hangs off a USB bus: removable
/// media (media.rs), never an extra volume.
pub fn on_usb(entry: &Path) -> io::Result<bool> {
    let real = fs::canonicalize(entry)?;
    Ok(real.components().any(|part| {
        part.as_os_str()
            .to_str()
            .is_some_and(|text| text.starts_with("usb"))
    }))
}

/// Whether `entry`, a whole disk, has partitions. A whole disk that was
/// repartitioned keeps the old filesystem's superblock, which names a
/// filesystem that is no longer there.
pub fn partitioned(entry: &Path, name: &str) -> io::Result<bool> {
    if entry.join("partition").exists() {
        return Ok(false);
    }
    for child in fs::read_dir(entry)?.take(MAX_DEVICES) {
        let child = child?;
        let named = child
            .file_name()
            .to_str()
            .is_some_and(|text| text.len() > name.len() && text.starts_with(name));
        if named && child.path().join("partition").exists() {
            return Ok(true);
        }
    }
    Ok(false)
}

fn probe(sys: &Path, node: &Path, name: &str) -> io::Result<Option<Found>> {
    let expected = device_number(&sysfs(&sys.join("dev"))?)
        .ok_or_else(|| invalid("invalid sysfs device number"))?;
    if fs::read_dir(sys.join("holders"))?.next().is_some() {
        // Held by device-mapper or md: its filesystem is not on this node.
        return Ok(None);
    }
    if partitioned(sys, name)? {
        return Ok(None);
    }
    if on_usb(sys)? {
        return Err(invalid(
            "attached by USB; removable media are not extra volumes",
        ));
    }
    let before = fs::symlink_metadata(node)?;
    if !same(&before, expected) {
        return Err(invalid("device node disagrees with sysfs"));
    }
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(OPEN_FLAGS)
        .open(node)?;
    let opened = file.metadata()?;
    if !same(&opened, expected) || opened.ino() != before.ino() {
        return Err(invalid("device node changed during open"));
    }
    Ok(identify(&file)?.map(|(uuid, kind, member)| Found {
        device: node.into(),
        uuid,
        kind,
        member,
    }))
}

/// Every Btrfs or ext4 member under `sys` (`/sys/class/block`) with its
/// node in `dev` (`/dev`), and a note for each device that could not be
/// read. Sorted by device path.
pub fn scan(sys: &Path, dev: &Path) -> (Vec<Found>, Vec<String>) {
    let mut found = Vec::new();
    let mut notes = Vec::new();
    let entries = match fs::read_dir(sys) {
        Ok(entries) => entries,
        Err(error) => {
            notes.push(format!("{}: {error}", sys.display()));
            return (found, notes);
        }
    };
    let mut names: Vec<String> = entries
        .take(MAX_DEVICES)
        .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
        .filter(|name| supported_name(name))
        .collect();
    names.sort();
    for name in names {
        match probe(&sys.join(&name), &dev.join(&name), &name) {
            Ok(Some(member)) => found.push(member),
            Ok(None) => {}
            Err(error) => notes.push(format!("{name}: {error}")),
        }
    }
    (found, notes)
}

/// The members carrying `uuid` as `kind`, and whether they make the whole
/// filesystem: ext4 is one device; Btrfs needs as many distinct devids as
/// every member's agreed device count. Devids need not be 1..=count: Btrfs
/// numbers an added device past the highest and never closes a gap.
pub fn members(found: &[Found], uuid: Uuid, kind: Kind) -> (Vec<&Found>, bool) {
    let members: Vec<&Found> = found
        .iter()
        .filter(|member| member.uuid == uuid && member.kind == kind)
        .collect();
    let complete = match kind {
        Kind::Ext4 => members.len() == 1,
        Kind::Btrfs => {
            let count = members.first().and_then(|m| m.member).map(|(_, c)| c);
            let mut ids: Vec<u64> = members
                .iter()
                .filter_map(|m| m.member)
                .filter(|(id, c)| Some(*c) == count && *id != 0)
                .map(|(id, _)| id)
                .collect();
            ids.sort_unstable();
            ids.dedup();
            count.is_some_and(|count| {
                ids.len() == members.len() && u64::try_from(ids.len()).ok() == Some(count)
            })
        }
    };
    (members, complete)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[test]
    fn ext4_superblock_names_its_uuid() {
        let mut block = vec![0u8; EXT4_BYTES];
        block[56..58].copy_from_slice(&EXT4_MAGIC.to_le_bytes());
        block[104..120].copy_from_slice(&[7; 16]);
        assert_eq!(ext4(&block), Some(Uuid([7; 16])));
        block[96..100].copy_from_slice(&EXT4_JOURNAL_DEV.to_le_bytes());
        assert_eq!(ext4(&block), None, "an external journal is no volume");
        block[96..100].copy_from_slice(&[0; 4]);
        block[56] = 0;
        assert_eq!(ext4(&block), None);
        assert_eq!(ext4(&block[..60]), None);
    }

    #[test]
    fn btrfs_written_by_td_btrfs_is_identified_and_damage_refused() {
        let dir = td_fs::TempDir::new("td-disks-probe").unwrap();
        let path = dir.path().join("volume.img");
        File::create(&path).unwrap().set_len(256 << 20).unwrap();
        let uuid = [0x3f; 16];
        td_btrfs::mkfs::make(&td_btrfs::mkfs::Options {
            uuid: Some(uuid),
            target: path.clone(),
            ..Default::default()
        })
        .unwrap();
        let file = File::open(&path).unwrap();
        assert_eq!(
            identify(&file).unwrap(),
            Some((Uuid(uuid), Kind::Btrfs, Some((1, 1))))
        );
        let mut block = vec![0u8; BTRFS_SUPER];
        file.read_exact_at(&mut block, BTRFS_OFFSET).unwrap();
        block[200] ^= 1;
        assert!(btrfs(&block).is_err(), "a checksum failure is refused");
        let empty = dir.path().join("empty.img");
        File::create(&empty).unwrap().set_len(1 << 20).unwrap();
        assert_eq!(identify(&File::open(&empty).unwrap()).unwrap(), None);
    }

    #[test]
    fn names_are_disks_and_partitions_only() {
        for name in ["sda", "sdb1", "vda", "vdz12", "nvme0n1", "nvme0n1p3"] {
            assert!(supported_name(name), "{name}");
        }
        for name in [
            "dm-0", "loop0", "ram0", "sr0", "zram0", "md0", "sd", "nvme0", "nvme0n1p", "sda1x",
        ] {
            assert!(!supported_name(name), "{name}");
        }
    }

    #[test]
    fn usb_disks_and_partitioned_whole_disks_are_not_candidates() {
        let dir = td_fs::TempDir::new("td-disks-sysfs").unwrap();
        let root = dir.path();
        let sata = root.join("devices/pci0000:00/0000:00:17.0/ata1/host0/block/sda");
        let usb = root.join("devices/pci0000:00/0000:00:14.0/usb2/2-1/host6/block/sdc");
        for path in [&sata, &usb] {
            fs::create_dir_all(path).unwrap();
        }
        fs::create_dir_all(sata.join("sda1")).unwrap();
        fs::write(sata.join("sda1/partition"), "1\n").unwrap();
        let class = root.join("class-block");
        fs::create_dir(&class).unwrap();
        std::os::unix::fs::symlink(&sata, class.join("sda")).unwrap();
        std::os::unix::fs::symlink(&usb, class.join("sdc")).unwrap();
        std::os::unix::fs::symlink(sata.join("sda1"), class.join("sda1")).unwrap();
        assert!(!on_usb(&class.join("sda")).unwrap());
        assert!(on_usb(&class.join("sdc")).unwrap());
        assert!(partitioned(&class.join("sda"), "sda").unwrap());
        assert!(
            !partitioned(&class.join("sda1"), "sda1").unwrap(),
            "a partition"
        );
        assert!(!partitioned(&class.join("sdc"), "sdc").unwrap(), "no table");
    }

    fn member(uuid: u8, kind: Kind, member: Option<(u64, u64)>) -> Found {
        Found {
            device: PathBuf::from(format!("/dev/x{uuid}{member:?}")),
            uuid: Uuid([uuid; 16]),
            kind,
            member,
        }
    }

    #[test]
    fn a_volume_is_complete_only_with_every_member() {
        let two = [
            member(1, Kind::Btrfs, Some((1, 2))),
            member(1, Kind::Btrfs, Some((2, 2))),
            member(2, Kind::Ext4, None),
        ];
        assert!(members(&two, Uuid([1; 16]), Kind::Btrfs).1);
        assert!(members(&two, Uuid([2; 16]), Kind::Ext4).1);
        assert!(!members(&two, Uuid([2; 16]), Kind::Btrfs).1, "wrong kind");
        assert!(
            !members(&two[..1], Uuid([1; 16]), Kind::Btrfs).1,
            "missing devid 2"
        );
        let doubled = [
            member(1, Kind::Btrfs, Some((1, 2))),
            member(1, Kind::Btrfs, Some((1, 2))),
        ];
        assert!(
            !members(&doubled, Uuid([1; 16]), Kind::Btrfs).1,
            "devid twice"
        );
        let ext_twice = [member(2, Kind::Ext4, None), member(2, Kind::Ext4, None)];
        assert!(
            !members(&ext_twice, Uuid([2; 16]), Kind::Ext4).1,
            "ambiguous"
        );
        assert!(!members(&[], Uuid([1; 16]), Kind::Btrfs).1);
        // After `device remove 1` and `device add`, devids are {2, 3}.
        let gapped = [
            member(1, Kind::Btrfs, Some((2, 2))),
            member(1, Kind::Btrfs, Some((3, 2))),
        ];
        assert!(members(&gapped, Uuid([1; 16]), Kind::Btrfs).1);
        let disagree = [
            member(1, Kind::Btrfs, Some((1, 2))),
            member(1, Kind::Btrfs, Some((2, 3))),
        ];
        assert!(!members(&disagree, Uuid([1; 16]), Kind::Btrfs).1);
        let replace = [
            member(1, Kind::Btrfs, Some((1, 2))),
            member(1, Kind::Btrfs, Some((0, 2))),
        ];
        assert!(!members(&replace, Uuid([1; 16]), Kind::Btrfs).1);
    }

    /// A superblock as btrfs-progs and the kernel write it for a member of
    /// someone else's filesystem, with nothing td's own mkfs would make.
    fn foreign(
        fsid: [u8; 16],
        devid: u64,
        count: u64,
        incompat: u64,
        dev_fsid: [u8; 16],
    ) -> Vec<u8> {
        let mut block = vec![0u8; BTRFS_SUPER];
        block[32..48].copy_from_slice(&fsid);
        block[48..56].copy_from_slice(&BTRFS_OFFSET.to_le_bytes());
        block[64..72].copy_from_slice(b"_BHRfS_M");
        block[136..144].copy_from_slice(&count.to_le_bytes());
        block[144..148].copy_from_slice(&4096u32.to_le_bytes());
        block[148..152].copy_from_slice(&32768u32.to_le_bytes());
        block[188..196].copy_from_slice(&incompat.to_le_bytes());
        block[201..209].copy_from_slice(&devid.to_le_bytes());
        block[283..299].copy_from_slice(&dev_fsid);
        block[571..587].copy_from_slice(&[0x5a; 16]);
        let sum = td_btrfs::crc32c::checksum(&block[32..]);
        block[0..4].copy_from_slice(&sum.to_le_bytes());
        block
    }

    #[test]
    fn foreign_multi_device_btrfs_is_identified_by_identity_alone() {
        const ZSTD: u64 = 1 << 4;
        let fsid = [0x11; 16];
        let block = foreign(fsid, 2, 2, 0x161 | ZSTD, fsid);
        assert_eq!(btrfs(&block).unwrap(), Some((Uuid(fsid), 2, 2)));
        // METADATA_UUID: the device item carries metadata_uuid instead.
        let changed = foreign(fsid, 3, 2, BTRFS_METADATA_UUID, [0x5a; 16]);
        assert_eq!(btrfs(&changed).unwrap(), Some((Uuid(fsid), 3, 2)));
        assert!(
            btrfs(&foreign(fsid, 1, 1, 0, [0x22; 16])).is_err(),
            "device of another fs"
        );
        assert!(
            btrfs(&foreign(fsid, 0, 2, 0, fsid)).is_err(),
            "replace target"
        );
        assert!(
            btrfs(&foreign([0; 16], 1, 1, 0, [0; 16])).is_err(),
            "nil fsid"
        );
        let mut moved = foreign(fsid, 1, 1, 0, fsid);
        moved[48..56].copy_from_slice(&(1u64 << 26).to_le_bytes());
        let sum = td_btrfs::crc32c::checksum(&moved[32..]);
        moved[0..4].copy_from_slice(&sum.to_le_bytes());
        assert!(btrfs(&moved).is_err(), "a mirror copy is not the primary");
        let mut xxhash = foreign(fsid, 1, 1, 0, fsid);
        xxhash[196] = 1;
        assert!(btrfs(&xxhash).is_err(), "only crc32c is verified");
        assert_eq!(btrfs(&vec![0u8; BTRFS_SUPER]).unwrap(), None, "no magic");
    }
}

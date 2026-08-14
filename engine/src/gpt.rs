//! gpt.rs — td-native, zero-dep, DETERMINISTIC GUID Partition Table writer and
//! readback parser (UEFI 2.10 §5.3).
//!
//! This is the first piece of real-hardware boot. Until now the machine only
//! ever booted by `qemu -kernel`: there is no partitioner and no bootloader
//! anywhere in the target tree. A UEFI machine needs neither a
//! third-party partitioner nor a bootloader binary — it needs a GPT whose ESP
//! the firmware will read, which is bytes, and this module writes them.
//!
//! It computes bytes and performs no I/O, so it is a pure function of its
//! `Layout` and compiles into both planes: the control plane links it as part
//! of `td-engine`, and the target-side installer `#[path]`-includes this file
//! the way td-boot includes `sha256.rs`. That crate must expose the CRC-32 at
//! `crate::crc32` (its own `#[path]` include of `engine/src/crc32.rs`), which is
//! the module's one cross-file contract.
//!
//! Format — the parts of §5.3 a partition table actually consists of:
//!
//! * **LBA 0** is a protective MBR: one 0xEE partition record covering the disk,
//!   so a tool that understands only MBR sees a full disk rather than free space.
//!   It is 512 bytes at the start of LBA 0 whatever the sector size is.
//! * **LBA 1** is the primary header: `EFI PART`, revision 1.0, a CRC over its
//!   own first `header_size` bytes computed with the CRC field zeroed, and a
//!   second CRC over the partition entry array.
//! * **LBA 2..** is that array — 128 entries of 128 bytes, the size every
//!   firmware and partitioner expects, so 16384 bytes however the sectors fall.
//! * The **backup** is the same array followed by a header at the LAST LBA,
//!   with `MyLBA`/`AlternateLBA` exchanged. Firmware that finds the primary
//!   corrupt recovers from it, so it is not optional.
//!
//! Determinism: there is nothing to normalize away. Every GUID is supplied by
//! the caller rather than generated here, so a `Layout` maps to exactly one byte
//! sequence, and two builds of one deployment image compare equal. Where those
//! GUIDs come from is the installer's decision, not the format's.
//!
//! Refusals are the point of the validation half. A GPT is a structure firmware
//! reads without complaining: overlapping partitions, a partition running past
//! the last usable block, or two entries sharing a unique GUID all produce a
//! table that parses cleanly and a disk that corrupts itself later. Each is
//! refused here rather than written.

use crate::crc32::crc32;

/// Bytes per partition entry. Fixed at the value every firmware expects; the
/// header records it, but writing anything else is how you find out which
/// implementations only pretend to read the field.
pub const ENTRY_SIZE: u32 = 128;
/// Entries in the array. 128 is the universal convention and what makes the
/// array exactly 16384 bytes.
pub const ENTRY_COUNT: u32 = 128;
/// `ENTRY_SIZE * ENTRY_COUNT`, the array's byte length.
pub const ENTRY_ARRAY_BYTES: u64 = (ENTRY_SIZE as u64) * (ENTRY_COUNT as u64);
/// Header bytes covered by the header CRC. Revision 1.0's `HeaderSize`.
pub const HEADER_SIZE: u32 = 92;
/// `EFI PART`.
pub const HEADER_SIGNATURE: &[u8; 8] = b"EFI PART";
/// Revision 1.0, as a packed major/minor.
pub const HEADER_REVISION: u32 = 0x0001_0000;
/// EFI System Partition — `C12A7328-F81F-11D2-BA4B-00A0C93EC93B`, the type GUID
/// firmware looks for. Byte order is the on-disk mixed-endian encoding; the
/// `esp_type_guid_matches_its_canonical_text` test pins it against the parser.
pub const TYPE_ESP: Guid = Guid([
    0x28, 0x73, 0x2a, 0xc1, 0x1f, 0xf8, 0xd2, 0x11, 0xba, 0x4b, 0x00, 0xa0, 0xc9, 0x3e, 0xc9, 0x3b,
]);
/// Linux filesystem data — `0FC63DAF-8483-4772-8E79-3D69D8477DE4`, the type the
/// persistent Btrfs volume carries.
pub const TYPE_LINUX_FS: Guid = Guid([
    0xaf, 0x3d, 0xc6, 0x0f, 0x83, 0x84, 0x72, 0x47, 0x8e, 0x79, 0x3d, 0x69, 0xd8, 0x47, 0x7d, 0xe4,
]);

/// A GUID in its ON-DISK byte order: the first three fields little-endian, the
/// last two as written. Canonical text goes through `parse`/`Display` rather
/// than through a `[u8; 16]` literal, since the mixed endianness is exactly the
/// thing a hand-written literal gets wrong.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Guid(pub [u8; 16]);

impl Guid {
    /// All-zero, which GPT reads as "this entry is unused".
    pub const ZERO: Guid = Guid([0; 16]);

    /// Parse canonical `8-4-4-4-12` hex text, upper or lower case.
    pub fn parse(text: &str) -> Result<Guid, String> {
        let groups: Vec<&str> = text.split('-').collect();
        let widths = [8usize, 4, 4, 4, 12];
        if groups.len() != widths.len() {
            return Err(format!(
                "gpt: {text:?} is not a GUID: expected 5 hyphen-separated groups, found {}",
                groups.len()
            ));
        }
        let mut hex = String::with_capacity(32);
        for (i, want) in widths.iter().enumerate() {
            let group = groups.get(i).copied().unwrap_or_default();
            if group.len() != *want {
                return Err(format!(
                    "gpt: {text:?} is not a GUID: group {i} is {} hex digits, expected {want}",
                    group.len()
                ));
            }
            for c in group.chars() {
                if !c.is_ascii_hexdigit() {
                    return Err(format!("gpt: {text:?} is not a GUID: {c:?} is not hex"));
                }
                hex.push(c);
            }
        }
        let mut disk = [0u8; 16];
        for (i, slot) in disk.iter_mut().enumerate() {
            let pair = hex
                .get(i * 2..i * 2 + 2)
                .ok_or_else(|| format!("gpt: {text:?} is not a GUID: too few hex digits"))?;
            *slot = u8::from_str_radix(pair, 16)
                .map_err(|e| format!("gpt: {text:?} is not a GUID: {e}"))?;
        }
        // The first three fields are stored little-endian on disk; the last two
        // keep the order they are written in.
        for range in [0..4, 4..6, 6..8] {
            disk.get_mut(range)
                .ok_or_else(|| "gpt: GUID is not 16 bytes".to_string())?
                .reverse();
        }
        Ok(Guid(disk))
    }

    fn is_zero(&self) -> bool {
        self.0.iter().all(|b| *b == 0)
    }
}

impl std::fmt::Display for Guid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Undo the on-disk mixed endianness, then group 8-4-4-4-12.
        let order = [3usize, 2, 1, 0, 5, 4, 7, 6, 8, 9, 10, 11, 12, 13, 14, 15];
        for (i, src) in order.iter().enumerate() {
            if matches!(i, 4 | 6 | 8 | 10) {
                write!(f, "-")?;
            }
            write!(f, "{:02X}", self.0.get(*src).copied().unwrap_or(0))?;
        }
        Ok(())
    }
}

/// One partition, as the caller describes it. `end_lba` is INCLUSIVE, which is
/// how GPT stores it — an exclusive end written into that field is an
/// off-by-one no reader can detect.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Partition {
    pub type_guid: Guid,
    pub unique_guid: Guid,
    pub start_lba: u64,
    pub end_lba: u64,
    pub attributes: u64,
    /// UTF-16LE on disk, at most 36 code units.
    pub name: String,
}

/// The whole table to write.
#[derive(Clone, Debug)]
pub struct Layout {
    pub sector_size: u64,
    /// Sectors on the WHOLE device, so the backup header lands on the last one.
    pub disk_sectors: u64,
    pub disk_guid: Guid,
    /// Required alignment of every partition start, in sectors. 1 disables the
    /// check and 0 is REFUSED — a zero-initialized `Layout` asking for no
    /// alignment by accident is the one case where silence is wrong. A real
    /// disk wants 1 MiB, and a start that ignores it is a partition that reads
    /// and writes across every physical block boundary forever with nothing
    /// reporting it.
    pub align_sectors: u64,
    pub partitions: Vec<Partition>,
}

/// The two byte ranges a table occupies, each with the disk offset it belongs
/// at. Nothing here touches a file: the caller writes them, which is what keeps
/// this module usable from both the control plane and the target installer.
#[derive(Clone, Debug)]
pub struct Image {
    /// Always 0 — the protective MBR is the first thing on the disk.
    pub primary_offset: u64,
    /// LBA 0 through the end of the primary entry array.
    pub primary: Vec<u8>,
    pub backup_offset: u64,
    /// The backup entry array followed by the backup header on the LAST LBA.
    pub backup: Vec<u8>,
}

/// A table read back off a disk.
///
/// This is a VERIFICATION readback, not a read-modify-write surface: unused
/// entries are dropped rather than kept as holes, so feeding these partitions
/// back to `build` would renumber a disk whose used slots were not contiguous.
/// td writes its tables whole, so nothing here needs the holes; a tool that
/// edited someone else's disk would.
#[derive(Clone, Debug)]
pub struct Table {
    pub sector_size: u64,
    pub disk_sectors: u64,
    pub disk_guid: Guid,
    pub first_usable_lba: u64,
    pub last_usable_lba: u64,
    pub partitions: Vec<Partition>,
}

/// Sectors the 16384-byte entry array occupies at `sector_size`.
pub fn entry_array_sectors(sector_size: u64) -> Result<u64, String> {
    check_sector_size(sector_size)?;
    // Every permitted sector size divides 16384, so this is exact.
    Ok(ENTRY_ARRAY_BYTES / sector_size)
}

/// First LBA a partition may start on.
pub fn first_usable_lba(sector_size: u64) -> Result<u64, String> {
    Ok(2 + entry_array_sectors(sector_size)?)
}

/// Last LBA a partition may end on, given the whole device size.
///
/// Refuses a disk too small to hold a table at all rather than returning a
/// range below `first_usable_lba`: this is the natural way for a caller to ask
/// "what is the last block I may use", and an inverted range answers it with a
/// number.
pub fn last_usable_lba(sector_size: u64, disk_sectors: u64) -> Result<u64, String> {
    let minimum = minimum_disk_sectors(sector_size)?;
    if disk_sectors < minimum {
        return Err(format!(
            "gpt: a {disk_sectors}-sector disk cannot hold a partition table: {minimum} sectors \
             is the minimum at a {sector_size}-byte sector"
        ));
    }
    let reserved = entry_array_sectors(sector_size)?
        .checked_add(2)
        .ok_or_else(|| "gpt: reserved tail overflows".to_string())?;
    disk_sectors.checked_sub(reserved).ok_or_else(|| {
        format!("gpt: a {disk_sectors}-sector disk is too small to hold the backup table")
    })
}

/// The smallest device this table fits on: two headers, two entry arrays and at
/// least one usable block between them.
pub fn minimum_disk_sectors(sector_size: u64) -> Result<u64, String> {
    let entries = entry_array_sectors(sector_size)?;
    entries
        .checked_mul(2)
        .and_then(|n| n.checked_add(4))
        .ok_or_else(|| "gpt: minimum disk size overflows".to_string())
}

fn check_sector_size(sector_size: u64) -> Result<(), String> {
    if !(512..=4096).contains(&sector_size) || !sector_size.is_power_of_two() {
        return Err(format!(
            "gpt: sector size {sector_size} is not a power of two in 512..=4096"
        ));
    }
    Ok(())
}

/// Build the primary and backup byte ranges for `layout`.
pub fn build(layout: &Layout) -> Result<Image, String> {
    check_sector_size(layout.sector_size)?;
    let ss = layout.sector_size;
    let entry_sectors = entry_array_sectors(ss)?;
    let minimum = minimum_disk_sectors(ss)?;
    if layout.disk_sectors < minimum {
        return Err(format!(
            "gpt: a {}-sector disk cannot hold a partition table: {minimum} sectors is the \
             minimum at a {ss}-byte sector",
            layout.disk_sectors
        ));
    }
    if layout.disk_guid.is_zero() {
        return Err("gpt: the disk GUID is all zeros, which GPT reads as absent".into());
    }
    if layout.align_sectors == 0 {
        return Err(
            "gpt: an alignment of 0 sectors is not a request for no alignment — use 1".into(),
        );
    }
    let first_usable = first_usable_lba(ss)?;
    let last_usable = last_usable_lba(ss, layout.disk_sectors)?;
    let last_lba = layout
        .disk_sectors
        .checked_sub(1)
        .ok_or_else(|| "gpt: a zero-sector disk has no last LBA".to_string())?;

    let entries = encode_entries(layout, first_usable, last_usable)?;
    let entries_crc = crc32(&entries);

    // Primary: MBR, header, array. Backup: array, header on the last LBA.
    let backup_entries_lba = last_lba
        .checked_sub(entry_sectors)
        .ok_or_else(|| "gpt: backup entry array does not fit".to_string())?;
    let primary_header = encode_header(
        layout, 1, last_lba, 2, first_usable, last_usable, entries_crc,
    )?;
    let backup_header = encode_header(
        layout,
        last_lba,
        1,
        backup_entries_lba,
        first_usable,
        last_usable,
        entries_crc,
    )?;

    let usize_ss = usize::try_from(ss).map_err(|_| "gpt: sector size exceeds usize".to_string())?;
    let mut primary = protective_mbr(layout)?;
    primary.resize(usize_ss, 0);
    primary.extend_from_slice(&primary_header);
    primary.resize(
        usize_ss
            .checked_mul(2)
            .ok_or_else(|| "gpt: primary buffer overflows".to_string())?,
        0,
    );
    primary.extend_from_slice(&entries);

    // The array occupies whole sectors, so the backup header follows it
    // immediately and lands on the last LBA with no padding between them.
    let mut backup = entries.clone();
    backup.extend_from_slice(&backup_header);
    let backup_len = usize::try_from(
        entry_sectors
            .checked_add(1)
            .and_then(|n| n.checked_mul(ss))
            .ok_or_else(|| "gpt: backup buffer overflows".to_string())?,
    )
    .map_err(|_| "gpt: backup buffer exceeds usize".to_string())?;
    backup.resize(backup_len, 0);

    Ok(Image {
        primary_offset: 0,
        primary,
        backup_offset: backup_entries_lba
            .checked_mul(ss)
            .ok_or_else(|| "gpt: backup offset overflows".to_string())?,
        backup,
    })
}

fn protective_mbr(layout: &Layout) -> Result<Vec<u8>, String> {
    let mut mbr = vec![0u8; 512];
    // One 0xEE record covering everything after LBA 0, clamped because the
    // field is 32 bits and a disk can be larger than it can express.
    let size_in_lba = u32::try_from(layout.disk_sectors.saturating_sub(1)).unwrap_or(u32::MAX);
    write_at(&mut mbr, 446, &[0x00])?; // not bootable
    write_at(&mut mbr, 447, &[0x00, 0x02, 0x00])?; // starting CHS of LBA 1
    write_at(&mut mbr, 450, &[0xee])?; // GPT protective
    // Ending CHS: unrepresentable for any modern disk, and firmware reads the
    // LBA fields, so the spec's "not possible" value is what every partitioner
    // writes here.
    write_at(&mut mbr, 451, &[0xff, 0xff, 0xff])?;
    write_at(&mut mbr, 454, &1u32.to_le_bytes())?;
    write_at(&mut mbr, 458, &size_in_lba.to_le_bytes())?;
    write_at(&mut mbr, 510, &[0x55, 0xaa])?;
    Ok(mbr)
}

fn encode_entries(layout: &Layout, first_usable: u64, last_usable: u64) -> Result<Vec<u8>, String> {
    let count = u32::try_from(layout.partitions.len())
        .map_err(|_| "gpt: absurd partition count".to_string())?;
    if count > ENTRY_COUNT {
        return Err(format!(
            "gpt: {count} partitions exceeds the {ENTRY_COUNT}-entry array"
        ));
    }
    let mut array = vec![
        0u8;
        usize::try_from(ENTRY_ARRAY_BYTES)
            .map_err(|_| "gpt: entry array exceeds usize".to_string())?
    ];
    validate_partitions(&layout.partitions, first_usable, last_usable)?;
    let mut previous_end: Option<u64> = None;
    for (i, part) in layout.partitions.iter().enumerate() {
        // Ascending order and alignment are `build`'s rules rather than GPT's:
        // a disk partitioned elsewhere may legitimately be neither, so `parse`
        // does not enforce them.
        if let Some(prev) = previous_end {
            if part.start_lba <= prev {
                return Err(format!(
                    "gpt: partition {i} starts at LBA {} but partition {} ends at {prev}: \
                     partitions must be given in ascending order and must not overlap",
                    part.start_lba,
                    i.saturating_sub(1)
                ));
            }
        }
        if layout.align_sectors > 1 && part.start_lba % layout.align_sectors != 0 {
            return Err(format!(
                "gpt: partition {i} starts at LBA {}, which is not a multiple of the \
                 {}-sector alignment",
                part.start_lba, layout.align_sectors
            ));
        }
        previous_end = Some(part.end_lba);

        let name: Vec<u16> = part.name.encode_utf16().collect();
        if name.len() > 36 {
            return Err(format!(
                "gpt: partition {i}'s name is {} UTF-16 code units; the field holds 36",
                name.len()
            ));
        }
        // The field is NUL-padded, so a NUL inside a name is a name the reader
        // stops early on — `build` would write it and `parse` would hand back
        // something shorter.
        if name.contains(&0) {
            return Err(format!(
                "gpt: partition {i}'s name contains a NUL, which terminates the on-disk field"
            ));
        }
        let at = i
            .checked_mul(
                usize::try_from(ENTRY_SIZE)
                    .map_err(|_| "gpt: entry size exceeds usize".to_string())?,
            )
            .ok_or_else(|| "gpt: entry offset overflows".to_string())?;
        write_at(&mut array, at, &part.type_guid.0)?;
        write_at(&mut array, at + 16, &part.unique_guid.0)?;
        write_at(&mut array, at + 32, &part.start_lba.to_le_bytes())?;
        write_at(&mut array, at + 40, &part.end_lba.to_le_bytes())?;
        write_at(&mut array, at + 48, &part.attributes.to_le_bytes())?;
        for (u, unit) in name.iter().enumerate() {
            write_at(&mut array, at + 56 + u * 2, &unit.to_le_bytes())?;
        }
    }
    Ok(array)
}

/// The geometry every used entry must satisfy, whoever wrote the table. Shared
/// by `build` and `parse` so a disk this module ACCEPTS is one it would also
/// produce: a CRC says the bytes are intact, never that they describe a disk
/// that works, and overlapping partitions pass every checksum on it.
///
/// Order and alignment are deliberately NOT here — see `encode_entries`.
fn validate_partitions(
    partitions: &[Partition],
    first_usable: u64,
    last_usable: u64,
) -> Result<(), String> {
    for (i, part) in partitions.iter().enumerate() {
        if part.type_guid.is_zero() {
            return Err(format!(
                "gpt: partition {i} has an all-zero type GUID, which GPT reads as an unused entry"
            ));
        }
        if part.unique_guid.is_zero() {
            return Err(format!("gpt: partition {i} has an all-zero unique GUID"));
        }
        if part.start_lba > part.end_lba {
            return Err(format!(
                "gpt: partition {i} starts at LBA {} and ends at {} — the end is INCLUSIVE",
                part.start_lba, part.end_lba
            ));
        }
        if part.start_lba < first_usable {
            return Err(format!(
                "gpt: partition {i} starts at LBA {} but the table itself occupies through {}",
                part.start_lba,
                first_usable.saturating_sub(1)
            ));
        }
        if part.end_lba > last_usable {
            return Err(format!(
                "gpt: partition {i} ends at LBA {} past the last usable block {last_usable} — \
                 it would overwrite the backup table",
                part.end_lba
            ));
        }
        // Pairwise rather than against the previous entry, so this holds
        // whatever order the entries are in.
        for (j, other) in partitions.iter().enumerate().take(i) {
            if other.unique_guid == part.unique_guid {
                return Err(format!(
                    "gpt: partitions {j} and {i} share the unique GUID {} — a unique \
                     GUID is how everything above the table names a partition",
                    part.unique_guid
                ));
            }
            if part.start_lba <= other.end_lba && other.start_lba <= part.end_lba {
                return Err(format!(
                    "gpt: partitions {j} ({}..={}) and {i} ({}..={}) overlap",
                    other.start_lba, other.end_lba, part.start_lba, part.end_lba
                ));
            }
        }
    }
    Ok(())
}

fn encode_header(
    layout: &Layout,
    my_lba: u64,
    alternate_lba: u64,
    entries_lba: u64,
    first_usable: u64,
    last_usable: u64,
    entries_crc: u32,
) -> Result<Vec<u8>, String> {
    let mut h = vec![
        0u8;
        usize::try_from(HEADER_SIZE)
            .map_err(|_| "gpt: header size exceeds usize".to_string())?
    ];
    write_at(&mut h, 0, HEADER_SIGNATURE)?;
    write_at(&mut h, 8, &HEADER_REVISION.to_le_bytes())?;
    write_at(&mut h, 12, &HEADER_SIZE.to_le_bytes())?;
    // 16..20 is the header CRC, left zero while it is computed over these bytes.
    // 20..24 is reserved and must stay zero.
    write_at(&mut h, 24, &my_lba.to_le_bytes())?;
    write_at(&mut h, 32, &alternate_lba.to_le_bytes())?;
    write_at(&mut h, 40, &first_usable.to_le_bytes())?;
    write_at(&mut h, 48, &last_usable.to_le_bytes())?;
    write_at(&mut h, 56, &layout.disk_guid.0)?;
    write_at(&mut h, 72, &entries_lba.to_le_bytes())?;
    write_at(&mut h, 80, &ENTRY_COUNT.to_le_bytes())?;
    write_at(&mut h, 84, &ENTRY_SIZE.to_le_bytes())?;
    write_at(&mut h, 88, &entries_crc.to_le_bytes())?;
    let header_crc = crc32(&h);
    write_at(&mut h, 16, &header_crc.to_le_bytes())?;
    Ok(h)
}

/// Read a table back, checking everything a firmware would and the things it
/// would not. `primary` starts at LBA 0; `backup` is the range `build` returned,
/// ending on the last LBA. The two are compared against each other because a
/// disk where they disagree boots today and loses a partition the first time
/// anything recovers from the backup.
pub fn parse(primary: &[u8], backup: &[u8], sector_size: u64) -> Result<Table, String> {
    check_sector_size(sector_size)?;
    let ss = usize::try_from(sector_size).map_err(|_| "gpt: sector size exceeds usize".to_string())?;
    let mbr = read_at(primary, 0, 512)?;
    if read_at(mbr, 510, 2)? != [0x55, 0xaa] {
        return Err("gpt: no 0x55AA signature in the protective MBR".into());
    }
    if read_at(mbr, 450, 1)? != [0xee] {
        return Err("gpt: the protective MBR record is not type 0xEE".into());
    }
    // The record must actually COVER the disk: a 0xEE entry describing one
    // sector leaves the rest looking unallocated to every MBR-only tool, which
    // is the whole failure the protective MBR exists to prevent.
    let mbr_start = u32_at(mbr, 454)?;
    if mbr_start != 1 {
        return Err(format!(
            "gpt: the protective MBR record starts at LBA {mbr_start}, not 1"
        ));
    }

    let head = parse_header(read_at(primary, ss, ss)?, sector_size)?;
    if head.my_lba != 1 {
        return Err(format!(
            "gpt: the header at LBA 1 says it lives at LBA {}",
            head.my_lba
        ));
    }
    if head.entries_lba != 2 {
        return Err(format!(
            "gpt: the primary entry array is at LBA {}, not 2",
            head.entries_lba
        ));
    }
    // The MBR's size field is clamped to 32 bits, so compare against the same
    // clamp rather than the raw sector count.
    let want_size = u32::try_from(head.alternate_lba).unwrap_or(u32::MAX);
    let mbr_size = u32_at(mbr, 458)?;
    if mbr_size != want_size {
        return Err(format!(
            "gpt: the protective MBR record covers {mbr_size} sectors where the disk has \
             {want_size}"
        ));
    }
    let entries_at = usize::try_from(
        head.entries_lba
            .checked_mul(sector_size)
            .ok_or_else(|| "gpt: entry array offset overflows".to_string())?,
    )
    .map_err(|_| "gpt: entry array offset exceeds usize".to_string())?;
    let array_len = usize::try_from(
        u64::from(head.entry_count)
            .checked_mul(u64::from(head.entry_size))
            .ok_or_else(|| "gpt: entry array length overflows".to_string())?,
    )
    .map_err(|_| "gpt: entry array length exceeds usize".to_string())?;
    let array = read_at(primary, entries_at, array_len)?;
    let got = crc32(array);
    if got != head.entries_crc {
        return Err(format!(
            "gpt: partition entry array CRC is {got:08x}, header says {:08x}",
            head.entries_crc
        ));
    }

    // The backup header is the LAST sector of the backup range; its array
    // precedes it.
    let backup_header_at = backup
        .len()
        .checked_sub(ss)
        .ok_or_else(|| "gpt: the backup range is shorter than one sector".to_string())?;
    let alt = parse_header(read_at(backup, backup_header_at, ss)?, sector_size)?;
    if alt.my_lba != head.alternate_lba || alt.alternate_lba != head.my_lba {
        return Err(format!(
            "gpt: the two headers do not point at each other: primary {}<->{}, backup {}<->{}",
            head.my_lba, head.alternate_lba, alt.my_lba, alt.alternate_lba
        ));
    }
    if alt.disk_guid != head.disk_guid {
        return Err(format!(
            "gpt: the backup header carries disk GUID {} where the primary carries {}",
            alt.disk_guid, head.disk_guid
        ));
    }
    if (alt.first_usable, alt.last_usable) != (head.first_usable, head.last_usable) {
        return Err("gpt: the two headers disagree about the usable range".into());
    }
    // The backup is located and sized from ITS OWN fields, not the primary's:
    // firmware recovering from it reads what THAT header points at, so a backup
    // whose array lives somewhere else — or is a different length — is a disk
    // with no usable recovery table however well the primary checksums.
    let backup_sectors = backup
        .len()
        .checked_div(ss)
        .filter(|n| *n >= 1)
        .ok_or_else(|| "gpt: the backup range is shorter than one sector".to_string())?;
    let backup_start_lba = alt
        .my_lba
        .checked_sub(u64::try_from(backup_sectors.saturating_sub(1)).unwrap_or(u64::MAX))
        .ok_or_else(|| "gpt: the backup range starts before the disk does".to_string())?;
    if alt.entries_lba != backup_start_lba {
        return Err(format!(
            "gpt: the backup header points its entry array at LBA {} but the array before it \
             starts at {backup_start_lba}",
            alt.entries_lba
        ));
    }
    let alt_array_len = usize::try_from(
        u64::from(alt.entry_count)
            .checked_mul(u64::from(alt.entry_size))
            .ok_or_else(|| "gpt: backup entry array length overflows".to_string())?,
    )
    .map_err(|_| "gpt: backup entry array length exceeds usize".to_string())?;
    if alt_array_len > backup_header_at {
        return Err(format!(
            "gpt: the backup header claims a {alt_array_len}-byte entry array, which does not \
             fit in the {backup_header_at} bytes before it"
        ));
    }
    let backup_array = read_at(backup, 0, alt_array_len)?;
    if crc32(backup_array) != alt.entries_crc {
        return Err("gpt: the backup header's entry array CRC does not match its array".into());
    }
    if backup_array != array {
        return Err(
            "gpt: the backup partition entry array differs from the primary — a recovery \
             from it would change the partitions"
                .into(),
        );
    }

    let disk_sectors = head
        .alternate_lba
        .checked_add(1)
        .ok_or_else(|| "gpt: disk size overflows".to_string())?;
    let partitions = decode_entries(array, head.entry_count, head.entry_size)?;
    validate_partitions(&partitions, head.first_usable, head.last_usable)?;
    Ok(Table {
        sector_size,
        disk_sectors,
        disk_guid: head.disk_guid,
        first_usable_lba: head.first_usable,
        last_usable_lba: head.last_usable,
        partitions,
    })
}

struct Header {
    my_lba: u64,
    alternate_lba: u64,
    first_usable: u64,
    last_usable: u64,
    disk_guid: Guid,
    entries_lba: u64,
    entry_count: u32,
    entry_size: u32,
    entries_crc: u32,
}

fn parse_header(sector: &[u8], sector_size: u64) -> Result<Header, String> {
    if read_at(sector, 0, 8)? != HEADER_SIGNATURE.as_slice() {
        return Err("gpt: header signature is not \"EFI PART\"".into());
    }
    let revision = u32_at(sector, 8)?;
    if revision != HEADER_REVISION {
        return Err(format!(
            "gpt: header revision {revision:#010x}, expected {HEADER_REVISION:#010x}"
        ));
    }
    let header_size = u32_at(sector, 12)?;
    if u64::from(header_size) < u64::from(HEADER_SIZE) || u64::from(header_size) > sector_size {
        return Err(format!(
            "gpt: header size {header_size} is outside {HEADER_SIZE}..={sector_size}"
        ));
    }
    // UEFI Table 5.5 makes offset 20 a MUST-be-zero, and it is inside the CRC'd
    // region — so a non-zero word here is a header written by something that
    // disagrees with the spec about the layout, not a bit flip.
    let reserved = u32_at(sector, 20)?;
    if reserved != 0 {
        return Err(format!(
            "gpt: the header's reserved word is {reserved:#010x}, and the spec requires zero"
        ));
    }
    // The CRC covers header_size bytes with its own field zeroed.
    let mut copy = read_at(
        sector,
        0,
        usize::try_from(header_size).map_err(|_| "gpt: header size exceeds usize".to_string())?,
    )?
    .to_vec();
    let want = u32_at(&copy, 16)?;
    write_at(&mut copy, 16, &0u32.to_le_bytes())?;
    let got = crc32(&copy);
    if got != want {
        return Err(format!("gpt: header CRC is {got:08x}, header says {want:08x}"));
    }
    let entry_size = u32_at(sector, 84)?;
    if entry_size != ENTRY_SIZE {
        return Err(format!(
            "gpt: entry size {entry_size}, expected {ENTRY_SIZE}"
        ));
    }
    // UEFI §5.3.2 sets a FLOOR on the array — at least 16384 bytes — not a
    // ceiling, so a table with more than 128 entries is valid and one with
    // fewer is not. A count the caller's buffer cannot cover is refused by the
    // read that follows rather than by a second limit here.
    let entry_count = u32_at(sector, 80)?;
    let array_bytes = u64::from(entry_count)
        .checked_mul(u64::from(entry_size))
        .ok_or_else(|| "gpt: entry array length overflows".to_string())?;
    if array_bytes < ENTRY_ARRAY_BYTES {
        return Err(format!(
            "gpt: {entry_count} entries of {entry_size} bytes is {array_bytes} bytes, below the \
             {ENTRY_ARRAY_BYTES}-byte minimum the spec requires"
        ));
    }
    Ok(Header {
        my_lba: u64_at(sector, 24)?,
        alternate_lba: u64_at(sector, 32)?,
        first_usable: u64_at(sector, 40)?,
        last_usable: u64_at(sector, 48)?,
        disk_guid: Guid(
            read_at(sector, 56, 16)?
                .try_into()
                .map_err(|_| "gpt: disk GUID is not 16 bytes".to_string())?,
        ),
        entries_lba: u64_at(sector, 72)?,
        entry_count,
        entry_size,
        entries_crc: u32_at(sector, 88)?,
    })
}

fn decode_entries(array: &[u8], count: u32, size: u32) -> Result<Vec<Partition>, String> {
    let size = usize::try_from(size).map_err(|_| "gpt: entry size exceeds usize".to_string())?;
    let mut out = Vec::new();
    for i in 0..usize::try_from(count).map_err(|_| "gpt: entry count exceeds usize".to_string())? {
        let at = i
            .checked_mul(size)
            .ok_or_else(|| "gpt: entry offset overflows".to_string())?;
        let type_guid = Guid(
            read_at(array, at, 16)?
                .try_into()
                .map_err(|_| "gpt: type GUID is not 16 bytes".to_string())?,
        );
        if type_guid.is_zero() {
            continue;
        }
        let mut name = String::new();
        let units: Vec<u16> = (0..36)
            .map(|u| u16_at(array, at + 56 + u * 2))
            .collect::<Result<Vec<u16>, String>>()?;
        // The field is NUL-padded, so the name ends at the first zero unit.
        let live: Vec<u16> = units.into_iter().take_while(|u| *u != 0).collect();
        for c in char::decode_utf16(live) {
            name.push(c.map_err(|e| format!("gpt: partition name is not UTF-16: {e}"))?);
        }
        out.push(Partition {
            type_guid,
            unique_guid: Guid(
                read_at(array, at + 16, 16)?
                    .try_into()
                    .map_err(|_| "gpt: unique GUID is not 16 bytes".to_string())?,
            ),
            start_lba: u64_at(array, at + 32)?,
            end_lba: u64_at(array, at + 40)?,
            attributes: u64_at(array, at + 48)?,
            name,
        });
    }
    Ok(out)
}

fn write_at(buf: &mut [u8], at: usize, bytes: &[u8]) -> Result<(), String> {
    let end = at
        .checked_add(bytes.len())
        .ok_or_else(|| "gpt: field offset overflows".to_string())?;
    let have = buf.len();
    buf.get_mut(at..end)
        .ok_or_else(|| format!("gpt: field {at}..{end} lies outside a {have}-byte buffer"))?
        .copy_from_slice(bytes);
    Ok(())
}

fn read_at(buf: &[u8], at: usize, len: usize) -> Result<&[u8], String> {
    let end = at
        .checked_add(len)
        .ok_or_else(|| "gpt: field offset overflows".to_string())?;
    buf.get(at..end).ok_or_else(|| {
        format!(
            "gpt: field {at}..{end} lies outside a {}-byte buffer",
            buf.len()
        )
    })
}

fn u16_at(buf: &[u8], at: usize) -> Result<u16, String> {
    Ok(u16::from_le_bytes(
        read_at(buf, at, 2)?
            .try_into()
            .map_err(|_| "gpt: short u16".to_string())?,
    ))
}

fn u32_at(buf: &[u8], at: usize) -> Result<u32, String> {
    Ok(u32::from_le_bytes(
        read_at(buf, at, 4)?
            .try_into()
            .map_err(|_| "gpt: short u32".to_string())?,
    ))
}

fn u64_at(buf: &[u8], at: usize) -> Result<u64, String> {
    Ok(u64::from_le_bytes(
        read_at(buf, at, 8)?
            .try_into()
            .map_err(|_| "gpt: short u64".to_string())?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    const DISK_GUID: Guid = Guid([
        0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f,
        0x10,
    ]);
    const ESP_GUID: Guid = Guid([
        0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x1b, 0x1c, 0x1d, 0x1e, 0x1f,
        0x20,
    ]);
    const ROOT_GUID: Guid = Guid([
        0x21, 0x22, 0x23, 0x24, 0x25, 0x26, 0x27, 0x28, 0x29, 0x2a, 0x2b, 0x2c, 0x2d, 0x2e, 0x2f,
        0x30,
    ]);

    /// A 64 MiB disk of 512-byte sectors: a 16 MiB ESP at 1 MiB, then the rest.
    fn layout() -> Layout {
        Layout {
            sector_size: 512,
            disk_sectors: 131_072,
            disk_guid: DISK_GUID,
            align_sectors: 2048,
            partitions: vec![
                Partition {
                    type_guid: TYPE_ESP,
                    unique_guid: ESP_GUID,
                    start_lba: 2048,
                    end_lba: 34_815,
                    attributes: 0,
                    name: "td-esp".into(),
                },
                Partition {
                    type_guid: TYPE_LINUX_FS,
                    unique_guid: ROOT_GUID,
                    start_lba: 34_816,
                    end_lba: 131_038,
                    attributes: 0,
                    name: "td-root".into(),
                },
            ],
        }
    }

    #[test]
    fn esp_type_guid_matches_its_canonical_text() {
        assert_eq!(
            Guid::parse("C12A7328-F81F-11D2-BA4B-00A0C93EC93B").unwrap(),
            TYPE_ESP
        );
        assert_eq!(
            Guid::parse("0FC63DAF-8483-4772-8E79-3D69D8477DE4").unwrap(),
            TYPE_LINUX_FS
        );
    }

    /// The mixed endianness is the thing a hand-written literal gets wrong, so
    /// text must survive a round trip through the on-disk bytes.
    #[test]
    fn guid_text_round_trips_through_disk_order() {
        for text in [
            "C12A7328-F81F-11D2-BA4B-00A0C93EC93B",
            "0FC63DAF-8483-4772-8E79-3D69D8477DE4",
            "00000000-0000-0000-0000-000000000001",
        ] {
            assert_eq!(Guid::parse(text).unwrap().to_string(), text);
        }
        // Lower case parses; Display normalizes to the spec's upper case.
        assert_eq!(
            Guid::parse("c12a7328-f81f-11d2-ba4b-00a0c93ec93b").unwrap(),
            TYPE_ESP
        );
    }

    #[test]
    fn guid_refuses_malformed_text() {
        for bad in [
            "",
            "C12A7328-F81F-11D2-BA4B",
            "C12A7328-F81F-11D2-BA4B-00A0C93EC93",
            "C12A7328-F81F-11D2-BA4B-00A0C93EC93BB",
            "G12A7328-F81F-11D2-BA4B-00A0C93EC93B",
        ] {
            assert!(Guid::parse(bad).is_err(), "{bad:?} parsed");
        }
    }

    #[test]
    fn a_built_table_parses_back_to_what_was_asked_for() {
        let l = layout();
        let img = build(&l).unwrap();
        let table = parse(&img.primary, &img.backup, l.sector_size).unwrap();
        assert_eq!(table.disk_guid, DISK_GUID);
        assert_eq!(table.disk_sectors, l.disk_sectors);
        assert_eq!(table.first_usable_lba, 34);
        assert_eq!(table.last_usable_lba, 131_038);
        assert_eq!(table.partitions, l.partitions);
    }

    /// The two ranges land where the spec says, so a caller writing them at the
    /// returned offsets produces a disk firmware reads.
    #[test]
    fn the_ranges_cover_the_spec_lbas() {
        let l = layout();
        let img = build(&l).unwrap();
        assert_eq!(img.primary_offset, 0);
        // LBA 0 (MBR), LBA 1 (header), LBAs 2..=33 (array).
        assert_eq!(img.primary.len(), 34 * 512);
        // The array plus the last LBA's header.
        assert_eq!(img.backup.len(), 33 * 512);
        assert_eq!(img.backup_offset, (131_072 - 33) * 512);
        // The backup ends exactly at the end of the disk.
        assert_eq!(
            img.backup_offset + img.backup.len() as u64,
            l.disk_sectors * 512
        );
    }

    #[test]
    fn the_protective_mbr_covers_the_disk() {
        let img = build(&layout()).unwrap();
        assert_eq!(img.primary.get(450).copied(), Some(0xee));
        assert_eq!(img.primary.get(510..512), Some([0x55, 0xaa].as_slice()));
        assert_eq!(u32_at(&img.primary, 454).unwrap(), 1);
        assert_eq!(u32_at(&img.primary, 458).unwrap(), 131_071);
        // No second record, and no boot code.
        assert!(img.primary.get(0..446).unwrap().iter().all(|b| *b == 0));
        assert!(img.primary.get(462..510).unwrap().iter().all(|b| *b == 0));
    }

    /// Field offsets are the half of the format nothing else observes: a header
    /// whose fields are all in the wrong place still has a valid CRC.
    #[test]
    fn header_fields_sit_at_their_spec_offsets() {
        let img = build(&layout()).unwrap();
        let h = img.primary.get(512..604).unwrap();
        assert_eq!(h.get(0..8), Some(b"EFI PART".as_slice()));
        assert_eq!(u32_at(h, 8).unwrap(), 0x0001_0000);
        assert_eq!(u32_at(h, 12).unwrap(), 92);
        assert_eq!(u32_at(h, 20).unwrap(), 0, "reserved word must stay zero");
        assert_eq!(u64_at(h, 24).unwrap(), 1);
        assert_eq!(u64_at(h, 32).unwrap(), 131_071);
        assert_eq!(u64_at(h, 40).unwrap(), 34);
        assert_eq!(u64_at(h, 48).unwrap(), 131_038);
        assert_eq!(u64_at(h, 72).unwrap(), 2);
        assert_eq!(u32_at(h, 80).unwrap(), 128);
        assert_eq!(u32_at(h, 84).unwrap(), 128);
    }

    #[test]
    fn entry_fields_sit_at_their_spec_offsets() {
        let img = build(&layout()).unwrap();
        let e = img.primary.get(2 * 512..2 * 512 + 128).unwrap();
        assert_eq!(e.get(0..16), Some(TYPE_ESP.0.as_slice()));
        assert_eq!(e.get(16..32), Some(ESP_GUID.0.as_slice()));
        assert_eq!(u64_at(e, 32).unwrap(), 2048);
        assert_eq!(u64_at(e, 40).unwrap(), 34_815);
        assert_eq!(u64_at(e, 48).unwrap(), 0);
        // "td-esp" as UTF-16LE, then NUL padding to the end of the entry.
        assert_eq!(
            e.get(56..68),
            Some([b't', 0, b'd', 0, b'-', 0, b'e', 0, b's', 0, b'p', 0].as_slice())
        );
        assert!(e.get(68..128).unwrap().iter().all(|b| *b == 0));
    }

    /// A corrupt byte anywhere the CRCs cover must be REFUSED, not read.
    #[test]
    fn a_flipped_bit_fails_the_crcs() {
        let l = layout();
        let img = build(&l).unwrap();
        for at in [512 + 24, 512 + 56, 2 * 512 + 32, 2 * 512 + 60] {
            let mut broken = img.primary.clone();
            if let Some(b) = broken.get_mut(at) {
                *b ^= 0x01;
            }
            assert!(
                parse(&broken, &img.backup, l.sector_size).is_err(),
                "a flipped bit at {at} parsed cleanly"
            );
        }
    }

    /// A backup that disagrees is the failure that boots today and loses a
    /// partition the first time anything recovers from it.
    #[test]
    fn a_backup_that_disagrees_is_refused() {
        let l = layout();
        let img = build(&l).unwrap();
        let mut other = l.clone();
        if let Some(p) = other.partitions.get_mut(1) {
            p.end_lba = 100_000;
        }
        let different = build(&other).unwrap();
        let err = parse(&img.primary, &different.backup, l.sector_size).unwrap_err();
        assert!(err.contains("backup"), "{err}");
    }

    #[test]
    fn a_4096_byte_sector_disk_reserves_four_sectors_for_the_array() {
        let mut l = layout();
        l.sector_size = 4096;
        l.disk_sectors = 16_384;
        l.align_sectors = 256;
        l.partitions = vec![Partition {
            type_guid: TYPE_ESP,
            unique_guid: ESP_GUID,
            start_lba: 256,
            // Exactly the last usable block, so the inclusive end is pinned at
            // its boundary rather than short of it.
            end_lba: 16_378,
            attributes: 0,
            name: "td-esp".into(),
        }];
        let img = build(&l).unwrap();
        assert_eq!(entry_array_sectors(4096).unwrap(), 4);
        assert_eq!(img.primary.len(), 6 * 4096);
        assert_eq!(img.backup.len(), 5 * 4096);
        let table = parse(&img.primary, &img.backup, 4096).unwrap();
        assert_eq!(table.first_usable_lba, 6);
        assert_eq!(table.last_usable_lba, 16_378);
        assert_eq!(table.partitions, l.partitions);
    }

    #[test]
    fn refuses_layouts_that_would_corrupt_themselves() {
        let base = layout();
        let cases: Vec<(&str, Box<dyn Fn(&mut Layout)>)> = vec![
            (
                "overlap",
                Box::new(|l: &mut Layout| {
                    if let Some(p) = l.partitions.get_mut(1) {
                        p.start_lba = 34_000;
                    }
                }),
            ),
            (
                "descending",
                Box::new(|l: &mut Layout| l.partitions.reverse()),
            ),
            (
                "past the last usable block",
                Box::new(|l: &mut Layout| {
                    if let Some(p) = l.partitions.get_mut(1) {
                        p.end_lba = 131_071;
                    }
                }),
            ),
            (
                "inside the table",
                Box::new(|l: &mut Layout| {
                    if let Some(p) = l.partitions.get_mut(0) {
                        p.start_lba = 4;
                    }
                }),
            ),
            (
                "end before start",
                Box::new(|l: &mut Layout| {
                    if let Some(p) = l.partitions.get_mut(0) {
                        p.end_lba = 100;
                    }
                }),
            ),
            (
                "misaligned",
                Box::new(|l: &mut Layout| {
                    if let Some(p) = l.partitions.get_mut(0) {
                        p.start_lba = 2049;
                    }
                }),
            ),
            (
                "duplicate unique GUID",
                Box::new(|l: &mut Layout| {
                    if let Some(p) = l.partitions.get_mut(1) {
                        p.unique_guid = ESP_GUID;
                    }
                }),
            ),
            (
                "zero type GUID",
                Box::new(|l: &mut Layout| {
                    if let Some(p) = l.partitions.get_mut(0) {
                        p.type_guid = Guid::ZERO;
                    }
                }),
            ),
            (
                "zero disk GUID",
                Box::new(|l: &mut Layout| l.disk_guid = Guid::ZERO),
            ),
            (
                "37-code-unit name",
                Box::new(|l: &mut Layout| {
                    if let Some(p) = l.partitions.get_mut(0) {
                        p.name = "x".repeat(37);
                    }
                }),
            ),
            ("tiny disk", Box::new(|l: &mut Layout| l.disk_sectors = 67)),
            (
                "odd sector size",
                Box::new(|l: &mut Layout| l.sector_size = 520),
            ),
        ];
        for (what, mutate) in cases {
            let mut l = base.clone();
            mutate(&mut l);
            assert!(build(&l).is_err(), "{what} was accepted");
        }
    }

    /// 36 code units is the field's whole width, so it must be accepted — the
    /// refusal above is off-by-one otherwise.
    #[test]
    fn a_name_filling_the_field_is_accepted() {
        let mut l = layout();
        let full = "x".repeat(36);
        if let Some(p) = l.partitions.get_mut(0) {
            p.name = full.clone();
        }
        let img = build(&l).unwrap();
        let table = parse(&img.primary, &img.backup, l.sector_size).unwrap();
        assert_eq!(table.partitions.first().map(|p| p.name.clone()), Some(full));
    }

    #[test]
    fn a_full_entry_array_is_accepted_and_one_more_is_not() {
        let mut l = layout();
        l.align_sectors = 1;
        l.partitions = (0..128u32)
            .map(|i| Partition {
                type_guid: TYPE_LINUX_FS,
                unique_guid: Guid([u8::try_from(i + 1).unwrap_or(1); 16]),
                start_lba: 2048 + u64::from(i) * 16,
                end_lba: 2048 + u64::from(i) * 16 + 15,
                attributes: 0,
                name: format!("p{i}"),
            })
            .collect();
        let img = build(&l).unwrap();
        assert_eq!(
            parse(&img.primary, &img.backup, 512).unwrap().partitions.len(),
            128
        );
        l.partitions.push(Partition {
            type_guid: TYPE_LINUX_FS,
            unique_guid: Guid([0xff; 16]),
            start_lba: 100_000,
            end_lba: 100_015,
            attributes: 0,
            name: "overflow".into(),
        });
        assert!(build(&l).is_err());
    }

    /// A table with no partitions is a valid table — that is what an installer
    /// writes before it lays anything down.
    #[test]
    fn an_empty_table_is_valid() {
        let mut l = layout();
        l.partitions.clear();
        let img = build(&l).unwrap();
        assert!(parse(&img.primary, &img.backup, 512).unwrap().partitions.is_empty());
    }

    /// Recompute a header's two CRCs so a MUTATED table is as checksum-clean as
    /// a real one. Without this, every "parse refuses X" test below would be
    /// passing on the CRC rather than on X.
    fn seal_header(sector: &mut [u8], entries_crc: u32) {
        write_at(sector, 88, &entries_crc.to_le_bytes()).unwrap();
        write_at(sector, 16, &0u32.to_le_bytes()).unwrap();
        let size = usize::try_from(u32_at(sector, 12).unwrap()).unwrap();
        let crc = crc32(sector.get(..size).unwrap());
        write_at(sector, 16, &crc.to_le_bytes()).unwrap();
    }

    /// Re-seal a whole image after its primary entry array was edited.
    fn reseal(img: &mut Image, ss: usize) {
        let array_at = 2 * ss;
        let array = img
            .primary
            .get(array_at..array_at + 16384)
            .unwrap()
            .to_vec();
        let crc = crc32(&array);
        seal_header(img.primary.get_mut(ss..ss * 2).unwrap(), crc);
        img.backup.get_mut(..16384).unwrap().copy_from_slice(&array);
        let header_at = img.backup.len() - ss;
        seal_header(img.backup.get_mut(header_at..).unwrap(), crc);
    }

    /// A CRC says the bytes are intact, never that they describe a disk that
    /// works. Each of these tables checksums perfectly and would destroy data.
    #[test]
    fn parse_refuses_a_checksum_clean_but_impossible_table() {
        let l = layout();
        let cases: Vec<(&str, &str, Box<dyn Fn(&mut [u8])>)> = vec![
            (
                "overlap",
                "overlap",
                // Pull the second partition's start back inside the first.
                Box::new(|a: &mut [u8]| write_at(a, 128 + 32, &30_000u64.to_le_bytes()).unwrap()),
            ),
            (
                "past the last usable block",
                "past the last usable block",
                Box::new(|a: &mut [u8]| write_at(a, 128 + 40, &131_070u64.to_le_bytes()).unwrap()),
            ),
            (
                "end before start",
                "INCLUSIVE",
                Box::new(|a: &mut [u8]| write_at(a, 40, &100u64.to_le_bytes()).unwrap()),
            ),
            (
                "duplicate unique GUID",
                "share the unique GUID",
                Box::new(|a: &mut [u8]| {
                    let first = a.get(16..32).unwrap().to_vec();
                    write_at(a, 128 + 16, &first).unwrap();
                }),
            ),
            (
                "zero unique GUID",
                "all-zero unique GUID",
                Box::new(|a: &mut [u8]| write_at(a, 16, &[0u8; 16]).unwrap()),
            ),
        ];
        for (what, want, mutate) in cases {
            let mut img = build(&l).unwrap();
            let array_at = 2 * 512;
            mutate(img.primary.get_mut(array_at..array_at + 16384).unwrap());
            reseal(&mut img, 512);
            let err = match parse(&img.primary, &img.backup, 512) {
                Ok(_) => panic!("{what} parsed cleanly"),
                Err(e) => e,
            };
            assert!(err.contains(want), "{what}: got {err}");
        }
    }

    /// Firmware recovering from the backup reads what THAT header points at, so
    /// a backup header aimed somewhere else is a disk with no recovery table
    /// however well the primary checksums.
    #[test]
    fn parse_refuses_a_backup_header_that_points_elsewhere() {
        let l = layout();
        let mut img = build(&l).unwrap();
        let header_at = img.backup.len() - 512;
        let header = img.backup.get_mut(header_at..).unwrap();
        // Claim the array lives one LBA earlier than it does.
        let was = u64_at(header, 72).unwrap();
        write_at(header, 72, &(was - 1).to_le_bytes()).unwrap();
        let crc = u32_at(header, 88).unwrap();
        seal_header(header, crc);
        let err = parse(&img.primary, &img.backup, 512).unwrap_err();
        assert!(err.contains("entry array at LBA"), "{err}");
    }

    /// The spec's 16384 bytes is a FLOOR, so a bigger array is valid and a
    /// smaller one is not — the check that used to be here had it backwards.
    #[test]
    fn parse_refuses_an_entry_array_below_the_spec_minimum() {
        let l = layout();
        let mut img = build(&l).unwrap();
        let header = img.primary.get_mut(512..1024).unwrap();
        write_at(header, 80, &64u32.to_le_bytes()).unwrap();
        let crc = u32_at(header, 88).unwrap();
        seal_header(header, crc);
        let err = parse(&img.primary, &img.backup, 512).unwrap_err();
        assert!(err.contains("minimum the spec requires"), "{err}");
    }

    /// The helpers an installer computes a layout FROM must refuse a disk that
    /// cannot hold a table, rather than answering with an inverted range.
    #[test]
    fn the_usable_range_helpers_refuse_a_disk_too_small_for_a_table() {
        assert_eq!(minimum_disk_sectors(512).unwrap(), 68);
        assert_eq!(last_usable_lba(512, 68).unwrap(), 34);
        assert_eq!(first_usable_lba(512).unwrap(), 34);
        for too_small in [0, 1, 34, 40, 67] {
            assert!(
                last_usable_lba(512, too_small).is_err(),
                "a {too_small}-sector disk answered with a usable range"
            );
        }
    }

    /// A zero-initialized `Layout` asking for no alignment by accident is the
    /// one case where taking it silently is wrong.
    #[test]
    fn a_zero_alignment_is_refused_rather_than_read_as_none() {
        let mut l = layout();
        l.align_sectors = 0;
        let err = build(&l).unwrap_err();
        assert!(err.contains("use 1"), "{err}");
        l.align_sectors = 1;
        assert!(build(&l).is_ok());
    }

    /// A protective MBR that does not COVER the disk leaves the rest looking
    /// unallocated to every MBR-only tool — the failure it exists to prevent.
    #[test]
    fn parse_refuses_a_protective_mbr_that_does_not_cover_the_disk() {
        let l = layout();
        let img = build(&l).unwrap();
        for (at, value) in [(454u64, 2u32), (458, 4096)] {
            let mut broken = img.primary.clone();
            write_at(&mut broken, usize::try_from(at).unwrap(), &value.to_le_bytes()).unwrap();
            let err = parse(&broken, &img.backup, 512).unwrap_err();
            assert!(err.contains("protective MBR record"), "{err}");
        }
    }

    /// The reserved word is inside the CRC'd region, so a non-zero one is a
    /// header written against a different idea of the layout, not a bit flip.
    #[test]
    fn parse_refuses_a_non_zero_reserved_word() {
        let l = layout();
        let mut img = build(&l).unwrap();
        let header = img.primary.get_mut(512..1024).unwrap();
        write_at(header, 20, &1u32.to_le_bytes()).unwrap();
        let crc = u32_at(header, 88).unwrap();
        seal_header(header, crc);
        let err = parse(&img.primary, &img.backup, 512).unwrap_err();
        assert!(err.contains("reserved word"), "{err}");
    }

    /// A NUL terminates the on-disk field, so `build` writing one would hand
    /// `parse` back a shorter name than the caller asked for.
    #[test]
    fn a_name_containing_a_nul_is_refused() {
        let mut l = layout();
        if let Some(p) = l.partitions.get_mut(0) {
            p.name = "td\0esp".into();
        }
        let err = build(&l).unwrap_err();
        assert!(err.contains("NUL"), "{err}");
    }

    /// Nothing here reads a clock or an allocator address, so one layout is one
    /// byte sequence.
    #[test]
    fn two_builds_of_one_layout_are_byte_identical() {
        let l = layout();
        let a = build(&l).unwrap();
        let b = build(&l).unwrap();
        assert_eq!(a.primary, b.primary);
        assert_eq!(a.backup, b.backup);
    }
}

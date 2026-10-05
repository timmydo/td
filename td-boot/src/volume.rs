//! Read-only identity discovery; a match grants neither trust nor write authority.
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileExt, FileTypeExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use td_protector::luks2;

use crate::{invalid, protocol};

const SUPER_OFFSET: u64 = 65536;
const SUPER_BYTES: usize = 4096;
const MAX_DEVICES: usize = 4096;
const OPEN_FLAGS: i32 = 0o400000 | 0o4000; // x86-64 Linux O_NOFOLLOW | O_NONBLOCK.
const CLASS_BLOCK: &str = "/sys/class/block";
// Whole disks, where device-mapper nodes appear.
const SYS_BLOCK: &str = "/sys/block";
// cryptsetup 2.8.8 lib/libdevmapper.c dm_prepare_uuid: the LUKS2 UUID
// without its dashes, then the mapping's name.
const MAPPING_UUID_PREFIX: &str = "CRYPT-LUKS2-";
// A crypt mapping sits over one device; a few more entries are read only
// to refuse them.
const MAX_SLAVES: usize = 16;
// An ordinary sysfs attribute here: a device number, a size, a dm name
// (DM_NAME_LEN 128 with its NUL) and its newline.
const MAX_SYSFS_BYTES: usize = 128;
// DM_UUID_LEN is 129 with its NUL: 128 characters and a newline.
const MAX_DM_UUID_BYTES: usize = 129;
// A dm-N that vanishes mid-walk makes the walk incomplete; it is retried
// this long.
const MAPPING_WAIT: Duration = Duration::from_secs(5);
// Distinct notes one resolve reports; the rest are dropped.
const MAX_NOTES: usize = 64;
pub(crate) const ENCRYPTED_UNSUPPORTED: &str = "encrypted volume: not yet supported";

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Uuid([u8; 16]);

impl Uuid {
    pub(crate) fn parse(text: &str) -> io::Result<Self> {
        if text.len() != 36 {
            return Err(invalid("volume UUID must be canonical lowercase hex"));
        }
        let mut bytes = [0; 16];
        let mut digits = Vec::with_capacity(32);
        for (index, byte) in text.bytes().enumerate() {
            if matches!(index, 8 | 13 | 18 | 23) {
                if byte != b'-' {
                    return Err(invalid("invalid volume UUID separator"));
                }
            } else {
                digits.push(match byte {
                    b'0'..=b'9' => byte - b'0',
                    b'a'..=b'f' => byte - b'a' + 10,
                    _ => return Err(invalid("invalid volume UUID digit")),
                });
            }
        }
        for (out, [high, low]) in bytes.iter_mut().zip(digits.as_chunks::<2>().0) {
            *out = high * 16 + low;
        }
        Self::from_bytes(bytes)
    }

    fn from_bytes(bytes: [u8; 16]) -> io::Result<Self> {
        if bytes == [0; 16] {
            return Err(invalid("zero volume UUID"));
        }
        Ok(Self(bytes))
    }

    /// A fresh version-4 UUID for one live boot's volatile volume.
    pub(crate) fn random() -> io::Result<Self> {
        let mut bytes = [0; 16];
        File::open("/dev/urandom")?.read_exact(&mut bytes)?;
        Ok(Self::from_random(bytes))
    }

    fn from_random(mut bytes: [u8; 16]) -> Self {
        bytes[6] = (bytes[6] & 0x0f) | 0x40;
        bytes[8] = (bytes[8] & 0x3f) | 0x80;
        // The version bits make it nonzero.
        Self(bytes)
    }
}

impl std::fmt::Display for Uuid {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for (index, byte) in self.0.iter().enumerate() {
            if matches!(index, 4 | 6 | 8 | 10) {
                out.write_str("-")?;
            }
            write!(out, "{byte:02x}")?;
        }
        Ok(())
    }
}

fn field<const N: usize>(bytes: &[u8], offset: usize) -> io::Result<[u8; N]> {
    bytes
        .get(offset..offset.saturating_add(N))
        .and_then(|value| value.try_into().ok())
        .ok_or_else(|| invalid("truncated Btrfs superblock"))
}

fn crc32c(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0x82f63b78 & 0u32.wrapping_sub(crc & 1));
        }
    }
    !crc
}

// btrfs-progs v7.0 kernel-shared/uapi/btrfs_tree.h, btrfs_super_block.
fn identify(bytes: &[u8]) -> io::Result<Option<Uuid>> {
    if bytes.len() != SUPER_BYTES {
        return Err(invalid("truncated Btrfs superblock"));
    }
    if field::<8>(bytes, 64)? != *b"_BHRfS_M" {
        return Ok(None);
    }
    let label = field::<256>(bytes, 299)?;
    let label = label.split(|byte| *byte == 0).next().unwrap_or_default();
    if label != protocol::VOLUME_LABEL.as_bytes() {
        return Ok(None);
    }
    let payload = bytes.get(32..).ok_or_else(|| invalid("short superblock"))?;
    if u16::from_le_bytes(field(bytes, 196)?) != 0
        || u32::from_le_bytes(field(bytes, 0)?) != crc32c(payload)
    {
        return Err(invalid(
            "td volume superblock checksum is unsupported or invalid",
        ));
    }
    if u64::from_le_bytes(field(bytes, 48)?) != SUPER_OFFSET
        || u64::from_le_bytes(field(bytes, 136)?) != 1
    {
        return Err(invalid(
            "td volume requires a primary single-device superblock",
        ));
    }
    // Seed/metadump/UUID-changing filesystems cannot serve this boot profile.
    if u64::from_le_bytes(field(bytes, 56)?) & !1 != 0 {
        return Err(invalid("unsupported td volume superblock flags"));
    }
    Ok(Some(Uuid::from_bytes(field(bytes, 32)?)?))
}

/// Which td volume a device carries.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Kind {
    /// A td Btrfs filesystem on the device itself.
    Btrfs,
    /// A td LUKS2 header, whose dm-crypt mapping carries the Btrfs volume.
    Luks2,
}

fn read_block<R: Read + Seek>(device: &mut R, offset: u64, out: &mut [u8]) -> io::Result<()> {
    device.seek(SeekFrom::Start(offset))?;
    device.read_exact(out)
}

fn wanted(claimed: &[u8], expected: Option<&Uuid>) -> bool {
    expected.is_none_or(|uuid| claimed == uuid.to_string().as_bytes())
}

/// A td LUKS2 header's UUID, by identity alone. A binary header copy at
/// the primary's offset or one of cryptsetup's secondary offsets that
/// carries its magic, version 2 and the td label is a claim. Only a claim
/// naming the volume sought (any, without `expected`) hands the device to
/// `identity`, the copy cryptsetup would use with its checksum verified;
/// another volume's claim is passed over with a note and never verified,
/// so another disk's header cannot stop this volume's discovery. A header
/// read that fails before any wanted claim passes the device over too.
/// The tokens are read on the selected volume only, when it is opened.
fn identify_luks2<R: Read + Seek>(
    device: &mut R,
    size: u64,
    expected: Option<&Uuid>,
    notes: &mut Vec<String>,
    identity: impl FnOnce(&mut R) -> Result<luks2::Identity, String>,
) -> io::Result<Option<Uuid>> {
    let label = protocol::VOLUME_LABEL.as_bytes();
    let mut binary = [0; luks2::BINARY_HEADER_LEN];
    let copies =
        std::iter::once((0, false)).chain(luks2::HEADER_SIZES.iter().map(|at| (*at, true)));
    let mut claimed = false;
    let mut other: Option<String> = None;
    for (offset, secondary) in copies {
        if offset.saturating_add(luks2::BINARY_HEADER_LEN as u64) > size {
            continue;
        }
        if let Err(error) = read_block(device, offset, &mut binary) {
            notes.push(format!(
                "LUKS2 header probe at byte {offset} failed ({error}); not a td LUKS2 volume"
            ));
            return Ok(None);
        }
        let Some(claim) = luks2::binary_claim(&binary, secondary) else {
            continue;
        };
        if claim.label != label {
            continue;
        }
        if wanted(claim.uuid, expected) {
            claimed = true;
            break;
        }
        other.get_or_insert_with(|| String::from_utf8_lossy(claim.uuid).into_owned());
    }
    if !claimed {
        if let Some(uuid) = other {
            notes.push(format!(
                "td LUKS2 header claiming volume {uuid:?} passed over, unverified: not the volume sought"
            ));
        }
        return Ok(None);
    }
    let found = identity(device).map_err(|error| invalid(format!("td LUKS2 header: {error}")))?;
    if found.label.as_bytes() != label {
        notes.push("the td LUKS2 header copy in use has another label; not a td volume".into());
        return Ok(None);
    }
    let uuid = Uuid::parse(&found.uuid)
        .map_err(|error| invalid(format!("td LUKS2 header UUID: {error}")))?;
    if expected.is_some_and(|expected| expected != &uuid) {
        notes.push(format!(
            "the td LUKS2 header copy in use names volume {uuid}, not the volume sought"
        ));
        return Ok(None);
    }
    Ok(Some(uuid))
}

/// The td volume `device`, of `size` bytes, carries. LUKS2 and Btrfs are
/// counted together, so a device carrying both is ambiguous.
fn identify_volume<R: Read + Seek>(
    device: &mut R,
    size: u64,
    expected: Option<&Uuid>,
    notes: &mut Vec<String>,
    identity: impl FnOnce(&mut R) -> Result<luks2::Identity, String>,
) -> io::Result<Option<(Uuid, Kind)>> {
    let mut superblock = [0; SUPER_BYTES];
    read_block(device, SUPER_OFFSET, &mut superblock)?;
    let btrfs = identify(&superblock)?;
    match (btrfs, identify_luks2(device, size, expected, notes, identity)?) {
        (Some(_), Some(_)) => Err(invalid(
            "ambiguous td volume identity: one device carries a td LUKS2 header and a td Btrfs superblock",
        )),
        (Some(uuid), None) => Ok(Some((uuid, Kind::Btrfs))),
        (None, Some(uuid)) => Ok(Some((uuid, Kind::Luks2))),
        (None, None) => Ok(None),
    }
}

/// The scan's candidates. A device-mapper node is never one: it is
/// admitted only as the active mapping of a pinned LUKS2 partition
/// (`mapping_name`, `find_mapping`), so a mapping and its partition are
/// one volume rather than two devices carrying the UUID.
fn supported_name(name: &str) -> bool {
    // brd's device, where a live boot keeps its volatile volume.
    if let Some(number) = name.strip_prefix("ram") {
        return !number.is_empty() && number.bytes().all(|b| b.is_ascii_digit());
    }
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

fn text(path: &Path) -> io::Result<String> {
    text_bounded(path, MAX_SYSFS_BYTES)
}

fn text_bounded(path: &Path, max: usize) -> io::Result<String> {
    let mut bytes = Vec::new();
    File::open(path)?
        .take(max.saturating_add(1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > max {
        return Err(invalid(format!("oversized sysfs value {}", path.display())));
    }
    String::from_utf8(bytes)
        .map_err(|_| invalid(format!("non-ASCII sysfs value {}", path.display())))
}

fn device_number(value: &str) -> io::Result<(u64, u64)> {
    let value = value.strip_suffix('\n').unwrap_or(value);
    let Some((major, minor)) = value.split_once(':') else {
        return Err(invalid("invalid sysfs device number"));
    };
    let parse = |part: &str| -> io::Result<u64> {
        if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
            return Err(invalid("invalid sysfs device number"));
        }
        part.parse()
            .map_err(|_| invalid("oversized sysfs device number"))
    };
    Ok((parse(major)?, parse(minor)?))
}

fn rdev_numbers(meta: &fs::Metadata) -> (u64, u64) {
    let dev = meta.rdev();
    let major = ((dev >> 8) & 0xfff) | ((dev >> 32) & 0xfffff000);
    let minor = (dev & 0xff) | ((dev >> 12) & 0xffffff00);
    (major, minor)
}

fn matches_device(meta: &fs::Metadata, expected: (u64, u64)) -> bool {
    meta.file_type().is_block_device() && rdev_numbers(meta) == expected
}

fn probe_open(
    sys: &Path,
    path: &Path,
    expected_uuid: Option<&Uuid>,
    notes: &mut Vec<String>,
) -> io::Result<Option<(Uuid, Kind, File)>> {
    let expected = device_number(&text(&sys.join("dev"))?)?;
    let sectors = text(&sys.join("size"))?;
    let sectors: u64 = sectors
        .trim_end()
        .parse()
        .map_err(|_| invalid("invalid block capacity"))?;
    if sectors < (SUPER_OFFSET + SUPER_BYTES as u64) / 512 {
        return Ok(None);
    }
    let size = sectors.saturating_mul(512);
    let before = fs::symlink_metadata(path)?;
    if !matches_device(&before, expected) {
        return Err(invalid("volume path disagrees with sysfs block identity"));
    }
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(OPEN_FLAGS)
        .open(path)?;
    let opened = file.metadata()?;
    if !matches_device(&opened, expected)
        || before.dev() != opened.dev()
        || before.ino() != opened.ino()
    {
        return Err(invalid("volume device changed during open"));
    }
    let result = identify_volume(&mut &file, size, expected_uuid, notes, luks2::identity)?;
    let after = fs::symlink_metadata(path)?;
    if !matches_device(&after, expected)
        || after.dev() != opened.dev()
        || after.ino() != opened.ino()
    {
        return Err(invalid("volume device changed during probe"));
    }
    Ok(result.map(|(uuid, kind)| (uuid, kind, file)))
}

pub(crate) struct Pinned {
    file: File,
    pub(crate) device: PathBuf,
}

impl Pinned {
    // The parent retains this descriptor across every child mount and transaction.
    pub(crate) fn path(&self) -> PathBuf {
        PathBuf::from(format!(
            "/proc/{}/fd/{}",
            std::process::id(),
            self.file.as_raw_fd()
        ))
    }

    /// The held descriptor, which td's LUKS2 header reader reads through.
    pub(crate) fn file(&self) -> &File {
        &self.file
    }

    #[cfg(test)]
    pub(crate) fn for_test(file: File, device: PathBuf) -> Self {
        Self { file, device }
    }
}

/// What sysfs says of a dm-N that claims the volume.
#[derive(Debug, Eq, PartialEq)]
struct MappingEntry {
    /// The kernel's node name, `dm-N`.
    node: String,
    /// The device-mapper name cryptsetup opened it under.
    name: String,
}

/// An active dm-crypt mapping admitted for a pinned LUKS2 partition, held
/// open for the consumer that mounts through it.
pub(crate) struct Mapping {
    entry: MappingEntry,
    file: File,
}

impl Mapping {
    /// Its device-mapper name, `dm/name`.
    pub(crate) fn name(&self) -> &str {
        &self.entry.name
    }

    /// The held mapping as the operation's device, reported as `device`,
    /// the partition it maps.
    pub(crate) fn into_pinned(self, device: PathBuf) -> Pinned {
        Pinned {
            file: self.file,
            device,
        }
    }

    /// The held mapping, as `Pinned::path` names a partition.
    pub(crate) fn path(&self) -> PathBuf {
        PathBuf::from(format!(
            "/proc/{}/fd/{}",
            std::process::id(),
            self.file.as_raw_fd()
        ))
    }

    /// `dm-N (NAME)`, for the console.
    pub(crate) fn describe(&self) -> String {
        format!("{} ({})", self.entry.node, self.entry.name)
    }

    #[cfg(test)]
    pub(crate) fn for_test(node: &str, name: &str, file: File) -> Self {
        Self {
            entry: MappingEntry {
                node: node.into(),
                name: name.into(),
            },
            file,
        }
    }
}

/// The volume discovery selected, reopened and pinned.
pub(crate) enum Opened {
    Btrfs(Pinned),
    /// The LUKS2 partition, and its mapping when one is active.
    Luks2 {
        partition: Pinned,
        mapping: Option<Mapping>,
    },
}

impl Opened {
    pub(crate) fn open(uuid: &Uuid) -> io::Result<Self> {
        let found = resolve(Some(uuid))?;
        let name = found
            .path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| invalid("missing volume name"))?;
        // The scan already reported what it passed over.
        let mut notes = Vec::new();
        let (id, kind, file) = probe_open(
            &Path::new(CLASS_BLOCK).join(name),
            &found.path,
            Some(uuid),
            &mut notes,
        )?
        .ok_or_else(|| invalid("resolved volume disappeared"))?;
        if &id != uuid || kind != found.kind {
            return Err(invalid("resolved volume identity changed"));
        }
        let mapping = match kind {
            Kind::Btrfs => None,
            Kind::Luks2 => open_mapping(name, uuid)?,
        };
        let partition = Pinned {
            file,
            device: found.path,
        };
        Ok(match kind {
            Kind::Btrfs => Self::Btrfs(partition),
            Kind::Luks2 => Self::Luks2 { partition, mapping },
        })
    }
}

pub(crate) fn encrypted_unsupported(device: &Path, mapping: Option<&Mapping>) -> io::Error {
    let mapping = match mapping {
        Some(mapping) => {
            let number = match mapping.file.metadata() {
                Ok(meta) => {
                    let (major, minor) = rdev_numbers(&meta);
                    format!("{major}:{minor}")
                }
                Err(error) => format!("device number unreadable: {error}"),
            };
            format!(
                "its mapping {} ({}, {number}) is active",
                mapping.entry.node, mapping.entry.name
            )
        }
        None => "no mapping of it is active".to_owned(),
    };
    io::Error::new(
        io::ErrorKind::Unsupported,
        format!(
            "{ENCRYPTED_UNSUPPORTED}: {} holds a td LUKS2 volume; {mapping}",
            device.display()
        ),
    )
}

/// A device-mapper node name, `dm-N`.
fn mapping_name(name: &str) -> bool {
    name.strip_prefix("dm-")
        .is_some_and(|number| !number.is_empty() && number.bytes().all(|b| b.is_ascii_digit()))
}

fn mapping_uuid_prefix(uuid: &Uuid) -> String {
    let mut prefix = String::from(MAPPING_UUID_PREFIX);
    for byte in &uuid.0 {
        prefix.push_str(&format!("{byte:02x}"));
    }
    prefix.push('-');
    prefix
}

fn sysfs_line(path: &Path, max: usize) -> io::Result<String> {
    let value = text_bounded(path, max)?;
    Ok(value.strip_suffix('\n').unwrap_or(&value).to_owned())
}

fn slaves(directory: &Path) -> io::Result<Vec<String>> {
    let mut names = Vec::new();
    for entry in fs::read_dir(directory)? {
        if names.len() == MAX_SLAVES {
            return Err(invalid(format!(
                "too many devices under {}",
                directory.display()
            )));
        }
        let name = entry?.file_name();
        let name = name
            .to_str()
            .ok_or_else(|| invalid("non-ASCII device-mapper slave name"))?;
        names.push(name.to_owned());
    }
    names.sort_unstable();
    Ok(names)
}

/// The dm-N at `directory` if its `dm/uuid` claims the volume; a claim
/// whose name disagrees with `dm/name` or whose `slaves/` is not exactly
/// `partition` refuses.
fn mapping_entry(
    directory: &Path,
    node: &str,
    uuid: &Uuid,
    partition: &str,
) -> io::Result<Option<MappingEntry>> {
    let dm_uuid = sysfs_line(&directory.join("dm/uuid"), MAX_DM_UUID_BYTES)?;
    let Some(named) = dm_uuid.strip_prefix(&mapping_uuid_prefix(uuid)) else {
        return Ok(None);
    };
    let name = sysfs_line(&directory.join("dm/name"), MAX_SYSFS_BYTES)?;
    if named != name {
        return Err(invalid(format!(
            "mapping {node} of td volume {uuid} is named {name:?} but its dm uuid names {named:?}"
        )));
    }
    if slaves(&directory.join("slaves"))? != [partition] {
        return Err(invalid(format!(
            "mapping {node} of td volume {uuid} is not over exactly {partition}"
        )));
    }
    Ok(Some(MappingEntry {
        node: node.to_owned(),
        name,
    }))
}

#[derive(Debug, Eq, PartialEq)]
enum MappingScan {
    Complete(Option<MappingEntry>),
    /// A node or attribute vanished during the walk: walk again.
    Incomplete,
}

/// The one dm-N under `sys_block` that claims `partition`'s volume, found
/// through sysfs alone and never by a `/dev/mapper` name: its `dm/uuid`
/// is `CRYPT-LUKS2-<the UUID's 32 hex digits>-<its dm/name>` and its
/// `slaves/` names exactly `partition`. A malformed claim and a second
/// claim refuse.
fn find_mapping(sys_block: &Path, partition: &str, uuid: &Uuid) -> io::Result<MappingScan> {
    let mut found: Option<MappingEntry> = None;
    for (index, entry) in fs::read_dir(sys_block)?.enumerate() {
        if index >= MAX_DEVICES {
            return Err(invalid("too many block devices for mapping discovery"));
        }
        let entry = match entry {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(MappingScan::Incomplete)
            }
            other => other?,
        };
        let node = entry.file_name();
        let Some(node) = node.to_str().filter(|node| mapping_name(node)) else {
            continue;
        };
        let claim = match mapping_entry(&entry.path(), node, uuid, partition) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(MappingScan::Incomplete)
            }
            other => other?,
        };
        let Some(claim) = claim else {
            continue;
        };
        if let Some(first) = &found {
            return Err(invalid(format!(
                "td volume {uuid} has two active mappings, {} and {node}",
                first.node
            )));
        }
        found = Some(claim);
    }
    Ok(MappingScan::Complete(found))
}

/// The admitted mapping, opened through `/dev/dm-N` and held. The node is
/// opened under probe_open's device-number and inode checks and must carry
/// the volume's Btrfs; its sysfs claim and `dev` are then read again and
/// must still describe the held node.
fn hold_mapping(entry: MappingEntry, partition: &str, uuid: &Uuid) -> io::Result<Mapping> {
    let directory = Path::new(SYS_BLOCK).join(&entry.node);
    let path = Path::new("/dev").join(&entry.node);
    let mut notes = Vec::new();
    let file = match probe_open(&directory, &path, Some(uuid), &mut notes)? {
        Some((found, Kind::Btrfs, file)) if &found == uuid => file,
        _ => {
            return Err(invalid(format!(
                "mapping {} of td volume {uuid} does not carry its Btrfs volume",
                entry.node
            )))
        }
    };
    let again = mapping_entry(&directory, &entry.node, uuid, partition)?;
    let number = device_number(&text(&directory.join("dev"))?)?;
    if again.as_ref() != Some(&entry) || !matches_device(&file.metadata()?, number) {
        return Err(invalid(format!(
            "mapping {} of td volume {uuid} changed while it was opened",
            entry.node
        )));
    }
    Ok(Mapping { entry, file })
}

/// The mapping the selector's cryptsetup opened over the pinned LUKS2
/// `partition` under `expected`, admitted as discovery admits one and held
/// for the mounts; one under another name is refused.
pub(crate) fn admit_mapping(
    partition: &Pinned,
    uuid: &Uuid,
    expected: &str,
) -> io::Result<Mapping> {
    let name = partition
        .device
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| invalid("missing volume name"))?;
    let mapping = open_mapping(name, uuid)?.ok_or_else(|| {
        invalid(format!(
            "td volume {uuid} has no active mapping after cryptsetup opened it"
        ))
    })?;
    admitted_name(mapping, expected)
}

/// `mapping` when device-mapper names it `expected`.
fn admitted_name(mapping: Mapping, expected: &str) -> io::Result<Mapping> {
    if mapping.entry.name != expected {
        return Err(invalid(format!(
            "the active mapping {} is not the {expected:?} the selector opened",
            mapping.describe()
        )));
    }
    Ok(mapping)
}

/// The partition's active mapping, held; `None` when none is active.
pub(crate) fn open_mapping(partition: &str, uuid: &Uuid) -> io::Result<Option<Mapping>> {
    let started = Instant::now();
    loop {
        match find_mapping(Path::new(SYS_BLOCK), partition, uuid)? {
            MappingScan::Complete(None) => return Ok(None),
            MappingScan::Complete(Some(entry)) => {
                return hold_mapping(entry, partition, uuid).map(Some)
            }
            MappingScan::Incomplete => {}
        }
        if started.elapsed() >= MAPPING_WAIT {
            return Err(invalid(format!(
                "mapping discovery for td volume {uuid} did not settle"
            )));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

pub(crate) fn configured(bytes: &[u8]) -> io::Result<Uuid> {
    let text = std::str::from_utf8(bytes).map_err(|_| invalid("non-ASCII volume UUID"))?;
    Uuid::parse(
        text.strip_suffix('\n')
            .ok_or_else(|| invalid("volume UUID needs a newline"))?,
    )
}

pub(crate) fn handoff(bytes: &[u8]) -> io::Result<Uuid> {
    let text = std::str::from_utf8(bytes).map_err(|_| invalid("non-ASCII kernel command line"))?;
    let mut found = None;
    for word in text.split_ascii_whitespace() {
        if let Some(uuid) = word.strip_prefix(protocol::VOLUME_CMDLINE_PREFIX) {
            if found.is_some() {
                return Err(invalid("duplicate td.volume handoff"));
            }
            found = Some(Uuid::parse(uuid)?);
        }
    }
    found.ok_or_else(|| invalid("missing td.volume handoff"))
}

pub(crate) fn command_line(bytes: &[u8], uuid: &Uuid) -> io::Result<std::ffi::OsString> {
    use std::os::unix::ffi::OsStringExt;
    if bytes
        .split(u8::is_ascii_whitespace)
        .any(|word| word.starts_with(protocol::VOLUME_CMDLINE_PREFIX.as_bytes()))
    {
        return Err(invalid("selector command line already has td.volume"));
    }
    let mut result = bytes.to_vec();
    result.extend_from_slice(format!(" {}{uuid}", protocol::VOLUME_CMDLINE_PREFIX).as_bytes());
    Ok(std::ffi::OsString::from_vec(result))
}

/// One device a scan identified as a td volume.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Found {
    pub(crate) uuid: Uuid,
    pub(crate) kind: Kind,
    pub(crate) path: PathBuf,
}

impl Found {
    /// `td-boot volume`'s line. It refuses an encrypted volume as every
    /// other consumer does, naming the mapping discovery finds.
    pub(crate) fn describe(&self) -> io::Result<String> {
        self.describe_with(open_mapping)
    }

    fn describe_with(
        &self,
        mapping: impl FnOnce(&str, &Uuid) -> io::Result<Option<Mapping>>,
    ) -> io::Result<String> {
        match self.kind {
            Kind::Btrfs => Ok(format!("{} {}", self.uuid, self.path.display())),
            Kind::Luks2 => {
                let name = self
                    .path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .ok_or_else(|| invalid("missing volume name"))?;
                let mapping = mapping(name, &self.uuid)?;
                Err(encrypted_unsupported(&self.path, mapping.as_ref()))
            }
        }
    }
}

fn choose(found: &mut Option<Found>, candidate: Found, expected: Option<&Uuid>) -> io::Result<()> {
    if expected.is_some_and(|expected| expected != &candidate.uuid) {
        return Ok(());
    }
    if found.is_some() {
        return Err(invalid("ambiguous td volume identity"));
    }
    *found = Some(candidate);
    Ok(())
}

fn select_scan(
    entries: impl IntoIterator<Item = io::Result<Option<Found>>>,
    expected: Option<&Uuid>,
) -> io::Result<Option<Found>> {
    let mut found = None;
    for entry in entries {
        let entry = match entry {
            // A disappearing/not-yet-published node makes this entire scan incomplete.
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            other => other?,
        };
        if let Some(candidate) = entry {
            choose(&mut found, candidate, expected)?;
        }
    }
    Ok(found)
}

fn scan(expected: Option<&Uuid>, notes: &mut Vec<String>) -> io::Result<Option<Found>> {
    let entries = fs::read_dir(CLASS_BLOCK)?
        .enumerate()
        .map(|(index, entry)| {
            if index >= MAX_DEVICES {
                return Err(invalid("too many block devices for volume discovery"));
            }
            let entry = entry?;
            let name = entry.file_name();
            let name = name
                .to_str()
                .ok_or_else(|| invalid("non-ASCII block name"))?;
            if !supported_name(name) {
                return Ok(None);
            }
            let path = Path::new("/dev").join(name);
            let mut device_notes = Vec::new();
            let identity =
                probe_open(&entry.path(), &path, expected, &mut device_notes).map_err(|error| {
                    io::Error::new(
                        error.kind(),
                        format!("probe volume {}: {error}", path.display()),
                    )
                })?;
            notes.extend(
                device_notes
                    .into_iter()
                    .map(|note| format!("{}: {note}", path.display())),
            );
            Ok(identity.map(|(uuid, kind, _)| Found { uuid, kind, path }))
        });
    select_scan(entries, expected)
}

/// Writes each note not yet reported, whole, to standard error.
fn report_notes(
    out: &mut dyn Write,
    reported: &mut Vec<String>,
    notes: Vec<String>,
) -> io::Result<()> {
    for note in notes {
        if reported.contains(&note) || reported.len() >= MAX_NOTES {
            continue;
        }
        out.write_all(format!("td-boot: {note}\n").as_bytes())?;
        reported.push(note);
    }
    Ok(())
}

pub(crate) fn resolve(expected: Option<&Uuid>) -> io::Result<Found> {
    let started = Instant::now();
    let mut reported = Vec::new();
    loop {
        let mut notes = Vec::new();
        let found = scan(expected, &mut notes);
        report_notes(&mut io::stderr(), &mut reported, notes)?;
        if let Some(volume) = found? {
            return Ok(volume);
        }
        if started.elapsed() >= Duration::from_secs(30) {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "td volume did not appear",
            ));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

// Install media: the ISO-9660 primary volume descriptor at sector 16, whose
// identifier `engine/src/iso9660.rs` writes. A hybrid image carries it at the
// same offset whether firmware reads it as optical media or as a whole USB disk.
const ISO_SECTOR_BYTES: usize = 2048;
const ISO_PVD_OFFSET: u64 = 16 * ISO_SECTOR_BYTES as u64;
const ISO_VOLUME_ID: std::ops::Range<usize> = 40..72;
const ENOMEDIUM: i32 = 123;
const MEDIA_WAIT: Duration = Duration::from_secs(30);

/// Whole devices a hybrid image can be attached as: optical, and SCSI-named,
/// virtio or NVMe disks. Partitions are excluded because the descriptor is at
/// the start of the whole device.
fn media_device_name(name: &str) -> bool {
    let digits = |value: &str| !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit());
    if let Some(number) = name.strip_prefix("sr") {
        return digits(number);
    }
    if let Some(letters) = name.strip_prefix("sd").or_else(|| name.strip_prefix("vd")) {
        return !letters.is_empty() && letters.bytes().all(|b| b.is_ascii_lowercase());
    }
    name.strip_prefix("nvme")
        .and_then(|rest| rest.split_once('n'))
        .is_some_and(|(controller, namespace)| digits(controller) && digits(namespace))
}

fn identify_media(bytes: &[u8]) -> bool {
    let mut expected = [b' '; ISO_VOLUME_ID.end - ISO_VOLUME_ID.start];
    let label = protocol::MEDIA_VOLUME_ID.as_bytes();
    if let Some(prefix) = expected.get_mut(..label.len()) {
        prefix.copy_from_slice(label);
    }
    bytes.get(..7) == Some(b"\x01CD001\x01".as_slice())
        && bytes.get(ISO_VOLUME_ID) == Some(expected.as_slice())
}

/// One opened install medium, pinned for the mount that follows.
pub(crate) struct Medium {
    file: File,
    pub(crate) device: PathBuf,
}

impl Medium {
    pub(crate) fn path(&self) -> PathBuf {
        PathBuf::from(format!(
            "/proc/{}/fd/{}",
            std::process::id(),
            self.file.as_raw_fd()
        ))
    }
}

/// `Ok(None)` for a device that is not install media or holds no medium.
fn probe_media(sys: &Path, path: &Path) -> io::Result<Option<File>> {
    let expected = device_number(&text(&sys.join("dev"))?)?;
    let sectors: u64 = text(&sys.join("size"))?
        .trim_end()
        .parse()
        .map_err(|_| invalid("invalid block capacity"))?;
    if sectors < (ISO_PVD_OFFSET + ISO_SECTOR_BYTES as u64) / 512 {
        return Ok(None);
    }
    let before = fs::symlink_metadata(path)?;
    if !matches_device(&before, expected) {
        return Err(invalid("media path disagrees with sysfs block identity"));
    }
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(OPEN_FLAGS)
        .open(path)?;
    let opened = file.metadata()?;
    if !matches_device(&opened, expected)
        || before.dev() != opened.dev()
        || before.ino() != opened.ino()
    {
        return Err(invalid("media device changed during open"));
    }
    let mut bytes = [0; ISO_SECTOR_BYTES];
    match file.read_exact_at(&mut bytes, ISO_PVD_OFFSET) {
        // An empty optical drive can publish a placeholder capacity.
        Err(error) if error.raw_os_error() == Some(ENOMEDIUM) => return Ok(None),
        other => other?,
    }
    let after = fs::symlink_metadata(path)?;
    if !matches_device(&after, expected)
        || after.dev() != opened.dev()
        || after.ino() != opened.ino()
    {
        return Err(invalid("media device changed during probe"));
    }
    Ok(identify_media(&bytes).then_some(file))
}

enum MediaScan {
    Found(Medium),
    None(Vec<String>),
}

fn select_media(
    entries: impl IntoIterator<Item = (PathBuf, io::Result<Option<File>>)>,
) -> io::Result<MediaScan> {
    let mut found: Option<Medium> = None;
    let mut skipped = Vec::new();
    let mut incomplete = false;
    for (device, probed) in entries {
        match probed {
            Ok(Some(file)) => {
                if let Some(first) = &found {
                    return Err(invalid(format!(
                        "more than one td installation medium: {} and {}; \
                         attach only the one to boot",
                        first.device.display(),
                        device.display()
                    )));
                }
                found = Some(Medium { file, device });
            }
            Ok(None) => {}
            // A node or sysfs value still being published makes the whole
            // scan incomplete, as it does for volume discovery: that device
            // may be a second medium, so nothing is selected until it reads.
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                incomplete = true;
                skipped.push(format!("{}: still appearing ({error})", device.display()));
            }
            // A device that cannot be read is not the medium, and a card
            // reader or a failing disk must not stop a live boot. Its reason
            // is kept in case no medium is found at all.
            Err(error) => skipped.push(format!("{}: {error}", device.display())),
        }
    }
    Ok(match found {
        Some(medium) if !incomplete => MediaScan::Found(medium),
        Some(medium) => {
            skipped.insert(
                0,
                format!(
                    "{} holds a td installation medium, held back until every device reads",
                    medium.device.display()
                ),
            );
            MediaScan::None(skipped)
        }
        None => MediaScan::None(skipped),
    })
}

fn scan_media() -> io::Result<MediaScan> {
    let mut entries = Vec::new();
    for (index, entry) in fs::read_dir("/sys/class/block")?.enumerate() {
        if index >= MAX_DEVICES {
            return Err(invalid("too many block devices for media discovery"));
        }
        let entry = match entry {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(MediaScan::None(vec![format!(
                    "the block device list changed during the scan ({error})"
                )]));
            }
            other => other?,
        };
        let name = entry.file_name();
        let Some(name) = name.to_str().filter(|name| media_device_name(name)) else {
            continue;
        };
        let path = Path::new("/dev").join(name);
        let probed = probe_media(&entry.path(), &path);
        entries.push((path, probed));
    }
    select_media(entries)
}

/// The one attached td installation medium, waiting for slow USB enumeration.
pub(crate) fn find_medium() -> io::Result<Medium> {
    let started = Instant::now();
    loop {
        match scan_media()? {
            MediaScan::Found(medium) => return Ok(medium),
            MediaScan::None(skipped) if started.elapsed() >= MEDIA_WAIT => {
                let mut message = format!(
                    "no td installation medium (ISO volume {}) could be selected",
                    protocol::MEDIA_VOLUME_ID
                );
                for reason in skipped {
                    message.push_str("; skipped ");
                    message.push_str(&reason);
                }
                return Err(io::Error::new(io::ErrorKind::NotFound, message));
            }
            MediaScan::None(_) => std::thread::sleep(Duration::from_millis(250)),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[test]
    fn provisioning_and_handoff_require_one_exact_uuid() {
        let text = "12345678-90ab-cdef-1234-567890abcdef";
        let uuid = Uuid::parse(text).unwrap();
        assert_eq!(configured(format!("{text}\n").as_bytes()).unwrap(), uuid);
        for bad in [text.to_owned(), format!("{text}\n\n"), format!("{text} \n")] {
            assert!(configured(bad.as_bytes()).is_err());
        }
        assert_eq!(
            handoff(format!("quiet td.volume={text}\n").as_bytes()).unwrap(),
            uuid
        );
        for bad in [
            "quiet".to_owned(),
            "td.volume=".to_owned(),
            format!("td.volume={text} td.volume={text}"),
            format!("td.volume={} ", text.to_uppercase()),
        ] {
            assert!(handoff(bad.as_bytes()).is_err(), "{bad}");
        }
        let line = command_line(b"quiet", &uuid).unwrap();
        assert_eq!(handoff(line.as_encoded_bytes()).unwrap(), uuid);
        for bad in [
            b"td.volume=".as_slice(),
            b"quiet td.volume=bad",
            line.as_encoded_bytes(),
        ] {
            assert!(command_line(bad, &uuid).is_err());
        }
    }

    #[test]
    fn on_volume_accepts_only_operations_without_a_device_operand() {
        let parse =
            |values: &[&str]| crate::parse_args(values.iter().map(std::ffi::OsString::from));
        for good in [
            vec!["on-volume", "boot", "/volume", "quiet"],
            vec!["on-volume", "mount-root", "/volume"],
            vec!["on-volume", "mount-var", "/sysroot/var"],
            vec!["on-volume", "install", "/update", "/source", "/key"],
            vec!["on-volume", "rollback", "/update"],
        ] {
            assert!(matches!(
                parse(&good).unwrap(),
                crate::Mode::OnVolume { .. }
            ));
        }
        for bad in [
            vec!["on-volume", "volume"],
            vec!["on-volume", "on-volume", "boot", "/volume", "quiet"],
            vec!["on-volume", "boot", "/dev/vda", "/volume", "quiet"],
            vec!["on-volume", "mount-var", "/var", "extra"],
            vec!["on-volume", "success", "/update", "invalid-id"],
        ] {
            assert!(parse(&bad).is_err());
        }
    }

    #[test]
    fn held_descriptor_keeps_the_original_node_when_a_name_is_replaced() {
        let directory = std::env::temp_dir().join(format!("td-volume-pin-{}", std::process::id()));
        fs::create_dir(&directory).unwrap();
        let path = directory.join("node");
        fs::write(&path, b"original").unwrap();
        let pinned = Pinned {
            file: File::open(&path).unwrap(),
            device: path.clone(),
        };
        fs::rename(&path, directory.join("old")).unwrap();
        fs::write(&path, b"replacement").unwrap();
        assert_eq!(fs::read(pinned.path()).unwrap(), b"original");
        assert_eq!(fs::read(&path).unwrap(), b"replacement");
        fs::remove_dir_all(&directory).unwrap();
    }

    fn checksum(bytes: &mut [u8]) {
        let sum = crc32c(&bytes[32..]);
        bytes[..4].copy_from_slice(&sum.to_le_bytes());
    }

    fn superblock() -> Vec<u8> {
        let mut bytes = vec![0; SUPER_BYTES];
        bytes[32..48].copy_from_slice(&[0x12; 16]);
        bytes[48..56].copy_from_slice(&SUPER_OFFSET.to_le_bytes());
        bytes[56..64].copy_from_slice(&1u64.to_le_bytes());
        bytes[64..72].copy_from_slice(b"_BHRfS_M");
        bytes[136..144].copy_from_slice(&1u64.to_le_bytes());
        bytes[299..299 + protocol::VOLUME_LABEL.len()]
            .copy_from_slice(protocol::VOLUME_LABEL.as_bytes());
        checksum(&mut bytes);
        bytes
    }

    #[test]
    fn uuid_grammar_is_canonical_and_nonzero() {
        let text = "12345678-90ab-cdef-1234-567890abcdef";
        assert_eq!(Uuid::parse(text).unwrap().to_string(), text);
        for bad in [
            "",
            "1234567890ab-cdef-1234-567890abcdef",
            "12345678-90AB-cdef-1234-567890abcdef",
            "00000000-0000-0000-0000-000000000000",
            "12345678-90ab-cdef-1234-567890abcdef\n",
        ] {
            assert!(Uuid::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn crc32c_uses_the_castagnoli_check_value() {
        assert_eq!(crc32c(b"123456789"), 0xe3069283);
        assert_eq!(crc32c(b""), 0);
    }

    #[test]
    fn probe_reads_primary_identity_and_refuses_corruption() {
        let bytes = superblock();
        assert_eq!(identify(&bytes).unwrap(), Some(Uuid([0x12; 16])));
        let mut unwritten = bytes.clone();
        unwritten[56..64].fill(0);
        checksum(&mut unwritten);
        assert_eq!(identify(&unwritten).unwrap(), Some(Uuid([0x12; 16])));
        for offset in [0, 32, 1000, 4095] {
            let mut bad = bytes.clone();
            bad[offset] ^= 1;
            assert!(identify(&bad).is_err());
        }
        for length in [0, 72, 299, 4095] {
            assert!(identify(&bytes[..length]).is_err());
        }
        let mut foreign = bytes.clone();
        foreign[299] = b'x';
        assert_eq!(identify(&foreign).unwrap(), None);
        assert_eq!(identify(&vec![0; SUPER_BYTES]).unwrap(), None);
    }

    #[test]
    fn valid_checksum_does_not_admit_unsupported_filesystems() {
        for (offset, value) in [(48, 0), (136, 2), (196, 1), (56, 4)] {
            let mut bad = superblock();
            bad[offset] = value;
            if offset == 48 {
                bad[50] = 0;
            }
            checksum(&mut bad);
            assert!(identify(&bad).is_err(), "offset {offset}");
        }
        let mut bad = superblock();
        bad[56..64].copy_from_slice(&(1u64 << 32).to_le_bytes());
        checksum(&mut bad);
        assert!(identify(&bad).is_err());
        let mut bad = superblock();
        bad[32..48].fill(0);
        checksum(&mut bad);
        assert!(identify(&bad).is_err());
    }

    #[test]
    fn names_admit_direct_disks_and_partitions_without_paths() {
        for name in [
            "vda",
            "vdb2",
            "sdaa",
            "sda2",
            "nvme0n1",
            "nvme12n34p5",
            "ram0",
            "ram15",
        ] {
            assert!(supported_name(name), "{name}");
        }
        for name in [
            "",
            "ram",
            "ram0p1",
            "zram0",
            "vd",
            "sda/../../x",
            "sda2junk",
            "sr0",
            "loop0",
            "dm-0",
            "md0",
            "nvmen1",
            "nvme0n",
            "nvme0n1p",
            "nvme0n1p2x",
        ] {
            assert!(!supported_name(name), "{name}");
        }
    }

    #[test]
    fn selection_is_order_independent_and_refuses_clones() {
        let uuid = Uuid([0x12; 16]);
        let other = Uuid([0x34; 16]);
        for order in [false, true] {
            let mut found = None;
            let mut inputs = [(other.clone(), "/dev/vda"), (uuid.clone(), "/dev/vdb2")];
            if order {
                inputs.reverse();
            }
            for (id, path) in inputs {
                choose(&mut found, btrfs(id, path), Some(&uuid)).unwrap();
            }
            assert_eq!(found, Some(btrfs(uuid.clone(), "/dev/vdb2")));
            assert!(choose(&mut found, btrfs(uuid.clone(), "/dev/sda2"), Some(&uuid)).is_err());
        }
        let mut found = None;
        choose(&mut found, btrfs(uuid, "/dev/vda2"), None).unwrap();
        assert!(choose(&mut found, btrfs(other, "/dev/sda2"), None).is_err());
    }

    fn btrfs(uuid: Uuid, path: &str) -> Found {
        Found {
            uuid,
            kind: Kind::Btrfs,
            path: path.into(),
        }
    }

    fn encrypted(uuid: Uuid, path: &str) -> Found {
        Found {
            uuid,
            kind: Kind::Luks2,
            path: path.into(),
        }
    }

    #[test]
    fn luks2_and_btrfs_candidates_are_counted_together() {
        let uuid = Uuid([0x12; 16]);
        let other = Uuid([0x34; 16]);
        for order in [false, true] {
            // A LUKS2 partition and a Btrfs one carrying its UUID are two.
            let mut inputs = [
                btrfs(uuid.clone(), "/dev/vda2"),
                encrypted(uuid.clone(), "/dev/vdb2"),
            ];
            if order {
                inputs.reverse();
            }
            let mut found = None;
            let [first, second] = inputs;
            choose(&mut found, first, Some(&uuid)).unwrap();
            let error = choose(&mut found, second, Some(&uuid)).unwrap_err();
            assert!(error.to_string().contains("ambiguous"), "{error}");
            // Two LUKS2 headers carrying the UUID are two as well.
            let mut found = None;
            choose(&mut found, encrypted(uuid.clone(), "/dev/vda2"), None).unwrap();
            assert!(choose(&mut found, encrypted(uuid.clone(), "/dev/sda2"), None).is_err());
            // Without an expected UUID, any two td volumes are ambiguous.
            let mut found = None;
            choose(&mut found, encrypted(uuid.clone(), "/dev/vda2"), None).unwrap();
            assert!(choose(&mut found, btrfs(other.clone(), "/dev/sda2"), None).is_err());
        }
        // Another UUID's volume is not counted against the expected one.
        let mut found = None;
        choose(&mut found, btrfs(other, "/dev/vda2"), Some(&uuid)).unwrap();
        choose(
            &mut found,
            encrypted(uuid.clone(), "/dev/vdb2"),
            Some(&uuid),
        )
        .unwrap();
        assert_eq!(found, Some(encrypted(uuid, "/dev/vdb2")));
    }

    #[test]
    fn sysfs_numbers_and_nonblock_nodes_cannot_fake_device_identity() {
        assert_eq!(device_number("259:123\n").unwrap(), (259, 123));
        for bad in ["1", "1:2:3", "+1:2", "1:-2", "1:2\n\n", " :2"] {
            assert!(device_number(bad).is_err());
        }
        assert!(!matches_device(&fs::metadata("/dev/null").unwrap(), (1, 3)));
    }

    #[test]
    fn cli_admits_only_an_optional_canonical_uuid() {
        use std::ffi::OsString;
        let args = |values: &[&str]| {
            values
                .iter()
                .map(OsString::from)
                .collect::<Vec<_>>()
                .into_iter()
        };
        assert!(matches!(
            crate::parse_args(args(&["volume"])).unwrap(),
            crate::Mode::Volume { uuid: None }
        ));
        assert!(matches!(
            crate::parse_args(args(&["volume", "12345678-90ab-cdef-1234-567890abcdef"])).unwrap(),
            crate::Mode::Volume { uuid: Some(_) }
        ));
        assert!(crate::parse_args(args(&["volume", "/dev/vda"])).is_err());
        assert!(crate::parse_args(args(&[
            "volume",
            "12345678-90ab-cdef-1234-567890abcdef",
            "extra"
        ]))
        .is_err());
    }
    #[test]
    fn random_uuids_carry_version_four_and_the_rfc_variant() {
        for seed in [[0u8; 16], [0xff; 16]] {
            let text = Uuid::from_random(seed).to_string();
            assert_eq!(text.as_bytes()[14], b'4', "{text}");
            assert!(matches!(text.as_bytes()[19], b'8'..=b'b'), "{text}");
            assert_eq!(Uuid::parse(&text).unwrap().to_string(), text);
        }
        assert_ne!(Uuid::random().unwrap(), Uuid::random().unwrap());
    }

    #[test]
    fn media_names_are_whole_optical_and_disk_devices() {
        for name in ["sr0", "sr12", "sda", "sdab", "vda", "nvme0n1", "nvme3n12"] {
            assert!(media_device_name(name), "{name}");
        }
        for name in [
            "",
            "sr",
            "sra",
            "sda1",
            "vda2",
            "nvme0n1p1",
            "nvme0n",
            "nvmen1",
            "ram0",
            "loop0",
            "dm-0",
            "mmcblk0",
        ] {
            assert!(!media_device_name(name), "{name}");
        }
    }

    fn descriptor(label: &[u8]) -> Vec<u8> {
        let mut bytes = vec![0; ISO_SECTOR_BYTES];
        bytes[..7].copy_from_slice(b"\x01CD001\x01");
        bytes[40..72].fill(b' ');
        bytes[40..40 + label.len()].copy_from_slice(label);
        bytes
    }

    #[test]
    fn only_a_primary_descriptor_with_the_exact_label_is_media() {
        assert!(identify_media(&descriptor(b"TD_INSTALL")));
        for label in [b"TD_INSTAL".as_slice(), b"TD_INSTALLX", b"td_install", b""] {
            assert!(!identify_media(&descriptor(label)), "{label:?}");
        }
        let mut unpadded = descriptor(b"TD_INSTALL");
        unpadded[50] = 0;
        assert!(!identify_media(&unpadded));
        for (offset, value) in [(0, 2), (1, b'X'), (6, 2)] {
            let mut bad = descriptor(b"TD_INSTALL");
            bad[offset] = value;
            assert!(!identify_media(&bad), "offset {offset}");
        }
        assert!(!identify_media(&descriptor(b"TD_INSTALL")[..71]));
    }

    #[test]
    fn media_selection_requires_one_medium_and_skips_unreadable_devices() {
        let file = || Ok(Some(File::open("/dev/null").unwrap()));
        let failed = || Err(io::Error::from(io::ErrorKind::PermissionDenied));
        let scan = |entries: Vec<(&str, io::Result<Option<File>>)>| {
            select_media(
                entries
                    .into_iter()
                    .map(|(name, probed)| (name.into(), probed)),
            )
        };
        let found = scan(vec![
            ("/dev/sr1", Ok(None)),
            ("/dev/sr0", file()),
            ("/dev/sdb", failed()),
        ]);
        assert!(matches!(
            found,
            Ok(MediaScan::Found(ref medium)) if medium.device == Path::new("/dev/sr0")
        ));
        let none = scan(vec![("/dev/sdb", failed()), ("/dev/sr1", Ok(None))]);
        assert!(matches!(none, Ok(MediaScan::None(ref skipped)) if skipped.len() == 1));
        let error = scan(vec![("/dev/sr0", file()), ("/dev/sda", file())])
            .err()
            .unwrap();
        assert!(error.to_string().contains("more than one"), "{error}");
        // A device still appearing may be a second medium: select nothing yet.
        let appearing = || Err(io::Error::from(io::ErrorKind::NotFound));
        for order in [false, true] {
            let mut entries = vec![("/dev/sr0", file()), ("/dev/sdb", appearing())];
            if order {
                entries.reverse();
            }
            let Ok(MediaScan::None(skipped)) = scan(entries) else {
                panic!("order {order}: a medium was selected from an incomplete scan");
            };
            assert_eq!(skipped.len(), 2, "order {order}");
            assert!(skipped[0].starts_with("/dev/sr0 holds"), "{skipped:?}");
        }
    }

    #[test]
    fn an_incomplete_scan_cannot_select_a_partial_match() {
        let good = || Ok(Some(btrfs(Uuid([0x12; 16]), "/dev/vda2")));
        let missing = || Err(io::Error::from(io::ErrorKind::NotFound));
        assert_eq!(select_scan([good(), missing()], None).unwrap(), None);
        assert_eq!(select_scan([missing(), good()], None).unwrap(), None);
        assert!(select_scan(
            [
                good(),
                Err(io::Error::from(io::ErrorKind::PermissionDenied))
            ],
            None
        )
        .is_err());
        assert!(select_scan([good(), good()], None).is_err());
        assert_eq!(
            select_scan([good(), Ok(None)], None).unwrap(),
            Some(btrfs(Uuid([0x12; 16]), "/dev/vda2"))
        );
    }

    const LUKS_UUID: &str = "0f0e0d0c-0b0a-4908-8706-050403020100";
    const OTHER_UUID: &str = "00112233-4455-4677-8899-aabbccddeeff";
    const VOLUME_BYTES: usize = 128 * 1024;

    fn seal_luks2(area: &mut [u8]) {
        area[448..512].fill(0);
        let digest = td_tpm::digest(area);
        area[448..480].copy_from_slice(&digest);
    }

    /// One 16 KiB LUKS2 header copy as cryptsetup writes it, with `json`.
    fn luks2_copy_with(
        secondary: bool,
        version: u16,
        label: &[u8],
        uuid: &str,
        json: &[u8],
    ) -> Vec<u8> {
        let mut area = vec![0u8; 0x4000];
        area[..6].copy_from_slice(if secondary {
            b"SKUL\xba\xbe"
        } else {
            b"LUKS\xba\xbe"
        });
        area[6..8].copy_from_slice(&version.to_be_bytes());
        area[8..16].copy_from_slice(&0x4000u64.to_be_bytes());
        area[16..24].copy_from_slice(&3u64.to_be_bytes());
        area[24..24 + label.len()].copy_from_slice(label);
        area[72..78].copy_from_slice(b"sha256");
        area[168..168 + uuid.len()].copy_from_slice(uuid.as_bytes());
        let offset: u64 = if secondary { 0x4000 } else { 0 };
        area[256..264].copy_from_slice(&offset.to_be_bytes());
        area[4096..4096 + json.len()].copy_from_slice(json);
        seal_luks2(&mut area);
        area
    }

    const PLAIN_JSON: &[u8] = br#"{"keyslots":{"0":{"type":"luks2"}},"tokens":{},"segments":{"0":{"type":"crypt"}},"digests":{},"config":{}}"#;
    // A td token naming a keyslot the header lacks: td-protector's full
    // reader refuses it; identity never reads it.
    const REFUSED_TOKEN_JSON: &[u8] = br#"{"keyslots":{"0":{"type":"luks2"}},"tokens":{"0":{"type":"td-protector","keyslots":["7"],"role":"first-boot","public":"00","private":"00"}},"segments":{},"digests":{},"config":{}}"#;

    fn luks2_copy(secondary: bool, version: u16, label: &[u8], uuid: &str) -> Vec<u8> {
        luks2_copy_with(secondary, version, label, uuid, PLAIN_JSON)
    }

    /// A volume with both LUKS2 copies of one version, label and UUID.
    fn luks2_volume_with(version: u16, label: &[u8], uuid: &str, json: &[u8]) -> Vec<u8> {
        let mut bytes = luks2_copy_with(false, version, label, uuid, json);
        bytes.extend(luks2_copy_with(true, version, label, uuid, json));
        bytes.resize(VOLUME_BYTES, 0xaa);
        bytes
    }

    fn luks2_volume(version: u16, label: &[u8], uuid: &str) -> Vec<u8> {
        luks2_volume_with(version, label, uuid, PLAIN_JSON)
    }

    fn btrfs_volume() -> Vec<u8> {
        let mut bytes = vec![0; VOLUME_BYTES];
        bytes[SUPER_OFFSET as usize..][..SUPER_BYTES].copy_from_slice(&superblock());
        bytes
    }

    fn identify_expecting(
        bytes: Vec<u8>,
        expected: Option<&str>,
    ) -> (io::Result<Option<(Uuid, Kind)>>, Vec<String>, usize) {
        let size = bytes.len() as u64;
        let expected = expected.map(|text| Uuid::parse(text).unwrap());
        let mut notes = Vec::new();
        let mut calls = 0;
        let result = identify_volume(
            &mut io::Cursor::new(bytes),
            size,
            expected.as_ref(),
            &mut notes,
            |device| {
                calls += 1;
                luks2::identity(device)
            },
        );
        (result, notes, calls)
    }

    fn identify_bytes(bytes: Vec<u8>) -> io::Result<Option<(Uuid, Kind)>> {
        identify_expecting(bytes, None).0
    }

    #[test]
    fn a_td_luks2_header_identifies_its_volume() {
        let uuid = Uuid::parse(LUKS_UUID).unwrap();
        let volume = luks2_volume(2, b"td-system", LUKS_UUID);
        assert_eq!(
            identify_bytes(volume.clone()).unwrap(),
            Some((uuid.clone(), Kind::Luks2))
        );
        let (found, notes, calls) = identify_expecting(volume.clone(), Some(LUKS_UUID));
        assert_eq!(found.unwrap(), Some((uuid.clone(), Kind::Luks2)));
        assert_eq!((notes.len(), calls), (0, 1));
        // A primary that fails its checksum: identity uses the secondary,
        // as cryptsetup does.
        let mut damaged = volume.clone();
        damaged[1000] ^= 1;
        assert_eq!(
            identify_bytes(damaged).unwrap(),
            Some((uuid.clone(), Kind::Luks2))
        );
        // The primary's magic gone: the secondary still selects the device.
        let mut wiped = volume.clone();
        wiped[..4096].fill(0);
        assert_eq!(
            identify_bytes(wiped).unwrap(),
            Some((uuid.clone(), Kind::Luks2))
        );
        // Tokens are the opened volume's concern, not discovery's.
        let tokened = luks2_volume_with(2, b"td-system", LUKS_UUID, REFUSED_TOKEN_JSON);
        assert!(luks2::read(&mut io::Cursor::new(tokened.clone())).is_err());
        assert_eq!(
            identify_expecting(tokened, Some(LUKS_UUID)).0.unwrap(),
            Some((uuid, Kind::Luks2))
        );
        assert_eq!(
            identify_bytes(btrfs_volume()).unwrap(),
            Some((Uuid([0x12; 16]), Kind::Btrfs))
        );
        assert_eq!(identify_bytes(vec![0; VOLUME_BYTES]).unwrap(), None);
    }

    #[test]
    fn other_luks_versions_and_labels_are_not_td_volumes() {
        // LUKS1 and any other version carry the magic without being ours.
        for version in [1, 3] {
            let volume = luks2_volume(version, b"td-system", LUKS_UUID);
            assert_eq!(identify_bytes(volume).unwrap(), None, "version {version}");
        }
        for label in [b"".as_slice(), b"td-system2", b"TD-SYSTEM", b"other"] {
            let volume = luks2_volume(2, label, LUKS_UUID);
            let (found, _, calls) = identify_expecting(volume, None);
            assert_eq!((found.unwrap(), calls), (None, 0), "{label:?}");
        }
        // A secondary of a later sequence number in use with another label.
        let mut relabelled = luks2_volume(2, b"td-system", LUKS_UUID);
        let mut secondary = luks2_copy(true, 2, b"other", LUKS_UUID);
        secondary[16..24].copy_from_slice(&4u64.to_be_bytes());
        seal_luks2(&mut secondary);
        relabelled[0x4000..0x8000].copy_from_slice(&secondary);
        let (found, notes, _) = identify_expecting(relabelled, None);
        assert_eq!(found.unwrap(), None);
        assert!(notes[0].contains("another label"), "{notes:?}");
    }

    #[test]
    fn a_wanted_td_header_that_fails_identity_refuses_discovery() {
        // Both copies corrupt: a td label with no valid copy.
        let mut corrupt = luks2_volume(2, b"td-system", LUKS_UUID);
        corrupt[1000] ^= 1;
        corrupt[0x4000 + 1000] ^= 1;
        for expected in [None, Some(LUKS_UUID)] {
            let error = identify_expecting(corrupt.clone(), expected).0.unwrap_err();
            assert!(error.to_string().contains("td LUKS2 header"), "{error}");
        }
        for uuid in [
            "0F0E0D0C-0B0A-4908-8706-050403020100",
            "00000000-0000-0000-0000-000000000000",
            "0f0e0d0c0b0a49088706050403020100",
        ] {
            let error = identify_bytes(luks2_volume(2, b"td-system", uuid)).unwrap_err();
            assert!(error.to_string().contains("UUID"), "{uuid}: {error}");
        }
    }

    /// Another disk's td header, however malformed, is passed over by its
    /// unverified UUID claim before identity runs: the counter is the
    /// mutation check that the pre-filter is there.
    #[test]
    fn another_volume_td_header_is_passed_over_unverified() {
        let mut corrupt = luks2_volume_with(2, b"td-system", OTHER_UUID, REFUSED_TOKEN_JSON);
        corrupt[1000] ^= 1;
        corrupt[0x4000 + 1000] ^= 1;
        for foreign in [
            corrupt,
            luks2_volume_with(2, b"td-system", OTHER_UUID, REFUSED_TOKEN_JSON),
        ] {
            let (found, notes, calls) = identify_expecting(foreign, Some(LUKS_UUID));
            assert_eq!(found.unwrap(), None);
            assert_eq!(calls, 0, "identity ran on another volume's header");
            assert!(notes[0].contains(OTHER_UUID), "{notes:?}");
        }
        // The same header, sought, refuses.
        let mut corrupt = luks2_volume(2, b"td-system", OTHER_UUID);
        corrupt[1000] ^= 1;
        corrupt[0x4000 + 1000] ^= 1;
        let (found, _, calls) = identify_expecting(corrupt, Some(OTHER_UUID));
        assert!(found.is_err());
        assert_eq!(calls, 1);
        // A claim in a stale copy whose verified copy names another volume
        // is passed over after identity.
        let mut stale = luks2_volume(2, b"td-system", OTHER_UUID);
        stale[..0x4000].copy_from_slice(&luks2_copy(false, 2, b"td-system", LUKS_UUID));
        stale[1000] ^= 1;
        let (found, notes, calls) = identify_expecting(stale, Some(LUKS_UUID));
        assert_eq!((found.unwrap(), calls), (None, 1));
        assert!(notes[0].contains("not the volume sought"), "{notes:?}");
    }

    #[test]
    fn a_foreign_header_beside_this_volume_lets_the_btrfs_volume_bind() {
        let expected = Uuid([0x12; 16]);
        let foreign = luks2_volume_with(2, b"td-system", OTHER_UUID, REFUSED_TOKEN_JSON);
        let size = foreign.len() as u64;
        let mut notes = Vec::new();
        let devices = [(foreign, "/dev/vdb2"), (btrfs_volume(), "/dev/vda2")];
        let entries = devices.into_iter().map(|(bytes, path)| {
            let identity = identify_volume(
                &mut io::Cursor::new(bytes),
                size,
                Some(&expected),
                &mut notes,
                luks2::identity,
            )?;
            Ok(identity.map(|(uuid, kind)| Found {
                uuid,
                kind,
                path: path.into(),
            }))
        });
        assert_eq!(
            select_scan(entries, Some(&expected)).unwrap(),
            Some(btrfs(expected, "/dev/vda2"))
        );
        assert_eq!(notes.len(), 1, "{notes:?}");
    }

    /// A device whose reads fail at chosen offsets.
    struct Failing {
        bytes: io::Cursor<Vec<u8>>,
        bad: Vec<u64>,
    }

    impl Read for Failing {
        fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
            if self.bad.contains(&self.bytes.position()) {
                return Err(io::Error::other("medium error"));
            }
            self.bytes.read(out)
        }
    }

    impl Seek for Failing {
        fn seek(&mut self, to: SeekFrom) -> io::Result<u64> {
            self.bytes.seek(to)
        }
    }

    #[test]
    fn a_failed_header_probe_is_not_a_luks2_candidate() {
        let identify_failing = |bytes: Vec<u8>, bad: Vec<u64>| {
            let size = bytes.len() as u64;
            let mut notes = Vec::new();
            let mut device = Failing {
                bytes: io::Cursor::new(bytes),
                bad,
            };
            let found = identify_volume(&mut device, size, None, &mut notes, luks2::identity);
            (found, notes)
        };
        // Bad sectors before the superblock leave the Btrfs volume.
        let (found, notes) = identify_failing(btrfs_volume(), vec![0, 0x4000]);
        assert_eq!(found.unwrap(), Some((Uuid([0x12; 16]), Kind::Btrfs)));
        assert!(notes[0].contains("byte 0"), "{notes:?}");
        // A blank device with a bad first sector is no candidate at all.
        let (found, notes) = identify_failing(vec![0; VOLUME_BYTES], vec![0]);
        assert_eq!(found.unwrap(), None);
        assert_eq!(notes.len(), 1);
        // A short read past a claim is past the pre-check: identity decides.
        let (found, _) = identify_failing(luks2_volume(2, b"td-system", LUKS_UUID), vec![0x8000]);
        assert_eq!(
            found.unwrap(),
            Some((Uuid::parse(LUKS_UUID).unwrap(), Kind::Luks2))
        );
    }

    #[test]
    fn one_device_carrying_both_identities_is_ambiguous() {
        let mut both = luks2_volume(2, b"td-system", LUKS_UUID);
        both[SUPER_OFFSET as usize..][..SUPER_BYTES].copy_from_slice(&superblock());
        let error = identify_bytes(both).unwrap_err();
        assert!(error.to_string().contains("ambiguous"), "{error}");
        // A foreign LUKS2 header beside a td Btrfs is the Btrfs volume.
        let mut foreign = luks2_volume(2, b"other", LUKS_UUID);
        foreign[SUPER_OFFSET as usize..][..SUPER_BYTES].copy_from_slice(&superblock());
        assert_eq!(
            identify_bytes(foreign).unwrap(),
            Some((Uuid([0x12; 16]), Kind::Btrfs))
        );
    }

    #[test]
    fn notes_are_reported_whole_and_once() {
        let mut out = Vec::new();
        let mut reported = Vec::new();
        let note = || vec!["/dev/vdb2: passed over".to_string()];
        report_notes(&mut out, &mut reported, note()).unwrap();
        report_notes(&mut out, &mut reported, note()).unwrap();
        assert_eq!(out, b"td-boot: /dev/vdb2: passed over\n");
        let many = (0..MAX_NOTES + 5).map(|n| n.to_string()).collect();
        report_notes(&mut Vec::new(), &mut reported, many).unwrap();
        assert_eq!(reported.len(), MAX_NOTES);
    }

    #[test]
    fn mappings_are_dm_nodes_and_never_scan_candidates() {
        for name in ["dm-0", "dm-12"] {
            assert!(mapping_name(name), "{name}");
            assert!(!supported_name(name), "{name}");
        }
        for name in ["dm-", "dm", "dm-0p1", "dm-x", "dm0", "vda2", "mapper"] {
            assert!(!mapping_name(name), "{name}");
        }
    }

    /// A private sysfs `block` directory.
    struct FakeSysfs(PathBuf);

    impl FakeSysfs {
        fn new(tag: &str) -> Self {
            let root =
                std::env::temp_dir().join(format!("td-boot-sysfs-{tag}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&root);
            fs::create_dir_all(&root).unwrap();
            // A disk is not a mapping.
            fs::create_dir_all(root.join("vda/vda2")).unwrap();
            Self(root)
        }

        fn mapping(&self, node: &str, uuid: &str, name: &str, slaves: &[&str]) {
            let dir = self.0.join(node);
            fs::create_dir_all(dir.join("dm")).unwrap();
            fs::create_dir_all(dir.join("slaves")).unwrap();
            fs::write(dir.join("dm/uuid"), format!("{uuid}\n")).unwrap();
            fs::write(dir.join("dm/name"), format!("{name}\n")).unwrap();
            for slave in slaves {
                fs::write(dir.join("slaves").join(slave), b"").unwrap();
            }
        }

        fn find(&self) -> io::Result<MappingScan> {
            find_mapping(&self.0, "vda2", &Uuid::parse(LUKS_UUID).unwrap())
        }
    }

    impl Drop for FakeSysfs {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    const DM_UUID: &str = "CRYPT-LUKS2-0f0e0d0c0b0a49088706050403020100";

    fn entry(node: &str, name: &str) -> MappingScan {
        MappingScan::Complete(Some(MappingEntry {
            node: node.into(),
            name: name.into(),
        }))
    }

    #[test]
    fn the_mapping_is_found_by_dm_uuid_and_its_one_slave() {
        let sysfs = FakeSysfs::new("match");
        assert_eq!(sysfs.find().unwrap(), MappingScan::Complete(None));
        sysfs.mapping("dm-0", "LVM-abc", "vg-root", &["vda2"]);
        sysfs.mapping(
            "dm-1",
            "CRYPT-LUKS2-00112233445566778899aabbccddeeff-other",
            "other",
            &["vdb2"],
        );
        sysfs.mapping("dm-2", "", "plain", &["vda2"]);
        assert_eq!(sysfs.find().unwrap(), MappingScan::Complete(None));
        sysfs.mapping("dm-3", &format!("{DM_UUID}-td-root"), "td-root", &["vda2"]);
        assert_eq!(sysfs.find().unwrap(), entry("dm-3", "td-root"));
        assert_eq!(
            mapping_uuid_prefix(&Uuid::parse(LUKS_UUID).unwrap()),
            format!("{DM_UUID}-")
        );
    }

    #[test]
    fn a_dm_uuid_of_the_kernel_maximum_is_read() {
        // 128 characters and the newline: an unrelated one is skipped, and
        // the volume's own with a long name is admitted.
        let sysfs = FakeSysfs::new("long");
        let unrelated = format!("LVM-{}", "a".repeat(124));
        assert_eq!(unrelated.len(), 128);
        sysfs.mapping("dm-0", &unrelated, "vg-long", &["vdb"]);
        assert_eq!(sysfs.find().unwrap(), MappingScan::Complete(None));
        let name = "n".repeat(128 - DM_UUID.len() - 1);
        sysfs.mapping("dm-1", &format!("{DM_UUID}-{name}"), &name, &["vda2"]);
        assert_eq!(sysfs.find().unwrap(), entry("dm-1", &name));
        // One byte more is no dm uuid the kernel writes.
        let sysfs = FakeSysfs::new("over");
        sysfs.mapping("dm-0", &format!("{unrelated}b"), "vg-long", &["vdb"]);
        assert!(sysfs.find().is_err());
    }

    #[test]
    fn a_vanishing_mapping_makes_the_walk_incomplete() {
        let sysfs = FakeSysfs::new("vanish");
        // A dm-N whose attributes are gone, as one removed mid-walk.
        fs::create_dir_all(sysfs.0.join("dm-4")).unwrap();
        assert_eq!(sysfs.find().unwrap(), MappingScan::Incomplete);
    }

    #[test]
    fn a_mapping_over_another_device_refuses() {
        for slaves in [&["vdb2"][..], &[], &["vda2", "vdb2"], &["vda"]] {
            let sysfs = FakeSysfs::new("slave");
            sysfs.mapping("dm-0", &format!("{DM_UUID}-td-root"), "td-root", slaves);
            let error = sysfs.find().unwrap_err();
            assert!(
                error.to_string().contains("not over exactly vda2"),
                "{slaves:?}: {error}"
            );
        }
    }

    #[test]
    fn two_mappings_of_one_volume_refuse() {
        let sysfs = FakeSysfs::new("two");
        sysfs.mapping("dm-0", &format!("{DM_UUID}-td-root"), "td-root", &["vda2"]);
        sysfs.mapping(
            "dm-1",
            &format!("{DM_UUID}-td-again"),
            "td-again",
            &["vda2"],
        );
        let error = sysfs.find().unwrap_err();
        assert!(error.to_string().contains("two active mappings"), "{error}");
    }

    #[test]
    fn a_mapping_whose_name_disagrees_with_its_dm_uuid_refuses() {
        let sysfs = FakeSysfs::new("name");
        sysfs.mapping("dm-0", &format!("{DM_UUID}-td-root"), "td-other", &["vda2"]);
        let error = sysfs.find().unwrap_err();
        assert!(error.to_string().contains("is named"), "{error}");
    }

    #[test]
    fn every_consumer_refuses_an_encrypted_volume() {
        let found = encrypted(Uuid::parse(LUKS_UUID).unwrap(), "/dev/vda2");
        // `td-boot volume` asks mapping discovery for the partition.
        let mut asked = None;
        let error = found
            .describe_with(|partition, uuid| {
                asked = Some((partition.to_owned(), uuid.clone()));
                Ok(None)
            })
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Unsupported);
        assert!(
            error.to_string().starts_with(ENCRYPTED_UNSUPPORTED),
            "{error}"
        );
        assert!(error.to_string().contains("no mapping"), "{error}");
        assert_eq!(
            asked,
            Some(("vda2".into(), Uuid::parse(LUKS_UUID).unwrap()))
        );
        let error = found
            .describe_with(|_, _| {
                Ok(Some(Mapping::for_test(
                    "dm-0",
                    "td-root",
                    File::open("/dev/null").unwrap(),
                )))
            })
            .unwrap_err();
        assert!(
            error.to_string().contains("dm-0 (td-root, 1:3) is active"),
            "{error}"
        );
        let error = found
            .describe_with(|_, _| Err(invalid("two active mappings")))
            .unwrap_err();
        assert!(error.to_string().contains("two active mappings"), "{error}");
        assert_eq!(
            btrfs(Uuid::parse(LUKS_UUID).unwrap(), "/dev/vda2")
                .describe_with(|_, _| panic!("a Btrfs volume has no mapping"))
                .unwrap(),
            format!("{LUKS_UUID} /dev/vda2")
        );
        for mapping in [
            None,
            Some(Mapping::for_test(
                "dm-0",
                "td-root",
                File::open("/dev/null").unwrap(),
            )),
        ] {
            let error = encrypted_unsupported(Path::new("/dev/vda2"), mapping.as_ref());
            assert_eq!(error.kind(), io::ErrorKind::Unsupported);
            assert!(
                error.to_string().starts_with(ENCRYPTED_UNSUPPORTED),
                "{error}"
            );
        }
    }

    fn mode_device(mode: &crate::Mode) -> Option<PathBuf> {
        match mode {
            crate::Mode::Boot { device, .. }
            | crate::Mode::Install { device, .. }
            | crate::Mode::Update { device, .. }
            | crate::Mode::Rollback { device, .. }
            | crate::Mode::Success { device, .. }
            | crate::Mode::MountVolume { device, .. } => Some(device.clone()),
            _ => None,
        }
    }

    fn on_volume_operation(words: &[&str]) -> crate::Mode {
        let args = std::iter::once("on-volume")
            .chain(words.iter().copied())
            .map(std::ffi::OsString::from);
        let crate::Mode::OnVolume { operation } = crate::parse_args(args).unwrap() else {
            panic!("{words:?} is not an on-volume operation");
        };
        *operation
    }

    fn luks2(mapping: Option<&str>) -> Opened {
        Opened::Luks2 {
            partition: Pinned::for_test(File::open("/dev/null").unwrap(), "/dev/vda2".into()),
            mapping: mapping
                .map(|name| Mapping::for_test("dm-0", name, File::open("/dev/null").unwrap())),
        }
    }

    fn btrfs_opened() -> Opened {
        Opened::Btrfs(Pinned::for_test(
            File::open("/dev/null").unwrap(),
            "/dev/vda2".into(),
        ))
    }

    fn no_unlock(_: &Pinned, _: crate::unlock::VolumeKey) -> io::Result<crate::unlock::Unlocked> {
        panic!("unlock reached")
    }

    fn handed_key() -> crate::unlock::VolumeKey {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let directory =
            std::env::temp_dir().join(format!("td-boot-bind-key-{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&directory);
        fs::create_dir(&directory).unwrap();
        let member = directory.join(protocol::VOLUME_KEY_MEMBER);
        fs::write(&member, [7; protocol::VOLUME_KEY_BYTES]).unwrap();
        fs::set_permissions(&member, std::os::unix::fs::PermissionsExt::from_mode(0o400)).unwrap();
        let uid = fs::metadata(&member).unwrap().uid();
        let key = crate::unlock::take_key(&directory, uid).unwrap().unwrap();
        fs::remove_dir_all(&directory).unwrap();
        key
    }

    fn bound(binding: io::Result<crate::Binding>) -> Pinned {
        match binding.unwrap() {
            crate::Binding::Bound(pinned) => pinned,
            crate::Binding::Halt(reason) => panic!("halted: {reason}"),
        }
    }

    /// The selector's `boot` and the running system's transactions bind
    /// their device through `bind_volume`: an encrypted volume refuses each,
    /// with or without a mapping, and a Btrfs one binds each to its held
    /// descriptor as before.
    #[test]
    fn each_on_volume_operation_but_the_mounts_refuses_an_encrypted_volume() {
        let id = "a".repeat(64);
        let operations: &[&[&str]] = &[
            &["boot", "/volume", "quiet"],
            &["install", "/update", "/source", "/key"],
            &["update", "/update", "/volume", "/volume/channel", "/key"],
            &["rollback", "/update"],
            &["success", "/update", &id],
        ];
        for words in operations {
            for mapping in [None, Some(crate::protocol::VOLUME_MAPPING_NAME)] {
                let mut operation = on_volume_operation(words);
                let error = crate::bind_volume(&mut operation, luks2(mapping), None, no_unlock)
                    .err()
                    .unwrap();
                assert!(
                    error.to_string().starts_with(ENCRYPTED_UNSUPPORTED),
                    "{words:?}: {error}"
                );
                assert_eq!(
                    mode_device(&operation).unwrap(),
                    Path::new("/volume-device")
                );
            }
        }
        for words in operations.iter().copied().chain([
            ["mount-root", "/volume"].as_slice(),
            &["mount-var", "/sysroot/var"],
        ]) {
            let mut operation = on_volume_operation(words);
            let pinned = bound(crate::bind_volume(
                &mut operation,
                btrfs_opened(),
                None,
                no_unlock,
            ));
            assert_eq!(mode_device(&operation).unwrap(), pinned.path(), "{words:?}");
        }
    }

    /// A key on an unencrypted volume and an encrypted volume without one
    /// refuse; so does a mapping already active at mount-root, and none, or
    /// another, at mount-var. None reaches the unlock.
    #[test]
    fn the_deployment_mounts_refuse_a_mismatched_key_or_mapping() {
        let refusals: &[(&[&str], fn() -> Opened, bool, &str)] = &[
            (
                &["mount-root", "/volume"],
                btrfs_opened,
                true,
                "not encrypted",
            ),
            (
                &["mount-root", "/volume"],
                || luks2(None),
                false,
                "no volume key",
            ),
            (
                &["mount-root", "/volume"],
                || luks2(Some("td-system")),
                true,
                "already has the active mapping",
            ),
            (
                &["mount-var", "/sysroot/var"],
                || luks2(None),
                false,
                "no active mapping",
            ),
            (
                &["mount-var", "/sysroot/var"],
                || luks2(Some("td-other")),
                false,
                "td-other",
            ),
        ];
        for (words, opened, with_key, message) in refusals {
            let mut operation = on_volume_operation(words);
            let key = with_key.then(handed_key);
            let error = crate::bind_volume(&mut operation, opened(), key, no_unlock)
                .err()
                .unwrap();
            assert!(error.to_string().contains(message), "{words:?}: {error}");
            assert_eq!(
                mode_device(&operation).unwrap(),
                Path::new("/volume-device")
            );
        }
    }

    /// mount-root hands the key and the partition to the unlock and binds the
    /// mapping it returns; mount-var binds the active td-system mapping; a
    /// halt is returned for the caller to halt on.
    #[test]
    fn the_deployment_mounts_bind_the_mapping() {
        let mut operation = on_volume_operation(&["mount-root", "/volume"]);
        let mut reached = false;
        let pinned = bound(crate::bind_volume(
            &mut operation,
            luks2(None),
            Some(handed_key()),
            |partition, key| {
                reached = true;
                assert_eq!(partition.device, Path::new("/dev/vda2"));
                drop(key);
                Ok(crate::unlock::Unlocked::Mapping(Mapping::for_test(
                    "dm-3",
                    "td-system",
                    File::open("/dev/null").unwrap(),
                )))
            },
        ));
        assert!(reached);
        assert_eq!(pinned.device, Path::new("/dev/vda2"));
        assert_eq!(mode_device(&operation).unwrap(), pinned.path());

        let mut operation = on_volume_operation(&["mount-var", "/sysroot/var"]);
        let pinned = bound(crate::bind_volume(
            &mut operation,
            luks2(Some("td-system")),
            None,
            no_unlock,
        ));
        assert_eq!(mode_device(&operation).unwrap(), pinned.path());

        let mut operation = on_volume_operation(&["mount-root", "/volume"]);
        let binding =
            crate::bind_volume(&mut operation, luks2(None), Some(handed_key()), |_, _| {
                Ok(crate::unlock::Unlocked::Halt("released".into()))
            })
            .unwrap();
        assert!(matches!(binding, crate::Binding::Halt(reason) if reason == "released"));
        assert_eq!(
            mode_device(&operation).unwrap(),
            Path::new("/volume-device")
        );
    }

    /// The selector admits only the mapping it opened, by its dm name.
    #[test]
    fn the_selector_admits_only_its_own_mapping_name() {
        let mapping = Mapping::for_test("dm-0", "td-selector", File::open("/dev/null").unwrap());
        assert_eq!(
            admitted_name(mapping, "td-selector").unwrap().describe(),
            "dm-0 (td-selector)"
        );
        let other = Mapping::for_test("dm-1", "td-root", File::open("/dev/null").unwrap());
        let error = admitted_name(other, "td-selector").err().unwrap();
        assert!(error.to_string().contains("dm-1 (td-root)"), "{error}");
    }
}

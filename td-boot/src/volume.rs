//! Read-only identity discovery; a match grants neither trust nor write authority.
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read};
use std::os::unix::fs::{FileExt, FileTypeExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::{invalid, protocol};

const SUPER_OFFSET: u64 = 65536;
const SUPER_BYTES: usize = 4096;
const MAX_DEVICES: usize = 4096;
const OPEN_FLAGS: i32 = 0o400000 | 0o4000; // x86-64 Linux O_NOFOLLOW | O_NONBLOCK.

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

fn supported_name(name: &str) -> bool {
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
    let mut bytes = Vec::new();
    File::open(path)?.take(129).read_to_end(&mut bytes)?;
    if bytes.len() > 128 {
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

fn matches_device(meta: &fs::Metadata, expected: (u64, u64)) -> bool {
    let dev = meta.rdev();
    let major = ((dev >> 8) & 0xfff) | ((dev >> 32) & 0xfffff000);
    let minor = (dev & 0xff) | ((dev >> 12) & 0xffffff00);
    meta.file_type().is_block_device() && (major, minor) == expected
}

fn probe(sys: &Path, path: &Path) -> io::Result<Option<Uuid>> {
    let expected = device_number(&text(&sys.join("dev"))?)?;
    let sectors = text(&sys.join("size"))?;
    let sectors: u64 = sectors
        .trim_end()
        .parse()
        .map_err(|_| invalid("invalid block capacity"))?;
    if sectors < (SUPER_OFFSET + SUPER_BYTES as u64) / 512 {
        return Ok(None);
    }
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
    let mut bytes = [0; SUPER_BYTES];
    file.read_exact_at(&mut bytes, SUPER_OFFSET)?;
    let result = identify(&bytes)?;
    let after = fs::symlink_metadata(path)?;
    if !matches_device(&after, expected)
        || after.dev() != opened.dev()
        || after.ino() != opened.ino()
    {
        return Err(invalid("volume device changed during probe"));
    }
    Ok(result)
}

fn choose(
    found: &mut Option<(Uuid, PathBuf)>,
    uuid: Uuid,
    path: PathBuf,
    expected: Option<&Uuid>,
) -> io::Result<()> {
    if expected.is_some_and(|expected| expected != &uuid) {
        return Ok(());
    }
    if found.is_some() {
        return Err(invalid("ambiguous td volume identity"));
    }
    *found = Some((uuid, path));
    Ok(())
}

fn select_scan(
    entries: impl IntoIterator<Item = io::Result<Option<(Uuid, PathBuf)>>>,
    expected: Option<&Uuid>,
) -> io::Result<Option<(Uuid, PathBuf)>> {
    let mut found = None;
    for entry in entries {
        let entry = match entry {
            // A disappearing/not-yet-published node makes this entire scan incomplete.
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            other => other?,
        };
        if let Some((uuid, path)) = entry {
            choose(&mut found, uuid, path, expected)?;
        }
    }
    Ok(found)
}

fn scan(expected: Option<&Uuid>) -> io::Result<Option<(Uuid, PathBuf)>> {
    let entries = fs::read_dir("/sys/class/block")?
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
            let uuid = probe(&entry.path(), &path).map_err(|error| {
                io::Error::new(
                    error.kind(),
                    format!("probe volume {}: {error}", path.display()),
                )
            })?;
            Ok(uuid.map(|uuid| (uuid, path)))
        });
    select_scan(entries, expected)
}

pub(crate) fn resolve(expected: Option<&Uuid>) -> io::Result<(Uuid, PathBuf)> {
    let started = Instant::now();
    loop {
        if let Some(volume) = scan(expected)? {
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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

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
        for name in ["vda", "vdb2", "sdaa", "sda2", "nvme0n1", "nvme12n34p5"] {
            assert!(supported_name(name), "{name}");
        }
        for name in [
            "",
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
                choose(&mut found, id, path.into(), Some(&uuid)).unwrap();
            }
            assert_eq!(found, Some((uuid.clone(), PathBuf::from("/dev/vdb2"))));
            assert!(choose(&mut found, uuid.clone(), "/dev/sda2".into(), Some(&uuid)).is_err());
        }
        let mut found = None;
        choose(&mut found, uuid, "/dev/vda2".into(), None).unwrap();
        assert!(choose(&mut found, other, "/dev/sda2".into(), None).is_err());
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
        assert!(
            crate::parse_args(args(&[
                "volume",
                "12345678-90ab-cdef-1234-567890abcdef",
                "extra"
            ]))
            .is_err()
        );
    }
    #[test]
    fn an_incomplete_scan_cannot_select_a_partial_match() {
        let good = || Ok(Some((Uuid([0x12; 16]), PathBuf::from("/dev/vda2"))));
        let missing = || Err(io::Error::from(io::ErrorKind::NotFound));
        assert_eq!(select_scan([good(), missing()], None).unwrap(), None);
        assert_eq!(select_scan([missing(), good()], None).unwrap(), None);
        assert!(
            select_scan(
                [
                    good(),
                    Err(io::Error::from(io::ErrorKind::PermissionDenied))
                ],
                None
            )
            .is_err()
        );
        assert!(select_scan([good(), good()], None).is_err());
        assert_eq!(
            select_scan([good(), Ok(None)], None).unwrap(),
            Some((Uuid([0x12; 16]), PathBuf::from("/dev/vda2")))
        );
    }
}

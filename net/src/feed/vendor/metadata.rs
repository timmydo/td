//! Read two pinned crate members without an unpacker or archive path writes.
use super::*;

const MAX_COMPRESSED_BYTES: u64 = 64 * 1024 * 1024;
const MAX_HEADERS: usize = 32_768;
const BLOCK: usize = 512;

pub(super) struct Metadata {
    pub lock: String,
    pub manifest: String,
}

pub(super) fn read_pinned(
    archive: &Path,
    checksum: &str,
    package: &str,
) -> Result<Metadata, String> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(OPEN_REGULAR_NOFOLLOW)
        .open(archive)
        .map_err(|e| format!("open pinned crate metadata: {e}"))?;
    let meta = file.metadata().map_err(|e| e.to_string())?;
    if !meta.is_file() || meta.len() > MAX_COMPRESSED_BYTES {
        return Err("crate metadata input is not a bounded regular file".into());
    }
    let capacity =
        usize::try_from(meta.len()).map_err(|_| "crate input length exceeds address space")?;
    let mut compressed = Vec::with_capacity(capacity);
    file.take(MAX_COMPRESSED_BYTES + 1)
        .read_to_end(&mut compressed)
        .map_err(|e| format!("read pinned crate metadata: {e}"))?;
    if compressed.len() as u64 > MAX_COMPRESSED_BYTES {
        return Err("crate metadata input grew past its byte limit".into());
    }
    if hex_sha256(&compressed) != checksum {
        return Err("crate metadata input does not match its source pin".into());
    }
    // The shared decoder checks gzip CRC/length and bounds expanded bytes,
    // members and DEFLATE blocks before the tar reader sees any metadata.
    let expanded = td_engine::gzip::decompress_bytes(&compressed)?;
    read_tar(&expanded, package)
}

fn field(header: &[u8], start: usize, length: usize) -> Result<&[u8], String> {
    let end = start
        .checked_add(length)
        .ok_or("crate tar field offset overflow")?;
    header
        .get(start..end)
        .ok_or_else(|| "truncated crate tar header".into())
}

fn octal(bytes: &[u8]) -> Result<u64, String> {
    let bytes = bytes.trim_ascii();
    let end = bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len());
    let digits = bytes.get(..end).ok_or("invalid tar number")?.trim_ascii();
    if bytes
        .get(end..)
        .ok_or("invalid tar number")?
        .iter()
        .any(|b| *b != 0 && *b != b' ')
    {
        return Err("invalid tar number padding".into());
    }
    if digits.is_empty() {
        return Err("crate tar number contains no digits".into());
    }
    let mut value = 0u64;
    for digit in digits {
        if !(b'0'..=b'7').contains(digit) {
            return Err("crate tar number is not octal".into());
        }
        value = value
            .checked_mul(8)
            .and_then(|v| v.checked_add(u64::from(*digit) - u64::from(b'0')))
            .ok_or("crate tar number overflow")?;
    }
    Ok(value)
}

fn text_field(bytes: &[u8]) -> Result<&str, String> {
    let end = bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len());
    std::str::from_utf8(bytes.get(..end).ok_or("invalid tar text field")?)
        .map_err(|_| "crate tar path is not UTF-8".into())
}

fn member_text(bytes: &[u8], limit: u64) -> Result<String, String> {
    if bytes.len() as u64 > limit {
        return Err("crate metadata member exceeds byte limit".into());
    }
    std::str::from_utf8(bytes)
        .map(str::to_string)
        .map_err(|_| "crate metadata member is not UTF-8".into())
}

fn read_tar(bytes: &[u8], package: &str) -> Result<Metadata, String> {
    if !bytes.len().is_multiple_of(BLOCK) {
        return Err("crate tar length is not block aligned".into());
    }
    let lock_name = format!("{package}/Cargo.lock");
    let manifest_name = format!("{package}/Cargo.toml");
    let mut lock = None;
    let mut manifest = None;
    let mut position = 0usize;
    let mut headers = 0usize;
    loop {
        let end = position
            .checked_add(BLOCK)
            .ok_or("crate tar offset overflow")?;
        let header = bytes
            .get(position..end)
            .ok_or("truncated crate tar header")?;
        if header.iter().all(|b| *b == 0) {
            let second_end = end.checked_add(BLOCK).ok_or("crate tar offset overflow")?;
            let second = bytes
                .get(end..second_end)
                .ok_or("missing second crate tar end marker")?;
            if second.iter().any(|b| *b != 0)
                || bytes
                    .get(second_end..)
                    .ok_or("truncated crate tar padding")?
                    .iter()
                    .any(|b| *b != 0)
            {
                return Err("nonzero data after crate tar end marker".into());
            }
            break;
        }
        headers = headers
            .checked_add(1)
            .ok_or("crate tar header count overflow")?;
        if headers > MAX_HEADERS {
            return Err("crate tar exceeds header limit".into());
        }
        let checksum = octal(field(header, 148, 8)?)?;
        let computed: u64 = header
            .iter()
            .enumerate()
            .map(|(i, b)| {
                if (148..156).contains(&i) {
                    u64::from(b' ')
                } else {
                    u64::from(*b)
                }
            })
            .sum();
        if checksum != computed {
            return Err("crate tar header checksum mismatch".into());
        }
        let format = field(header, 257, 8)?;
        if format != b"ustar\x0000" && format != b"ustar  \0" {
            return Err("unsupported crate tar header format".into());
        }
        let name = text_field(field(header, 0, 100)?)?;
        let prefix = if format == b"ustar\x0000" {
            text_field(field(header, 345, 155)?)?
        } else {
            ""
        };
        let path = if prefix.is_empty() {
            std::borrow::Cow::Borrowed(name)
        } else {
            std::borrow::Cow::Owned(format!("{prefix}/{name}"))
        };
        if !relative(&path) {
            return Err("crate tar path is not a plain relative path".into());
        }
        let kind = *header.get(156).ok_or("missing crate tar entry kind")?;
        if !matches!(kind, 0 | b'0' | b'5') {
            return Err(format!("unsupported crate tar entry kind 0x{kind:02x} at {path:?}; metadata requires ordinary files/directories"));
        }
        let size = usize::try_from(octal(field(header, 124, 12)?)?)
            .map_err(|_| "crate tar member size exceeds address space")?;
        if kind == b'5' && size != 0 {
            return Err("crate tar directory has a payload".into());
        }
        let data_end = end.checked_add(size).ok_or("crate tar offset overflow")?;
        let data = bytes
            .get(end..data_end)
            .ok_or("truncated crate tar member")?;
        let slot = if path == lock_name {
            Some((&mut lock, MAX_CARGO_LOCK_BYTES))
        } else if path == manifest_name {
            Some((&mut manifest, MAX_MANIFEST_BYTES))
        } else {
            None
        };
        if let Some((slot, limit)) = slot {
            if kind == b'5' || slot.is_some() {
                return Err("crate metadata member is repeated or not a regular file".into());
            }
            *slot = Some(member_text(data, limit)?);
        }
        let padding = (BLOCK - size % BLOCK) % BLOCK;
        position = data_end
            .checked_add(padding)
            .ok_or("crate tar offset overflow")?;
        let padding = bytes
            .get(data_end..position)
            .ok_or("truncated crate tar member padding")?;
        if padding.iter().any(|byte| *byte != 0) {
            return Err("nonzero crate tar member padding".into());
        }
    }
    Ok(Metadata {
        lock: lock.ok_or("pinned crate has no Cargo.lock")?,
        manifest: manifest.ok_or("pinned crate has no Cargo.toml")?,
    })
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    pub(in crate::feed::vendor) fn entry(out: &mut Vec<u8>, name: &str, kind: u8, data: &[u8]) {
        let mut header = [0u8; BLOCK];
        header[..name.len()].copy_from_slice(name.as_bytes());
        header[124..136].copy_from_slice(format!("{:011o}\0", data.len()).as_bytes());
        header[156] = kind;
        header[257..265].copy_from_slice(b"ustar\x0000");
        seal(&mut header);
        out.extend_from_slice(&header);
        out.extend_from_slice(data);
        out.resize(out.len().next_multiple_of(BLOCK), 0);
    }

    fn seal(header: &mut [u8]) {
        header[148..156].fill(b' ');
        let sum: u64 = header.iter().map(|b| u64::from(*b)).sum();
        header[148..156].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());
    }

    fn archive() -> Vec<u8> {
        let mut bytes = Vec::new();
        entry(&mut bytes, "p/Cargo.lock", b'0', b"the selected lock");
        entry(&mut bytes, "p/Cargo.toml", b'0', b"the selected manifest");
        entry(
            &mut bytes,
            "p/src/not-written",
            b'0',
            b"only the build extracts this",
        );
        bytes.resize(bytes.len() + BLOCK * 2, 0);
        bytes
    }

    pub(in crate::feed::vendor) fn gzip(bytes: &[u8]) -> Vec<u8> {
        let mut out = vec![0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 255];
        let blocks: Vec<_> = bytes.chunks(u16::MAX as usize).collect();
        for (index, block) in blocks.iter().enumerate() {
            out.push(u8::from(index + 1 == blocks.len()));
            let length = block.len() as u16;
            out.extend_from_slice(&length.to_le_bytes());
            out.extend_from_slice(&(!length).to_le_bytes());
            out.extend_from_slice(block);
        }
        out.extend_from_slice(&td_engine::crc32::crc32(bytes).to_le_bytes());
        out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
        out
    }

    #[test]
    fn pinned_metadata_is_read_without_extracting_archive_paths() {
        let dir = super::super::tests::scratch("native-metadata");
        let source = dir.join("p.crate");
        let bytes = gzip(&archive());
        std::fs::write(&source, &bytes).unwrap();
        let selected = read_pinned(&source, &hex_sha256(&bytes), "p").unwrap();
        assert_eq!(selected.lock, "the selected lock");
        assert_eq!(selected.manifest, "the selected manifest");
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        assert!(read_pinned(&source, &hex_sha256(b"different bytes"), "p").is_err());
        let alias = dir.join("alias.crate");
        std::os::unix::fs::symlink(&source, &alias).unwrap();
        assert!(read_pinned(&alias, &hex_sha256(&bytes), "p").is_err());
        assert!(read_pinned(&dir, "unused", "p").is_err());
        let mut corrupt = bytes;
        let last = corrupt.len() - 8;
        corrupt[last] ^= 1;
        std::fs::write(&source, &corrupt).unwrap();
        assert!(read_pinned(&source, &hex_sha256(&corrupt), "p").is_err());
        std::fs::OpenOptions::new()
            .write(true)
            .open(&source)
            .unwrap()
            .set_len(MAX_COMPRESSED_BYTES + 1)
            .unwrap();
        assert!(read_pinned(&source, "unused", "p").is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn ambiguous_or_unsupported_metadata_is_refused() {
        for (name, kind, data) in [
            ("p/Cargo.lock", b'0', b"duplicate".as_slice()),
            ("p/link", b'2', b"".as_slice()),
            ("p/extended", b'x', b"".as_slice()),
            ("../escape", b'0', b"".as_slice()),
        ] {
            let mut bytes = archive();
            bytes.truncate(bytes.len() - BLOCK * 2);
            entry(&mut bytes, name, kind, data);
            bytes.resize(bytes.len() + BLOCK * 2, 0);
            assert!(read_tar(&bytes, "p").is_err(), "{name} {kind}");
        }
        let mut bytes = Vec::new();
        entry(&mut bytes, "p/Cargo.lock", b'5', b"");
        entry(&mut bytes, "p/Cargo.toml", b'0', b"manifest");
        bytes.resize(bytes.len() + BLOCK * 2, 0);
        assert!(read_tar(&bytes, "p").is_err());
        assert!(read_tar(&archive(), "absent").is_err());
    }

    #[test]
    fn truncated_corrupt_and_oversized_members_are_refused() {
        let good = archive();
        for size in [0, 511, 512, 515, good.len() - BLOCK, good.len() - 1] {
            assert!(read_tar(&good[..size], "p").is_err(), "{size}");
        }
        let mut bad = good.clone();
        bad[0] ^= 1;
        assert!(read_tar(&bad, "p").is_err());
        let mut bad = good.clone();
        bad[124] = b'9';
        seal(&mut bad[..BLOCK]);
        assert!(read_tar(&bad, "p").is_err());
        let mut bad = good;
        *bad.last_mut().unwrap() = 1;
        assert!(read_tar(&bad, "p").is_err());
        let mut bytes = Vec::new();
        entry(&mut bytes, "p/Cargo.lock", b'0', b"lock");
        entry(
            &mut bytes,
            "p/Cargo.toml",
            b'0',
            &vec![b'a'; MAX_MANIFEST_BYTES as usize + 1],
        );
        bytes.resize(bytes.len() + BLOCK * 2, 0);
        assert!(read_tar(&bytes, "p").err().unwrap().contains("byte limit"));
        assert!(member_text(b"\xff", 1).is_err());
    }

    #[test]
    fn member_padding_and_numeric_fields_must_have_their_declared_form() {
        let mut bytes = archive();
        bytes[BLOCK + b"the selected lock".len()] = 1;
        assert!(read_tar(&bytes, "p")
            .err()
            .unwrap()
            .contains("member padding"));
        for empty in [b"".as_slice(), b"   ", b"\0\0"] {
            assert!(octal(empty).is_err());
        }
        assert_eq!(octal(b"00000000000\0").unwrap(), 0);
    }

    #[test]
    fn header_count_is_bounded_even_for_empty_unselected_members() {
        let mut bytes = archive();
        bytes.truncate(bytes.len() - BLOCK * 2);
        for _ in 3..MAX_HEADERS {
            entry(&mut bytes, "p/unused", b'0', b"");
        }
        bytes.resize(bytes.len() + BLOCK * 2, 0);
        assert!(read_tar(&bytes, "p").is_ok());
        bytes.truncate(bytes.len() - BLOCK * 2);
        entry(&mut bytes, "p/one-more", b'0', b"");
        bytes.resize(bytes.len() + BLOCK * 2, 0);
        assert!(read_tar(&bytes, "p")
            .err()
            .unwrap()
            .contains("header limit"));
    }

    #[test]
    fn ordinary_gnu_and_ustar_prefix_headers_are_supported() {
        let mut gnu = archive();
        gnu[257..265].copy_from_slice(b"ustar  \0");
        seal(&mut gnu[..BLOCK]);
        assert!(read_tar(&gnu, "p").is_ok());
        let mut prefixed = archive();
        prefixed[..100].fill(0);
        prefixed[..10].copy_from_slice(b"Cargo.lock");
        prefixed[345] = b'p';
        seal(&mut prefixed[..BLOCK]);
        assert!(read_tar(&prefixed, "p").is_ok());
    }
}

//! Narrow reader for td's compact-inode, flat/plain or 16-byte-extent images.
use crate::{invalid, sha256, zstd, Result};
use std::collections::BTreeMap;
use std::ops::Range;

const BLOCK: usize = 4096;
const EXTENT: usize = 128 * 1024;
const MAX_INODES: usize = 200_000;
const MAX_EXTENTS: usize = 1_000_000;

struct Extent {
    physical: Range<usize>,
    logical: usize,
    compressed: bool,
}

pub struct Image {
    files: Vec<Vec<Extent>>,
}

fn bytes(data: &[u8], at: usize, size: usize) -> Result<&[u8]> {
    let end = at
        .checked_add(size)
        .ok_or_else(|| invalid("EROFS offset overflow"))?;
    data.get(at..end)
        .ok_or_else(|| invalid("truncated EROFS field").into())
}

fn number(data: &[u8], at: usize, size: usize) -> Result<usize> {
    let mut n = 0u64;
    for (i, b) in bytes(data, at, size)?.iter().enumerate() {
        n |= u64::from(*b) << (i * 8);
    }
    Ok(usize::try_from(n)?)
}

impl Image {
    pub fn parse(data: &[u8]) -> Result<Self> {
        if number(data, 1024, 4)? != 0xe0f5e1e2
            || number(data, 1036, 1)? != 12
            || number(data, 1064, 4)? != 1
            || number(data, 1060, 4)?.checked_mul(BLOCK) != Some(data.len())
            || number(data, 1114, 1)? != 0
            || !matches!(number(data, 1104, 4)?, 0 | 2)
            || number(data, 1110, 2)? != 0
        {
            return Err(invalid("unsupported td EROFS superblock").into());
        }
        let count = number(data, 1040, 8)?;
        if count == 0 || count > MAX_INODES {
            return Err(invalid("EROFS inode count out of bounds").into());
        }
        let mut cursor = BLOCK;
        let mut files = Vec::new();
        let mut extent_count = 0;
        for _ in 0..count {
            let format = number(data, cursor, 2)?;
            let mode = number(data, cursor + 4, 2)? & 0o170000;
            let size = number(data, cursor + 8, 4)?;
            if number(data, cursor + 2, 2)? != 0 || !matches!(format, 0 | 2) {
                return Err(invalid("unsupported EROFS inode layout or xattrs").into());
            }
            bytes(data, cursor, 32)?;
            let mut extents = Vec::new();
            if format == 2 {
                if mode != 0o100000
                    || size == 0
                    || number(data, cursor + 36, 2)? != 5
                    || number(data, cursor + 38, 2)? != 0
                {
                    return Err(invalid("unsupported EROFS compressed inode").into());
                }
                let n = number(data, cursor + 32, 4)?;
                if n != size.div_ceil(EXTENT) || n > MAX_EXTENTS - extent_count {
                    return Err(invalid("invalid EROFS extent count").into());
                }
                extent_count += n;
                for k in 0..n {
                    let at = cursor + 48 + k * 16;
                    let encoded = number(data, at, 4)?;
                    let length = encoded & 0x0fff_ffff;
                    let kind = encoded >> 28;
                    let start = number(data, at + 4, 8)?;
                    let logical = size.saturating_sub(k * EXTENT).min(EXTENT);
                    if !matches!(kind, 0 | 4)
                        || length == 0
                        || length > EXTENT
                        || number(data, at + 12, 4)? != k * EXTENT
                        || (kind == 0 && length != logical)
                    {
                        return Err(invalid("invalid EROFS extent record").into());
                    }
                    bytes(data, start, length)?;
                    extents.push(Extent {
                        physical: start..start + length,
                        logical,
                        compressed: kind == 4,
                    });
                }
                cursor += (48 + n * 16).div_ceil(32) * 32;
            } else {
                if mode == 0o100000 && size > 0 {
                    let start = number(data, cursor + 16, 4)?
                        .checked_mul(BLOCK)
                        .ok_or_else(|| invalid("EROFS block address overflow"))?;
                    bytes(data, start, size)?;
                    extents.push(Extent {
                        physical: start..start + size,
                        logical: size,
                        compressed: false,
                    });
                }
                cursor += 32;
            }
            if mode == 0o100000 {
                files.push(extents);
            }
        }
        let metadata_end = cursor.div_ceil(BLOCK) * BLOCK;
        if files
            .iter()
            .flatten()
            .any(|e| e.physical.start < metadata_end)
        {
            return Err(invalid("EROFS file data overlaps inode table").into());
        }
        let image = Self { files };
        image.partition(data.len())?;
        Ok(image)
    }

    /// All compressed/raw file extents plus every metadata and padding byte.
    pub fn partition(&self, size: usize) -> Result<Vec<Range<usize>>> {
        let mut unique = BTreeMap::new();
        for extent in self.files.iter().flatten() {
            if let Some(end) = unique.insert(extent.physical.start, extent.physical.end) {
                if end != extent.physical.end {
                    return Err(invalid("EROFS extents disagree at one address").into());
                }
            }
        }
        let mut spans = Vec::new();
        let mut cursor = 0;
        for (start, end) in unique {
            if start < cursor || end > size {
                return Err(invalid("EROFS extents overlap or exceed image").into());
            }
            fill(&mut spans, cursor, start);
            spans.push(start..end);
            cursor = end;
        }
        fill(&mut spans, cursor, size);
        Ok(spans)
    }

    /// Logical file hashes ignore paths, inode numbers and hard-link aliases.
    pub fn files(&self, data: &[u8]) -> Result<BTreeMap<([u8; 32], usize), u64>> {
        let mut files = BTreeMap::new();
        for extents in &self.files {
            let mut hash = sha256::Sha256::new();
            let mut size = 0;
            for extent in extents {
                let encoded = data
                    .get(extent.physical.clone())
                    .ok_or_else(|| invalid("file outside image"))?;
                if extent.compressed {
                    let decoded = zstd::decompress(encoded, extent.logical)?;
                    if decoded.len() != extent.logical {
                        return Err(invalid("EROFS decoded length mismatch").into());
                    }
                    hash.update(&decoded);
                } else {
                    hash.update(encoded);
                }
                size += extent.logical;
            }
            files.insert((hash.finalize(), size), size as u64);
        }
        Ok(files)
    }
}

fn fill(spans: &mut Vec<Range<usize>>, mut start: usize, end: usize) {
    while start < end {
        let next = (start + 64 * 1024).min(end);
        spans.push(start..next);
        start = next;
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]
    use super::*;
    use crate::{packer, test_temp::TempDir, transfer};
    use std::fs;

    #[test]
    fn real_packer_extent_reconstruction_and_logical_file_hashes() {
        let root = TempDir::new("image-transfer").unwrap();
        let data = vec![42; EXTENT * 3 + 13];
        fs::write(root.join("a"), &data).unwrap();
        fs::write(root.join("empty"), []).unwrap();
        fs::hard_link(root.join("a"), root.join("linked")).unwrap();
        std::os::unix::fs::symlink("a", root.join("symlink")).unwrap();
        let a = packer::build_image(&root).unwrap();
        let old = Image::parse(&a).unwrap();
        let mut expected = sha256::Sha256::new();
        expected.update(&data);
        let files = old.files(&a).unwrap();
        assert_eq!(files.len(), 2);
        assert_eq!(
            files.get(&(expected.finalize(), data.len())),
            Some(&(data.len() as u64))
        );
        let mut changed = data.clone();
        changed[EXTENT + 1] = 100;
        fs::write(root.join("a"), changed).unwrap();
        fs::write(root.join("0"), b"moves inode and data offsets").unwrap();
        let b = packer::build_image(&root).unwrap();
        let new = Image::parse(&b).unwrap();
        let before = transfer::ranges(&a, &old.partition(a.len()).unwrap()).unwrap();
        let after = transfer::ranges(&b, &new.partition(b.len()).unwrap()).unwrap();
        let mut result = Vec::new();
        transfer::reconstruct(&a, &b, &before, &after, &mut result).unwrap();
        assert_eq!(result, b);
    }

    #[test]
    fn malformed_and_truncated_images_refuse() {
        let root = TempDir::new("image-transfer-bad").unwrap();
        fs::write(root.join("file"), vec![1; EXTENT]).unwrap();
        let good = packer::build_image(&root).unwrap();
        for length in [0, 1024, 4096, good.len() - 1] {
            assert!(Image::parse(&good[..length]).is_err());
        }
        for at in [1024, 1036, 1047, 1064, 1104, 1114, 4096] {
            let mut bad = good.clone();
            bad[at] = 0xff;
            assert!(Image::parse(&bad).is_err(), "offset {at}");
        }
    }
}

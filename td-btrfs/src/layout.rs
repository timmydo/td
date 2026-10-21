//! Where a new filesystem's chunks lie on its one device.
//!
//! The layout is td's own, not btrfs-progs': the system chunk and the
//! metadata chunk are DUP, data is single, and every chunk's logical address
//! is its first stripe's device offset, which keeps logical ranges disjoint
//! because device ranges are. No device extent covers a superblock copy, so
//! no block group carries the stripe the kernel excludes around one.

use std::io;

use crate::format::{
    invalid, round_up, Chunk, Stripe, BG_DATA, BG_DUP, BG_METADATA, BG_SYSTEM, MIB, SUPER_EXCLUDE,
    SUPER_MIRRORS,
};

/// The first megabyte holds the boot area and the primary superblock.
pub const RESERVED: u64 = MIB;
pub const SYSTEM_SIZE: u64 = 8 * MIB;
/// The smallest data chunk, as btrfs-progs makes for an empty filesystem.
pub const MIN_DATA: u64 = 8 * MIB;
/// The largest data chunk td writes.
pub const MAX_DATA_CHUNK: u64 = 1 << 30;

/// The default metadata chunk: the kernel's 256 MiB stripe (1 GiB above
/// 50 GiB), held to a tenth of the device but never under 32 MiB.
pub fn default_metadata(total: u64) -> u64 {
    let full = if total > 50 << 30 { 1 << 30 } else { 256 * MIB };
    full.min((total / 10 / MIB * MIB).max(32 * MIB))
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Layout {
    pub total: u64,
    pub system: Chunk,
    pub metadata: Chunk,
    pub data: Vec<Chunk>,
}

impl Layout {
    /// Place a system chunk, a metadata chunk of `metadata` bytes and data
    /// chunks for `data` bytes on a device of `total` bytes.
    pub fn plan(total: u64, metadata: u64, data: u64) -> io::Result<Self> {
        let mut next = RESERVED;
        let mut take = |len: u64| -> io::Result<u64> {
            let mut start = next;
            for &mirror in SUPER_MIRRORS {
                let end = start.saturating_add(len);
                if start < mirror.saturating_add(SUPER_EXCLUDE) && mirror < end {
                    start = round_up(mirror + SUPER_EXCLUDE, MIB);
                }
            }
            let end = start
                .checked_add(len)
                .filter(|end| *end <= total)
                .ok_or_else(|| {
                    invalid(format!(
                        "a {total}-byte device is too small for this filesystem's contents"
                    ))
                })?;
            next = end;
            Ok(start)
        };
        let dup = |take: &mut dyn FnMut(u64) -> io::Result<u64>, len: u64, flags: u64| {
            let first = take(len)?;
            let second = take(len)?;
            Ok::<_, io::Error>(Chunk {
                logical: first,
                length: len,
                flags: flags | BG_DUP,
                stripes: vec![
                    Stripe {
                        devid: 1,
                        offset: first,
                    },
                    Stripe {
                        devid: 1,
                        offset: second,
                    },
                ],
            })
        };
        let system = dup(&mut take, SYSTEM_SIZE, BG_SYSTEM)?;
        let metadata = dup(&mut take, round_up(metadata, MIB), BG_METADATA)?;
        let mut remaining = round_up(data, MIB).max(MIN_DATA);
        let mut chunks = Vec::new();
        while remaining > 0 {
            let len = remaining.min(MAX_DATA_CHUNK);
            let at = take(len)?;
            chunks.push(Chunk {
                logical: at,
                length: len,
                flags: BG_DATA,
                stripes: vec![Stripe {
                    devid: 1,
                    offset: at,
                }],
            });
            remaining -= len;
        }
        Ok(Self {
            total,
            system,
            metadata,
            data: chunks,
        })
    }

    pub fn chunks(&self) -> impl Iterator<Item = &Chunk> {
        [&self.system, &self.metadata]
            .into_iter()
            .chain(self.data.iter())
    }

    /// Bytes of the device the chunks occupy.
    pub fn device_used(&self) -> u64 {
        self.chunks()
            .map(|chunk| chunk.length * chunk.stripes.len() as u64)
            .sum()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn ranges(layout: &Layout) -> Vec<(u64, u64)> {
        let mut out: Vec<(u64, u64)> = layout
            .chunks()
            .flat_map(|chunk| {
                chunk
                    .stripes
                    .iter()
                    .map(move |stripe| (stripe.offset, stripe.offset + chunk.length))
            })
            .collect();
        out.sort();
        out
    }

    #[test]
    fn chunks_are_disjoint_and_clear_of_superblocks() {
        for (total, metadata, data) in [
            (256 * MIB, 32 * MIB, 0),
            (256 * MIB, 32 * MIB, 100 * MIB),
            (8 << 30, 256 * MIB, 3 << 30),
            (300 << 30, 1 << 30, 260 << 30),
        ] {
            let layout = Layout::plan(total, metadata, data).unwrap();
            let all = ranges(&layout);
            for pair in all.windows(2) {
                assert!(pair[0].1 <= pair[1].0, "{pair:?}");
            }
            for (start, end) in &all {
                assert!(*start >= RESERVED && *end <= total);
                for mirror in SUPER_MIRRORS {
                    assert!(*end <= *mirror || *start >= mirror + SUPER_EXCLUDE);
                }
            }
            let data_len: u64 = layout.data.iter().map(|c| c.length).sum();
            assert_eq!(data_len, round_up(data, MIB).max(MIN_DATA));
            assert!(layout.data.iter().all(|c| c.length <= MAX_DATA_CHUNK));
        }
    }

    #[test]
    fn a_device_too_small_is_refused() {
        assert!(Layout::plan(64 * MIB, 32 * MIB, 0).is_err());
        assert!(Layout::plan(256 * MIB, 32 * MIB, 200 * MIB).is_err());
    }

    #[test]
    fn the_default_metadata_chunk_scales_with_the_device() {
        assert_eq!(default_metadata(256 * MIB), 32 * MIB);
        assert_eq!(default_metadata(6 << 30), 256 * MIB);
        assert_eq!(default_metadata(100 << 30), 1 << 30);
    }
}

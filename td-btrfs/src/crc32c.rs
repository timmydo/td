//! CRC-32C (Castagnoli), which Btrfs uses for blocks, data and name hashes.

use std::sync::OnceLock;

const POLY: u32 = 0x82f6_3b78;

type Tables = [[u32; 256]; 8];

fn tables() -> &'static Tables {
    static TABLES: OnceLock<Tables> = OnceLock::new();
    TABLES.get_or_init(|| {
        let mut tables = [[0u32; 256]; 8];
        for (byte, slot) in tables[0].iter_mut().enumerate() {
            let mut crc = byte as u32;
            for _ in 0..8 {
                crc = (crc >> 1) ^ (POLY & 0u32.wrapping_sub(crc & 1));
            }
            *slot = crc;
        }
        let first = tables[0];
        for level in 1..8 {
            let previous = tables.get(level - 1).copied().unwrap_or([0; 256]);
            if let Some(table) = tables.get_mut(level) {
                for (slot, prior) in table.iter_mut().zip(previous) {
                    *slot = (prior >> 8) ^ lookup(&first, prior);
                }
            }
        }
        tables
    })
}

fn lookup(table: &[u32; 256], value: u32) -> u32 {
    table.get((value & 0xff) as usize).copied().unwrap_or(0)
}

/// The raw register update, with no pre- or post-inversion: the kernel's
/// `crc32c(seed, data, len)`.
pub fn update(mut crc: u32, data: &[u8]) -> u32 {
    let t = tables();
    let (words, tail) = data.as_chunks::<8>();
    for &[b0, b1, b2, b3, b4, b5, b6, b7] in words {
        let low = crc ^ u32::from_le_bytes([b0, b1, b2, b3]);
        let high = u32::from_le_bytes([b4, b5, b6, b7]);
        crc = lookup(&t[7], low)
            ^ lookup(&t[6], low >> 8)
            ^ lookup(&t[5], low >> 16)
            ^ lookup(&t[4], low >> 24)
            ^ lookup(&t[3], high)
            ^ lookup(&t[2], high >> 8)
            ^ lookup(&t[1], high >> 16)
            ^ lookup(&t[0], high >> 24);
    }
    for byte in tail {
        crc = (crc >> 8) ^ lookup(&t[0], crc ^ u32::from(*byte));
    }
    crc
}

/// The standard CRC-32C of `data`, as block and data checksums store it.
pub fn checksum(data: &[u8]) -> u32 {
    !update(!0, data)
}

/// `btrfs_name_hash`: the DIR_ITEM and XATTR_ITEM key offset.
pub fn name_hash(name: &[u8]) -> u64 {
    u64::from(update(!1, name))
}

/// `hash_extent_data_ref`: the sort key of an inline EXTENT_DATA_REF.
pub fn extent_data_ref_hash(root: u64, owner: u64, offset: u64) -> u64 {
    let high = update(!0, &root.to_le_bytes());
    let low = update(update(!0, &owner.to_le_bytes()), &offset.to_le_bytes());
    (u64::from(high) << 31) ^ u64::from(low)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn bitwise(data: &[u8]) -> u32 {
        let mut crc = !0u32;
        for byte in data {
            crc ^= u32::from(*byte);
            for _ in 0..8 {
                crc = (crc >> 1) ^ (POLY & 0u32.wrapping_sub(crc & 1));
            }
        }
        !crc
    }

    #[test]
    fn matches_the_reference_vectors() {
        assert_eq!(checksum(b"123456789"), 0xe306_9283);
        assert_eq!(checksum(&[0; 32]), 0x8a91_36aa);
        let data: Vec<u8> = (0..1000u32).map(|n| (n * 7 + 3) as u8).collect();
        for len in 0..data.len() {
            assert_eq!(checksum(&data[..len]), bitwise(&data[..len]), "{len}");
        }
    }

    // Values read from a btrfs-progs v7.0 image's dump-tree.
    #[test]
    fn hashes_match_btrfs_progs() {
        assert_eq!(name_hash(b"default"), 2378154706);
        assert_eq!(name_hash(b"@var"), 1171531964);
        assert_eq!(name_hash(b"emptydir"), 286931479);
        assert_eq!(extent_data_ref_hash(5, 261, 0), 0x0dfb_591f_6e09_3901);
    }
}

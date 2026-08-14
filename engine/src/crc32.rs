//! CRC-32/ISO-HDLC (the zlib/PNG/GPT/xz polynomial, reflected 0xEDB88320) —
//! pure `std`, the one copy shared by every td format that needs it.
//!
//! Two unrelated on-disk formats in this tree want the same checksum: the xz
//! decoder verifies stream/index/block CRCs with it, and the GPT writer
//! computes its header and partition-array CRCs with it. A second private copy
//! is exactly what the engine crate exists to prevent.
//!
//! The table is built once behind a `OnceLock` rather than per call: the xz
//! decoder checksums whole decompressed blocks, so the table is on that hot
//! loop's path and a fresh 1 KiB of it per call was pure waste.

use std::sync::OnceLock;

/// CRC-32 of `input`, initial value all-ones and final complement — the form
/// GPT §5.3 and xz both specify.
pub fn crc32(input: &[u8]) -> u32 {
    let table = table();
    let mut crc = 0xffff_ffffu32;
    for b in input {
        // Masked to a byte, so the lookup is always in range.
        let idx = ((crc ^ u32::from(*b)) & 0xff) as usize;
        crc = (crc >> 8) ^ table.get(idx).copied().unwrap_or(0);
    }
    !crc
}

fn table() -> &'static [u32; 256] {
    static TABLE: OnceLock<[u32; 256]> = OnceLock::new();
    TABLE.get_or_init(|| {
        let mut table = [0u32; 256];
        for (i, slot) in table.iter_mut().enumerate() {
            let mut crc = u32::try_from(i).unwrap_or(0);
            for _ in 0..8 {
                if crc & 1 == 0 {
                    crc >>= 1;
                } else {
                    crc = (crc >> 1) ^ 0xedb8_8320;
                }
            }
            *slot = crc;
        }
        table
    })
}

#[cfg(test)]
mod tests {
    use super::crc32;

    /// The check value every CRC-32/ISO-HDLC catalogue entry publishes.
    #[test]
    fn matches_the_published_check_value() {
        assert_eq!(crc32(b"123456789"), 0xcbf4_3926);
    }

    #[test]
    fn empty_input_is_zero() {
        assert_eq!(crc32(b""), 0);
    }

    /// Known zlib/PNG vectors, which pin the reflection and the final
    /// complement together — a mirrored-but-uncomplemented variant agrees with
    /// neither.
    #[test]
    fn matches_known_vectors() {
        assert_eq!(crc32(b"a"), 0xe8b7_be43);
        assert_eq!(crc32(b"abc"), 0x3524_41c2);
        assert_eq!(crc32(&[0u8; 32]), 0x190a_55ad);
        assert_eq!(crc32(&[0xffu8; 32]), 0xff6c_ab0b);
    }

    /// A buffer long enough to walk the whole table, against zlib's answer for
    /// the same bytes — the short vectors above exercise only a handful of the
    /// 256 entries, so a table wrong in its tail passes all of them.
    #[test]
    fn a_long_buffer_matches_zlib() {
        let long: Vec<u8> = (0..=255u8).cycle().take(4096).collect();
        assert_eq!(crc32(&long), 0xa291_2082);
        // Flipping one bit in the middle must change it — and to zlib's value
        // for THAT buffer, not merely to something else.
        let mut flipped = long;
        if let Some(b) = flipped.get_mut(2000) {
            *b ^= 1;
        }
        assert_eq!(crc32(&flipped), 0x26b8_801b);
    }
}

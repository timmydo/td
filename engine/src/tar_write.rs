//! A streaming ustar writer: headers in the classic octal form, GNU base-256
//! where a number overflows its field, and GNU `@LongLink` records for a name or
//! link target past the 100 bytes a header holds. `oci` packs deterministic
//! layers with it and td-util's `tar -c` packs trees; `tar_extract` reads both.

use std::io::{self, Write};

pub const BLOCK: usize = 512;
pub const REGULAR: u8 = b'0';
pub const SYMLINK: u8 = b'2';
pub const DIRECTORY: u8 = b'5';

const NAME_FIELD: usize = 100;
const LONG_LINK: &[u8] = b"././@LongLink";
const ZEROS: [u8; BLOCK] = [0; BLOCK];

/// One member's header. The body, `size` bytes, is the caller's to write, then
/// `pad` after it.
pub struct Header<'a> {
    pub name: &'a [u8],
    pub typeflag: u8,
    /// Only the permission bits are kept.
    pub mode: u32,
    pub uid: u64,
    pub gid: u64,
    pub size: u64,
    pub mtime: u64,
    pub link: &'a [u8],
}

/// The member's header, after a long-link record for a link target, then for a
/// name, that its fields cannot hold.
pub fn write_header<W: Write>(out: &mut W, header: &Header<'_>) -> io::Result<()> {
    if header.link.len() > NAME_FIELD {
        long_link(out, b'K', header.link, header.mtime)?;
    }
    if header.name.len() > NAME_FIELD {
        long_link(out, b'L', header.name, header.mtime)?;
    }
    out.write_all(&block(header))
}

/// Zeros to the end of the block that a body of `size` bytes ends in.
pub fn pad<W: Write>(out: &mut W, size: u64) -> io::Result<()> {
    let used = (size % BLOCK as u64) as usize;
    if used == 0 {
        return Ok(());
    }
    out.write_all(ZEROS.get(used..).unwrap_or_default())
}

/// The two zero blocks that end an archive.
pub fn finish<W: Write>(out: &mut W) -> io::Result<()> {
    out.write_all(&ZEROS)?;
    out.write_all(&ZEROS)
}

fn long_link<W: Write>(out: &mut W, kind: u8, text: &[u8], mtime: u64) -> io::Result<()> {
    let size = text.len() as u64 + 1;
    let record = Header {
        name: LONG_LINK,
        typeflag: kind,
        mode: 0o644,
        uid: 0,
        gid: 0,
        size,
        mtime,
        link: b"",
    };
    out.write_all(&block(&record))?;
    out.write_all(text)?;
    out.write_all(&[0])?;
    pad(out, size)
}

fn field(header: &mut [u8; BLOCK], start: usize, len: usize) -> &mut [u8] {
    header.get_mut(start..start + len).unwrap_or_default()
}

fn put(header: &mut [u8; BLOCK], start: usize, len: usize, bytes: &[u8]) {
    for (slot, byte) in field(header, start, len).iter_mut().zip(bytes) {
        *slot = *byte;
    }
}

fn block(header: &Header<'_>) -> [u8; BLOCK] {
    let mut h = [0u8; BLOCK];
    put(&mut h, 0, NAME_FIELD, field_prefix(header.name));
    numeric(field(&mut h, 100, 8), u64::from(header.mode & 0o7777));
    numeric(field(&mut h, 108, 8), header.uid);
    numeric(field(&mut h, 116, 8), header.gid);
    numeric(field(&mut h, 124, 12), header.size);
    numeric(field(&mut h, 136, 12), header.mtime);
    put(&mut h, 156, 1, &[header.typeflag]);
    put(&mut h, 157, NAME_FIELD, field_prefix(header.link));
    put(&mut h, 257, 6, b"ustar\0");
    put(&mut h, 263, 2, b"00");
    // The checksum is taken with its own field as spaces, then written as six
    // octal digits, a NUL and a space.
    field(&mut h, 148, 8).fill(b' ');
    let sum: u32 = h.iter().map(|b| u32::from(*b)).sum();
    put(&mut h, 148, 8, format!("{sum:06o}\0 ").as_bytes());
    h
}

/// What a name or target field holds of one its record carries whole: the first
/// 100 bytes, short of a UTF-8 character they would split, since a reader may
/// decode the field before it applies the record, as `tar_extract` does.
fn field_prefix(text: &[u8]) -> &[u8] {
    let Some(prefix) = text.get(..NAME_FIELD) else {
        return text;
    };
    match std::str::from_utf8(prefix) {
        Err(error) if error.error_len().is_none() => {
            prefix.get(..error.valid_up_to()).unwrap_or(prefix)
        }
        _ => prefix,
    }
}

/// A numeric field: zero-padded octal and a NUL where it fits, and otherwise GNU
/// base-256, the value big-endian across the field with the top bit of its first
/// byte set, which GNU tar and Go's archive/tar both read.
fn numeric(field: &mut [u8], value: u64) {
    let digits = field.len().saturating_sub(1);
    let fits = digits >= 22 || u128::from(value) < (1u128 << (3 * digits));
    if fits {
        let text = format!("{value:0digits$o}");
        for (slot, digit) in field.iter_mut().zip(text.bytes().chain([0])) {
            *slot = digit;
        }
        return;
    }
    let mut rest = value;
    for slot in field.iter_mut().rev() {
        *slot = (rest & 0xff) as u8;
        rest >>= 8;
    }
    if let Some(first) = field.first_mut() {
        *first |= 0x80;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Mounted into td-util too, whose lints reach test code: so no unwrap and no
    // indexing, and no `allow` in an engine file to permit them.
    type Outcome = Result<(), Box<dyn std::error::Error>>;

    fn header<'a>(name: &'a [u8], link: &'a [u8], size: u64) -> Header<'a> {
        Header {
            name,
            typeflag: REGULAR,
            mode: 0o104755,
            uid: 1000,
            gid: 100,
            size,
            mtime: 1_700_000_000,
            link,
        }
    }

    fn at(bytes: &[u8], start: usize, len: usize) -> &[u8] {
        bytes.get(start..start + len).unwrap_or_default()
    }

    #[test]
    fn numbers_are_octal_until_they_overflow_and_then_base_256() {
        let mut small = [0xffu8; 12];
        numeric(&mut small, 0o777);
        assert_eq!(&small, b"00000000777\0");
        let mut largest = [0u8; 12];
        numeric(&mut largest, (1 << 33) - 1);
        assert_eq!(&largest, b"77777777777\0");
        let mut big = [0u8; 12];
        numeric(&mut big, 1 << 33);
        assert_eq!(big, [0x80, 0, 0, 0, 0, 0, 0, 2, 0, 0, 0, 0]);
        let mut eight = [0u8; 8];
        numeric(&mut eight, 1 << 21);
        assert_eq!(eight, [0x80, 0, 0, 0, 0, 0x20, 0, 0]);
    }

    #[test]
    fn a_header_is_one_checksummed_block_with_permission_bits_only() -> Outcome {
        let mut out = Vec::new();
        write_header(&mut out, &header(b"dir/file", b"", 5))?;
        assert_eq!(out.len(), BLOCK);
        assert_eq!(at(&out, 0, 9), b"dir/file\0");
        assert_eq!(at(&out, 100, 8), b"0004755\0");
        assert_eq!(at(&out, 108, 8), b"0001750\0");
        assert_eq!(at(&out, 257, 8), b"ustar\x0000");
        let stored = std::str::from_utf8(at(&out, 148, 6))?;
        let mut blank = out.clone();
        blank
            .get_mut(148..156)
            .ok_or("no checksum field")?
            .fill(b' ');
        let sum: u32 = blank.iter().map(|b| u32::from(*b)).sum();
        assert_eq!(u32::from_str_radix(stored, 8)?, sum);
        assert_eq!(at(&out, 154, 2), b"\0 ");
        Ok(())
    }

    #[test]
    fn long_names_and_targets_come_first_as_their_records() -> Outcome {
        let name = vec![b'n'; 150];
        let link = vec![b'l'; 101];
        let mut out = Vec::new();
        write_header(&mut out, &header(&name, &link, 0))?;
        // K, its 102 bytes padded, then L, its 151 padded, then the member.
        assert_eq!(out.len(), 5 * BLOCK);
        assert_eq!(at(&out, 0, 14), b"././@LongLink\0");
        assert_eq!(at(&out, 156, 1), b"K");
        assert_eq!(at(&out, BLOCK, 102), [&link[..], b"\0"].concat());
        assert_eq!(at(&out, 2 * BLOCK + 156, 1), b"L");
        assert_eq!(at(&out, 3 * BLOCK, 151), [&name[..], b"\0"].concat());
        assert_eq!(at(&out, 4 * BLOCK, 100), at(&name, 0, 100));
        assert_eq!(at(&out, 4 * BLOCK + 157, 100), at(&link, 0, 100));
        // A character the 100th byte would split is left to the record.
        let split = [&[b'a'; 99][..], "\u{e9}b".as_bytes()].concat();
        let mut out = Vec::new();
        write_header(&mut out, &header(&split, &split, 0))?;
        assert_eq!(out.len(), 5 * BLOCK);
        assert_eq!(at(&out, 4 * BLOCK, 100), [&[b'a'; 99][..], b"\0"].concat());
        assert_eq!(
            at(&out, 4 * BLOCK + 157, 100),
            [&[b'a'; 99][..], b"\0"].concat()
        );
        // Exactly 100 bytes fits the field and needs no record.
        let mut fits = Vec::new();
        write_header(&mut fits, &header(&[b'x'; 100], &[b'y'; 100], 0))?;
        assert_eq!(fits.len(), BLOCK);
        Ok(())
    }

    #[test]
    fn padding_ends_the_block_and_an_archive_ends_in_two() -> Outcome {
        for (size, zeros) in [(0, 0), (1, 511), (512, 0), (513, 511)] {
            let mut out = Vec::new();
            pad(&mut out, size)?;
            assert_eq!(out, vec![0; zeros], "{size}");
        }
        let mut out = Vec::new();
        finish(&mut out)?;
        assert_eq!(out, vec![0; 2 * BLOCK]);
        Ok(())
    }
}

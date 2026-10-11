//! The Authenticode SHA-256 of a PE/COFF image, as UEFI firmware measures a
//! boot application into PCR 4: Microsoft's PE/COFF hash, with EDK2's
//! treatment of the bytes after the last section. The selector's
//! prediction compares it with the logged application digest and puts a
//! staged `BOOTX64.NEW`'s in its place (td-install/ENCRYPTION.md
//! "Prediction"). OVMF's agreement is recorded over td's unsigned kernel;
//! the certificate-table path is tested on constructed images alone until
//! increment 8i records a signed image under OVMF.
use crate::sha256::Sha256;

/// An image larger than this is refused; td's kernel is a few tens of MiB.
pub const MAX_IMAGE_BYTES: usize = 256 << 20;
const MAX_SECTIONS: usize = 96;
const SECTION_HEADER: usize = 40;
/// The certificate table's index among the optional header's data
/// directories.
const SECURITY: u32 = 4;

fn bytes(image: &[u8], from: usize, to: usize) -> Result<&[u8], &'static str> {
    image.get(from..to).ok_or("the PE image is truncated")
}

fn u16_at(image: &[u8], at: usize) -> Result<u16, &'static str> {
    match bytes(
        image,
        at,
        at.checked_add(2).ok_or("the PE image is truncated")?,
    )? {
        &[a, b] => Ok(u16::from_le_bytes([a, b])),
        _ => Err("the PE image is truncated"),
    }
}

fn u32_at(image: &[u8], at: usize) -> Result<usize, &'static str> {
    match bytes(
        image,
        at,
        at.checked_add(4).ok_or("the PE image is truncated")?,
    )? {
        &[a, b, c, d] => usize::try_from(u32::from_le_bytes([a, b, c, d]))
            .map_err(|_| "a PE field exceeds the address space"),
        _ => Err("the PE image is truncated"),
    }
}

fn add(a: usize, b: usize) -> Result<usize, &'static str> {
    a.checked_add(b).ok_or("a PE offset overflows")
}

pub fn sha256(image: &[u8]) -> Result<[u8; 32], &'static str> {
    if image.len() > MAX_IMAGE_BYTES {
        return Err("the PE image is too large");
    }
    let pe = if u16_at(image, 0)? == 0x5a4d {
        u32_at(image, 0x3c)?
    } else {
        0
    };
    if bytes(image, pe, add(pe, 4)?)? != b"PE\0\0" {
        return Err("the image has no PE signature");
    }
    let file_header = add(pe, 4)?;
    let sections = usize::from(u16_at(image, add(file_header, 2)?)?);
    let optional_size = usize::from(u16_at(image, add(file_header, 16)?)?);
    let optional = add(file_header, 20)?;
    let (rva_count_at, directories) = match u16_at(image, optional)? {
        0x10b => (add(optional, 92)?, add(optional, 96)?),
        0x20b => (add(optional, 108)?, add(optional, 112)?),
        _ => return Err("the PE optional header's magic is neither PE32 nor PE32+"),
    };
    let checksum = add(optional, 64)?;
    let headers = u32_at(image, add(optional, 60)?)?;
    let rva_count = u32_at(image, rva_count_at)?;
    let optional_end = add(optional, optional_size)?;
    if add(rva_count_at, 4)? > optional_end {
        return Err("the PE optional header is short");
    }
    let after_checksum = add(checksum, 4)?;
    let mut hash = Sha256::new();
    hash.update(bytes(image, 0, checksum)?);
    let certificates = if rva_count <= SECURITY as usize {
        hash.update(bytes(image, after_checksum, headers)?);
        0
    } else {
        let entry = add(directories, SECURITY as usize * 8)?;
        let after_entry = add(entry, 8)?;
        if after_entry > optional_end {
            return Err("the PE optional header is short");
        }
        hash.update(bytes(image, after_checksum, entry)?);
        hash.update(bytes(image, after_entry, headers)?);
        u32_at(image, add(entry, 4)?)?
    };
    if sections > MAX_SECTIONS {
        return Err("the PE image has too many sections");
    }
    let mut table = Vec::with_capacity(sections);
    for index in 0..sections {
        let at = add(optional_end, index * SECTION_HEADER)?;
        table.push((u32_at(image, add(at, 20)?)?, u32_at(image, add(at, 16)?)?));
    }
    // Stable, as EDK2's insertion sort is.
    table.sort_by_key(|(pointer, _)| *pointer);
    let mut hashed = headers;
    for (pointer, size) in table {
        if size == 0 {
            continue;
        }
        hash.update(bytes(image, pointer, add(pointer, size)?)?);
        hashed = add(hashed, size)?;
    }
    if image.len() > hashed {
        let covered = add(hashed, certificates)?;
        if image.len() > covered {
            hash.update(bytes(image, hashed, image.len() - certificates)?);
        } else if image.len() < covered {
            return Err("the PE certificate table overruns the image");
        }
    }
    Ok(hash.finalize())
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod tests {
    use super::*;

    fn put16(image: &mut [u8], at: usize, value: u16) {
        image[at..at + 2].copy_from_slice(&value.to_le_bytes());
    }

    fn put32(image: &mut [u8], at: usize, value: u32) {
        image[at..at + 4].copy_from_slice(&value.to_le_bytes());
    }

    fn digest(parts: &[&[u8]]) -> [u8; 32] {
        let mut hash = Sha256::new();
        for part in parts {
            hash.update(part);
        }
        hash.finalize()
    }

    const PE: usize = 0x80;
    const OPTIONAL: usize = PE + 24;

    /// A PE32+ image: headers to 0x200, section `b` at 0x400 listed before
    /// section `a` at 0x200, then 0x100 bytes of trailing data and a 0x80
    /// byte certificate table.
    fn image() -> Vec<u8> {
        let mut image: Vec<u8> = (0..0x780u32).map(|i| (i * 7 + 3) as u8).collect();
        put16(&mut image, 0, 0x5a4d);
        put32(&mut image, 0x3c, PE as u32);
        image[PE..PE + 4].copy_from_slice(b"PE\0\0");
        put16(&mut image, PE + 6, 2);
        put16(&mut image, PE + 20, 240);
        put16(&mut image, OPTIONAL, 0x20b);
        put32(&mut image, OPTIONAL + 60, 0x200);
        put32(&mut image, OPTIONAL + 108, 16);
        put32(&mut image, OPTIONAL + 112 + 32, 0x700);
        put32(&mut image, OPTIONAL + 112 + 36, 0x80);
        let table = OPTIONAL + 240;
        for (index, (pointer, size)) in [(0x400u32, 0x200u32), (0x200, 0x200)].iter().enumerate() {
            let at = table + index * 40;
            put32(&mut image, at + 16, *size);
            put32(&mut image, at + 20, *pointer);
        }
        image
    }

    #[test]
    fn the_hash_covers_headers_sections_in_file_order_and_trailing_data() {
        let image = image();
        let checksum = OPTIONAL + 64;
        let entry = OPTIONAL + 112 + 32;
        let expected = digest(&[
            &image[..checksum],
            &image[checksum + 4..entry],
            &image[entry + 8..0x200],
            &image[0x200..0x400],
            &image[0x400..0x600],
            &image[0x600..0x700],
        ]);
        assert_eq!(sha256(&image), Ok(expected));
    }

    #[test]
    fn the_checksum_the_certificate_entry_and_table_are_left_out() {
        let base = sha256(&image()).unwrap();
        for at in [OPTIONAL + 64, OPTIONAL + 112 + 33, 0x700, 0x77f] {
            let mut changed = image();
            changed[at] ^= 0xff;
            assert_eq!(sha256(&changed), Ok(base), "{at:#x}");
        }
        for at in [0x2, OPTIONAL + 68, 0x1ff, 0x200, 0x5ff, 0x6ff] {
            let mut changed = image();
            changed[at] ^= 0xff;
            assert_ne!(sha256(&changed), Ok(base), "{at:#x}");
        }
    }

    #[test]
    fn pe32_and_a_missing_certificate_directory_are_hashed_too() {
        let mut image = image();
        put16(&mut image, OPTIONAL, 0x10b);
        put32(&mut image, OPTIONAL + 92, 16);
        let entry = OPTIONAL + 96 + 32;
        put32(&mut image, entry, 0x700);
        put32(&mut image, entry + 4, 0x80);
        let checksum = OPTIONAL + 64;
        let expected = digest(&[
            &image[..checksum],
            &image[checksum + 4..entry],
            &image[entry + 8..0x200],
            &image[0x200..0x600],
            &image[0x600..0x700],
        ]);
        assert_eq!(sha256(&image), Ok(expected));
        put32(&mut image, OPTIONAL + 92, 4);
        let expected = digest(&[&image[..checksum], &image[checksum + 4..]]);
        assert_eq!(sha256(&image), Ok(expected));
    }

    #[test]
    fn malformed_images_are_refused() {
        let mut no_signature = image();
        no_signature[PE] = b'X';
        let mut magic = image();
        put16(&mut magic, OPTIONAL, 0x107);
        let mut section = image();
        put32(&mut section, OPTIONAL + 240 + 16, 0x1000);
        let mut certificates = image();
        put32(&mut certificates, OPTIONAL + 112 + 36, 0x200);
        let mut headers = image();
        put32(&mut headers, OPTIONAL + 60, 0x10000);
        let mut short = image();
        put16(&mut short, PE + 20, 100);
        for (why, image) in [
            ("signature", no_signature),
            ("magic", magic),
            ("section", section),
            ("certificates", certificates),
            ("headers", headers),
            ("short optional header", short),
            ("truncated", image()[..0x150].to_vec()),
            ("empty", Vec::new()),
        ] {
            assert!(sha256(&image).is_err(), "{why}");
        }
    }
}

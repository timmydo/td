//! A NoCloud seed for `qemu-boot-server` (td-svc/DESIGN.md §10, "Instance
//! metadata"): an ISO 9660 volume labelled `cidata` whose root holds
//! `meta-data` and `user-data`, named by Rock Ridge as `cloud-localds` and
//! `genisoimage -volid cidata -rock` write it. The engine's installer writer
//! cannot: it labels every volume `TD_INSTALL`, carries an ESP and writes
//! no Rock Ridge.

use std::fs;
use std::path::Path;

const SECTOR: usize = 2048;
const PVD: usize = 16;
const L_PATH_TABLE: usize = 18;
const M_PATH_TABLE: usize = 19;
const ROOT: usize = 20;
const FIRST_FILE: usize = 21;
/// 2026-01-01 00:00:00 UTC, in a directory record's seven bytes.
const RECORD_DATE: [u8; 7] = [126, 1, 1, 0, 0, 0, 0];
const DESCRIPTOR_DATE: &[u8; 17] = b"2026010100000000\0";
const FILE_MODE: u32 = 0o100_444;
const DIRECTORY_MODE: u32 = 0o040_555;

/// The seed's bytes at `path`, which must not exist: `meta-data` and
/// `user-data` with the given text.
pub(super) fn write_seed(path: &Path, meta: &str, user: &str) -> Result<(), String> {
    let image = seed(&[
        ("METADATA.;1", "meta-data", meta.as_bytes()),
        ("USERDATA.;1", "user-data", user.as_bytes()),
    ])?;
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .and_then(|mut file| std::io::Write::write_all(&mut file, &image))
        .map_err(|error| format!("write the NoCloud seed {}: {error}", path.display()))
}

/// One root directory of `(ISO name, Rock Ridge name, bytes)` files, in
/// ISO-name order, each in its own run of sectors.
fn seed(files: &[(&str, &str, &[u8])]) -> Result<Vec<u8>, String> {
    let mut extents = Vec::new();
    let mut next = FIRST_FILE;
    for (_, _, bytes) in files {
        extents.push(next);
        next += bytes.len().div_ceil(SECTOR).max(1);
    }
    let total = next;
    let mut image = vec![0u8; total * SECTOR];
    let sector = |image: &mut Vec<u8>, index: usize, bytes: &[u8]| -> Result<(), String> {
        image
            .get_mut(index * SECTOR..index * SECTOR + bytes.len())
            .ok_or("a seed sector lies outside the image")?
            .copy_from_slice(bytes);
        Ok(())
    };

    let mut pvd = vec![0u8; SECTOR];
    put(&mut pvd, 0, b"\x01CD001\x01")?;
    put(&mut pvd, 8, &padded(b"LINUX", 32))?;
    put(&mut pvd, 40, &padded(b"cidata", 32))?;
    put(&mut pvd, 80, &both32(total))?;
    put(&mut pvd, 120, &both16(1))?;
    put(&mut pvd, 124, &both16(1))?;
    put(&mut pvd, 128, &both16(SECTOR))?;
    put(&mut pvd, 132, &both32(10))?;
    put(
        &mut pvd,
        140,
        &u32::try_from(L_PATH_TABLE)
            .map_err(|e| e.to_string())?
            .to_le_bytes(),
    )?;
    put(
        &mut pvd,
        148,
        &u32::try_from(M_PATH_TABLE)
            .map_err(|e| e.to_string())?
            .to_be_bytes(),
    )?;
    put(&mut pvd, 156, &record(&[0], ROOT, SECTOR, true, &[])?)?;
    for (offset, length) in [(190, 128), (318, 128), (446, 128), (574, 128)] {
        put(&mut pvd, offset, &padded(b"", length))?;
    }
    for (offset, length) in [(702, 37), (739, 37), (776, 37)] {
        put(&mut pvd, offset, &padded(b"", length))?;
    }
    for offset in [813, 830, 847, 864] {
        put(&mut pvd, offset, DESCRIPTOR_DATE)?;
    }
    put(&mut pvd, 881, &[1])?;
    sector(&mut image, PVD, &pvd)?;
    sector(&mut image, PVD + 1, b"\xffCD001\x01")?;

    let root = u32::try_from(ROOT).map_err(|error| error.to_string())?;
    let mut l_table = vec![1, 0];
    l_table.extend_from_slice(&root.to_le_bytes());
    l_table.extend_from_slice(&[1, 0, 0, 0]);
    let mut m_table = vec![1, 0];
    m_table.extend_from_slice(&root.to_be_bytes());
    m_table.extend_from_slice(&[0, 1, 0, 0]);
    sector(&mut image, L_PATH_TABLE, &l_table)?;
    sector(&mut image, M_PATH_TABLE, &m_table)?;

    // "." carries SUSP's SP and the RRIP ER that announce Rock Ridge.
    let mut dot_use = vec![b'S', b'P', 7, 1, 0xbe, 0xef, 0];
    let id = b"RRIP_1991A";
    dot_use.extend_from_slice(&[b'E', b'R', 8 + id.len() as u8, 1, id.len() as u8, 0, 0, 1]);
    dot_use.extend_from_slice(id);
    dot_use.extend_from_slice(&px(DIRECTORY_MODE, 2));
    let mut directory = record(&[0], ROOT, SECTOR, true, &dot_use)?;
    directory.extend(record(&[1], ROOT, SECTOR, true, &px(DIRECTORY_MODE, 2))?);
    for ((iso, name, bytes), extent) in files.iter().zip(&extents) {
        let mut system_use = px(FILE_MODE, 1);
        let name_len = u8::try_from(5 + name.len()).map_err(|error| error.to_string())?;
        system_use.extend_from_slice(&[b'N', b'M', name_len, 1, 0]);
        system_use.extend_from_slice(name.as_bytes());
        directory.extend(record(
            iso.as_bytes(),
            *extent,
            bytes.len(),
            false,
            &system_use,
        )?);
        sector(&mut image, *extent, bytes)?;
    }
    if directory.len() > SECTOR {
        return Err("the seed's root directory outgrew one sector".into());
    }
    sector(&mut image, ROOT, &directory)?;
    Ok(image)
}

fn put(buffer: &mut [u8], offset: usize, bytes: &[u8]) -> Result<(), String> {
    buffer
        .get_mut(offset..offset + bytes.len())
        .ok_or("a seed field lies outside its descriptor")?
        .copy_from_slice(bytes);
    Ok(())
}

fn padded(text: &[u8], length: usize) -> Vec<u8> {
    let mut field = text.to_vec();
    field.resize(length, b' ');
    field
}

fn both16(value: usize) -> [u8; 4] {
    let [low, high] = (value as u16).to_le_bytes();
    [low, high, high, low]
}

fn both32(value: usize) -> [u8; 8] {
    let [a, b, c, d] = (value as u32).to_le_bytes();
    [a, b, c, d, d, c, b, a]
}

/// Rock Ridge's POSIX attributes: mode, links, root's UID and GID.
fn px(mode: u32, links: u32) -> Vec<u8> {
    let mut entry = vec![b'P', b'X', 36, 1];
    for value in [mode, links, 0, 0] {
        entry.extend_from_slice(&both32(value as usize));
    }
    entry
}

/// One directory record, padded to even length after its name, then its
/// system use area.
fn record(
    name: &[u8],
    extent: usize,
    size: usize,
    directory: bool,
    system_use: &[u8],
) -> Result<Vec<u8>, String> {
    let mut entry = vec![0u8, 0];
    entry.extend_from_slice(&both32(extent));
    entry.extend_from_slice(&both32(size));
    entry.extend_from_slice(&RECORD_DATE);
    entry.extend_from_slice(&[u8::from(directory) * 2, 0, 0]);
    entry.extend_from_slice(&both16(1));
    entry.push(u8::try_from(name.len()).map_err(|error| error.to_string())?);
    entry.extend_from_slice(name);
    if !entry.len().is_multiple_of(2) {
        entry.push(0);
    }
    entry.extend_from_slice(system_use);
    if !entry.len().is_multiple_of(2) {
        entry.push(0);
    }
    let length = u8::try_from(entry.len()).map_err(|_| "a seed directory record is too long")?;
    if let Some(first) = entry.first_mut() {
        *first = length;
    }
    Ok(entry)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]
    use super::*;

    /// What td-firstboot's probe reads, and what Linux's isofs walks to the
    /// two Rock Ridge names.
    #[test]
    fn the_seed_is_a_cidata_volume_naming_both_files() {
        let image = seed(&[
            ("METADATA.;1", "meta-data", b"local-hostname: a\n"),
            ("USERDATA.;1", "user-data", &[b'x'; 3000]),
        ])
        .unwrap();
        // Two files, the second two sectors.
        assert_eq!(image.len(), (FIRST_FILE + 3) * SECTOR);
        let pvd = &image[PVD * SECTOR..(PVD + 1) * SECTOR];
        assert_eq!(&pvd[..7], b"\x01CD001\x01");
        assert_eq!(&pvd[40..72], &padded(b"cidata", 32)[..]);
        assert_eq!(&pvd[80..88], &both32(FIRST_FILE + 3));
        assert_eq!(
            &image[(PVD + 1) * SECTOR..(PVD + 1) * SECTOR + 7],
            b"\xffCD001\x01"
        );
        // The root record names sector 20.
        assert_eq!(&pvd[156 + 2..156 + 10], &both32(ROOT));
        let root = &image[ROOT * SECTOR..(ROOT + 1) * SECTOR];
        let mut at = 0;
        let mut names = Vec::new();
        while root[at] != 0 {
            let length = usize::from(root[at]);
            assert_eq!(length % 2, 0);
            let record = &root[at..at + length];
            let name_len = usize::from(record[32]);
            let mut use_at = 33 + name_len + (33 + name_len) % 2;
            if at == 0 {
                assert_eq!(
                    &record[use_at..use_at + 7],
                    &[b'S', b'P', 7, 1, 0xbe, 0xef, 0]
                );
            }
            while use_at + 4 <= length {
                let entry_len = usize::from(record[use_at + 2]);
                if entry_len < 4 {
                    break;
                }
                if &record[use_at..use_at + 2] == b"NM" {
                    names.push(
                        String::from_utf8(record[use_at + 5..use_at + entry_len].to_vec()).unwrap(),
                    );
                    let extent = u32::from_le_bytes(record[2..6].try_into().unwrap()) as usize;
                    let size = u32::from_le_bytes(record[10..14].try_into().unwrap()) as usize;
                    let body = &image[extent * SECTOR..extent * SECTOR + size];
                    assert!(body == b"local-hostname: a\n" || body == [b'x'; 3000]);
                }
                use_at += entry_len;
            }
            at += length;
        }
        assert_eq!(names, ["meta-data", "user-data"]);
    }
}

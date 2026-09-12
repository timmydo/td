//! Deterministic flat ISO-9660 level-3 media with an El Torito/GPT shared ESP.
//! Returns metadata and stream placements, never file contents or a whole image.
//! The caller writes these extents and all streams onto fresh zeroed space.
//! ISO sectors are 2048 bytes; GPT/FAT sectors are 512 bytes. Firmware uses
//! El Torito on optical media and GPT when the same bytes are flashed to USB.
use crate::gpt;

pub const BLOCK: u64 = 2048;
pub const ESP_OFFSET: u64 = 1024 * 1024;
const ROOT_BLOCK: u32 = 22;
const MAX_FILE: u64 = 64 * 1024 * 1024 * 1024;
const SECTION: u64 = 0xffff_f800;

#[derive(Clone, Debug)]
pub struct FileSpec {
    /// Uppercase ASCII root name, at most 31 bytes, without a version suffix.
    pub name: String,
    pub len: u64,
}

#[derive(Clone, Debug)]
pub struct Volume {
    pub disk_guid: gpt::Guid,
    pub esp_guid: gpt::Guid,
    /// Caller supplies a FAT32 image of this size at `Image::esp_offset`.
    pub esp_bytes: u64,
    pub files: Vec<FileSpec>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Extent {
    pub offset: u64,
    pub bytes: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Placement {
    pub name: String,
    pub offset: u64,
    pub len: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Image {
    pub total_bytes: u64,
    pub esp_offset: u64,
    pub extents: Vec<Extent>,
    pub placements: Vec<Placement>,
}

fn put(out: &mut [u8], at: usize, value: &[u8]) -> Result<(), String> {
    let end = at
        .checked_add(value.len())
        .ok_or("ISO field offset overflow")?;
    out.get_mut(at..end)
        .ok_or("ISO field outside record")?
        .copy_from_slice(value);
    Ok(())
}

fn both16(out: &mut [u8], at: usize, value: u16) -> Result<(), String> {
    put(out, at, &value.to_le_bytes())?;
    put(out, at + 2, &value.to_be_bytes())
}

fn both32(out: &mut [u8], at: usize, value: u32) -> Result<(), String> {
    put(out, at, &value.to_le_bytes())?;
    put(out, at + 4, &value.to_be_bytes())
}

fn block(offset: u64) -> Result<u32, String> {
    if !offset.is_multiple_of(BLOCK) {
        return Err("unaligned ISO extent".into());
    }
    u32::try_from(offset / BLOCK).map_err(|_| "ISO volume exceeds 32-bit block addressing".into())
}

fn align(value: u64, unit: u64) -> Result<u64, String> {
    value
        .checked_add(unit - 1)
        .map(|n| n / unit * unit)
        .ok_or_else(|| "ISO length overflow".into())
}

fn identifier(name: &str) -> Result<String, String> {
    let mut dots = 0;
    if name.is_empty()
        || name.len() > 31
        || name == "."
        || name.bytes().any(|b| {
            if b == b'.' {
                dots += 1;
                false
            } else {
                !b.is_ascii_uppercase() && !b.is_ascii_digit() && b != b'_'
            }
        })
        || dots > 1
        || name.len() + usize::from(dots == 0) > 31
    {
        return Err(format!("unsupported ISO root filename {name:?}"));
    }
    Ok(format!("{name}{};1", if dots == 0 { "." } else { "" }))
}

fn identifier_order(a: &str, b: &str) -> std::cmp::Ordering {
    // With version fixed at one, separators sort like right-hand filler:
    // a shorter name or extension precedes every permitted d-character.
    let weight = |byte| match byte {
        b';' => 0,
        b'.' => 1,
        other => other,
    };
    a.bytes().map(weight).cmp(b.bytes().map(weight))
}

fn record(id: &[u8], offset: u64, len: u32, flags: u8) -> Result<Vec<u8>, String> {
    let size = 33 + id.len() + usize::from(id.len().is_multiple_of(2));
    let mut out = vec![0; size];
    put(
        &mut out,
        0,
        &[u8::try_from(size).map_err(|_| "ISO identifier too long")?],
    )?;
    both32(&mut out, 2, block(offset)?)?;
    both32(&mut out, 10, len)?;
    put(&mut out, 18, &[70, 1, 1, 0, 0, 0, 0])?;
    put(&mut out, 25, &[flags])?;
    both16(&mut out, 28, 1)?;
    put(
        &mut out,
        32,
        &[u8::try_from(id.len()).map_err(|_| "ISO identifier too long")?],
    )?;
    put(&mut out, 33, id)?;
    Ok(out)
}

fn append_record(out: &mut Vec<u8>, row: &[u8]) {
    let remaining = BLOCK as usize - out.len() % BLOCK as usize;
    if row.len() > remaining {
        out.resize(out.len() + remaining, 0);
    }
    out.extend_from_slice(row);
}

fn file_records(out: &mut Vec<u8>, id: &str, offset: u64, len: u64) -> Result<(), String> {
    let mut remaining = len;
    let mut position = offset;
    loop {
        let size = remaining.min(SECTION);
        let more = remaining > size;
        append_record(
            out,
            &record(
                id.as_bytes(),
                position,
                size as u32,
                if more { 0x80 } else { 0 },
            )?,
        );
        if !more {
            return Ok(());
        }
        position = position
            .checked_add(size)
            .ok_or("ISO file offset overflow")?;
        remaining -= size;
    }
}

fn descriptor(kind: u8) -> Result<Vec<u8>, String> {
    let mut out = vec![0; BLOCK as usize];
    put(&mut out, 0, &[kind])?;
    put(&mut out, 1, b"CD001\x01")?;
    Ok(out)
}

/// Build the metadata around an ESP and up to 64 flat payload files. Files
/// larger than a 32-bit section use adjacent level-3 multi-extent records.
/// Names are sorted; duplicate normalized identifiers and reserved names fail.
pub fn build(volume: &Volume) -> Result<Image, String> {
    if volume.disk_guid == volume.esp_guid {
        return Err("ISO disk and ESP GUIDs must differ".into());
    }
    if volume.files.len() > 64
        || !(64 * 1024 * 1024..=512 * 1024 * 1024).contains(&volume.esp_bytes)
        || !volume.esp_bytes.is_multiple_of(ESP_OFFSET)
    {
        return Err("ISO needs at most 64 files and a 64–512 MiB ESP aligned to 1 MiB".into());
    }
    let mut files = Vec::with_capacity(volume.files.len());
    for file in &volume.files {
        if file.len > MAX_FILE {
            return Err(format!("ISO file {} exceeds 64 GiB", file.name));
        }
        let id = identifier(&file.name)?;
        if id == "EFI.IMG;1" || id == "BOOT.CAT;1" {
            return Err(format!("reserved ISO filename {}", file.name));
        }
        files.push((id, file));
    }
    files.sort_by(|a, b| identifier_order(&a.0, &b.0));
    if files.windows(2).any(|pair| {
        pair.first()
            .zip(pair.get(1))
            .is_some_and(|(a, b)| a.0 == b.0)
    }) {
        return Err("duplicate ISO filename".into());
    }
    let mut position = ESP_OFFSET + volume.esp_bytes;
    let mut placements = Vec::with_capacity(files.len());
    let mut rows = vec![
        ("BOOT.CAT;1".to_string(), 19 * BLOCK, BLOCK),
        ("EFI.IMG;1".to_string(), ESP_OFFSET, volume.esp_bytes),
    ];
    for (id, file) in files {
        placements.push(Placement {
            name: file.name.clone(),
            offset: position,
            len: file.len,
        });
        rows.push((id, position, file.len));
        position = position
            .checked_add(align(file.len, BLOCK)?)
            .ok_or("ISO payload length overflow")?;
    }
    // GPT's backup gets private space after the payload and inside the volume.
    let total_bytes = align(
        position
            .checked_add(34 * 512)
            .ok_or("ISO backup offset overflow")?,
        ESP_OFFSET,
    )?;
    let total_blocks = block(total_bytes)?;
    rows.sort_by(|a, b| identifier_order(&a.0, &b.0));
    let mut root = Vec::new();
    append_record(
        &mut root,
        &record(&[0], u64::from(ROOT_BLOCK) * BLOCK, 0, 2)?,
    );
    append_record(
        &mut root,
        &record(&[1], u64::from(ROOT_BLOCK) * BLOCK, 0, 2)?,
    );
    for (id, offset, len) in rows {
        file_records(&mut root, &id, offset, len)?;
    }
    let root_size = align(root.len() as u64, BLOCK)?;
    if u64::from(ROOT_BLOCK) * BLOCK + root_size > ESP_OFFSET {
        return Err("ISO directory overlaps ESP".into());
    }
    root.resize(
        usize::try_from(root_size).map_err(|_| "ISO directory too large")?,
        0,
    );
    both32(&mut root, 10, root_size as u32)?;
    both32(&mut root, 34 + 10, root_size as u32)?;

    let mut pvd = descriptor(1)?;
    put(&mut pvd, 8, &[b' '; 64])?;
    put(&mut pvd, 8, b"TD")?;
    put(&mut pvd, 40, b"TD_INSTALL")?;
    both32(&mut pvd, 80, total_blocks)?;
    both16(&mut pvd, 120, 1)?;
    both16(&mut pvd, 124, 1)?;
    both16(&mut pvd, 128, BLOCK as u16)?;
    both32(&mut pvd, 132, 10)?;
    put(&mut pvd, 140, &20u32.to_le_bytes())?;
    put(&mut pvd, 148, &21u32.to_be_bytes())?;
    put(
        &mut pvd,
        156,
        &record(&[0], u64::from(ROOT_BLOCK) * BLOCK, root_size as u32, 2)?,
    )?;
    put(&mut pvd, 190, &[b' '; 623])?;
    for at in [813, 830, 847, 864] {
        put(&mut pvd, at, b"0000000000000000\0")?;
    }
    put(&mut pvd, 881, &[1])?;
    let mut boot = descriptor(0)?;
    put(&mut boot, 7, b"EL TORITO SPECIFICATION")?;
    put(&mut boot, 71, &19u32.to_le_bytes())?;
    let mut catalog = vec![0; BLOCK as usize];
    put(&mut catalog, 0, &[1, 0xef])?;
    put(&mut catalog, 30, &[0x55, 0xaa])?;
    let mut sum = 0u16;
    for &pair in catalog.get(..32).ok_or("short catalog")?.as_chunks::<2>().0 {
        sum = sum.wrapping_add(u16::from_le_bytes(pair));
    }
    put(&mut catalog, 28, &0u16.wrapping_sub(sum).to_le_bytes())?;
    put(&mut catalog, 32, &[0x88, 0])?;
    // UEFI 2.10 §13.3.2.1: one means from the boot image to the disc end;
    // the FAT BPB still declares its exact, smaller filesystem size.
    put(&mut catalog, 38, &1u16.to_le_bytes())?;
    put(&mut catalog, 40, &block(ESP_OFFSET)?.to_le_bytes())?;
    let mut little_path = vec![0; BLOCK as usize];
    let mut big_path = vec![0; BLOCK as usize];
    for path in [&mut little_path, &mut big_path] {
        put(path, 0, &[1])?;
    }
    put(&mut little_path, 2, &ROOT_BLOCK.to_le_bytes())?;
    put(&mut little_path, 6, &1u16.to_le_bytes())?;
    put(&mut big_path, 2, &ROOT_BLOCK.to_be_bytes())?;
    put(&mut big_path, 6, &1u16.to_be_bytes())?;
    let gpt = gpt::build(&gpt::Layout {
        sector_size: 512,
        disk_sectors: total_bytes / 512,
        disk_guid: volume.disk_guid,
        align_sectors: ESP_OFFSET / 512,
        partitions: vec![gpt::Partition {
            type_guid: gpt::TYPE_ESP,
            unique_guid: volume.esp_guid,
            start_lba: ESP_OFFSET / 512,
            end_lba: (ESP_OFFSET + volume.esp_bytes) / 512 - 1,
            attributes: 0,
            name: "td installer ESP".into(),
        }],
    })?;
    let mut extents = vec![Extent {
        offset: gpt.primary_offset,
        bytes: gpt.primary,
    }];
    for (lba, bytes) in [
        (16, pvd),
        (17, boot),
        (18, descriptor(255)?),
        (19, catalog),
        (20, little_path),
        (21, big_path),
        (ROOT_BLOCK, root),
    ] {
        extents.push(Extent {
            offset: u64::from(lba) * BLOCK,
            bytes,
        });
    }
    extents.push(Extent {
        offset: gpt.backup_offset,
        bytes: gpt.backup,
    });
    Ok(Image {
        total_bytes,
        esp_offset: ESP_OFFSET,
        extents,
        placements,
    })
}

#[cfg(test)]
#[allow(clippy::indexing_slicing, clippy::unwrap_used)]
mod tests {
    use super::*;

    fn volume(files: &[(&str, u64)]) -> Volume {
        Volume {
            disk_guid: gpt::Guid([1; 16]),
            esp_guid: gpt::Guid([2; 16]),
            esp_bytes: 64 * 1024 * 1024,
            files: files
                .iter()
                .map(|(name, len)| FileSpec {
                    name: name.to_string(),
                    len: *len,
                })
                .collect(),
        }
    }

    fn at(image: &Image, offset: u64) -> &[u8] {
        &image
            .extents
            .iter()
            .find(|e| e.offset == offset)
            .unwrap()
            .bytes
    }

    fn le32(data: &[u8], at: usize) -> u32 {
        u32::from_le_bytes(data[at..at + 4].try_into().unwrap())
    }

    #[test]
    fn optical_catalog_and_gpt_name_the_same_esp() {
        let spec = volume(&[("ROOT.ERO", 3000)]);
        let image = build(&spec).unwrap();
        let last = image.extents.last().unwrap();
        let table = gpt::parse(at(&image, 0), &last.bytes, 512).unwrap();
        let esp = &table.partitions[0];
        assert_eq!(esp.type_guid, gpt::TYPE_ESP);
        assert_eq!(esp.start_lba * 512, image.esp_offset);
        assert_eq!((esp.end_lba - esp.start_lba + 1) * 512, spec.esp_bytes);
        assert_eq!(last.offset + last.bytes.len() as u64, image.total_bytes);
        let boot = at(&image, 17 * BLOCK);
        assert_eq!(&boot[..7], b"\0CD001\x01");
        assert_eq!(le32(boot, 71), 19);
        let catalog = at(&image, 19 * BLOCK);
        let checksum = catalog[..32].chunks_exact(2).fold(0u16, |n, p| {
            n.wrapping_add(u16::from_le_bytes(p.try_into().unwrap()))
        });
        assert_eq!(checksum, 0);
        assert_eq!(&catalog[..2], &[1, 0xef]);
        assert_eq!(&catalog[30..34], &[0x55, 0xaa, 0x88, 0]);
        assert_eq!(&catalog[38..40], &[1, 0]);
        assert_eq!(u64::from(le32(catalog, 40)) * BLOCK, image.esp_offset);
        let pvd = at(&image, 16 * BLOCK);
        assert_eq!(&pvd[..7], b"\x01CD001\x01");
        assert_eq!(u64::from(le32(pvd, 80)) * BLOCK, image.total_bytes);
        assert_eq!(
            u32::from_be_bytes(pvd[84..88].try_into().unwrap()),
            le32(pvd, 80)
        );
        assert_eq!(&at(&image, 18 * BLOCK)[..7], b"\xffCD001\x01");
    }

    #[test]
    fn large_payload_sections_keep_every_byte_and_stay_in_one_stream() {
        let length = 5 * 1024 * 1024 * 1024 + 123;
        let image = build(&volume(&[("ROOT.ERO", length)])).unwrap();
        let placement = &image.placements[0];
        assert_eq!(placement.len, length);
        let root = at(&image, u64::from(ROOT_BLOCK) * BLOCK);
        let mut rows = Vec::new();
        let mut position = 0;
        while position < root.len() {
            let size = root[position] as usize;
            if size == 0 {
                position = (position / BLOCK as usize + 1) * BLOCK as usize;
                continue;
            }
            assert!(position % BLOCK as usize + size <= BLOCK as usize);
            let row = &root[position..position + size];
            if row.get(33..33 + row[32] as usize) == Some(b"ROOT.ERO;1".as_slice()) {
                rows.push((
                    u64::from(le32(row, 2)) * BLOCK,
                    u64::from(le32(row, 10)),
                    row[25],
                ));
            }
            position += size;
        }
        assert_eq!(
            rows,
            vec![
                (placement.offset, SECTION, 0x80),
                (placement.offset + SECTION, length - SECTION, 0)
            ]
        );
        assert_eq!(le32(root, 10) as usize, root.len());
        assert_eq!(le32(root, 44) as usize, root.len());
        assert!(image.extents.iter().map(|e| e.bytes.len()).sum::<usize>() < 128 * 1024);
    }

    #[test]
    fn output_is_order_independent_and_extents_never_overlap_streams() {
        let a = build(&volume(&[("Z.BIN", 1), ("A.BIN", MAX_FILE)])).unwrap();
        let b = build(&volume(&[("A.BIN", MAX_FILE), ("Z.BIN", 1)])).unwrap();
        assert_eq!(a, b);
        let mut ranges: Vec<(u64, u64)> = a
            .extents
            .iter()
            .map(|e| (e.offset, e.offset + e.bytes.len() as u64))
            .collect();
        ranges.push((a.esp_offset, a.esp_offset + 64 * 1024 * 1024));
        ranges.extend(a.placements.iter().map(|p| (p.offset, p.offset + p.len)));
        ranges.sort();
        for pair in ranges.windows(2) {
            assert!(pair[0].1 <= pair[1].0, "{pair:?}");
        }
        assert!(ranges.last().unwrap().1 <= a.total_bytes);
    }

    #[test]
    fn shorter_extensions_sort_before_digit_extensions() {
        let image = build(&volume(&[("A.0", 1), ("A", 1), ("A.A", 1), ("A0", 1)])).unwrap();
        let names: Vec<_> = image.placements.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["A", "A.0", "A.A", "A0"]);
        let root = at(&image, u64::from(ROOT_BLOCK) * BLOCK);
        let mut names = Vec::new();
        let mut position = 68;
        while root[position] != 0 {
            let len = root[position + 32] as usize;
            names.push(&root[position + 33..position + 33 + len]);
            position += root[position] as usize;
        }
        assert_eq!(
            names,
            [
                b"A.;1".as_slice(),
                b"A.0;1",
                b"A.A;1",
                b"A0.;1",
                b"BOOT.CAT;1",
                b"EFI.IMG;1"
            ]
        );
    }

    #[test]
    fn names_and_bounds_fail_without_materializing_payloads() {
        for name in [
            "", ".", "../ROOT", "lower", "A;1", "A.B.C", "EFI.IMG", "BOOT.CAT", "é",
        ] {
            assert!(build(&volume(&[(name, 1)])).is_err(), "{name}");
        }
        assert!(build(&volume(&[("ROOT.ERO", MAX_FILE + 1)])).is_err());
        assert!(build(&volume(&[("ROOT.ERO", u64::MAX)])).is_err());
        assert!(build(&volume(&[("A", 1), ("A.", 2)])).is_err());
        let mut spec = volume(&[]);
        spec.files = (0..65)
            .map(|n| FileSpec {
                name: format!("F{n}"),
                len: 1,
            })
            .collect();
        assert!(build(&spec).is_err());
        spec.files.clear();
        for len in [0, 1024, 64 * 1024 * 1024 + 512, u64::MAX] {
            spec.esp_bytes = len;
            assert!(build(&spec).is_err());
        }
        assert!(build(&volume(&[])).is_ok());
        assert!(identifier(&"A".repeat(30)).is_ok());
        assert!(identifier(&"A".repeat(31)).is_err());
        let mut same_guid = volume(&[]);
        same_guid.esp_guid = same_guid.disk_guid;
        assert!(build(&same_guid).is_err());
    }
}

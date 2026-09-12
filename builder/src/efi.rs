//! Bounded PE32+ header checks for source-built x86-64 EFI applications.
//! This checks container identity and a file-backed executable entry point;
//! firmware execution, relocations and Secure Boot remain separate proofs.

use std::fs::File;
use std::io::Read;
use std::path::Path;

const MAX_HEADERS: usize = 64 * 1024;
const MAX_IMAGE: u64 = 256 * 1024 * 1024;

pub fn assert_application(path: &Path) -> Result<(), String> {
    let check = || -> Result<(), String> {
        let file = File::open(path).map_err(|e| e.to_string())?;
        let metadata = file.metadata().map_err(|e| e.to_string())?;
        if !metadata.is_file() || metadata.len() > MAX_IMAGE {
            return Err("expected a regular EFI image of at most 256 MiB".into());
        }
        let mut headers = Vec::with_capacity(MAX_HEADERS);
        file.take(MAX_HEADERS as u64)
            .read_to_end(&mut headers)
            .map_err(|e| e.to_string())?;
        validate(&headers, metadata.len())
    };
    check().map_err(|e| format!("EFI application {}: {e}", path.display()))
}

fn bytes(data: &[u8], at: usize, count: usize) -> Result<&[u8], String> {
    let end = at.checked_add(count).ok_or("header offset overflow")?;
    data.get(at..end)
        .ok_or_else(|| "truncated PE header".into())
}

fn u16_at(data: &[u8], at: usize) -> Result<u16, String> {
    Ok(u16::from_le_bytes(
        bytes(data, at, 2)?.try_into().map_err(|_| "short u16")?,
    ))
}

fn u32_at(data: &[u8], at: usize) -> Result<u32, String> {
    Ok(u32::from_le_bytes(
        bytes(data, at, 4)?.try_into().map_err(|_| "short u32")?,
    ))
}

fn validate(data: &[u8], file_len: u64) -> Result<(), String> {
    if bytes(data, 0, 2)? != b"MZ" {
        return Err("missing MZ signature".into());
    }
    let pe = usize::try_from(u32_at(data, 60)?).map_err(|_| "PE offset exceeds usize")?;
    if !(64..=MAX_HEADERS - 24).contains(&pe) || bytes(data, pe, 4)? != b"PE\0\0" {
        return Err("missing or out-of-bounds PE signature".into());
    }
    if u16_at(data, pe + 4)? != 0x8664 || u16_at(data, pe + 22)? & 0x2002 != 2 {
        return Err("expected an x86-64 executable image, not a DLL".into());
    }
    let count = usize::from(u16_at(data, pe + 6)?);
    let optional_len = usize::from(u16_at(data, pe + 20)?);
    if !(1..=96).contains(&count) || !(112..=4096).contains(&optional_len) {
        return Err("unsupported section count or optional-header size".into());
    }
    let optional = pe + 24;
    let table = optional + optional_len;
    let table_end = table + count * 40;
    if table_end > MAX_HEADERS {
        return Err("PE headers exceed 64 KiB".into());
    }
    bytes(data, optional, optional_len + count * 40)?;
    if u16_at(data, optional)? != 0x20b || u16_at(data, optional + 68)? != 10 {
        return Err("expected PE32+ EFI application subsystem".into());
    }
    let entry = u64::from(u32_at(data, optional + 16)?);
    let image_size = u64::from(u32_at(data, optional + 56)?);
    let header_size = u64::from(u32_at(data, optional + 60)?);
    if entry == 0
        || entry >= image_size
        || header_size < table_end as u64
        || header_size > MAX_HEADERS as u64
        || header_size > file_len
        || header_size > image_size
    {
        return Err("invalid entry point or image/header size".into());
    }
    let mut entry_sections = 0;
    for index in 0..count {
        let section = table + index * 40;
        let virtual_size = u64::from(u32_at(data, section + 8)?);
        let address = u64::from(u32_at(data, section + 12)?);
        let raw_size = u64::from(u32_at(data, section + 16)?);
        let raw_offset = u64::from(u32_at(data, section + 20)?);
        if (virtual_size.max(raw_size) != 0 && address < header_size)
            || address + virtual_size.max(raw_size) > image_size
            || (raw_size != 0 && (raw_offset < header_size || raw_offset + raw_size > file_len))
        {
            return Err("section extends outside image or file".into());
        }
        let backed_size = if virtual_size == 0 {
            raw_size
        } else {
            raw_size.min(virtual_size)
        };
        if entry >= address
            && entry < address + backed_size
            && u32_at(data, section + 36)? & 0x2000_0000 != 0
        {
            entry_sections += 1;
        }
    }
    if entry_sections != 1 {
        return Err("entry point must name one file-backed executable section".into());
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::indexing_slicing, clippy::unwrap_used)]
mod tests {
    use super::*;

    fn put16(data: &mut [u8], offset: usize, value: u16) {
        data[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
    }

    fn put32(data: &mut [u8], offset: usize, value: u32) {
        data[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }

    fn image() -> Vec<u8> {
        let mut data = vec![0; 1024];
        data[..2].copy_from_slice(b"MZ");
        put32(&mut data, 60, 64);
        data[64..68].copy_from_slice(b"PE\0\0");
        put16(&mut data, 68, 0x8664);
        put16(&mut data, 70, 1);
        put16(&mut data, 84, 240);
        put16(&mut data, 86, 2);
        put16(&mut data, 88, 0x20b);
        put32(&mut data, 104, 4096);
        put32(&mut data, 144, 8192);
        put32(&mut data, 148, 512);
        put16(&mut data, 156, 10);
        data[328..333].copy_from_slice(b".text");
        put32(&mut data, 336, 512);
        put32(&mut data, 340, 4096);
        put32(&mut data, 344, 512);
        put32(&mut data, 348, 512);
        put32(&mut data, 364, 0x6000_0020);
        data
    }

    #[test]
    fn accepts_a_file_backed_x64_efi_entry() {
        assert!(validate(&image(), 1024).is_ok());
    }

    #[test]
    fn rejects_other_container_types_and_subsystems() {
        for (at, value) in [
            (0, 0x457f),
            (64, 0),
            (68, 0x14c),
            (86, 0),
            (86, 0x2002),
            (88, 0x10b),
            (156, 3),
        ] {
            let mut data = image();
            put16(&mut data, at, value);
            assert!(validate(&data, 1024).is_err(), "accepted {at}={value}");
        }
    }

    #[test]
    fn rejects_invalid_entry_points_and_extents() {
        for (at, value) in [
            (104, 0),
            (104, 8192),
            (104, 4608),
            (144, 4096),
            (148, 367),
            (148, 1025),
            (336, u32::MAX),
            (340, 0),
            (348, 513),
            (348, 0),
            (364, 0x4000_0040),
        ] {
            let mut data = image();
            put32(&mut data, at, value);
            assert!(validate(&data, 1024).is_err(), "accepted {at}={value}");
        }
        let mut data = image();
        put32(&mut data, 344, 256);
        put32(&mut data, 104, 4096 + 256);
        assert!(validate(&data, 1024).is_err(), "entry in zero-fill tail");
    }

    #[test]
    fn rejects_an_ambiguous_executable_entry() {
        let mut data = image();
        data.copy_within(328..368, 368);
        put16(&mut data, 70, 2);
        assert!(validate(&data, 1024).is_err());
    }

    #[test]
    fn public_reader_rejects_an_actual_elf_executable() {
        let path = std::env::current_exe().unwrap();
        let error = assert_application(&path).unwrap_err();
        assert!(error.contains("missing MZ signature"), "{error}");
    }

    #[test]
    fn rejects_an_oversized_optional_header_with_valid_characteristics() {
        let mut data = image();
        put16(&mut data, 84, 4097);
        assert_eq!(u16_at(&data, 86).unwrap(), 2);
        assert!(validate(&data, 1024).unwrap_err().contains("optional-header size"));
    }

    #[test]
    fn rejects_a_declared_header_beyond_the_reader_limit() {
        let mut data = image();
        put32(&mut data, 104, 0x21000);
        put32(&mut data, 144, 0x22000);
        put32(&mut data, 148, 0x20000);
        put32(&mut data, 340, 0x21000);
        put32(&mut data, 348, 0x20000);
        assert!(validate(&data, 0x20200).is_err());
    }

    #[test]
    fn rejects_truncated_or_unbounded_headers_without_panicking() {
        let data = image();
        for len in 0..368 {
            assert!(
                validate(&data[..len], len as u64).is_err(),
                "accepted {len}"
            );
        }
        assert!(validate(&data, 1023).is_err());
        for (at, value) in [(60, u32::MAX), (70, 97)] {
            let mut data = image();
            put32(&mut data, at, value);
            assert!(validate(&data, 1024).is_err());
        }
    }
}

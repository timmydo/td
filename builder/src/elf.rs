//! Minimal, dependency-free ELF reader/writer — td's OWN replacement for the two
//! `patchelf` features the store-native relink/cleanup needs, so the build path adds NO
//! guix tool (patchelf would come from the host guix). This is deliberately NOT a full
//! patchelf: it reads and rewrites two strings —
//!   - the program interpreter (`PT_INTERP`), which the upstream-Rust relink retargets to
//!     the `/td/store` loader. A SHORTER path (e.g. `/td/store/ld`, 12 bytes vs
//!     `/lib64/ld-linux-x86-64.so.2`, 27 bytes) is written IN PLACE (NUL-padded); a LONGER
//!     path — the case that lets rustc/cargo point at the full hashed
//!     `/td/store/<hash>-glibc.../ld-linux-x86-64.so.2`, a NORMAL staged store path the
//!     build sandbox already mounts — is handled by GROWING: the new path is appended to the
//!     end of the file, the non-essential `PT_NOTE` program header is repurposed into a
//!     read-only `PT_LOAD` mapping it (the string must be MAPPED — the glibc dynamic linker
//!     re-reads the interp name from memory at `load_bias + p_vaddr`; verified-red: without
//!     the covering LOAD the relinked binary segfaults), and `PT_INTERP` is repointed at it.
//!     The standard patchelf-style trick, with no program-header-table relocation.
//!   - the run-path (`DT_RUNPATH` / legacy `DT_RPATH`), which makes a toolchain binary
//!     self-sufficient — e.g. retargeting an `ar`/`ranlib` build-dir search path to
//!     `/td/store/...lib` so it finds its shared libc without an `LD_LIBRARY_PATH` wrapper.
//!     This one is still IN-PLACE ONLY: a run-path string IS consumed by the dynamic loader
//!     from a mapped `.dynstr`, so growing it WOULD need the add-a-LOAD-segment / grow-.dynstr
//!     dance; a too-long run-path (or adding one where none exists) errors loudly rather than
//!     corrupting the file — a deliberate, visible boundary, not a silent truncation.
//!
//! Scope: 32- and 64-bit little-endian ELF. td's target is x86-64, with the
//! i686 class still read and rewritten; the portable mail build also validates
//! AArch64. Application launch checks remain explicitly x86-64. Other classes
//! and endiannesses are rejected.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable,
    clippy::todo,
    clippy::unimplemented,
    clippy::indexing_slicing
)] // grandfathered: pre-dates the rust-lint rules (AGENTS.md); remove when cleaned

use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

// The read-only half lives in td-engine, shared with the target's
// dynamic-application validator; this module keeps the writers and the
// debug-companion readers.
pub use td_engine::elf::*;

// Program-header types and dynamic-section tags (class-independent values).
const PT_NOTE: u32 = 4;
const PF_R: u32 = 4; // segment readable
const SHT_PROGBITS: u32 = 1;
const SHT_SYMTAB: u32 = 2;
const SHT_STRTAB: u32 = 3;
const SHT_NOTE: u32 = 7;
const SHT_NOBITS: u32 = 8;
const NT_GNU_BUILD_ID: u32 = 3;
const SHF_ALLOC: u64 = 2;
const SHF_COMPRESSED: u64 = 0x800;
const MAX_SECTION_NAME_TABLE_BYTES: u64 = 16 * 1024 * 1024;
const MAX_SECTION_HEADER_BYTES: usize = 4096;
const MAX_PROFILE_LINE_SECTION_BYTES: u64 =
    td_engine::target_profile::DEFAULT_PROFILE_LINE_SECTION_BYTES;
const MAX_PROFILE_LINE_FORMAT_FIELDS: usize = 32;
const MAX_PROFILE_LINE_TABLE_ENTRIES: u64 = 200_000;
const MAX_PROFILE_LINE_FORM_VALUES: u64 =
    MAX_PROFILE_LINE_TABLE_ENTRIES * MAX_PROFILE_LINE_FORMAT_FIELDS as u64;

#[derive(Clone, Copy, Default)]
struct DebugLineDependencies {
    debug_str: bool,
}

#[derive(Clone, Copy)]
struct LineTableFormat {
    form: u64,
}

struct LineHeaderCursor<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> LineHeaderCursor<'a> {
    fn new(bytes: &'a [u8], at: usize) -> Result<Self, String> {
        if at > bytes.len() {
            return Err(".debug_line table starts past its header".into());
        }
        Ok(Self { bytes, at })
    }

    fn byte(&mut self) -> Result<u8, String> {
        let value = self
            .bytes
            .get(self.at)
            .copied()
            .ok_or(".debug_line table is truncated")?;
        self.at = self
            .at
            .checked_add(1)
            .ok_or(".debug_line table cursor overflows")?;
        Ok(value)
    }

    fn take(&mut self, length: usize) -> Result<&'a [u8], String> {
        let end = self
            .at
            .checked_add(length)
            .ok_or(".debug_line table range overflows")?;
        let value = self
            .bytes
            .get(self.at..end)
            .ok_or(".debug_line table is truncated")?;
        self.at = end;
        Ok(value)
    }

    fn unsigned(&mut self, width: usize) -> Result<u64, String> {
        let bytes = self.take(width)?;
        match width {
            1 => bytes
                .first()
                .copied()
                .map(u64::from)
                .ok_or_else(|| ".debug_line integer is absent".to_string()),
            2 => Ok(u64::from(u16le(bytes, 0)?)),
            3 => {
                let bytes: [u8; 3] = bytes
                    .try_into()
                    .map_err(|_| ".debug_line three-byte integer is truncated")?;
                let [low, middle, high] = bytes;
                Ok(u64::from(low) | (u64::from(middle) << 8) | (u64::from(high) << 16))
            }
            4 => Ok(u64::from(u32le(bytes, 0)?)),
            8 => u64le(bytes, 0),
            _ => Err(format!(".debug_line has unsupported integer width {width}")),
        }
    }

    fn uleb(&mut self) -> Result<u64, String> {
        let mut value = 0u64;
        for shift in (0..=63).step_by(7) {
            let byte = self.byte()?;
            let low = u64::from(byte & 0x7f);
            if shift == 63 && low > 1 {
                return Err(".debug_line ULEB128 overflows".into());
            }
            value |= low << shift;
            if byte & 0x80 == 0 {
                return Ok(value);
            }
        }
        Err(".debug_line ULEB128 is too long".into())
    }

    fn sleb(&mut self) -> Result<i64, String> {
        let mut value = 0i128;
        let mut shift = 0u32;
        loop {
            if shift >= 70 {
                return Err(".debug_line SLEB128 is too long".into());
            }
            let byte = self.byte()?;
            value |= i128::from(byte & 0x7f) << shift;
            shift = shift.saturating_add(7);
            if byte & 0x80 == 0 {
                if byte & 0x40 != 0 {
                    value |= -1i128 << shift;
                }
                return i64::try_from(value).map_err(|_| ".debug_line SLEB128 overflows".into());
            }
        }
    }

    fn skip_cstring(&mut self) -> Result<(), String> {
        loop {
            if self.byte()? == 0 {
                return Ok(());
            }
        }
    }
}

fn line_table_formats(
    cursor: &mut LineHeaderCursor<'_>,
) -> Result<(Vec<LineTableFormat>, bool), String> {
    let count = usize::from(cursor.byte()?);
    if count == 0 || count > MAX_PROFILE_LINE_FORMAT_FIELDS {
        return Err(format!(
            ".debug_line has invalid version-5 format count {count}"
        ));
    }
    let mut formats = Vec::new();
    formats
        .try_reserve_exact(count)
        .map_err(|_| "cannot allocate bounded .debug_line format roster")?;
    let mut uses_debug_str = false;
    for _ in 0..count {
        let _content = cursor.uleb()?;
        let form = cursor.uleb()?;
        if form == 0x21 {
            let _ = cursor.sleb()?;
        }
        uses_debug_str |= form == 0x0e;
        formats.push(LineTableFormat { form });
    }
    Ok((formats, uses_debug_str))
}

fn skip_line_table_form(
    cursor: &mut LineHeaderCursor<'_>,
    form: u64,
    offset_size: usize,
    address_size: usize,
) -> Result<(), String> {
    let take_block = |cursor: &mut LineHeaderCursor<'_>, length: u64| -> Result<(), String> {
        let length = usize::try_from(length).map_err(|_| ".debug_line block length overflows")?;
        let _ = cursor.take(length)?;
        Ok(())
    };
    match form {
        0x01 => {
            let _ = cursor.unsigned(address_size)?;
        }
        0x03 => {
            let length = cursor.unsigned(2)?;
            take_block(cursor, length)?;
        }
        0x04 => {
            let length = cursor.unsigned(4)?;
            take_block(cursor, length)?;
        }
        0x05 => {
            let _ = cursor.unsigned(2)?;
        }
        0x06 => {
            let _ = cursor.unsigned(4)?;
        }
        0x07 => {
            let _ = cursor.unsigned(8)?;
        }
        0x08 => cursor.skip_cstring()?,
        0x09 | 0x18 => {
            let length = cursor.uleb()?;
            take_block(cursor, length)?;
        }
        0x0a => {
            let length = u64::from(cursor.byte()?);
            take_block(cursor, length)?;
        }
        0x0b | 0x0c | 0x11 | 0x25 | 0x29 => {
            let _ = cursor.unsigned(1)?;
        }
        0x0d => {
            let _ = cursor.sleb()?;
        }
        0x0e | 0x10 | 0x17 | 0x1d | 0x1f => {
            let _ = cursor.unsigned(offset_size)?;
        }
        0x0f | 0x15 | 0x1a | 0x1b | 0x22 | 0x23 => {
            let _ = cursor.uleb()?;
        }
        0x12 | 0x26 | 0x2a => {
            let _ = cursor.unsigned(2)?;
        }
        0x13 | 0x1c | 0x28 | 0x2c => {
            let _ = cursor.unsigned(4)?;
        }
        0x14 | 0x20 | 0x24 => {
            let _ = cursor.unsigned(8)?;
        }
        0x19 | 0x21 => {}
        0x1e => {
            let _ = cursor.take(16)?;
        }
        0x27 | 0x2b => {
            let _ = cursor.unsigned(3)?;
        }
        _ => {
            return Err(format!(
                ".debug_line has unsupported version-5 form {form:#x}"
            ))
        }
    }
    Ok(())
}

fn skip_line_table_entries(
    cursor: &mut LineHeaderCursor<'_>,
    formats: &[LineTableFormat],
    offset_size: usize,
    address_size: usize,
    remaining_entries: &mut u64,
    remaining_form_values: &mut u64,
) -> Result<(), String> {
    let count = cursor.uleb()?;
    if count > *remaining_entries {
        return Err(format!(
            ".debug_line version-5 directory and file tables exceed the combined \
             {MAX_PROFILE_LINE_TABLE_ENTRIES}-entry limit"
        ));
    }
    *remaining_entries = (*remaining_entries).saturating_sub(count);
    let format_count =
        u64::try_from(formats.len()).map_err(|_| ".debug_line format count does not fit u64")?;
    let form_values = count
        .checked_mul(format_count)
        .ok_or(".debug_line form-value count overflows")?;
    if form_values > *remaining_form_values {
        return Err(format!(
            ".debug_line version-5 table scan exceeds the \
             {MAX_PROFILE_LINE_FORM_VALUES}-form-value object limit"
        ));
    }
    *remaining_form_values = (*remaining_form_values).saturating_sub(form_values);
    for _ in 0..count {
        for format in formats {
            skip_line_table_form(cursor, format.form, offset_size, address_size)?;
        }
    }
    Ok(())
}

fn v5_line_tables_use_debug_str(
    header: &[u8],
    tables_at: usize,
    offset_size: usize,
    address_size: usize,
    remaining_form_values: &mut u64,
) -> Result<bool, String> {
    let mut cursor = LineHeaderCursor::new(header, tables_at)?;
    let mut remaining_entries = MAX_PROFILE_LINE_TABLE_ENTRIES;
    let (directories, directory_uses_debug_str) = line_table_formats(&mut cursor)?;
    skip_line_table_entries(
        &mut cursor,
        &directories,
        offset_size,
        address_size,
        &mut remaining_entries,
        remaining_form_values,
    )?;
    let (files, file_uses_debug_str) = line_table_formats(&mut cursor)?;
    skip_line_table_entries(
        &mut cursor,
        &files,
        offset_size,
        address_size,
        &mut remaining_entries,
        remaining_form_values,
    )?;
    Ok(directory_uses_debug_str || file_uses_debug_str)
}

fn validate_line_address_shape(address_size: u8, segment_size: u8) -> Result<(), String> {
    if !matches!(address_size, 1 | 2 | 4 | 8) || segment_size != 0 {
        return Err(format!(
            ".debug_line has unsupported address/segment sizes {address_size}/{segment_size}"
        ));
    }
    Ok(())
}

/// Write a class-width word (u64 on ELF64, low u32 on ELF32) at `off`, little-endian.
fn put_word(b: &mut [u8], off: usize, v: u64, is64: bool) -> Result<(), String> {
    if is64 {
        let end = off
            .checked_add(8)
            .ok_or_else(|| format!("ELF u64 write offset {off} overflows"))?;
        b.get_mut(off..end)
            .ok_or_else(|| format!("ELF truncated writing u64 at {off}"))?
            .copy_from_slice(&v.to_le_bytes());
    } else {
        let end = off
            .checked_add(4)
            .ok_or_else(|| format!("ELF u32 write offset {off} overflows"))?;
        b.get_mut(off..end)
            .ok_or_else(|| format!("ELF truncated writing u32 at {off}"))?
            .copy_from_slice(&(v as u32).to_le_bytes());
    }
    Ok(())
}

/// The mutable program-header fields, as a class-dependent byte offset within a ph entry.
/// (ELF64: p_offset@8 p_vaddr@16 p_paddr@24 p_filesz@32 p_memsz@40 p_align@48; ELF32:
/// p_offset@4 p_vaddr@8 p_paddr@12 p_filesz@16 p_memsz@20 p_align@28.)
enum PField {
    Type,
    Flags,
    Offset,
    Vaddr,
    Paddr,
    Filesz,
    Memsz,
    Align,
}
fn ph_field(f: &PField, is64: bool) -> usize {
    match (f, is64) {
        (PField::Type, _) => 0x00,
        (PField::Flags, true) => 0x04,
        (PField::Flags, false) => 0x18,
        (PField::Offset, true) => 0x08,
        (PField::Vaddr, true) => 0x10,
        (PField::Paddr, true) => 0x18,
        (PField::Filesz, true) => 0x20,
        (PField::Memsz, true) => 0x28,
        (PField::Align, true) => 0x30,
        (PField::Offset, false) => 0x04,
        (PField::Vaddr, false) => 0x08,
        (PField::Paddr, false) => 0x0C,
        (PField::Filesz, false) => 0x10,
        (PField::Memsz, false) => 0x14,
        (PField::Align, false) => 0x1C,
    }
}
fn set_ph_word(b: &mut [u8], ph: usize, is64: bool, f: PField, v: u64) -> Result<(), String> {
    let off = ph
        .checked_add(ph_field(&f, is64))
        .ok_or("ELF program-header field offset overflow")?;
    put_word(b, off, v, is64)
}
/// Write a 4-byte program-header field (`p_type`/`p_flags`, which are u32 in BOTH classes).
fn set_ph_u32(b: &mut [u8], ph: usize, is64: bool, f: PField, v: u32) -> Result<(), String> {
    let off = ph
        .checked_add(ph_field(&f, is64))
        .ok_or("ELF program-header field offset overflow")?;
    let end = off
        .checked_add(4)
        .ok_or_else(|| format!("ELF ph u32 write offset {off} overflows"))?;
    b.get_mut(off..end)
        .ok_or_else(|| format!("ELF truncated writing ph u32 at {off}"))?
        .copy_from_slice(&v.to_le_bytes());
    Ok(())
}

fn align4_u64(value: u64) -> Result<u64, String> {
    value
        .checked_add(3)
        .map(|rounded| rounded & !3)
        .ok_or_else(|| "ELF note alignment overflow".to_string())
}

#[derive(Clone, Copy, Debug)]
struct FileSection {
    name_offset: usize,
    kind: u32,
    flags: u64,
    offset: u64,
    size: u64,
    link: usize,
    entry_size: u64,
}

/// Bounded-memory view of the section metadata needed by the profile gate.
/// Large runtime and companion files are never copied into one allocation.
struct FileElf {
    file: std::fs::File,
    length: u64,
    is64: bool,
    names: Vec<u8>,
    sections: Vec<FileSection>,
}

impl FileElf {
    fn open(path: &Path) -> Result<Self, String> {
        let mut file =
            std::fs::File::open(path).map_err(|e| format!("read {}: {e}", path.display()))?;
        let length = file
            .metadata()
            .map_err(|e| format!("stat {}: {e}", path.display()))?
            .len();
        let header_len = usize::try_from(length.min(64))
            .map_err(|_| format!("{}: ELF header length does not fit usize", path.display()))?;
        let mut header = vec![0u8; header_len];
        file.read_exact(&mut header)
            .map_err(|e| format!("read ELF header {}: {e}", path.display()))?;
        let parsed = Elf::parse(&header).map_err(|e| format!("{}: {e}", path.display()))?;
        let is64 = parsed.is64;
        let (
            shoff_field,
            ents_field,
            num_field,
            str_field,
            min_ents,
            flags_field,
            off_field,
            size_field,
            link_field,
            entry_size_field,
        ) = if is64 {
            (
                0x28, 0x3a, 0x3c, 0x3e, 64usize, 0x08, 0x18, 0x20, 0x28, 0x38,
            )
        } else {
            (
                0x20, 0x2e, 0x30, 0x32, 40usize, 0x08, 0x10, 0x14, 0x18, 0x24,
            )
        };
        let shoff = parsed.word(shoff_field)?;
        let shentsize = u16le(&header, ents_field)? as usize;
        let shnum = u16le(&header, num_field)? as usize;
        let shstrndx = u16le(&header, str_field)? as usize;
        if shoff == 0 || shnum == 0 {
            return Err(format!(
                "{}: ELF has no section-header table",
                path.display()
            ));
        }
        if !(min_ents..=MAX_SECTION_HEADER_BYTES).contains(&shentsize) {
            return Err(format!(
                "{}: implausible e_shentsize {shentsize}",
                path.display()
            ));
        }
        if shstrndx == 0xffff {
            return Err(format!(
                "{}: ELF extended section-name index is unsupported",
                path.display()
            ));
        }
        if shstrndx >= shnum {
            return Err(format!(
                "{}: ELF e_shstrndx {shstrndx} is outside {shnum} sections",
                path.display()
            ));
        }
        let table_bytes = u64::try_from(shentsize)
            .ok()
            .and_then(|width| width.checked_mul(shnum as u64))
            .ok_or_else(|| format!("{}: ELF section-header table overflows", path.display()))?;
        let table_end = shoff
            .checked_add(table_bytes)
            .ok_or_else(|| format!("{}: ELF section-header table overflows", path.display()))?;
        if table_end > length {
            return Err(format!(
                "{}: ELF section-header table runs past end of file",
                path.display()
            ));
        }

        let read_header = |file: &mut std::fs::File, index: usize| -> Result<Vec<u8>, String> {
            let offset = (index as u64)
                .checked_mul(shentsize as u64)
                .and_then(|delta| shoff.checked_add(delta))
                .ok_or_else(|| {
                    format!("{}: ELF section-header {index} overflows", path.display())
                })?;
            file.seek(SeekFrom::Start(offset))
                .map_err(|e| format!("seek {} section {index}: {e}", path.display()))?;
            let mut bytes = vec![0u8; shentsize];
            file.read_exact(&mut bytes)
                .map_err(|e| format!("read {} section {index}: {e}", path.display()))?;
            Ok(bytes)
        };
        let range = |section: &[u8], file_backed: bool| -> Result<(u64, u64), String> {
            let offset = if is64 {
                u64le(section, off_field)?
            } else {
                u32le(section, off_field)? as u64
            };
            let size = if is64 {
                u64le(section, size_field)?
            } else {
                u32le(section, size_field)? as u64
            };
            if file_backed {
                let end = offset
                    .checked_add(size)
                    .ok_or("ELF section range overflow")?;
                if end > length {
                    return Err("ELF section runs past end of file".into());
                }
            }
            Ok((offset, size))
        };

        let names_header = read_header(&mut file, shstrndx)?;
        let names_kind = u32le(&names_header, 4)?;
        if names_kind != SHT_STRTAB {
            return Err(format!(
                "{}: ELF section-name table is not SHT_STRTAB",
                path.display()
            ));
        }
        let (names_offset, names_size) = range(&names_header, names_kind != SHT_NOBITS)?;
        if names_size > MAX_SECTION_NAME_TABLE_BYTES {
            return Err(format!(
                "{}: ELF section-name table is {names_size} bytes (limit {MAX_SECTION_NAME_TABLE_BYTES})",
                path.display()
            ));
        }
        let mut names = vec![0u8; names_size as usize];
        file.seek(SeekFrom::Start(names_offset))
            .map_err(|e| format!("seek ELF section names {}: {e}", path.display()))?;
        file.read_exact(&mut names)
            .map_err(|e| format!("read ELF section names {}: {e}", path.display()))?;

        let mut sections = Vec::with_capacity(shnum);
        for index in 0..shnum {
            let section = read_header(&mut file, index)?;
            let name_offset = u32le(&section, 0)? as usize;
            let kind = u32le(&section, 4)?;
            let flags = if is64 {
                u64le(&section, flags_field)?
            } else {
                u32le(&section, flags_field)? as u64
            };
            let raw_name = names
                .get(name_offset..)
                .ok_or("ELF section name offset is outside .shstrtab")?;
            raw_name
                .iter()
                .position(|byte| *byte == 0)
                .ok_or("ELF section name is not NUL-terminated")?;
            let (offset, size) = range(&section, kind != SHT_NOBITS)?;
            let link = u32le(&section, link_field)? as usize;
            let entry_size = if is64 {
                u64le(&section, entry_size_field)?
            } else {
                u32le(&section, entry_size_field)? as u64
            };
            sections.push(FileSection {
                name_offset,
                kind,
                flags,
                offset,
                size,
                link,
                entry_size,
            });
        }
        Ok(Self {
            file,
            length,
            is64,
            names,
            sections,
        })
    }

    fn section_name<'a>(&'a self, section: &FileSection) -> Result<&'a [u8], String> {
        let tail = self
            .names
            .get(section.name_offset..)
            .ok_or("ELF section name offset is outside .shstrtab")?;
        let end = tail
            .iter()
            .position(|byte| *byte == 0)
            .ok_or("ELF section name is not NUL-terminated")?;
        tail.get(..end)
            .ok_or_else(|| "ELF section name range overflow".to_string())
    }

    fn validate_symbol_table(&self, section: FileSection) -> Result<(), String> {
        let expected_entry_size = if self.is64 { 24 } else { 16 };
        if section.entry_size != expected_entry_size
            || section.size < expected_entry_size * 2
            || section.size % expected_entry_size != 0
        {
            return Err(format!(
                ".symtab has size {} and entry size {}, expected at least two {}-byte entries",
                section.size, section.entry_size, expected_entry_size
            ));
        }
        if self.sections.get(section.link).map(|linked| linked.kind) != Some(SHT_STRTAB) {
            return Err(".symtab does not link to a string table".into());
        }
        Ok(())
    }

    fn read_fixed<const N: usize>(&mut self, offset: u64, what: &str) -> Result<[u8; N], String> {
        let end = offset
            .checked_add(N as u64)
            .ok_or_else(|| format!("{what} offset overflows"))?;
        if end > self.length {
            return Err(format!("{what} runs past end of file"));
        }
        self.file
            .seek(SeekFrom::Start(offset))
            .map_err(|e| format!("seek {what}: {e}"))?;
        let mut bytes = [0u8; N];
        self.file
            .read_exact(&mut bytes)
            .map_err(|e| format!("read {what}: {e}"))?;
        Ok(bytes)
    }

    fn validate_debug_line(
        &mut self,
        section: FileSection,
    ) -> Result<DebugLineDependencies, String> {
        let section_end = section
            .offset
            .checked_add(section.size)
            .ok_or(".debug_line range overflows")?;
        if section.size == 0 {
            return Err(".debug_line is empty".into());
        }
        let mut cursor = section.offset;
        let mut units = 0usize;
        let mut program_units = 0usize;
        let mut dependencies = DebugLineDependencies::default();
        let mut v5_header = Vec::new();
        let mut remaining_v5_form_values = MAX_PROFILE_LINE_FORM_VALUES;
        while cursor < section_end {
            let initial = self.read_fixed::<4>(cursor, ".debug_line unit length")?;
            let initial_length = u32le(&initial, 0)?;
            cursor = cursor
                .checked_add(4)
                .ok_or(".debug_line cursor overflows")?;
            let (unit_length, dwarf64) = if initial_length == u32::MAX {
                let extended = self.read_fixed::<8>(cursor, ".debug_line 64-bit unit length")?;
                cursor = cursor
                    .checked_add(8)
                    .ok_or(".debug_line cursor overflows")?;
                (u64le(&extended, 0)?, true)
            } else if initial_length >= 0xfffffff0 {
                return Err(".debug_line uses a reserved initial length".into());
            } else {
                (initial_length as u64, false)
            };
            if unit_length == 0 {
                return Err(".debug_line contains an empty unit".into());
            }
            let unit_end = cursor
                .checked_add(unit_length)
                .ok_or(".debug_line unit range overflows")?;
            if unit_end > section_end {
                return Err(".debug_line unit runs past its section".into());
            }
            if cursor.checked_add(2).is_none_or(|end| end > unit_end) {
                return Err(".debug_line unit ends before its version".into());
            }
            let version_bytes = self.read_fixed::<2>(cursor, ".debug_line version")?;
            let version = u16le(&version_bytes, 0)?;
            if !(2..=5).contains(&version) {
                return Err(format!(
                    ".debug_line has unsupported DWARF version {version}"
                ));
            }
            cursor = cursor
                .checked_add(2)
                .ok_or(".debug_line cursor overflows")?;
            let address_size = if version == 5 {
                if cursor.checked_add(2).is_none_or(|end| end > unit_end) {
                    return Err(".debug_line version 5 unit ends before its address sizes".into());
                }
                let sizes = self.read_fixed::<2>(cursor, ".debug_line address sizes")?;
                let address_size = sizes
                    .first()
                    .copied()
                    .ok_or(".debug_line address size is absent")?;
                let segment_size = sizes
                    .get(1)
                    .copied()
                    .ok_or(".debug_line segment size is absent")?;
                validate_line_address_shape(address_size, segment_size)?;
                cursor = cursor
                    .checked_add(2)
                    .ok_or(".debug_line cursor overflows")?;
                address_size
            } else {
                8
            };
            let header_width = if dwarf64 { 8u64 } else { 4u64 };
            if cursor
                .checked_add(header_width)
                .is_none_or(|end| end > unit_end)
            {
                return Err(".debug_line unit ends before its header length".into());
            }
            let (header_length, width) = if dwarf64 {
                let bytes = self.read_fixed::<8>(cursor, ".debug_line header length")?;
                (u64le(&bytes, 0)?, 8u64)
            } else {
                let bytes = self.read_fixed::<4>(cursor, ".debug_line header length")?;
                (u32le(&bytes, 0)? as u64, 4u64)
            };
            cursor = cursor
                .checked_add(width)
                .ok_or(".debug_line cursor overflows")?;
            let header_start = cursor;
            let program_start = cursor
                .checked_add(header_length)
                .ok_or(".debug_line header range overflows")?;
            if header_length < 6 || program_start > unit_end {
                return Err(".debug_line has no complete header".into());
            }
            let prologue = self.read_fixed::<6>(cursor, ".debug_line prologue")?;
            let prologue_byte = |index| {
                prologue
                    .get(index)
                    .copied()
                    .ok_or(".debug_line prologue byte is absent")
            };
            let (minimum_instruction_length, maximum_operations, line_range, opcode_base) =
                if version <= 3 {
                    (prologue_byte(0)?, 1, prologue_byte(3)?, prologue_byte(4)?)
                } else {
                    (
                        prologue_byte(0)?,
                        prologue_byte(1)?,
                        prologue_byte(4)?,
                        prologue_byte(5)?,
                    )
                };
            if minimum_instruction_length == 0
                || maximum_operations == 0
                || line_range == 0
                || opcode_base == 0
            {
                return Err(".debug_line prologue has an invalid zero field".into());
            }
            let fixed_prologue = if version <= 3 { 5u64 } else { 6u64 };
            let table_terminators = if version == 5 { 4u64 } else { 2u64 };
            let minimum_header_length = fixed_prologue
                .checked_add((opcode_base - 1) as u64)
                .and_then(|length| length.checked_add(table_terminators))
                .ok_or(".debug_line minimum header length overflows")?;
            if header_length < minimum_header_length {
                return Err(format!(
                    ".debug_line header is {header_length} bytes, below its {minimum_header_length}-byte structural minimum"
                ));
            }
            if version == 5 {
                let header_bytes = usize::try_from(header_length)
                    .map_err(|_| ".debug_line header length does not fit usize")?;
                v5_header.clear();
                v5_header
                    .try_reserve_exact(header_bytes)
                    .map_err(|_| "cannot allocate bounded .debug_line header")?;
                v5_header.resize(header_bytes, 0);
                self.file
                    .seek(SeekFrom::Start(header_start))
                    .map_err(|e| format!("seek .debug_line version-5 header: {e}"))?;
                self.file
                    .read_exact(&mut v5_header)
                    .map_err(|e| format!("read .debug_line version-5 header: {e}"))?;
                let tables_at = 6usize
                    .checked_add(usize::from(opcode_base - 1))
                    .ok_or(".debug_line version-5 table offset overflows")?;
                dependencies.debug_str |= v5_line_tables_use_debug_str(
                    &v5_header,
                    tables_at,
                    if dwarf64 { 8 } else { 4 },
                    usize::from(address_size),
                    &mut remaining_v5_form_values,
                )?;
            }
            if program_start < unit_end {
                program_units = program_units
                    .checked_add(1)
                    .ok_or(".debug_line program-unit count overflows")?;
            }
            cursor = unit_end;
            units = units
                .checked_add(1)
                .ok_or(".debug_line unit count overflows")?;
        }
        if units == 0 {
            return Err(".debug_line contains no units".into());
        }
        if program_units == 0 {
            return Err(".debug_line contains no line program".into());
        }
        Ok(dependencies)
    }

    fn build_ids(&mut self) -> Result<Vec<[u8; 20]>, String> {
        let mut ids = Vec::new();
        for section in self
            .sections
            .iter()
            .filter(|section| section.kind == SHT_NOTE)
        {
            let end = section
                .offset
                .checked_add(section.size)
                .ok_or("ELF note section range overflow")?;
            let mut cursor = section.offset;
            while cursor < end {
                let header_end = cursor
                    .checked_add(12)
                    .ok_or("ELF note header offset overflow")?;
                if header_end > end {
                    return Err("ELF note section ends in a partial note header".into());
                }
                self.file
                    .seek(SeekFrom::Start(cursor))
                    .map_err(|e| format!("seek ELF note: {e}"))?;
                let mut header = [0u8; 12];
                self.file
                    .read_exact(&mut header)
                    .map_err(|e| format!("read ELF note: {e}"))?;
                let namesz = u32le(&header, 0)? as u64;
                let descsz = u32le(&header, 4)? as u64;
                let note_type = u32le(&header, 8)?;
                let name_start = header_end;
                let name_end = name_start
                    .checked_add(namesz)
                    .ok_or("ELF note name range overflow")?;
                let desc_start = align4_u64(name_end)?;
                let desc_end = desc_start
                    .checked_add(descsz)
                    .ok_or("ELF note descriptor range overflow")?;
                let next = align4_u64(desc_end)?;
                if next > end || next > self.length {
                    return Err("ELF note runs past its section".into());
                }
                let mut name = [0u8; 4];
                if namesz == name.len() as u64 {
                    self.file
                        .seek(SeekFrom::Start(name_start))
                        .map_err(|e| format!("seek ELF note name: {e}"))?;
                    self.file
                        .read_exact(&mut name)
                        .map_err(|e| format!("read ELF note name: {e}"))?;
                }
                if note_type == NT_GNU_BUILD_ID && namesz == 4 && name == *b"GNU\0" {
                    if descsz != 20 {
                        return Err(format!(
                            "GNU build ID is {descsz} bytes, expected SHA-1's 20"
                        ));
                    }
                    let mut id = [0u8; 20];
                    self.file
                        .seek(SeekFrom::Start(desc_start))
                        .map_err(|e| format!("seek GNU build ID: {e}"))?;
                    self.file
                        .read_exact(&mut id)
                        .map_err(|e| format!("read GNU build ID: {e}"))?;
                    ids.push(id);
                }
                cursor = next;
            }
        }
        Ok(ids)
    }
}

/// Whether `path` is an installed runtime ELF (ET_EXEC or ET_DYN). Non-ELF
/// files return false; a file carrying ELF magic but a truncated/unsupported
/// header is an error so the package walk cannot silently skip corrupt output.
pub fn is_runtime_elf(path: &Path) -> Result<bool, String> {
    let mut file =
        std::fs::File::open(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let mut header = Vec::with_capacity(18);
    let mut prefix = std::io::Read::take(&mut file, 18);
    std::io::Read::read_to_end(&mut prefix, &mut header)
        .map_err(|e| format!("read {}: {e}", path.display()))?;
    if header.len() < EI_MAG.len() || header.get(..EI_MAG.len()) != Some(EI_MAG) {
        return Ok(false);
    }
    if header.len() < 18 {
        return Err(format!("{}: truncated ELF header", path.display()));
    }
    let class = header
        .get(EI_CLASS)
        .copied()
        .ok_or_else(|| format!("{}: missing ELF class", path.display()))?;
    match class {
        1 | 2 => {}
        class => return Err(format!("{}: unsupported ELF class {class}", path.display())),
    }
    if header.get(EI_DATA).copied() != Some(1) {
        return Err(format!("{}: unsupported ELF byte order", path.display()));
    }
    Ok(matches!(u16le(&header, 0x10)?, ET_EXEC | ET_DYN))
}

/// Read the one deterministic GNU SHA-1 build ID. Missing, duplicate, or
/// non-SHA-1 notes are rejected rather than turning object identity into a
/// best-effort property.
pub fn read_build_id(path: &Path) -> Result<Vec<u8>, String> {
    let mut elf = FileElf::open(path)?;
    one_build_id(path, &mut elf)
}

/// Largest `.debug_str`/`.debug_info` `debug_producers` reads.
const MAX_PRODUCER_SECTION_BYTES: u64 = 64 * 1024 * 1024;

/// Every DWARF producer string (`GNU C17 …`, `GNU AS …`) in an object's
/// `.debug_str` or `.debug_info`: the compiler and the options it recorded.
pub fn debug_producers(path: &Path) -> Result<Vec<String>, String> {
    let mut elf = FileElf::open(path)?;
    let mut producers = Vec::new();
    for section in elf.sections.clone() {
        let name = elf.section_name(&section)?;
        if !matches!(name, b".debug_str" | b".debug_info") || section.kind == SHT_NOBITS {
            continue;
        }
        if section.size > MAX_PRODUCER_SECTION_BYTES {
            return Err(format!(
                "{}: {} bytes of debug strings (limit {MAX_PRODUCER_SECTION_BYTES})",
                path.display(),
                section.size
            ));
        }
        let mut bytes = vec![0u8; usize::try_from(section.size).map_err(|e| e.to_string())?];
        elf.file
            .seek(SeekFrom::Start(section.offset))
            .and_then(|_| elf.file.read_exact(&mut bytes))
            .map_err(|e| format!("read {} debug strings: {e}", path.display()))?;
        let mut rest = bytes.as_slice();
        while let Some(at) = rest.windows(4).position(|window| window == b"GNU ") {
            let tail = rest.get(at..).unwrap_or_default();
            let end = tail.iter().position(|b| *b == 0).unwrap_or(tail.len());
            let text = tail.get(..end).unwrap_or_default();
            producers.push(String::from_utf8_lossy(text).into_owned());
            rest = tail.get(end..).unwrap_or_default();
        }
    }
    Ok(producers)
}

fn one_build_id(path: &Path, elf: &mut FileElf) -> Result<Vec<u8>, String> {
    let ids = elf.build_ids()?;
    if ids.len() != 1 {
        return Err(format!(
            "{}: expected exactly one GNU build ID, found {}",
            path.display(),
            ids.len()
        ));
    }
    Ok(ids
        .into_iter()
        .next()
        .ok_or("GNU build ID vanished after count check")?
        .to_vec())
}

/// Determine whether a structurally valid line program declares
/// `DW_FORM_strp` in a DWARF-v5 directory or file table. Earlier line-table
/// versions carry those strings inline, so their `.debug_str` belongs only to
/// the full debugging payload and can be pruned from the companion.
pub fn debug_line_requires_debug_str(path: &Path) -> Result<bool, String> {
    let mut elf = FileElf::open(path)?;
    let mut line = None;
    for index in 0..elf.sections.len() {
        let section = elf
            .sections
            .get(index)
            .copied()
            .ok_or("debug section index vanished during validation")?;
        if elf.section_name(&section)? != b".debug_line" {
            continue;
        }
        if line.replace(section).is_some() {
            return Err(format!("{}: duplicate .debug_line section", path.display()));
        }
    }
    let line =
        line.ok_or_else(|| format!("{}: debug companion has no .debug_line", path.display()))?;
    if line.kind != SHT_PROGBITS || line.flags & SHF_COMPRESSED != 0 {
        return Err(format!(
            "{}: .debug_line has unsupported type or compression",
            path.display()
        ));
    }
    if line.size > MAX_PROFILE_LINE_SECTION_BYTES {
        return Err(format!(
            "{}: .debug_line uses {} bytes, exceeding the {}-byte ceiling",
            path.display(),
            line.size,
            MAX_PROFILE_LINE_SECTION_BYTES,
        ));
    }
    Ok(elf
        .validate_debug_line(line)
        .map_err(|error| format!("{}: {error}", path.display()))?
        .debug_str)
}

/// Whether `path` is debug-only, as `objcopy --only-keep-debug` writes it:
/// every allocated section other than a note is SHT_NOBITS, at least one
/// is, and no PT_INTERP or PT_DYNAMIC segment holds file bytes.
pub fn is_debug_only(path: &Path) -> Result<bool, String> {
    let elf = FileElf::open(path)?;
    let mut nobits = false;
    for section in &elf.sections {
        if section.flags & SHF_ALLOC == 0 || section.kind == SHT_NOTE {
            continue;
        }
        if section.kind != SHT_NOBITS {
            return Ok(false);
        }
        nobits = true;
    }
    if !nobits || !elf.is64 {
        return Ok(false);
    }
    let mut file =
        std::fs::File::open(path).map_err(|e| format!("open {}: {e}", path.display()))?;
    let mut header = [0u8; 64];
    file.read_exact(&mut header)
        .map_err(|e| format!("read {}: {e}", path.display()))?;
    let phoff = u64le(&header, 0x20)?;
    let entry_size = u64::from(u16le(&header, 0x36)?);
    let count = u64::from(u16le(&header, 0x38)?);
    if count != 0 && entry_size < 56 {
        return Err(format!(
            "{}: ELF program header is too small",
            path.display()
        ));
    }
    let mut segment = [0u8; 56];
    for index in 0..count {
        let at = index
            .checked_mul(entry_size)
            .and_then(|offset| offset.checked_add(phoff))
            .ok_or("ELF program header offset overflow")?;
        file.seek(SeekFrom::Start(at))
            .map_err(|e| format!("seek {}: {e}", path.display()))?;
        file.read_exact(&mut segment)
            .map_err(|e| format!("read {} program header: {e}", path.display()))?;
        let kind = u32le(&segment, 0)?;
        if matches!(kind, PT_INTERP | PT_DYNAMIC) && u64le(&segment, 0x20)? != 0 {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Verify the runtime/debug pair identity and the companion's minimum useful
/// contents. The runtime retains only allocated/runtime metadata and dynamic
/// symbols; the full ordinary symbol table lives in the companion.
pub fn assert_debug_pair(runtime: &Path, debug: &Path) -> Result<(), String> {
    assert_debug_pair_with_line_limit(runtime, debug, MAX_PROFILE_LINE_SECTION_BYTES, true)
}

pub fn assert_debug_pair_with_line_limit(
    runtime: &Path,
    debug: &Path,
    max_debug_line_section_bytes: u64,
    require_line_string_dependencies: bool,
) -> Result<(), String> {
    if !is_runtime_elf(runtime)? {
        return Err(format!(
            "{}: debug-pair runtime is not ET_EXEC/ET_DYN",
            runtime.display()
        ));
    }
    let mut runtime_elf = FileElf::open(runtime)?;
    let runtime_id = one_build_id(runtime, &mut runtime_elf)?;
    let mut debug_elf = FileElf::open(debug)?;
    let debug_id = one_build_id(debug, &mut debug_elf)?;
    if runtime_id != debug_id {
        return Err(format!(
            "{} and {} carry different GNU build IDs",
            runtime.display(),
            debug.display()
        ));
    }
    for section in &runtime_elf.sections {
        let name = runtime_elf.section_name(section)?;
        if section.kind == SHT_SYMTAB {
            return Err(format!(
                "{}: stripped runtime still carries an ordinary symbol table",
                runtime.display()
            ));
        }
        if section.flags & SHF_ALLOC == 0
            && (name.starts_with(b".debug_") || name.starts_with(b".zdebug_"))
        {
            return Err(format!(
                "{}: stripped runtime still carries debug section {}",
                runtime.display(),
                String::from_utf8_lossy(name)
            ));
        }
    }
    let mut has_symbols = false;
    let mut has_lines = false;
    let mut seen_symbols = false;
    let mut seen_lines = false;
    let mut seen_line_strings = false;
    let mut seen_debug_strings = false;
    let mut debug_string_size = None;
    let mut requires_debug_strings = false;
    for index in 0..debug_elf.sections.len() {
        let section = debug_elf
            .sections
            .get(index)
            .copied()
            .ok_or("debug section index vanished during validation")?;
        let name = debug_elf.section_name(&section)?;
        let seen = if name == b".symtab" {
            Some(&mut seen_symbols)
        } else if name == b".debug_line" {
            Some(&mut seen_lines)
        } else if name == b".debug_line_str" {
            Some(&mut seen_line_strings)
        } else if name == b".debug_str" {
            debug_string_size = Some(section.size);
            Some(&mut seen_debug_strings)
        } else {
            None
        };
        if let Some(seen) = seen {
            if *seen {
                return Err(format!(
                    "{}: duplicate {} section is outside the profiled companion policy",
                    debug.display(),
                    String::from_utf8_lossy(name)
                ));
            }
            *seen = true;
        }
        let is_symbols = section.kind == SHT_SYMTAB && name == b".symtab";
        let is_lines = section.kind == SHT_PROGBITS && name == b".debug_line";
        let line_data =
            name == b".debug_line" || name == b".debug_line_str" || name == b".debug_str";
        let max_section_bytes = if name == b".debug_line" {
            max_debug_line_section_bytes
        } else {
            MAX_PROFILE_LINE_SECTION_BYTES
        };
        let compressed_line_data = name == b".zdebug_line"
            || name == b".zdebug_line_str"
            || name == b".zdebug_str"
            || (line_data && section.flags & SHF_COMPRESSED != 0);
        if compressed_line_data {
            return Err(format!(
                "{}: compressed {} is outside the deterministic companion policy",
                debug.display(),
                String::from_utf8_lossy(name)
            ));
        }
        if line_data && section.kind != SHT_PROGBITS {
            return Err(format!(
                "{}: {} has unsupported section type {}",
                debug.display(),
                String::from_utf8_lossy(name),
                section.kind,
            ));
        }
        if line_data && section.size > max_section_bytes {
            return Err(format!(
                "{}: {} uses {} bytes, exceeding the {max_section_bytes}-byte ceiling",
                debug.display(),
                String::from_utf8_lossy(name),
                section.size,
            ));
        }
        if is_symbols {
            debug_elf.validate_symbol_table(section)?;
            has_symbols = true;
        }
        if is_lines {
            requires_debug_strings |= debug_elf
                .validate_debug_line(section)
                .map_err(|error| format!("{}: {error}", debug.display()))?
                .debug_str;
            has_lines = true;
        }
    }
    if !has_symbols || !has_lines {
        return Err(format!(
            "{}: debug companion requires .symtab and .debug_line (symbols={has_symbols}, lines={has_lines})",
            debug.display()
        ));
    }
    if require_line_string_dependencies
        && requires_debug_strings
        && debug_string_size.is_none_or(|size| size == 0)
    {
        return Err(format!(
            "{}: .debug_line requires a nonempty .debug_str section",
            debug.display()
        ));
    }
    Ok(())
}

/// Locate the PT_INTERP program-header ENTRY and return `(ph_entry_offset, string_off,
/// string_filesz, is64)`, or `None` if the ELF has no interpreter. Unlike `interp_slot`
/// this yields the ph ENTRY offset so the setter can grow the string (repoint p_offset/
/// p_filesz), not just overwrite it in place.
fn interp_ph_entry(b: &[u8]) -> Result<Option<(usize, usize, usize, bool)>, String> {
    let elf = Elf::parse(b)?;
    let (phoff, phentsize, phnum) = elf.phdr_table()?;
    let (p_off, _pv, p_filesz) = elf.ph_fields();
    for i in 0..phnum {
        let ph = elf.phdr_offset(phoff, phentsize, i)?;
        if u32le(b, ph)? == PT_INTERP {
            let off =
                usize::try_from(elf.word(Elf::field_offset(ph, p_off, "PT_INTERP offset")?)?)
                    .map_err(|_| "PT_INTERP string offset does not fit this architecture")?;
            let sz =
                usize::try_from(elf.word(Elf::field_offset(ph, p_filesz, "PT_INTERP size")?)?)
                    .map_err(|_| "PT_INTERP string size does not fit this architecture")?;
            let end = off
                .checked_add(sz)
                .ok_or("PT_INTERP string file range overflows")?;
            if end > b.len() {
                return Err("PT_INTERP string runs past end of file".into());
            }
            return Ok(Some((ph, off, sz, elf.is64)));
        }
    }
    Ok(None)
}

/// Rewrite the program interpreter (`PT_INTERP`) string. A path that fits the existing slot
/// (plus its NUL) is written IN PLACE (remaining bytes NUL-padded). A LONGER path is handled
/// by GROWING: the new path (NUL-terminated) is appended to the end of the file, the
/// non-essential `PT_NOTE` program header is repurposed into a read-only `PT_LOAD` mapping
/// it, and `PT_INTERP` is repointed at the new offset/vaddr. The covering LOAD is required —
/// the glibc dynamic linker re-reads the interp name from MEMORY at `load_bias + p_vaddr`
/// (verified-red: append + repoint alone segfaults at run time). Errors (without modifying
/// the file) if the ELF has no interpreter, or no `PT_NOTE` to repurpose when growth is
/// needed. Lets the upstream-Rust relink point rustc/cargo at the full hashed
/// `/td/store/<hash>-glibc.../ld…` loader (a normal staged store path), not just the short
/// `/td/store/ld`.
pub fn set_interp(path: &Path, new_interp: &str) -> Result<(), String> {
    let mut b = std::fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let (ph, off, sz, is64) = interp_ph_entry(&b)?.ok_or_else(|| {
        format!(
            "{}: no PT_INTERP (not an interpreted executable)",
            path.display()
        )
    })?;
    let nb = new_interp.as_bytes();
    if nb.contains(&0) {
        return Err("new interpreter contains a NUL byte".into());
    }
    if nb.len() + 1 <= sz {
        // fits — overwrite in place, NUL-padding the tail of the old slot.
        let end = off
            .checked_add(sz)
            .ok_or("PT_INTERP string file range overflows")?;
        let raw = b
            .get_mut(off..end)
            .ok_or("PT_INTERP string runs past end of file")?;
        for (i, slot) in raw.iter_mut().enumerate() {
            *slot = if i < nb.len() { nb[i] } else { 0 };
        }
    } else {
        // Too long for the slot — GROW. Appending the string and repointing PT_INTERP's file
        // offset is NOT enough: the glibc dynamic linker re-reads the interpreter NAME from
        // MEMORY at `(load_bias + p_vaddr)` when it walks the main program's headers, so the
        // string must live in a MAPPED (PT_LOAD) region. We append the string at EOF and
        // repurpose the non-essential PT_NOTE segment into a PT_LOAD covering it (the standard
        // ELF-patch trick — cheaper than relocating the whole program-header table, and the
        // build-id note it displaces is cosmetic). PT_INTERP then points at the mapped vaddr.
        let (note_ph, load_end) = {
            let elf = Elf::parse(&b)?;
            let (phoff, phentsize, phnum) = elf.phdr_table()?;
            let pv = ph_field(&PField::Vaddr, is64);
            let pm = ph_field(&PField::Memsz, is64);
            let mut note: Option<usize> = None;
            let mut end: u64 = 0;
            for i in 0..phnum {
                let e = elf.phdr_offset(phoff, phentsize, i)?;
                match u32le(&b, e)? {
                    PT_NOTE if note.is_none() => note = Some(e),
                    PT_LOAD => {
                        let va = elf.word(Elf::field_offset(e, pv, "PT_LOAD address")?)?;
                        let msz = elf.word(Elf::field_offset(e, pm, "PT_LOAD size")?)?;
                        let segment_end = va
                            .checked_add(msz)
                            .ok_or("PT_LOAD virtual address range overflow")?;
                        end = end.max(segment_end);
                    }
                    _ => {}
                }
            }
            (
                note.ok_or(
                    "cannot grow PT_INTERP: no PT_NOTE segment to repurpose into a PT_LOAD",
                )?,
                end,
            )
        };
        const PAGE: u64 = 0x1000;
        let new_off = b.len() as u64;
        let new_sz = (nb.len() + 1) as u64;
        b.extend_from_slice(nb);
        b.push(0);
        // A fresh mapped vaddr beyond every existing segment, congruent to the file offset mod
        // page (mmap requires p_vaddr ≡ p_offset (mod p_align)).
        let base = (load_end / PAGE + 2) * PAGE;
        let new_vaddr = base + (new_off % PAGE);
        // Repurpose the PT_NOTE entry as the covering PT_LOAD (read-only).
        set_ph_u32(&mut b, note_ph, is64, PField::Type, PT_LOAD)?;
        set_ph_u32(&mut b, note_ph, is64, PField::Flags, PF_R)?;
        set_ph_word(&mut b, note_ph, is64, PField::Offset, new_off)?;
        set_ph_word(&mut b, note_ph, is64, PField::Vaddr, new_vaddr)?;
        set_ph_word(&mut b, note_ph, is64, PField::Paddr, new_vaddr)?;
        set_ph_word(&mut b, note_ph, is64, PField::Filesz, new_sz)?;
        set_ph_word(&mut b, note_ph, is64, PField::Memsz, new_sz)?;
        set_ph_word(&mut b, note_ph, is64, PField::Align, PAGE)?;
        // Point PT_INTERP at the string's file offset AND its mapped vaddr.
        set_ph_word(&mut b, ph, is64, PField::Offset, new_off)?;
        set_ph_word(&mut b, ph, is64, PField::Vaddr, new_vaddr)?;
        set_ph_word(&mut b, ph, is64, PField::Paddr, new_vaddr)?;
        set_ph_word(&mut b, ph, is64, PField::Filesz, new_sz)?;
        set_ph_word(&mut b, ph, is64, PField::Memsz, new_sz)?;
        set_ph_word(&mut b, ph, is64, PField::Align, 1)?;
    }
    std::fs::write(path, &b).map_err(|e| format!("write {}: {e}", path.display()))?;
    Ok(())
}

/// Rewrite the run-path string of a dynamic ELF in place — every `DT_RPATH` and
/// `DT_RUNPATH` entry is pointed at the new value. The new path (plus its NUL terminator)
/// must fit the existing `.dynstr` slot; any remaining bytes are NUL-padded. Errors
/// (without modifying the file) if the ELF has no run-path to rewrite, or if the new path
/// is too long — the cases that would need growing `.dynstr` (out of scope for this
/// minimal rewriter). Lets a toolchain binary carry an absolute `/td/store/...lib`
/// run-path so it finds its shared libc with no `LD_LIBRARY_PATH` wrapper.
pub fn set_rpath(path: &Path, new_rpath: &str) -> Result<(), String> {
    let nb = new_rpath.as_bytes();
    if nb.contains(&0) {
        return Err("new run-path contains a NUL byte".into());
    }
    let mut b = std::fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let slots = rpath_slots(&b)?.ok_or_else(|| {
        format!(
            "{}: no DT_RPATH/DT_RUNPATH to rewrite (static binary, or no run-path is set — \
             adding one would need growing .dynamic/.dynstr, out of scope for this minimal rewriter)",
            path.display()
        )
    })?;
    // DT_RPATH and DT_RUNPATH may share one .dynstr string; rewrite each distinct slot
    // once. Validate every slot fits BEFORE touching the file so a too-long path is
    // refused atomically (the file is left unchanged).
    let mut offsets: Vec<usize> = slots
        .entries
        .iter()
        .map(|(_, value)| {
            let value = usize::try_from(*value)
                .map_err(|_| "DT_RPATH/DT_RUNPATH string offset does not fit this architecture")?;
            slots
                .strtab_off
                .checked_add(value)
                .ok_or_else(|| "DT_RPATH/DT_RUNPATH string offset overflow".to_string())
        })
        .collect::<Result<_, _>>()?;
    offsets.sort_unstable();
    offsets.dedup();
    let mut terms: Vec<(usize, usize)> = Vec::with_capacity(offsets.len());
    for &off in &offsets {
        let raw = b
            .get(off..)
            .ok_or("DT_RPATH/DT_RUNPATH string offset past end of file")?;
        let term = raw
            .iter()
            .position(|&c| c == 0)
            .ok_or("DT_RPATH/DT_RUNPATH string is not NUL-terminated (corrupt .dynstr)")?;
        if nb.len() > term {
            return Err(format!(
                "new run-path {:?} ({} bytes + NUL) does not fit the {}-byte .dynstr slot \
                 — would need growing .dynstr (out of scope for this minimal rewriter)",
                new_rpath,
                nb.len(),
                term
            ));
        }
        terms.push((off, term));
    }
    for (off, term) in terms {
        // Write the new string then a NUL, NUL-padding the rest of the old slot in place.
        for i in 0..=term {
            b[off + i] = if i < nb.len() { nb[i] } else { 0 };
        }
    }
    std::fs::write(path, &b).map_err(|e| format!("write {}: {e}", path.display()))?;
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::elf_synth::*;
    pub(crate) fn synth_profiled_elf(
        build_ids: &[[u8; 20]],
        with_lines: bool,
        with_symbols: bool,
    ) -> Vec<u8> {
        let names = b"\0.shstrtab\0.note.gnu.build-id\0.symtab\0.debug_line\0.strtab\0.text\0.bss\0.debug_gdb_scripts\0.debug_line_str\0.debug_str\0";
        let name_offset = |name: &[u8]| {
            names
                .windows(name.len())
                .position(|candidate| candidate == name)
                .unwrap()
        };
        let mut note = Vec::new();
        for build_id in build_ids {
            note.extend_from_slice(&4u32.to_le_bytes());
            note.extend_from_slice(&20u32.to_le_bytes());
            note.extend_from_slice(&NT_GNU_BUILD_ID.to_le_bytes());
            note.extend_from_slice(b"GNU\0");
            note.extend_from_slice(build_id);
        }
        let mut line_unit = Vec::new();
        line_unit.extend_from_slice(&17u32.to_le_bytes());
        line_unit.extend_from_slice(&4u16.to_le_bytes());
        line_unit.extend_from_slice(&8u32.to_le_bytes());
        line_unit.extend_from_slice(&[1, 1, 1, 0xfb, 14, 1, 0, 0]);
        line_unit.extend_from_slice(&[0, 1, 1]);
        let mut lines = line_unit.clone();
        lines.extend_from_slice(&line_unit);
        let symbol_names = b"\0td_symbol\0";

        let ehdr = 64usize;
        let names_off = ehdr;
        let note_off = (names_off + names.len() + 3) & !3;
        let symtab_off = note_off + note.len();
        let symtab_size = 48usize;
        let lines_off = symtab_off + symtab_size;
        let strtab_off = lines_off + lines.len();
        let allocated_debug_off = strtab_off + symbol_names.len();
        let line_strings = b"/td-build/src\0";
        let line_strings_off = allocated_debug_off + 1;
        let shoff = (line_strings_off + line_strings.len() + 7) & !7;
        let shentsize = 64usize;
        let shnum = 9usize;
        let mut bytes = vec![0u8; shoff + shentsize * shnum];
        bytes[0..4].copy_from_slice(EI_MAG);
        bytes[EI_CLASS] = 2;
        bytes[EI_DATA] = 1;
        bytes[6] = 1;
        bytes[0x10..0x12].copy_from_slice(&ET_EXEC.to_le_bytes());
        bytes[0x28..0x30].copy_from_slice(&(shoff as u64).to_le_bytes());
        bytes[0x3a..0x3c].copy_from_slice(&(shentsize as u16).to_le_bytes());
        bytes[0x3c..0x3e].copy_from_slice(&(shnum as u16).to_le_bytes());
        bytes[0x3e..0x40].copy_from_slice(&1u16.to_le_bytes());
        bytes[names_off..names_off + names.len()].copy_from_slice(names);
        bytes[note_off..note_off + note.len()].copy_from_slice(&note);
        bytes[symtab_off + 24..symtab_off + 28].copy_from_slice(&1u32.to_le_bytes());
        bytes[lines_off..lines_off + lines.len()].copy_from_slice(&lines);
        bytes[strtab_off..strtab_off + symbol_names.len()].copy_from_slice(symbol_names);
        bytes[line_strings_off..line_strings_off + line_strings.len()]
            .copy_from_slice(line_strings);
        let nobits_off = bytes.len() + 4096;

        let mut section = |index: usize,
                           name: &[u8],
                           kind: u32,
                           flags: u64,
                           off: usize,
                           size: usize,
                           link: u32,
                           entry_size: u64| {
            let header = shoff + index * shentsize;
            bytes[header..header + 4].copy_from_slice(&(name_offset(name) as u32).to_le_bytes());
            bytes[header + 4..header + 8].copy_from_slice(&kind.to_le_bytes());
            bytes[header + 8..header + 16].copy_from_slice(&flags.to_le_bytes());
            bytes[header + 0x18..header + 0x20].copy_from_slice(&(off as u64).to_le_bytes());
            bytes[header + 0x20..header + 0x28].copy_from_slice(&(size as u64).to_le_bytes());
            bytes[header + 0x28..header + 0x2c].copy_from_slice(&link.to_le_bytes());
            bytes[header + 0x38..header + 0x40].copy_from_slice(&entry_size.to_le_bytes());
        };
        section(1, b".shstrtab", SHT_STRTAB, 0, names_off, names.len(), 0, 0);
        section(
            2,
            b".note.gnu.build-id",
            SHT_NOTE,
            0,
            note_off,
            note.len(),
            0,
            0,
        );
        let symbol_name: &[u8] = if with_symbols { b".symtab" } else { b".text" };
        section(
            3,
            symbol_name,
            if with_symbols {
                SHT_SYMTAB
            } else {
                SHT_PROGBITS
            },
            0,
            symtab_off,
            symtab_size,
            5,
            24,
        );
        let line_name: &[u8] = if with_lines { b".debug_line" } else { b".text" };
        section(4, line_name, SHT_PROGBITS, 0, lines_off, lines.len(), 0, 0);
        section(
            5,
            b".strtab",
            SHT_STRTAB,
            0,
            strtab_off,
            symbol_names.len(),
            0,
            0,
        );
        // Debug companions produced by GNU objcopy commonly retain allocated
        // sections as SHT_NOBITS entries whose logical range is not file-backed.
        section(6, b".bss", SHT_NOBITS, SHF_ALLOC, nobits_off, 8192, 0, 0);
        // rustc's allocated debugger-registration payload is deliberately
        // retained by `objcopy --strip-all`; only non-allocated debug
        // sections belong exclusively in the companion.
        section(
            7,
            b".debug_gdb_scripts",
            SHT_PROGBITS,
            SHF_ALLOC,
            allocated_debug_off,
            1,
            0,
            0,
        );
        let line_string_name: &[u8] = if with_lines {
            b".debug_line_str"
        } else {
            b".text"
        };
        section(
            8,
            line_string_name,
            SHT_PROGBITS,
            0,
            line_strings_off,
            line_strings.len(),
            0,
            0,
        );
        bytes
    }

    pub(crate) fn profile_section_u64(bytes: &mut [u8], section: usize, field: usize, value: u64) {
        let shoff = u64le(bytes, 0x28).unwrap() as usize;
        let start = shoff + section * 64 + field;
        bytes
            .get_mut(start..start + 8)
            .unwrap()
            .copy_from_slice(&value.to_le_bytes());
    }

    pub(crate) fn profile_section_u32(bytes: &mut [u8], section: usize, field: usize, value: u32) {
        let shoff = u64le(bytes, 0x28).unwrap() as usize;
        let start = shoff + section * 64 + field;
        bytes
            .get_mut(start..start + 4)
            .unwrap()
            .copy_from_slice(&value.to_le_bytes());
    }

    fn profile_section_offset(bytes: &[u8], section: usize) -> usize {
        let shoff = u64le(bytes, 0x28).unwrap() as usize;
        u64le(bytes, shoff + section * 64 + 0x18).unwrap() as usize
    }

    fn profile_duplicate_section_name(bytes: &mut [u8], source: usize, target: usize) {
        let shoff = u64le(bytes, 0x28).unwrap() as usize;
        let source = shoff + source * 64;
        let target = shoff + target * 64;
        let name = bytes.get(source..source + 4).unwrap().to_vec();
        bytes
            .get_mut(target..target + 4)
            .unwrap()
            .copy_from_slice(&name);
    }

    fn profile_set_section_name(bytes: &mut [u8], section: usize, name: &[u8]) {
        let names_off = profile_section_offset(bytes, 1);
        let shoff = u64le(bytes, 0x28).unwrap() as usize;
        let names_size = u64le(bytes, shoff + 64 + 0x20).unwrap() as usize;
        let relative = bytes
            .get(names_off..names_off + names_size)
            .unwrap()
            .windows(name.len() + 1)
            .position(|candidate| {
                candidate
                    .get(..name.len())
                    .is_some_and(|prefix| prefix == name)
                    && candidate.last() == Some(&0)
            })
            .unwrap();
        profile_section_u32(bytes, section, 0, relative as u32);
    }

    fn v5_line_unit(path_form: u8) -> Vec<u8> {
        let mut header = vec![1, 1, 1, 0xfb, 14, 1];
        header.extend_from_slice(&[1, 1, path_form, 1]);
        header.extend_from_slice(&[0, 0, 0, 0]);
        header.extend_from_slice(&[2, 1, path_form, 2, 0x0f, 1]);
        header.extend_from_slice(&[0, 0, 0, 0, 0]);
        let mut unit = Vec::new();
        let unit_length = 2usize + 2 + 4 + header.len() + 3;
        unit.extend_from_slice(&(unit_length as u32).to_le_bytes());
        unit.extend_from_slice(&5u16.to_le_bytes());
        unit.extend_from_slice(&[8, 0]);
        unit.extend_from_slice(&(header.len() as u32).to_le_bytes());
        unit.extend_from_slice(&header);
        unit.extend_from_slice(&[0, 1, 1]);
        unit
    }

    fn synth_profiled_v5_elf(
        build_ids: &[[u8; 20]],
        path_form: u8,
        string_section: &[u8],
    ) -> Vec<u8> {
        let mut bytes = synth_profiled_elf(build_ids, true, true);
        let line_off = profile_section_offset(&bytes, 4);
        let shoff = u64le(&bytes, 0x28).unwrap() as usize;
        let old_line_size = u64le(&bytes, shoff + 4 * 64 + 0x20).unwrap() as usize;
        let line = v5_line_unit(path_form);
        assert!(line.len() <= old_line_size);
        bytes
            .get_mut(line_off..line_off + old_line_size)
            .unwrap()
            .fill(0);
        bytes
            .get_mut(line_off..line_off + line.len())
            .unwrap()
            .copy_from_slice(&line);
        profile_section_u64(&mut bytes, 4, 0x20, line.len() as u64);
        profile_set_section_name(&mut bytes, 8, string_section);
        bytes
    }

    fn v5_line_header(path_form: u8) -> Vec<u8> {
        let mut header = vec![1, 1, 1, 0xfb, 14, 13];
        header.extend_from_slice(&[0, 1, 1, 1, 1, 0, 0, 0, 1, 0, 0, 1]);
        header.extend_from_slice(&[1, 1, path_form, 1]);
        header.extend_from_slice(&[0, 0, 0, 0]);
        header.extend_from_slice(&[2, 1, path_form, 2, 0x0f, 1]);
        header.extend_from_slice(&[0, 0, 0, 0, 0]);
        header
    }

    #[test]
    fn v5_line_tables_distinguish_debug_and_line_string_forms() {
        let line_str = v5_line_header(0x1f);
        let mut form_values = MAX_PROFILE_LINE_FORM_VALUES;
        assert!(!v5_line_tables_use_debug_str(&line_str, 18, 4, 8, &mut form_values,).unwrap());
        let debug_str = v5_line_header(0x0e);
        let mut form_values = MAX_PROFILE_LINE_FORM_VALUES;
        assert!(v5_line_tables_use_debug_str(&debug_str, 18, 4, 8, &mut form_values,).unwrap());
    }

    #[test]
    fn v5_line_scan_handles_three_byte_forms_and_bounds_work() {
        let data = [1, 2, 3, 4, 5, 6];
        let mut cursor = LineHeaderCursor::new(&data, 0).unwrap();
        skip_line_table_form(&mut cursor, 0x27, 4, 8).unwrap();
        skip_line_table_form(&mut cursor, 0x2b, 4, 8).unwrap();
        assert_eq!(cursor.at, data.len());

        let counts = [2, 1];
        let formats = [LineTableFormat { form: 0x19 }];
        let mut cursor = LineHeaderCursor::new(&counts, 0).unwrap();
        let mut entries = 2;
        let mut form_values = 3;
        skip_line_table_entries(&mut cursor, &formats, 4, 8, &mut entries, &mut form_values)
            .unwrap();
        let error =
            skip_line_table_entries(&mut cursor, &formats, 4, 8, &mut entries, &mut form_values)
                .unwrap_err();
        assert!(error.contains("combined 200000-entry limit"));

        let mut cursor = LineHeaderCursor::new(&counts[..1], 0).unwrap();
        let mut entries = MAX_PROFILE_LINE_TABLE_ENTRIES;
        let mut form_values = 1;
        let error =
            skip_line_table_entries(&mut cursor, &formats, 4, 8, &mut entries, &mut form_values)
                .unwrap_err();
        assert!(error.contains("6400000-form-value object limit"));
    }

    #[test]
    fn profiled_v5_elf_enforces_its_declared_string_dependency() {
        let _dir_cleanup = crate::test_temp::TempDir::new("elf-test-profile-v5").unwrap();
        let dir = _dir_cleanup.join("fixture");
        std::fs::create_dir_all(&dir).unwrap();
        let runtime = dir.join("runtime");
        let debug = dir.join("debug");
        let id = [0x3cu8; 20];
        std::fs::write(&runtime, synth_profiled_elf(&[id], false, false)).unwrap();

        std::fs::write(
            &debug,
            synth_profiled_v5_elf(&[id], 0x1f, b".debug_line_str"),
        )
        .unwrap();
        assert!(!debug_line_requires_debug_str(&debug).unwrap());
        assert_debug_pair(&runtime, &debug).unwrap();

        std::fs::write(&debug, synth_profiled_v5_elf(&[id], 0x0e, b".debug_str")).unwrap();
        assert!(debug_line_requires_debug_str(&debug).unwrap());
        assert_debug_pair(&runtime, &debug).unwrap();

        std::fs::write(
            &debug,
            synth_profiled_v5_elf(&[id], 0x0e, b".debug_line_str"),
        )
        .unwrap();
        assert!(debug_line_requires_debug_str(&debug).unwrap());
        let error = assert_debug_pair(&runtime, &debug).unwrap_err();
        assert!(error.contains("requires a nonempty .debug_str section"));
        assert_debug_pair_with_line_limit(&runtime, &debug, MAX_PROFILE_LINE_SECTION_BYTES, false)
            .unwrap();
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn profiled_elf_requires_one_sha1_id_symbols_and_lines() {
        let _dir_cleanup = crate::test_temp::TempDir::new("elf-test-profile").unwrap();
        let dir = _dir_cleanup.join("fixture");
        std::fs::create_dir_all(&dir).unwrap();
        let runtime = dir.join("runtime");
        let debug = dir.join("debug");
        let id = [0x5au8; 20];
        std::fs::write(&runtime, synth_profiled_elf(&[id], false, false)).unwrap();
        std::fs::write(&debug, synth_profiled_elf(&[id], true, true)).unwrap();
        assert!(is_runtime_elf(&runtime).unwrap());
        assert_eq!(read_build_id(&runtime).unwrap(), id);
        assert!(!debug_line_requires_debug_str(&debug).unwrap());
        assert_debug_pair(&runtime, &debug).unwrap();
        let error = assert_debug_pair_with_line_limit(&runtime, &debug, 1, true).unwrap_err();
        assert!(
            error.contains("exceeding the 1-byte ceiling"),
            "unexpected error: {error}"
        );

        let mut oversized_line_strings = synth_profiled_elf(&[id], true, true);
        let line_strings = profile_section_offset(&oversized_line_strings, 8) as u64;
        let oversized = MAX_PROFILE_LINE_SECTION_BYTES + 1;
        profile_section_u64(&mut oversized_line_strings, 8, 0x20, oversized);
        std::fs::write(&debug, oversized_line_strings).unwrap();
        std::fs::OpenOptions::new()
            .write(true)
            .open(&debug)
            .unwrap()
            .set_len(line_strings + oversized)
            .unwrap();
        let error = assert_debug_pair_with_line_limit(
            &runtime,
            &debug,
            MAX_PROFILE_LINE_SECTION_BYTES * 5,
            true,
        )
        .unwrap_err();
        assert!(
            error.contains(
                ".debug_line_str uses 33554433 bytes, exceeding the 33554432-byte ceiling"
            ),
            "unexpected error: {error}"
        );

        let mut wrong_names_type = synth_profiled_elf(&[id], true, true);
        profile_section_u32(&mut wrong_names_type, 1, 4, SHT_PROGBITS);
        std::fs::write(&debug, wrong_names_type).unwrap();
        let error = assert_debug_pair(&runtime, &debug).unwrap_err();
        assert!(
            error.contains("section-name table is not SHT_STRTAB"),
            "unexpected error: {error}"
        );

        for (source, label) in [(3, ".symtab"), (4, ".debug_line"), (8, ".debug_line_str")] {
            let mut duplicate = synth_profiled_elf(&[id], true, true);
            profile_duplicate_section_name(&mut duplicate, source, 7);
            std::fs::write(&debug, duplicate).unwrap();
            let error = assert_debug_pair(&runtime, &debug).unwrap_err();
            assert!(
                error.contains(&format!("duplicate {label} section")),
                "unexpected error: {error}"
            );
        }

        std::fs::write(&runtime, synth_profiled_elf(&[id], false, true)).unwrap();
        let err = assert_debug_pair(&runtime, &debug).unwrap_err();
        assert!(
            err.contains("ordinary symbol table"),
            "unexpected error: {err}"
        );
        std::fs::write(&runtime, synth_profiled_elf(&[id], false, false)).unwrap();

        std::fs::write(&debug, synth_profiled_elf(&[[0xa5; 20]], true, true)).unwrap();
        let err = assert_debug_pair(&runtime, &debug).unwrap_err();
        assert!(
            err.contains("different GNU build IDs"),
            "unexpected error: {err}"
        );

        std::fs::write(&debug, synth_profiled_elf(&[], true, true)).unwrap();
        let err = read_build_id(&debug).unwrap_err();
        assert!(err.contains("found 0"), "unexpected error: {err}");

        std::fs::write(&debug, synth_profiled_elf(&[id, id], true, true)).unwrap();
        let err = read_build_id(&debug).unwrap_err();
        assert!(err.contains("found 2"), "unexpected error: {err}");

        std::fs::write(&runtime, synth_profiled_elf(&[id], true, false)).unwrap();
        std::fs::write(&debug, synth_profiled_elf(&[id], true, true)).unwrap();
        let err = assert_debug_pair(&runtime, &debug).unwrap_err();
        assert!(
            err.contains("still carries debug section"),
            "unexpected error: {err}"
        );

        std::fs::write(&runtime, synth_profiled_elf(&[id], false, false)).unwrap();
        let mut malformed = synth_profiled_elf(&[id], true, true);
        profile_section_u64(&mut malformed, 3, 0x20, 0);
        std::fs::write(&debug, &malformed).unwrap();
        let err = assert_debug_pair(&runtime, &debug).unwrap_err();
        assert!(
            err.contains(".symtab has size 0"),
            "unexpected error: {err}"
        );

        let mut malformed = synth_profiled_elf(&[id], true, true);
        profile_section_u64(&mut malformed, 3, 0x38, 1);
        std::fs::write(&debug, &malformed).unwrap();
        let err = assert_debug_pair(&runtime, &debug).unwrap_err();
        assert!(err.contains("entry size 1"), "unexpected error: {err}");

        let mut malformed = synth_profiled_elf(&[id], true, true);
        profile_section_u64(&mut malformed, 4, 0x08, SHF_COMPRESSED);
        std::fs::write(&debug, &malformed).unwrap();
        let err = assert_debug_pair(&runtime, &debug).unwrap_err();
        assert!(
            err.contains("compressed .debug_line"),
            "unexpected error: {err}"
        );

        let mut malformed = synth_profiled_elf(&[id], true, true);
        profile_section_u64(&mut malformed, 8, 0x08, SHF_COMPRESSED);
        std::fs::write(&debug, &malformed).unwrap();
        let err = assert_debug_pair(&runtime, &debug).unwrap_err();
        assert!(
            err.contains("compressed .debug_line_str"),
            "unexpected error: {err}"
        );

        let mut mixed = synth_profiled_elf(&[id], true, true);
        let lines = profile_section_offset(&mixed, 4);
        mixed
            .get_mut(lines + 6..lines + 10)
            .unwrap()
            .copy_from_slice(&11u32.to_le_bytes());
        std::fs::write(&debug, &mixed).unwrap();
        assert_debug_pair(&runtime, &debug).unwrap();
        mixed
            .get_mut(lines + 27..lines + 31)
            .unwrap()
            .copy_from_slice(&11u32.to_le_bytes());
        std::fs::write(&debug, &mixed).unwrap();
        let err = assert_debug_pair(&runtime, &debug).unwrap_err();
        assert!(
            err.contains("contains no line program"),
            "unexpected error: {err}"
        );

        let mut malformed = synth_profiled_elf(&[id], true, true);
        let lines = profile_section_offset(&malformed, 4);
        malformed
            .get_mut(lines + 4..lines + 6)
            .unwrap()
            .copy_from_slice(&1u16.to_le_bytes());
        std::fs::write(&debug, &malformed).unwrap();
        let err = assert_debug_pair(&runtime, &debug).unwrap_err();
        assert!(
            err.contains("unsupported DWARF version 1"),
            "unexpected error: {err}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn profiled_line_address_shape_matches_the_runtime_parser() {
        for address_size in [1, 2, 4, 8] {
            validate_line_address_shape(address_size, 0).unwrap();
        }
        assert!(validate_line_address_shape(3, 0)
            .unwrap_err()
            .contains("unsupported address/segment sizes 3/0"));
        assert!(validate_line_address_shape(8, 1)
            .unwrap_err()
            .contains("unsupported address/segment sizes 8/1"));
    }

    #[test]
    fn sets_shorter_interp_and_pads() {
        let _dir_cleanup = crate::test_temp::TempDir::new("elf-test-s").unwrap();
        let dir = _dir_cleanup.join("fixture");
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("a");
        std::fs::write(&f, synth_elf("/lib64/ld-linux-x86-64.so.2")).unwrap();
        let before = std::fs::metadata(&f).unwrap().len();
        set_interp(&f, "/td/store/ld").unwrap();
        // round-trips to the new value, and the file size is unchanged (in-place)
        assert_eq!(read_interp(&f).unwrap().as_deref(), Some("/td/store/ld"));
        assert_eq!(std::fs::metadata(&f).unwrap().len(), before);
        // the tail of the old string is NUL-padded, not left dangling
        let b = std::fs::read(&f).unwrap();
        assert!(b.ends_with(&[0]));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn grows_interp_that_does_not_fit() {
        // A path LONGER than the original slot is no longer refused: it is appended to the end
        // of the file, a PT_NOTE is repurposed as the covering PT_LOAD, and PT_INTERP is
        // repointed at both its file offset and mapped address. This is what lets rustc/cargo
        // name the full hashed /td/store/<hash>-glibc.../ld-linux-x86-64.so.2 loader.
        let _dir_cleanup = crate::test_temp::TempDir::new("elf-test-l").unwrap();
        let dir = _dir_cleanup.join("fixture");
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("a");
        std::fs::write(&f, synth_elf("/lib64/ld.so")).unwrap();
        let before = std::fs::metadata(&f).unwrap().len();
        let long =
            "/td/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-glibc-2.41-x86_64/lib/ld-linux-x86-64.so.2";
        assert!(
            long.len() + 1 > "/lib64/ld.so".len() + 1,
            "the test path must exceed the slot"
        );
        set_interp(&f, long).unwrap();
        // reads back the full long path, and the file GREW (the string was appended)
        assert_eq!(read_interp(&f).unwrap().as_deref(), Some(long));
        let after = std::fs::metadata(&f).unwrap().len();
        assert!(after > before, "file should grow ({before} -> {after})");
        assert_eq!(
            after as usize,
            before as usize + long.len() + 1,
            "grew by exactly the path + NUL"
        );
        // a subsequent SHORTER set still works (fits the now-large slot, in place)
        set_interp(&f, "/td/store/ld").unwrap();
        assert_eq!(read_interp(&f).unwrap().as_deref(), Some("/td/store/ld"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn grows_interp_elf32() {
        // the i686 class grows the same way.
        let _dir_cleanup = crate::test_temp::TempDir::new("elf-test-l32").unwrap();
        let dir = _dir_cleanup.join("fixture");
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("a");
        std::fs::write(&f, synth_interp_elf("/lib/ld.so", false)).unwrap();
        let long = "/td/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-glibc-2.41/lib/ld-linux.so.2";
        set_interp(&f, long).unwrap();
        assert_eq!(read_interp(&f).unwrap().as_deref(), Some(long));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn reads_and_sets_interp_elf32() {
        // i686 PT_INTERP round-trip: read, then rewrite in place to a shorter path.
        let _dir_cleanup = crate::test_temp::TempDir::new("elf-test-32").unwrap();
        let dir = _dir_cleanup.join("fixture");
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("a");
        std::fs::write(&f, synth_interp_elf("/lib/ld-linux.so.2", false)).unwrap();
        assert_eq!(
            read_interp(&f).unwrap().as_deref(),
            Some("/lib/ld-linux.so.2")
        );
        set_interp(&f, "/td/store/ld").unwrap();
        assert_eq!(read_interp(&f).unwrap().as_deref(), Some("/td/store/ld"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn sets_shorter_rpath_and_pads() {
        let _dir_cleanup = crate::test_temp::TempDir::new("elf-test-rps").unwrap();
        let dir = _dir_cleanup.join("fixture");
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("a");
        // a build-dir search path, retargeted to a (shorter) /td/store run-path
        std::fs::write(&f, synth_dyn_elf("/tmp/build-xyz/binutils/lib", true, true)).unwrap();
        let before = std::fs::metadata(&f).unwrap().len();
        set_rpath(&f, "/td/store/glibc/lib").unwrap();
        // round-trips to the new value, in place (file size unchanged), tail NUL-padded
        assert_eq!(
            read_rpath(&f).unwrap().as_deref(),
            Some("/td/store/glibc/lib")
        );
        assert_eq!(std::fs::metadata(&f).unwrap().len(), before);
        assert!(std::fs::read(&f).unwrap().ends_with(&[0]));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn refuses_too_long_rpath() {
        let _dir_cleanup = crate::test_temp::TempDir::new("elf-test-rpt").unwrap();
        let dir = _dir_cleanup.join("fixture");
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("a");
        std::fs::write(&f, synth_dyn_elf("/short/lib", true, true)).unwrap();
        let err = set_rpath(&f, "/td/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/lib").unwrap_err();
        assert!(err.contains("does not fit"), "unexpected error: {err}");
        // the file is unchanged — the old run-path still reads back
        assert_eq!(read_rpath(&f).unwrap().as_deref(), Some("/short/lib"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn rpath_absent_reads_none_and_set_errors() {
        let _dir_cleanup = crate::test_temp::TempDir::new("elf-test-rpa").unwrap();
        let dir = _dir_cleanup.join("fixture");
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("a");
        // an interp-only ELF (no PT_DYNAMIC at all) has no run-path
        std::fs::write(&f, synth_elf("/lib64/ld-linux-x86-64.so.2")).unwrap();
        assert_eq!(read_rpath(&f).unwrap(), None);
        let err = set_rpath(&f, "/td/store/glibc/lib").unwrap_err();
        assert!(
            err.contains("no DT_RPATH/DT_RUNPATH"),
            "unexpected error: {err}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn reads_and_sets_rpath_elf32() {
        // i686 run-path round-trip: a /td/store run-path baked into a 32-bit ELF.
        let _dir_cleanup = crate::test_temp::TempDir::new("elf-test-rp32").unwrap();
        let dir = _dir_cleanup.join("fixture");
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("a");
        std::fs::write(&f, synth_dyn_elf("/tmp/build/binutils/lib", true, false)).unwrap();
        assert_eq!(
            read_rpath(&f).unwrap().as_deref(),
            Some("/tmp/build/binutils/lib")
        );
        set_rpath(&f, "/td/store/glibc/lib").unwrap();
        assert_eq!(
            read_rpath(&f).unwrap().as_deref(),
            Some("/td/store/glibc/lib")
        );
        // legacy DT_RPATH on ELF32 reads back too
        std::fs::write(&f, synth_dyn_elf("/a/b/c", false, false)).unwrap();
        assert_eq!(read_rpath(&f).unwrap().as_deref(), Some("/a/b/c"));
        std::fs::remove_dir_all(&dir).ok();
    }
}

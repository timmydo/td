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
//! Scope: 32- and 64-bit little-endian ELF (i686 + x86-64) — the bootstrap toolchain is
//! i686, the rust/userland path is x86-64. Any other class/endianness is rejected.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unreachable, clippy::todo, clippy::unimplemented, clippy::indexing_slicing)] // grandfathered: pre-dates the rust-lint rules (AGENTS.md); remove when cleaned

use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

// ELF identification (class-independent).
const EI_MAG: &[u8] = b"\x7fELF";
const EI_CLASS: usize = 4; // 1 = ELFCLASS32, 2 = ELFCLASS64
const EI_DATA: usize = 5; // 1 = ELFDATA2LSB

// Program-header types and dynamic-section tags (class-independent values).
const PT_LOAD: u32 = 1;
const PT_DYNAMIC: u32 = 2;
const PT_INTERP: u32 = 3;
const PT_NOTE: u32 = 4;
const PF_X: u32 = 1; // segment executable
const PF_R: u32 = 4; // segment readable
const ET_EXEC: u16 = 2;
const ET_DYN: u16 = 3;
const EM_X86_64: u16 = 62;
const EV_CURRENT: u32 = 1;
const DT_NULL: u64 = 0; // end of the dynamic array
// Backs the `read_needed`/`assert_static` DT_NEEDED query. assert_static is now
// live in-crate: the bootstrap rungs' `Step::AssertStatic` calls it to reject a
// host loader/libc leak (re #469), so needed_slots → read_needed → assert_static
// is reachable and DT_NEEDED is used — no dead-code allow needed.
const DT_NEEDED: u64 = 1; // .dynstr offset of a required shared-object name
const DT_STRTAB: u64 = 5; // vaddr of the .dynstr string table
const DT_RPATH: u64 = 15; // legacy run-path (string offset into .dynstr)
const DT_RUNPATH: u64 = 29; // run-path, takes precedence over DT_RPATH at load time
const DT_DEPAUDIT: u64 = 0x6fff_fefb;
const DT_AUDIT: u64 = 0x6fff_fefc;
const DT_AUXILIARY: u64 = 0x7fff_fffd;
const DT_FILTER: u64 = 0x7fff_ffff;
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
                return i64::try_from(value)
                    .map_err(|_| ".debug_line SLEB128 overflows".into());
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
        _ => return Err(format!(".debug_line has unsupported version-5 form {form:#x}")),
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
    let format_count = u64::try_from(formats.len())
        .map_err(|_| ".debug_line format count does not fit u64")?;
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

fn u16le(b: &[u8], off: usize) -> Result<u16, String> {
    let end = off
        .checked_add(2)
        .ok_or_else(|| format!("ELF u16 offset {off} overflows"))?;
    let bytes = b
        .get(off..end)
        .ok_or_else(|| format!("ELF truncated at u16 offset {off}"))?
        .try_into()
        .map_err(|_| format!("ELF invalid u16 width at offset {off}"))?;
    Ok(u16::from_le_bytes(bytes))
}
fn u32le(b: &[u8], off: usize) -> Result<u32, String> {
    let end = off
        .checked_add(4)
        .ok_or_else(|| format!("ELF u32 offset {off} overflows"))?;
    let bytes = b
        .get(off..end)
        .ok_or_else(|| format!("ELF truncated at u32 offset {off}"))?
        .try_into()
        .map_err(|_| format!("ELF invalid u32 width at offset {off}"))?;
    Ok(u32::from_le_bytes(bytes))
}
fn u64le(b: &[u8], off: usize) -> Result<u64, String> {
    let end = off
        .checked_add(8)
        .ok_or_else(|| format!("ELF u64 offset {off} overflows"))?;
    let bytes = b
        .get(off..end)
        .ok_or_else(|| format!("ELF truncated at u64 offset {off}"))?
        .try_into()
        .map_err(|_| format!("ELF invalid u64 width at offset {off}"))?;
    Ok(u64::from_le_bytes(bytes))
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

/// A validated little-endian ELF buffer carrying its class. The header + program-header +
/// dynamic-entry field offsets differ between ELFCLASS32 (i686) and ELFCLASS64 (x86-64);
/// every class-dependent access goes through one of these methods so the PT_INTERP and
/// DT_RPATH/DT_RUNPATH paths share a single class dispatch.
struct Elf<'a> {
    b: &'a [u8],
    is64: bool,
}

impl<'a> Elf<'a> {
    fn parse(b: &'a [u8]) -> Result<Elf<'a>, String> {
        if b.len() < 52 || &b[0..4] != EI_MAG {
            return Err("not an ELF file (bad magic)".into());
        }
        let is64 = match b[EI_CLASS] {
            1 => false,
            2 => true,
            c => return Err(format!("unknown ELF class {c} (only ELFCLASS32/64 supported)")),
        };
        if b[EI_DATA] != 1 {
            return Err("not ELFDATA2LSB (only little-endian ELF is supported)".into());
        }
        Ok(Elf { b, is64 })
    }

    /// Read a class-width word — u64 on ELF64, u32 (zero-extended) on ELF32.
    fn word(&self, off: usize) -> Result<u64, String> {
        if self.is64 {
            u64le(self.b, off)
        } else {
            Ok(u32le(self.b, off)? as u64)
        }
    }

    /// `(e_phoff, e_phentsize, e_phnum)` for the program-header table.
    fn phdr_table(&self) -> Result<(usize, usize, usize), String> {
        // (e_phoff, e_phentsize, e_phnum, min plausible phentsize) per class.
        let (off, ents, num, min_ents) = if self.is64 {
            (0x20, 0x36, 0x38, 0x38)
        } else {
            (0x1C, 0x2A, 0x2C, 0x20)
        };
        let phoff = usize::try_from(self.word(off)?)
            .map_err(|_| "ELF program-header offset does not fit this architecture")?;
        let phentsize = u16le(self.b, ents)? as usize;
        let phnum = u16le(self.b, num)? as usize;
        if phnum != 0 && phentsize < min_ents {
            return Err(format!("implausible e_phentsize {phentsize}"));
        }
        Ok((phoff, phentsize, phnum))
    }

    fn phdr_offset(&self, phoff: usize, phentsize: usize, index: usize) -> Result<usize, String> {
        index
            .checked_mul(phentsize)
            .and_then(|offset| phoff.checked_add(offset))
            .ok_or_else(|| format!("ELF program-header {index} offset overflow"))
    }

    fn field_offset(ph: usize, field: usize, what: &str) -> Result<usize, String> {
        ph.checked_add(field)
            .ok_or_else(|| format!("ELF {what} field offset overflow"))
    }

    /// `(p_offset, p_vaddr, p_filesz)` field offsets within a program-header entry.
    fn ph_fields(&self) -> (usize, usize, usize) {
        if self.is64 {
            (0x08, 0x10, 0x20)
        } else {
            (0x04, 0x08, 0x10)
        }
    }

    /// Locate the first program header of type `pt` and return `(file_offset, filesz)` of
    /// the data it points at, or `None` if no such segment exists.
    fn segment_slot(&self, pt: u32, what: &str) -> Result<Option<(usize, usize)>, String> {
        let (phoff, phentsize, phnum) = self.phdr_table()?;
        let (p_off, _p_vaddr, p_filesz) = self.ph_fields();
        for i in 0..phnum {
            let ph = self.phdr_offset(phoff, phentsize, i)?;
            if u32le(self.b, ph)? == pt {
                let off = usize::try_from(self.word(Self::field_offset(ph, p_off, what)?)?)
                    .map_err(|_| format!("{what} offset does not fit this architecture"))?;
                let sz = usize::try_from(self.word(Self::field_offset(ph, p_filesz, what)?)?)
                    .map_err(|_| format!("{what} size does not fit this architecture"))?;
                let end = off
                    .checked_add(sz)
                    .ok_or_else(|| format!("{what} file range overflows"))?;
                if end > self.b.len() {
                    return Err(format!("{what} runs past end of file"));
                }
                return Ok(Some((off, sz)));
            }
        }
        Ok(None)
    }

    /// Map a virtual address to its file offset via the PT_LOAD segment that contains it,
    /// or `None` if no loadable segment covers it.
    fn vaddr_to_off(&self, vaddr: u64) -> Result<Option<usize>, String> {
        let (phoff, phentsize, phnum) = self.phdr_table()?;
        let (p_off, p_vaddr, p_filesz) = self.ph_fields();
        for i in 0..phnum {
            let ph = self.phdr_offset(phoff, phentsize, i)?;
            if u32le(self.b, ph)? != PT_LOAD {
                continue;
            }
            let off = self.word(Self::field_offset(ph, p_off, "PT_LOAD offset")?)?;
            let va = self.word(Self::field_offset(ph, p_vaddr, "PT_LOAD address")?)?;
            let fsz = self.word(Self::field_offset(ph, p_filesz, "PT_LOAD size")?)?;
            let va_end = va
                .checked_add(fsz)
                .ok_or("PT_LOAD virtual address range overflow")?;
            if vaddr >= va && vaddr < va_end {
                let file_offset = off
                    .checked_add(vaddr - va)
                    .ok_or("PT_LOAD file offset overflow")?;
                let file_offset = usize::try_from(file_offset)
                    .map_err(|_| "PT_LOAD file offset does not fit this architecture")?;
                if file_offset >= self.b.len() {
                    return Err("PT_LOAD mapped address runs past end of file".into());
                }
                return Ok(Some(file_offset));
            }
        }
        Ok(None)
    }
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
        let mut file = std::fs::File::open(path)
            .map_err(|e| format!("read {}: {e}", path.display()))?;
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
            return Err(format!("{}: ELF has no section-header table", path.display()));
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
                .ok_or_else(|| format!("{}: ELF section-header {index} overflows", path.display()))?;
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
                return Err(format!(".debug_line has unsupported DWARF version {version}"));
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
        for section in self.sections.iter().filter(|section| section.kind == SHT_NOTE) {
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
    let mut file = std::fs::File::open(path)
        .map_err(|e| format!("read {}: {e}", path.display()))?;
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
    let line = line
        .ok_or_else(|| format!("{}: debug companion has no .debug_line", path.display()))?;
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
        return Err(format!("{}: debug-pair runtime is not ET_EXEC/ET_DYN", runtime.display()));
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

/// Locate the PT_INTERP program header and return `(file_offset, filesz)` of its
/// interpreter string, or `None` if the ELF has no interpreter (e.g. a shared object).
fn interp_slot(b: &[u8]) -> Result<Option<(usize, usize)>, String> {
    Elf::parse(b)?.segment_slot(PT_INTERP, "PT_INTERP string")
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
            let off = usize::try_from(elf.word(Elf::field_offset(
                ph,
                p_off,
                "PT_INTERP offset",
            )?)?)
            .map_err(|_| "PT_INTERP string offset does not fit this architecture")?;
            let sz = usize::try_from(elf.word(Elf::field_offset(
                ph,
                p_filesz,
                "PT_INTERP size",
            )?)?)
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

/// The .dynstr file offset plus the `(tag, string-offset)` of every DT_RPATH/DT_RUNPATH
/// entry, or `None` if the ELF has no PT_DYNAMIC or no run-path entry at all.
struct RpathSlots {
    strtab_off: usize,        // file offset of .dynstr (DT_STRTAB vaddr mapped through PT_LOAD)
    entries: Vec<(u64, u64)>, // (DT_RPATH|DT_RUNPATH, string offset into .dynstr)
}

fn terminated_dynamic_entry_count(
    b: &[u8],
    elf: &Elf<'_>,
    offset: usize,
    size: usize,
    entry_size: usize,
    max_dynamic_entries: usize,
) -> Result<usize, String> {
    if size % entry_size != 0 {
        return Err("PT_DYNAMIC size is not aligned to its entry size".into());
    }
    let count = size / entry_size;
    if count > max_dynamic_entries {
        return Err(format!(
            "PT_DYNAMIC exceeds {max_dynamic_entries} entries"
        ));
    }
    for index in 0..count {
        let entry = index
            .checked_mul(entry_size)
            .and_then(|value| offset.checked_add(value))
            .ok_or("PT_DYNAMIC entry offset overflow")?;
        if elf.word(entry)? == DT_NULL {
            return Ok(index + 1);
        }
    }
    let end = offset
        .checked_add(size)
        .ok_or("PT_DYNAMIC file range overflows")?;
    if end > b.len() {
        return Err("PT_DYNAMIC runs past end of file".into());
    }
    Err("PT_DYNAMIC has no DT_NULL terminator inside its file range".into())
}

fn rpath_slots(b: &[u8]) -> Result<Option<RpathSlots>, String> {
    rpath_slots_with_limit(b, usize::MAX)
}

fn rpath_slots_with_limit(
    b: &[u8],
    max_dynamic_entries: usize,
) -> Result<Option<RpathSlots>, String> {
    let elf = Elf::parse(b)?;
    let (doff, dsize) = match elf.segment_slot(PT_DYNAMIC, "PT_DYNAMIC")? {
        None => return Ok(None), // static binary — no dynamic section
        Some(x) => x,
    };
    // Elf64_Dyn is 16 bytes (d_tag u64 @0, d_un u64 @8); Elf32_Dyn is 8 (u32 @0, u32 @4).
    let (entsize, d_un) = if elf.is64 { (16, 8) } else { (8, 4) };
    let entry_count = terminated_dynamic_entry_count(
        b,
        &elf,
        doff,
        dsize,
        entsize,
        max_dynamic_entries,
    )?;
    let mut strtab_vaddr: Option<u64> = None;
    let mut entries: Vec<(u64, u64)> = Vec::new();
    for i in 0..entry_count {
        let e = i
            .checked_mul(entsize)
            .and_then(|offset| doff.checked_add(offset))
            .ok_or("PT_DYNAMIC entry offset overflow")?;
        let tag = elf.word(e)?;
        let val = elf.word(
            e.checked_add(d_un)
                .ok_or("PT_DYNAMIC value offset overflow")?,
        )?;
        match tag {
            DT_NULL => break,
            DT_STRTAB => strtab_vaddr = Some(val),
            DT_RPATH | DT_RUNPATH => entries.push((tag, val)),
            _ => {}
        }
    }
    if entries.is_empty() {
        return Ok(None); // dynamic, but no run-path set
    }
    let sv = strtab_vaddr.ok_or("dynamic section has DT_RPATH/DT_RUNPATH but no DT_STRTAB")?;
    let strtab_off = elf
        .vaddr_to_off(sv)?
        .ok_or("DT_STRTAB vaddr is not covered by any PT_LOAD segment")?;
    Ok(Some(RpathSlots { strtab_off, entries }))
}

/// The .dynstr file offset plus the `.dynstr` string offset of every DT_NEEDED entry (each
/// names a shared object the loader would pull in at run time), or `None` if the ELF has no
/// PT_DYNAMIC or no DT_NEEDED at all. Mirrors `rpath_slots`: a fully static binary — the
/// static-bootstrap contract — has neither a dynamic section nor any needed library.
struct NeededSlots {
    strtab_off: usize,  // file offset of .dynstr (DT_STRTAB vaddr mapped through PT_LOAD)
    offsets: Vec<u64>,  // string offset into .dynstr of each DT_NEEDED name
}

fn needed_slots(b: &[u8]) -> Result<Option<NeededSlots>, String> {
    needed_slots_with_limit(b, usize::MAX)
}

fn needed_slots_with_limit(
    b: &[u8],
    max_dynamic_entries: usize,
) -> Result<Option<NeededSlots>, String> {
    let elf = Elf::parse(b)?;
    let (doff, dsize) = match elf.segment_slot(PT_DYNAMIC, "PT_DYNAMIC")? {
        None => return Ok(None), // static binary — no dynamic section
        Some(x) => x,
    };
    // Elf64_Dyn is 16 bytes (d_tag u64 @0, d_un u64 @8); Elf32_Dyn is 8 (u32 @0, u32 @4).
    let (entsize, d_un) = if elf.is64 { (16, 8) } else { (8, 4) };
    let entry_count = terminated_dynamic_entry_count(
        b,
        &elf,
        doff,
        dsize,
        entsize,
        max_dynamic_entries,
    )?;
    let mut strtab_vaddr: Option<u64> = None;
    let mut offsets: Vec<u64> = Vec::new();
    for i in 0..entry_count {
        let e = i
            .checked_mul(entsize)
            .and_then(|offset| doff.checked_add(offset))
            .ok_or("PT_DYNAMIC entry offset overflow")?;
        let tag = elf.word(e)?;
        let val = elf.word(
            e.checked_add(d_un)
                .ok_or("PT_DYNAMIC value offset overflow")?,
        )?;
        match tag {
            DT_NULL => break,
            DT_STRTAB => strtab_vaddr = Some(val),
            DT_NEEDED => offsets.push(val),
            _ => {}
        }
    }
    if offsets.is_empty() {
        return Ok(None); // dynamic, but links no shared objects
    }
    let sv = strtab_vaddr.ok_or("dynamic section has DT_NEEDED but no DT_STRTAB")?;
    let strtab_off = elf
        .vaddr_to_off(sv)?
        .ok_or("DT_STRTAB vaddr is not covered by any PT_LOAD segment")?;
    Ok(Some(NeededSlots { strtab_off, offsets }))
}

/// Read the program interpreter (`PT_INTERP`) string of an ELF file, or `None` if it has
/// no interpreter (a shared object / PIE library).
pub fn read_interp(path: &Path) -> Result<Option<String>, String> {
    let b = std::fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    match interp_slot(&b)? {
        None => Ok(None),
        Some((off, sz)) => {
            let end = off
                .checked_add(sz)
                .ok_or("PT_INTERP string file range overflows")?;
            let raw = b
                .get(off..end)
                .ok_or("PT_INTERP string runs past end of file")?;
            let end = raw.iter().position(|&c| c == 0).unwrap_or(raw.len());
            Ok(Some(String::from_utf8_lossy(&raw[..end]).into_owned()))
        }
    }
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
    let (ph, off, sz, is64) = interp_ph_entry(&b)?
        .ok_or_else(|| format!("{}: no PT_INTERP (not an interpreted executable)", path.display()))?;
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
                note.ok_or("cannot grow PT_INTERP: no PT_NOTE segment to repurpose into a PT_LOAD")?,
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

/// Read the run-path of a dynamic ELF — its `DT_RUNPATH` (which the loader prefers) or, if
/// absent, the legacy `DT_RPATH`. Returns `None` for a static binary or one with no run-path.
pub fn read_rpath(path: &Path) -> Result<Option<String>, String> {
    let b = std::fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let slots = match rpath_slots(&b)? {
        None => return Ok(None),
        Some(s) => s,
    };
    // entries is non-empty (rpath_slots returns None otherwise); prefer DT_RUNPATH.
    let pick = slots
        .entries
        .iter()
        .find(|(t, _)| *t == DT_RUNPATH)
        .or_else(|| slots.entries.first())
        .ok_or("run-path slot set is unexpectedly empty")?;
    let string_offset = usize::try_from(pick.1)
        .map_err(|_| "DT_RPATH/DT_RUNPATH string offset does not fit this architecture")?;
    let off = slots
        .strtab_off
        .checked_add(string_offset)
        .ok_or("DT_RPATH/DT_RUNPATH string offset overflow")?;
    let raw = b.get(off..).ok_or("DT_RPATH/DT_RUNPATH string offset past end of file")?;
    let end = raw.iter().position(|&c| c == 0).unwrap_or(raw.len());
    Ok(Some(String::from_utf8_lossy(&raw[..end]).into_owned()))
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
            let value = usize::try_from(*value).map_err(|_| {
                "DT_RPATH/DT_RUNPATH string offset does not fit this architecture"
            })?;
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
        let raw = b.get(off..).ok_or("DT_RPATH/DT_RUNPATH string offset past end of file")?;
        let term = raw
            .iter()
            .position(|&c| c == 0)
            .ok_or("DT_RPATH/DT_RUNPATH string is not NUL-terminated (corrupt .dynstr)")?;
        if nb.len() > term {
            return Err(format!(
                "new run-path {:?} ({} bytes + NUL) does not fit the {}-byte .dynstr slot \
                 — would need growing .dynstr (out of scope for this minimal rewriter)",
                new_rpath, nb.len(), term
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

/// The dynamic-linkage SEARCH references of a file: its program interpreter (`PT_INTERP`),
/// every colon-separated entry of its `DT_RUNPATH`/`DT_RPATH` run-path, and its
/// `DT_NEEDED` entries. A needed entry containing `/` is itself a loader pathname, while a
/// soname is resolved through the run-path. A NON-ELF file (a shell script, a header, a
/// static archive) yields three empty values; a static ELF does likewise. Reads the file at
/// most once.
///
/// This is the loader's OWN view, and it is deliberately NARROWER than a content scan
/// (`guix gc -R` / `scan::Scanner`): a store item can NAME another store path in a string
/// CONSTANT the loader never links — glibc's `libc.so.6` bakes the absolute bash-static
/// path into its `_PATH_BSHELL` constant (the default shell of `system()`/`popen()`), so a
/// content scan drags a runnable host shell into the control-plane builder's runtime closure
/// and thus the sandbox. Resolving the builder's closure by THIS search set instead stages
/// exactly the interpreter + run-path dirs the loader uses — glibc/gcc-lib — and leaves the
/// host shell absent (re #469). The run-path entries are returned verbatim (absolute store
/// dirs for a Guix binary, possibly with unnormalized `..` tails or `$ORIGIN`); the caller
/// extracts the store PATH, for which the `..` tail is irrelevant.
pub fn runtime_link_search(
    path: &Path,
) -> Result<(Option<String>, Vec<String>, Vec<String>), String> {
    let mut file =
        std::fs::File::open(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let mut magic = [0u8; 4];
    match std::io::Read::read_exact(&mut file, &mut magic) {
        Ok(()) if magic == EI_MAG => {}
        Ok(()) => return Ok((None, Vec::new(), Vec::new())),
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
            return Ok((None, Vec::new(), Vec::new()));
        }
        Err(e) => return Err(format!("read {}: {e}", path.display())),
    }
    let mut b = magic.to_vec();
    std::io::Read::read_to_end(&mut file, &mut b)
        .map_err(|e| format!("read {}: {e}", path.display()))?;
    let interp = match interp_slot(&b)? {
        None => None,
        Some((off, sz)) => {
            let end = off
                .checked_add(sz)
                .ok_or("PT_INTERP string file range overflows")?;
            let raw = b
                .get(off..end)
                .ok_or("PT_INTERP string runs past end of file")?;
            let end = raw.iter().position(|&c| c == 0).unwrap_or(raw.len());
            Some(String::from_utf8_lossy(&raw[..end]).into_owned())
        }
    };
    let mut dirs: Vec<String> = Vec::new();
    if let Some(slots) = rpath_slots(&b)? {
        // Every DT_RPATH and DT_RUNPATH slot (the loader prefers RUNPATH, but a closure over
        // ALL run-path store dirs is the safe superset — it never DROPS a real provider dir).
        for (_tag, v) in &slots.entries {
            let string_offset = usize::try_from(*v).map_err(|_| {
                "DT_RPATH/DT_RUNPATH string offset does not fit this architecture"
            })?;
            let off = slots
                .strtab_off
                .checked_add(string_offset)
                .ok_or("DT_RPATH/DT_RUNPATH string offset overflow")?;
            let raw = b
                .get(off..)
                .ok_or("DT_RPATH/DT_RUNPATH string offset past end of file")?;
            let end = raw.iter().position(|&c| c == 0).unwrap_or(raw.len());
            for entry in String::from_utf8_lossy(&raw[..end]).split(':') {
                if !entry.is_empty() {
                    dirs.push(entry.to_string());
                }
            }
        }
    }
    Ok((interp, dirs, needed_names(&b)?))
}

/// Admission limits for an ELF supplied by a foreign application package.
///
/// The ordinary bootstrap closure reader predates hostile-package admission
/// and retains its existing API above. Application admission uses this closed
/// variant so no file, dynamic array, string, or returned reference list can
/// allocate before a corresponding limit is checked.
#[derive(Clone, Copy, Debug)]
pub(crate) struct RuntimeLinkLimits {
    pub(crate) file_bytes: u64,
    pub(crate) dynamic_entries: usize,
    pub(crate) references: usize,
    pub(crate) string_bytes: usize,
    pub(crate) aggregate_text_bytes: usize,
}

#[derive(Clone, Debug)]
pub(crate) struct RuntimeLinkSearch {
    pub(crate) file_bytes: u64,
    pub(crate) kind: RuntimeElfKind,
    pub(crate) executable: bool,
    pub(crate) interpreter: Option<String>,
    pub(crate) run_paths: Vec<String>,
    pub(crate) run_path_kind: RuntimeRunPathKind,
    pub(crate) needed: Vec<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RuntimeRunPathKind {
    None,
    Rpath,
    Runpath,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RuntimeElfKind {
    NotElf,
    Executable,
    SharedObject,
    Other(u16),
}

fn validate_runtime_loadable_shape(bytes: &[u8], elf: &Elf<'_>) -> Result<(), String> {
    if bytes.get(6).copied() != Some(1) || u32le(bytes, 0x14)? != EV_CURRENT {
        return Err("dynamic application ELF has an unsupported version".into());
    }
    if u16le(bytes, 0x34)? != 64 {
        return Err("dynamic application ELF has an invalid ELF64 header size".into());
    }
    let (phoff, phentsize, phnum) = elf.phdr_table()?;
    if phnum == 0 || phentsize != 56 {
        return Err(
            "dynamic application loadable ELF has no canonical program-header table".into(),
        );
    }
    let table_end = phentsize
        .checked_mul(phnum)
        .and_then(|size| phoff.checked_add(size))
        .ok_or("dynamic application ELF program-header table overflows")?;
    if table_end > bytes.len() {
        return Err("dynamic application ELF program-header table runs past end of file".into());
    }
    let mut loadable = false;
    for index in 0..phnum {
        let offset = index
            .checked_mul(phentsize)
            .and_then(|value| phoff.checked_add(value))
            .ok_or("dynamic application ELF program-header offset overflows")?;
        if u32le(bytes, offset)? != PT_LOAD {
            continue;
        }
        loadable = true;
        let file_offset = elf.word(Elf::field_offset(offset, 0x08, "PT_LOAD file offset")?)?;
        let virtual_address = elf.word(Elf::field_offset(
            offset,
            0x10,
            "PT_LOAD virtual address",
        )?)?;
        let file_size = elf.word(Elf::field_offset(offset, 0x20, "PT_LOAD file size")?)?;
        let memory_size = elf.word(Elf::field_offset(offset, 0x28, "PT_LOAD memory size")?)?;
        if file_size > memory_size {
            return Err("dynamic application PT_LOAD has p_filesz larger than p_memsz".into());
        }
        let file_end = file_offset
            .checked_add(file_size)
            .ok_or("dynamic application PT_LOAD file range overflows")?;
        if file_end > bytes.len() as u64 {
            return Err("dynamic application PT_LOAD runs past end of file".into());
        }
        virtual_address
            .checked_add(memory_size)
            .ok_or("dynamic application PT_LOAD memory range overflows")?;
    }
    if !loadable {
        return Err("dynamic application loadable ELF has no PT_LOAD segment".into());
    }
    Ok(())
}

fn bounded_dynamic_string<'a>(
    bytes: &'a [u8],
    offset: usize,
    limits: RuntimeLinkLimits,
    aggregate: &mut usize,
    label: &str,
) -> Result<&'a str, String> {
    let raw = bytes
        .get(offset..)
        .ok_or_else(|| format!("{label} string offset is past end of file"))?;
    let scan_bytes = limits
        .string_bytes
        .checked_add(1)
        .ok_or("dynamic string limit overflows")?;
    let bounded = raw
        .get(..raw.len().min(scan_bytes))
        .ok_or("dynamic string range is invalid")?;
    let end = bounded.iter().position(|byte| *byte == 0).ok_or_else(|| {
        format!(
            "{label} is not NUL-terminated within {} bytes",
            limits.string_bytes
        )
    })?;
    *aggregate = aggregate
        .checked_add(end)
        .ok_or("dynamic string aggregate overflows")?;
    if *aggregate > limits.aggregate_text_bytes {
        return Err(format!(
            "dynamic strings exceed {} aggregate bytes",
            limits.aggregate_text_bytes
        ));
    }
    std::str::from_utf8(
        bounded
            .get(..end)
            .ok_or("dynamic string range is invalid")?,
    )
    .map_err(|_| format!("{label} is not UTF-8"))
}

fn account_runtime_reference(
    references: &mut usize,
    limits: RuntimeLinkLimits,
) -> Result<(), String> {
    *references = references
        .checked_add(1)
        .ok_or("dynamic reference count overflows")?;
    if *references > limits.references {
        return Err(format!(
            "dynamic linkage exceeds {} references",
            limits.references
        ));
    }
    Ok(())
}

fn reject_unmodeled_loader_objects(
    bytes: &[u8],
    max_dynamic_entries: usize,
) -> Result<(), String> {
    let elf = Elf::parse(bytes)?;
    let (offset, size) = match elf.segment_slot(PT_DYNAMIC, "PT_DYNAMIC")? {
        Some(slot) => slot,
        None => return Ok(()),
    };
    let (entry_size, value_offset) = if elf.is64 { (16, 8) } else { (8, 4) };
    let entry_count = terminated_dynamic_entry_count(
        bytes,
        &elf,
        offset,
        size,
        entry_size,
        max_dynamic_entries,
    )?;
    for index in 0..entry_count {
        let entry = index
            .checked_mul(entry_size)
            .and_then(|value| offset.checked_add(value))
            .ok_or("PT_DYNAMIC entry offset overflow")?;
        let tag = elf.word(entry)?;
        if tag == DT_NULL {
            break;
        }
        if matches!(tag, DT_DEPAUDIT | DT_AUDIT | DT_AUXILIARY | DT_FILTER) {
            let _ = elf.word(
                entry
                    .checked_add(value_offset)
                    .ok_or("PT_DYNAMIC value offset overflow")?,
            )?;
            return Err(format!(
                "dynamic application ELF uses unsupported loader object tag {tag:#x}"
            ));
        }
    }
    Ok(())
}

pub(crate) fn runtime_link_search_bounded(
    path: &Path,
    limits: RuntimeLinkLimits,
) -> Result<RuntimeLinkSearch, String> {
    if limits.file_bytes < 4
        || limits.dynamic_entries == 0
        || limits.references == 0
        || limits.string_bytes == 0
        || limits.aggregate_text_bytes == 0
    {
        return Err("dynamic linkage limits must all be nonzero".into());
    }
    let mut file = std::fs::File::open(path)
        .map_err(|error| format!("read {}: {error}", path.display()))?;
    let metadata = file
        .metadata()
        .map_err(|error| format!("stat {}: {error}", path.display()))?;
    if metadata.len() > limits.file_bytes {
        return Err(format!(
            "{} exceeds the {}-byte dynamic ELF limit",
            path.display(),
            limits.file_bytes
        ));
    }
    let mut magic = [0u8; 4];
    match std::io::Read::read_exact(&mut file, &mut magic) {
        Ok(()) if magic == EI_MAG => {}
        Ok(()) => {
            return Ok(RuntimeLinkSearch {
                file_bytes: metadata.len(),
                kind: RuntimeElfKind::NotElf,
                executable: false,
                interpreter: None,
                run_paths: Vec::new(),
                run_path_kind: RuntimeRunPathKind::None,
                needed: Vec::new(),
            });
        }
        Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => {
            return Ok(RuntimeLinkSearch {
                file_bytes: metadata.len(),
                kind: RuntimeElfKind::NotElf,
                executable: false,
                interpreter: None,
                run_paths: Vec::new(),
                run_path_kind: RuntimeRunPathKind::None,
                needed: Vec::new(),
            });
        }
        Err(error) => return Err(format!("read {}: {error}", path.display())),
    }
    let mut bytes = magic.to_vec();
    // Four bytes are already in `magic`; `file_bytes - 3` reads one byte
    // beyond the admitted size so concurrent growth is detected without an
    // unbounded read.
    std::io::Read::read_to_end(
        &mut file.take(limits.file_bytes.saturating_sub(3)),
        &mut bytes,
    )
    .map_err(|error| format!("read {}: {error}", path.display()))?;
    let read_bytes = u64::try_from(bytes.len())
        .map_err(|_| "dynamic application ELF length does not fit u64")?;
    if read_bytes > limits.file_bytes {
        return Err(format!(
            "{} changed while reading and exceeds the {}-byte dynamic ELF limit",
            path.display(),
            limits.file_bytes
        ));
    }
    let elf = Elf::parse(&bytes)?;
    if !elf.is64 {
        return Err(format!(
            "{}: dynamic application object is not ELFCLASS64",
            path.display()
        ));
    }
    if u16le(&bytes, 0x12)? != EM_X86_64 {
        return Err(format!(
            "{}: dynamic application object is not EM_X86_64",
            path.display()
        ));
    }
    let elf_type = u16le(&bytes, 0x10)?;
    let kind = match elf_type {
        ET_EXEC => RuntimeElfKind::Executable,
        ET_DYN => RuntimeElfKind::SharedObject,
        other => RuntimeElfKind::Other(other),
    };
    if matches!(kind, RuntimeElfKind::Executable | RuntimeElfKind::SharedObject) {
        validate_runtime_loadable_shape(&bytes, &elf)?;
    }

    let mut aggregate = 0usize;
    let mut references = 0usize;
    reject_unmodeled_loader_objects(&bytes, limits.dynamic_entries)?;
    let interpreter = match interp_slot(&bytes)? {
        None => None,
        Some((offset, size)) => {
            let end = offset
                .checked_add(size)
                .ok_or("PT_INTERP string file range overflows")?;
            if size > limits.string_bytes.saturating_add(1) {
                return Err(format!(
                    "PT_INTERP exceeds {} bytes",
                    limits.string_bytes
                ));
            }
            let raw = bytes
                .get(offset..end)
                .ok_or("PT_INTERP string runs past end of file")?;
            let value = bounded_dynamic_string(raw, 0, limits, &mut aggregate, "PT_INTERP")?;
            account_runtime_reference(&mut references, limits)?;
            Some(value.to_string())
        }
    };

    let mut run_paths = Vec::new();
    let mut run_path_kind = RuntimeRunPathKind::None;
    if let Some(slots) = rpath_slots_with_limit(&bytes, limits.dynamic_entries)? {
        let has_rpath = slots.entries.iter().any(|(tag, _)| *tag == DT_RPATH);
        let has_runpath = slots.entries.iter().any(|(tag, _)| *tag == DT_RUNPATH);
        if has_rpath && has_runpath {
            return Err(
                "dynamic application ELF carries both DT_RPATH and DT_RUNPATH".into(),
            );
        }
        if slots.entries.len() > 1 {
            return Err("dynamic application ELF carries duplicate run-path tags".into());
        }
        run_path_kind = if has_rpath {
            RuntimeRunPathKind::Rpath
        } else if has_runpath {
            RuntimeRunPathKind::Runpath
        } else {
            RuntimeRunPathKind::None
        };
        for (_, string_offset) in slots.entries {
            let string_offset = usize::try_from(string_offset)
                .map_err(|_| "DT_RPATH/DT_RUNPATH offset does not fit this architecture")?;
            let offset = slots
                .strtab_off
                .checked_add(string_offset)
                .ok_or("DT_RPATH/DT_RUNPATH string offset overflow")?;
            let raw = bounded_dynamic_string(
                &bytes,
                offset,
                limits,
                &mut aggregate,
                "DT_RPATH/DT_RUNPATH",
            )?;
            for entry in raw.split(':').filter(|entry| !entry.is_empty()) {
                account_runtime_reference(&mut references, limits)?;
                run_paths.push(entry.to_string());
            }
        }
    }

    let mut needed = Vec::new();
    if let Some(slots) = needed_slots_with_limit(&bytes, limits.dynamic_entries)? {
        for string_offset in slots.offsets {
            let string_offset = usize::try_from(string_offset)
                .map_err(|_| "DT_NEEDED offset does not fit this architecture")?;
            let offset = slots
                .strtab_off
                .checked_add(string_offset)
                .ok_or("DT_NEEDED string offset overflow")?;
            let value = bounded_dynamic_string(
                &bytes,
                offset,
                limits,
                &mut aggregate,
                "DT_NEEDED",
            )?;
            account_runtime_reference(&mut references, limits)?;
            needed.push(value.to_string());
        }
    }
    let executable = matches!(kind, RuntimeElfKind::Executable | RuntimeElfKind::SharedObject)
        && assert_x86_64_executable_bytes(path, &bytes).is_ok();
    Ok(RuntimeLinkSearch {
        file_bytes: metadata.len().max(read_bytes),
        kind,
        executable,
        interpreter,
        run_paths,
        run_path_kind,
        needed,
    })
}

fn needed_names(b: &[u8]) -> Result<Vec<String>, String> {
    let slots = match needed_slots(b)? {
        None => return Ok(Vec::new()),
        Some(slots) => slots,
    };
    let mut names = Vec::with_capacity(slots.offsets.len());
    for offset in slots.offsets {
        let offset = usize::try_from(offset)
            .map_err(|_| "DT_NEEDED string offset does not fit this architecture")?;
        let off = slots
            .strtab_off
            .checked_add(offset)
            .ok_or("DT_NEEDED string offset overflow")?;
        let raw = b.get(off..).ok_or("DT_NEEDED string offset past end of file")?;
        let end = raw.iter().position(|&c| c == 0).unwrap_or(raw.len());
        names.push(String::from_utf8_lossy(&raw[..end]).into_owned());
    }
    Ok(names)
}

/// Read the DT_NEEDED shared-object names of a dynamic ELF — the libraries the loader would
/// pull in at run time. Returns an EMPTY vector for a fully static binary (no PT_DYNAMIC) or a
/// dynamic ELF that declares no needed libraries. This is td's OWN DT_NEEDED query so the
/// static-binary verification asserts "this binary links nothing" without shelling out to a
/// host `readelf` (which would itself be host-executable ingress, re #469).
pub fn read_needed(path: &Path) -> Result<Vec<String>, String> {
    let b = std::fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    needed_names(&b)
}

/// Assert that PATH has the executable structure the x86-64 application launcher
/// needs. Static PIE is `ET_DYN`; a traditional static binary is `ET_EXEC`.
fn assert_x86_64_executable_bytes(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let elf = Elf::parse(bytes)?;
    if !elf.is64 {
        return Err(format!(
            "{}: application entry is not ELFCLASS64",
            path.display()
        ));
    }
    let kind = u16le(bytes, 0x10)?;
    if !matches!(kind, ET_EXEC | ET_DYN) {
        return Err(format!(
            "{}: application entry has ELF type {kind}, expected ET_EXEC or ET_DYN",
            path.display()
        ));
    }
    let machine = u16le(bytes, 0x12)?;
    if machine != EM_X86_64 {
        return Err(format!(
            "{}: application entry has ELF machine {machine}, expected EM_X86_64",
            path.display()
        ));
    }
    if bytes.get(6).copied() != Some(1) || u32le(bytes, 0x14)? != EV_CURRENT {
        return Err(format!(
            "{}: application entry has an unsupported ELF version",
            path.display()
        ));
    }
    if u16le(bytes, 0x34)? != 64 {
        return Err(format!(
            "{}: application entry has an invalid ELF64 header size",
            path.display()
        ));
    }
    let entry = elf.word(0x18)?;
    if entry == 0 {
        return Err(format!(
            "{}: application entry has a zero ELF entry point",
            path.display()
        ));
    }
    let (phoff, phentsize, phnum) = elf.phdr_table()?;
    if phnum == 0 || phentsize != 56 {
        return Err(format!(
            "{}: application entry has no canonical ELF64 program-header table",
            path.display()
        ));
    }
    let table_size = phentsize
        .checked_mul(phnum)
        .ok_or_else(|| format!("{}: ELF program-header table size overflow", path.display()))?;
    let table_end = phoff
        .checked_add(table_size)
        .ok_or_else(|| format!("{}: ELF program-header table offset overflow", path.display()))?;
    if table_end > bytes.len() {
        return Err(format!(
            "{}: ELF program-header table runs past end of file",
            path.display()
        ));
    }
    let mut entry_is_executable = false;
    for index in 0..phnum {
        let offset = index
            .checked_mul(phentsize)
            .and_then(|value| phoff.checked_add(value))
            .ok_or_else(|| format!("{}: ELF program-header offset overflow", path.display()))?;
        if u32le(bytes, offset)? != PT_LOAD {
            continue;
        }
        let flags_offset = offset
            .checked_add(4)
            .ok_or_else(|| format!("{}: PT_LOAD flags offset overflow", path.display()))?;
        let flags = u32le(bytes, flags_offset)?;
        let file_offset = elf.word(Elf::field_offset(offset, 0x08, "PT_LOAD file offset")?)?;
        let virtual_address = elf.word(Elf::field_offset(
            offset,
            0x10,
            "PT_LOAD virtual address",
        )?)?;
        let file_size = elf.word(Elf::field_offset(offset, 0x20, "PT_LOAD file size")?)?;
        let memory_size = elf.word(Elf::field_offset(offset, 0x28, "PT_LOAD memory size")?)?;
        if file_size > memory_size {
            return Err(format!(
                "{}: PT_LOAD has p_filesz larger than p_memsz",
                path.display()
            ));
        }
        let file_end = file_offset.checked_add(file_size).ok_or_else(|| {
            format!("{}: PT_LOAD file range overflow", path.display())
        })?;
        if file_end > bytes.len() as u64 {
            return Err(format!(
                "{}: PT_LOAD runs past end of file",
                path.display()
            ));
        }
        virtual_address.checked_add(memory_size).ok_or_else(|| {
            format!("{}: PT_LOAD memory address range overflow", path.display())
        })?;
        let file_backed_end = virtual_address.checked_add(file_size).ok_or_else(|| {
            format!("{}: PT_LOAD file-backed address range overflow", path.display())
        })?;
        if flags & PF_X != 0 && entry >= virtual_address && entry < file_backed_end {
            entry_is_executable = true;
        }
    }
    if !entry_is_executable {
        return Err(format!(
            "{}: ELF entry point is not covered by a file-backed executable PT_LOAD",
            path.display()
        ));
    }
    Ok(())
}

pub fn assert_x86_64_executable(path: &Path) -> Result<(), String> {
    let bytes = std::fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    assert_x86_64_executable_bytes(path, &bytes)
}

pub(crate) fn assert_x86_64_executable_bounded(
    path: &Path,
    max_bytes: u64,
) -> Result<(), String> {
    let file = std::fs::File::open(path)
        .map_err(|error| format!("read {}: {error}", path.display()))?;
    let length = file
        .metadata()
        .map_err(|error| format!("stat {}: {error}", path.display()))?
        .len();
    if length > max_bytes {
        return Err(format!(
            "{} exceeds the {max_bytes}-byte executable ELF limit",
            path.display()
        ));
    }
    let mut bytes = Vec::new();
    std::io::Read::read_to_end(
        &mut file.take(max_bytes.saturating_add(1)),
        &mut bytes,
    )
    .map_err(|error| format!("read {}: {error}", path.display()))?;
    if u64::try_from(bytes.len()).map_or(true, |read| read > max_bytes) {
        return Err(format!(
            "{} changed while reading and exceeds the {max_bytes}-byte executable ELF limit",
            path.display()
        ));
    }
    assert_x86_64_executable_bytes(path, &bytes)
}

/// Assert an ELF is FULLY STATIC — no program interpreter (`PT_INTERP`), no `DT_NEEDED`
/// shared libraries, and no `DT_RPATH`/`DT_RUNPATH` run-path. This is a runtime-provenance
/// contract (re #469): a *dynamically* linked binary drags a host loader + glibc back in at
/// run time — exactly the host-runtime ingress #469 closes. Its consumer is the pre-libc
/// bootstrap rungs — tcc/make/yacc are linked `-static`, and the `Step::AssertStatic` step
/// fails the rung if one regresses to a host loader/libc.
///
/// The check fails loudly (naming the offending entry) if a regression reintroduces any of
/// them; a non-ELF file reds too (the parser rejects bad magic).
pub fn assert_static(path: &Path) -> Result<(), String> {
    if let Some(interp) = read_interp(path)? {
        return Err(format!(
            "{}: expected a fully static binary but it has a program interpreter (PT_INTERP={interp})",
            path.display()
        ));
    }
    let needed = read_needed(path)?;
    if !needed.is_empty() {
        return Err(format!(
            "{}: expected a fully static binary but it dynamically links {}",
            path.display(),
            needed.join(", ")
        ));
    }
    if let Some(rpath) = read_rpath(path)? {
        return Err(format!(
            "{}: expected a fully static binary but it carries a run-path (DT_RPATH/DT_RUNPATH={rpath})",
            path.display()
        ));
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    // A minimal little-endian ELF buffer with exactly one PT_INTERP program header whose
    // string slot holds `interp` (NUL-terminated). `is64` selects ELFCLASS64 (x86-64) or
    // ELFCLASS32 (i686 — the class the bootstrap toolchain cc1/as/ld actually is). Enough
    // for the reader/writer; not a runnable binary (no sections), which is all this needs.
    fn synth_interp_elf(interp: &str, is64: bool) -> Vec<u8> {
        let (ehdr, phentsize) = if is64 { (64usize, 56usize) } else { (52usize, 32usize) };
        // Two program headers: PT_INTERP + a spare PT_NOTE (which the grow path repurposes into
        // a covering PT_LOAD). The interp string follows both phdr entries.
        let phnum = 2usize;
        let interp_off = ehdr + phnum * phentsize;
        let slot = interp.len() + 1; // include the NUL terminator
        let mut b = vec![0u8; interp_off + slot];
        b[0..4].copy_from_slice(EI_MAG);
        b[EI_CLASS] = if is64 { 2 } else { 1 };
        b[EI_DATA] = 1; // little-endian
        put_phdr_header(&mut b, ehdr, phentsize, phnum, is64);
        let (p_off, _p_vaddr, p_filesz) = ph_field_offsets(is64);
        // PHDR 0: PT_INTERP → the interp string.
        b[ehdr..ehdr + 4].copy_from_slice(&PT_INTERP.to_le_bytes());
        put_word(&mut b, ehdr + p_off, interp_off as u64, is64);
        put_word(&mut b, ehdr + p_filesz, slot as u64, is64);
        // PHDR 1: a spare PT_NOTE (small, points at the interp region — its fields are
        // overwritten if the grow path repurposes it).
        let n = ehdr + phentsize;
        b[n..n + 4].copy_from_slice(&PT_NOTE.to_le_bytes());
        put_word(&mut b, n + p_off, interp_off as u64, is64);
        put_word(&mut b, n + p_filesz, 1, is64);
        b[interp_off..interp_off + interp.len()].copy_from_slice(interp.as_bytes());
        b
    }
    fn synth_elf(interp: &str) -> Vec<u8> {
        synth_interp_elf(interp, true)
    }

    // Write a class-width word (u64 on ELF64, u32 on ELF32) at `off`.
    fn put_word(b: &mut [u8], off: usize, v: u64, is64: bool) {
        if is64 {
            b[off..off + 8].copy_from_slice(&v.to_le_bytes());
        } else {
            b[off..off + 4].copy_from_slice(&(v as u32).to_le_bytes());
        }
    }
    // Fill the e_phoff/e_phentsize/e_phnum header fields for the given class.
    fn put_phdr_header(b: &mut [u8], phoff: usize, phentsize: usize, phnum: usize, is64: bool) {
        let (off, header_size, ents, num) = if is64 {
            (0x20, 0x34, 0x36, 0x38)
        } else {
            (0x1C, 0x28, 0x2A, 0x2C)
        };
        put_word(b, off, phoff as u64, is64);
        let ehsize = if is64 { 64u16 } else { 52u16 };
        b[header_size..header_size + 2].copy_from_slice(&ehsize.to_le_bytes());
        b[ents..ents + 2].copy_from_slice(&(phentsize as u16).to_le_bytes());
        b[num..num + 2].copy_from_slice(&(phnum as u16).to_le_bytes());
    }
    fn ph_field_offsets(is64: bool) -> (usize, usize, usize) {
        if is64 { (0x08, 0x10, 0x20) } else { (0x04, 0x08, 0x10) }
    }

    // A minimal ELF with a PT_LOAD (identity-mapped: p_vaddr == p_offset == 0, so a
    // DT_STRTAB vaddr equals its file offset) + a PT_DYNAMIC holding DT_STRTAB, one run-path
    // entry (DT_RUNPATH if `runpath`, else legacy DT_RPATH), and DT_NULL. The .dynstr is
    // `"\0" <rpath> "\0"`. `is64` selects the ELF class. Enough for the run-path reader/writer.
    fn synth_dyn_elf(rpath: &str, runpath: bool, is64: bool) -> Vec<u8> {
        let (ehdr, phentsize, dyn_entsize, d_un) =
            if is64 { (64usize, 56usize, 16usize, 8usize) } else { (52usize, 32usize, 8usize, 4usize) };
        let phnum = 2usize;
        let dyn_off = ehdr + phnum * phentsize;
        // Keep a spare terminator so tests can insert a second tag while the
        // synthetic table remains well formed.
        let dyn_size = 4 * dyn_entsize;
        let strtab_off = dyn_off + dyn_size;
        let rpath_str_off = 1usize; // index 0 is the conventional empty string ("\0")
        let total = strtab_off + 1 + rpath.len() + 1;

        let mut b = vec![0u8; total];
        b[0..4].copy_from_slice(EI_MAG);
        b[EI_CLASS] = if is64 { 2 } else { 1 };
        b[EI_DATA] = 1;
        b[6] = 1;
        b[0x10..0x12].copy_from_slice(&ET_DYN.to_le_bytes());
        b[0x12..0x14].copy_from_slice(
            &(if is64 { EM_X86_64 } else { 3u16 }).to_le_bytes(),
        );
        b[0x14..0x18].copy_from_slice(&EV_CURRENT.to_le_bytes());
        put_phdr_header(&mut b, ehdr, phentsize, phnum, is64);
        let (p_off, p_vaddr, p_filesz) = ph_field_offsets(is64);

        // PHDR 0: PT_LOAD covering the whole file, identity-mapped.
        let p0 = ehdr;
        b[p0..p0 + 4].copy_from_slice(&PT_LOAD.to_le_bytes());
        put_word(&mut b, p0 + p_off, 0, is64);
        put_word(&mut b, p0 + p_vaddr, 0, is64);
        put_word(&mut b, p0 + p_filesz, total as u64, is64);
        let p_memsz = if is64 { p0 + 0x28 } else { p0 + 0x14 };
        put_word(&mut b, p_memsz, total as u64, is64);
        // PHDR 1: PT_DYNAMIC pointing at the dynamic array.
        let p1 = ehdr + phentsize;
        b[p1..p1 + 4].copy_from_slice(&PT_DYNAMIC.to_le_bytes());
        put_word(&mut b, p1 + p_off, dyn_off as u64, is64);
        put_word(&mut b, p1 + p_vaddr, dyn_off as u64, is64);
        put_word(&mut b, p1 + p_filesz, dyn_size as u64, is64);

        let put_dyn = |b: &mut [u8], idx: usize, tag: u64, val: u64| {
            let e = dyn_off + idx * dyn_entsize;
            put_word(b, e, tag, is64);
            put_word(b, e + d_un, val, is64);
        };
        put_dyn(&mut b, 0, DT_STRTAB, strtab_off as u64); // identity map ⇒ vaddr == file offset
        put_dyn(&mut b, 1, if runpath { DT_RUNPATH } else { DT_RPATH }, rpath_str_off as u64);
        put_dyn(&mut b, 2, DT_NULL, 0);
        put_dyn(&mut b, 3, DT_NULL, 0);

        b[strtab_off + rpath_str_off..strtab_off + rpath_str_off + rpath.len()]
            .copy_from_slice(rpath.as_bytes());
        b
    }

    // A minimal dynamic ELF whose .dynstr holds each `needed` name, with one DT_NEEDED entry
    // per name (plus DT_STRTAB and the DT_NULL terminator). The single PT_LOAD is identity-
    // mapped, so the DT_STRTAB vaddr equals its file offset. The entry points into an
    // executable PT_LOAD so application-graph tests can also use it as a real executable
    // role.
    pub(crate) fn synth_needed_elf(needed: &[&str], is64: bool) -> Vec<u8> {
        let (ehdr, phentsize, dyn_entsize, d_un) =
            if is64 { (64usize, 56usize, 16usize, 8usize) } else { (52usize, 32usize, 8usize, 4usize) };
        let phnum = 2usize;
        let dyn_off = ehdr + phnum * phentsize;
        let dyn_size = (2 + needed.len()) * dyn_entsize; // DT_STRTAB + N×DT_NEEDED + DT_NULL
        let strtab_off = dyn_off + dyn_size;
        // .dynstr: index 0 is the conventional empty string, then each name NUL-terminated.
        let mut dynstr = vec![0u8];
        let mut str_offs: Vec<usize> = Vec::with_capacity(needed.len());
        for n in needed {
            str_offs.push(dynstr.len());
            dynstr.extend_from_slice(n.as_bytes());
            dynstr.push(0);
        }
        let total = strtab_off + dynstr.len();

        let mut b = vec![0u8; total];
        b[0..4].copy_from_slice(EI_MAG);
        b[EI_CLASS] = if is64 { 2 } else { 1 };
        b[EI_DATA] = 1;
        b[6] = 1;
        b[0x10..0x12].copy_from_slice(&ET_DYN.to_le_bytes());
        b[0x12..0x14].copy_from_slice(
            &(if is64 { EM_X86_64 } else { 3u16 }).to_le_bytes(),
        );
        b[0x14..0x18].copy_from_slice(&EV_CURRENT.to_le_bytes());
        put_word(&mut b, 0x18, ehdr as u64, is64);
        put_phdr_header(&mut b, ehdr, phentsize, phnum, is64);
        let (p_off, p_vaddr, p_filesz) = ph_field_offsets(is64);

        // PHDR 0: PT_LOAD covering the whole file, identity-mapped.
        let p0 = ehdr;
        b[p0..p0 + 4].copy_from_slice(&PT_LOAD.to_le_bytes());
        let flags = if is64 { p0 + 0x04 } else { p0 + 0x18 };
        b[flags..flags + 4].copy_from_slice(&PF_X.to_le_bytes());
        put_word(&mut b, p0 + p_off, 0, is64);
        put_word(&mut b, p0 + p_vaddr, 0, is64);
        put_word(&mut b, p0 + p_filesz, total as u64, is64);
        let p_memsz = if is64 { p0 + 0x28 } else { p0 + 0x14 };
        put_word(&mut b, p_memsz, total as u64, is64);
        // PHDR 1: PT_DYNAMIC pointing at the dynamic array.
        let p1 = ehdr + phentsize;
        b[p1..p1 + 4].copy_from_slice(&PT_DYNAMIC.to_le_bytes());
        put_word(&mut b, p1 + p_off, dyn_off as u64, is64);
        put_word(&mut b, p1 + p_vaddr, dyn_off as u64, is64);
        put_word(&mut b, p1 + p_filesz, dyn_size as u64, is64);

        let put_dyn = |b: &mut [u8], idx: usize, tag: u64, val: u64| {
            let e = dyn_off + idx * dyn_entsize;
            put_word(b, e, tag, is64);
            put_word(b, e + d_un, val, is64);
        };
        put_dyn(&mut b, 0, DT_STRTAB, strtab_off as u64); // identity map ⇒ vaddr == file offset
        for (i, off) in str_offs.iter().enumerate() {
            put_dyn(&mut b, 1 + i, DT_NEEDED, *off as u64);
        }
        put_dyn(&mut b, 1 + needed.len(), DT_NULL, 0);

        b[strtab_off..strtab_off + dynstr.len()].copy_from_slice(&dynstr);
        b
    }

    // A minimal ELF with a single identity-mapped PT_LOAD and NO PT_INTERP / PT_DYNAMIC —
    // a fully static, non-dynamic executable (the shape a static bootstrap rung must produce).
    fn synth_static_elf(is64: bool) -> Vec<u8> {
        let (ehdr, phentsize) = if is64 { (64usize, 56usize) } else { (52usize, 32usize) };
        let phnum = 1usize;
        let total = ehdr + phnum * phentsize;
        let mut b = vec![0u8; total];
        b[0..4].copy_from_slice(EI_MAG);
        b[EI_CLASS] = if is64 { 2 } else { 1 };
        b[EI_DATA] = 1;
        b[6] = 1;
        b[0x10..0x12].copy_from_slice(&ET_EXEC.to_le_bytes());
        b[0x12..0x14].copy_from_slice(
            &(if is64 { EM_X86_64 } else { 3u16 }).to_le_bytes(),
        );
        b[0x14..0x18].copy_from_slice(&EV_CURRENT.to_le_bytes());
        put_word(&mut b, 0x18, ehdr as u64, is64);
        let ehsize = if is64 { 0x34 } else { 0x28 };
        b[ehsize..ehsize + 2].copy_from_slice(&(ehdr as u16).to_le_bytes());
        put_phdr_header(&mut b, ehdr, phentsize, phnum, is64);
        let (p_off, p_vaddr, p_filesz) = ph_field_offsets(is64);
        let p0 = ehdr;
        b[p0..p0 + 4].copy_from_slice(&PT_LOAD.to_le_bytes());
        let flags = if is64 { p0 + 0x04 } else { p0 + 0x18 };
        b[flags..flags + 4].copy_from_slice(&PF_X.to_le_bytes());
        put_word(&mut b, p0 + p_off, 0, is64);
        put_word(&mut b, p0 + p_vaddr, 0, is64);
        put_word(&mut b, p0 + p_filesz, total as u64, is64);
        let p_memsz = if is64 { p0 + 0x28 } else { p0 + 0x14 };
        put_word(&mut b, p_memsz, total as u64, is64);
        b
    }

    fn synth_profiled_elf(
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
            if with_symbols { SHT_SYMTAB } else { SHT_PROGBITS },
            0,
            symtab_off,
            symtab_size,
            5,
            24,
        );
        let line_name: &[u8] = if with_lines { b".debug_line" } else { b".text" };
        section(
            4,
            line_name,
            SHT_PROGBITS,
            0,
            lines_off,
            lines.len(),
            0,
            0,
        );
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

    fn profile_section_u64(bytes: &mut [u8], section: usize, field: usize, value: u64) {
        let shoff = u64le(bytes, 0x28).unwrap() as usize;
        let start = shoff + section * 64 + field;
        bytes
            .get_mut(start..start + 8)
            .unwrap()
            .copy_from_slice(&value.to_le_bytes());
    }

    fn profile_section_u32(bytes: &mut [u8], section: usize, field: usize, value: u32) {
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
        assert!(!v5_line_tables_use_debug_str(
            &line_str,
            18,
            4,
            8,
            &mut form_values,
        )
        .unwrap());
        let debug_str = v5_line_header(0x0e);
        let mut form_values = MAX_PROFILE_LINE_FORM_VALUES;
        assert!(v5_line_tables_use_debug_str(
            &debug_str,
            18,
            4,
            8,
            &mut form_values,
        )
        .unwrap());
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
        skip_line_table_entries(
            &mut cursor,
            &formats,
            4,
            8,
            &mut entries,
            &mut form_values,
        )
        .unwrap();
        let error = skip_line_table_entries(
            &mut cursor,
            &formats,
            4,
            8,
            &mut entries,
            &mut form_values,
        )
        .unwrap_err();
        assert!(error.contains("combined 200000-entry limit"));

        let mut cursor = LineHeaderCursor::new(&counts[..1], 0).unwrap();
        let mut entries = MAX_PROFILE_LINE_TABLE_ENTRIES;
        let mut form_values = 1;
        let error = skip_line_table_entries(
            &mut cursor,
            &formats,
            4,
            8,
            &mut entries,
            &mut form_values,
        )
        .unwrap_err();
        assert!(error.contains("6400000-form-value object limit"));
    }

    #[test]
    fn profiled_v5_elf_enforces_its_declared_string_dependency() {
        let dir = std::env::temp_dir().join(format!("elf-test-profile-v5-{}", std::process::id()));
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

        std::fs::write(
            &debug,
            synth_profiled_v5_elf(&[id], 0x0e, b".debug_str"),
        )
        .unwrap();
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
        assert_debug_pair_with_line_limit(
            &runtime,
            &debug,
            MAX_PROFILE_LINE_SECTION_BYTES,
            false,
        )
        .unwrap();
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn reads_interp() {
        let dir = std::env::temp_dir().join(format!("elf-test-r-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("a");
        std::fs::write(&f, synth_elf("/lib64/ld-linux-x86-64.so.2")).unwrap();
        assert_eq!(read_interp(&f).unwrap().as_deref(), Some("/lib64/ld-linux-x86-64.so.2"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn profiled_elf_requires_one_sha1_id_symbols_and_lines() {
        let dir = std::env::temp_dir().join(format!("elf-test-profile-{}", std::process::id()));
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

        for (source, label) in [
            (3, ".symtab"),
            (4, ".debug_line"),
            (8, ".debug_line_str"),
        ] {
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
        assert!(err.contains("ordinary symbol table"), "unexpected error: {err}");
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
        assert!(err.contains(".symtab has size 0"), "unexpected error: {err}");

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
        let dir = std::env::temp_dir().join(format!("elf-test-s-{}", std::process::id()));
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
        let dir = std::env::temp_dir().join(format!("elf-test-l-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("a");
        std::fs::write(&f, synth_elf("/lib64/ld.so")).unwrap();
        let before = std::fs::metadata(&f).unwrap().len();
        let long = "/td/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-glibc-2.41-x86_64/lib/ld-linux-x86-64.so.2";
        assert!(long.len() + 1 > "/lib64/ld.so".len() + 1, "the test path must exceed the slot");
        set_interp(&f, long).unwrap();
        // reads back the full long path, and the file GREW (the string was appended)
        assert_eq!(read_interp(&f).unwrap().as_deref(), Some(long));
        let after = std::fs::metadata(&f).unwrap().len();
        assert!(after > before, "file should grow ({before} -> {after})");
        assert_eq!(after as usize, before as usize + long.len() + 1, "grew by exactly the path + NUL");
        // a subsequent SHORTER set still works (fits the now-large slot, in place)
        set_interp(&f, "/td/store/ld").unwrap();
        assert_eq!(read_interp(&f).unwrap().as_deref(), Some("/td/store/ld"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn grows_interp_elf32() {
        // the i686 class grows the same way (the bootstrap toolchain is i686).
        let dir = std::env::temp_dir().join(format!("elf-test-l32-{}", std::process::id()));
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
        let dir = std::env::temp_dir().join(format!("elf-test-32-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("a");
        std::fs::write(&f, synth_interp_elf("/lib/ld-linux.so.2", false)).unwrap();
        assert_eq!(read_interp(&f).unwrap().as_deref(), Some("/lib/ld-linux.so.2"));
        set_interp(&f, "/td/store/ld").unwrap();
        assert_eq!(read_interp(&f).unwrap().as_deref(), Some("/td/store/ld"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn rejects_non_elf() {
        let dir = std::env::temp_dir().join(format!("elf-test-n-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("a");
        std::fs::write(&f, b"not an elf at all, just text padding padding padding padding").unwrap();
        assert!(read_interp(&f).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn reads_runpath_and_legacy_rpath() {
        let dir = std::env::temp_dir().join(format!("elf-test-rp-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("a");
        std::fs::write(&f, synth_dyn_elf("/build/dir/lib", true, true)).unwrap();
        assert_eq!(read_rpath(&f).unwrap().as_deref(), Some("/build/dir/lib"));
        // legacy DT_RPATH reads back too
        std::fs::write(&f, synth_dyn_elf("/build/dir/lib", false, true)).unwrap();
        assert_eq!(read_rpath(&f).unwrap().as_deref(), Some("/build/dir/lib"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn sets_shorter_rpath_and_pads() {
        let dir = std::env::temp_dir().join(format!("elf-test-rps-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("a");
        // a build-dir search path, retargeted to a (shorter) /td/store run-path
        std::fs::write(&f, synth_dyn_elf("/tmp/build-xyz/binutils/lib", true, true)).unwrap();
        let before = std::fs::metadata(&f).unwrap().len();
        set_rpath(&f, "/td/store/glibc/lib").unwrap();
        // round-trips to the new value, in place (file size unchanged), tail NUL-padded
        assert_eq!(read_rpath(&f).unwrap().as_deref(), Some("/td/store/glibc/lib"));
        assert_eq!(std::fs::metadata(&f).unwrap().len(), before);
        assert!(std::fs::read(&f).unwrap().ends_with(&[0]));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn refuses_too_long_rpath() {
        let dir = std::env::temp_dir().join(format!("elf-test-rpt-{}", std::process::id()));
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
        let dir = std::env::temp_dir().join(format!("elf-test-rpa-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("a");
        // an interp-only ELF (no PT_DYNAMIC at all) has no run-path
        std::fs::write(&f, synth_elf("/lib64/ld-linux-x86-64.so.2")).unwrap();
        assert_eq!(read_rpath(&f).unwrap(), None);
        let err = set_rpath(&f, "/td/store/glibc/lib").unwrap_err();
        assert!(err.contains("no DT_RPATH/DT_RUNPATH"), "unexpected error: {err}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn reads_and_sets_rpath_elf32() {
        // i686 run-path round-trip — the class the bootstrap toolchain ar/ranlib actually
        // are, so a /td/store run-path can be baked to drop their LD_LIBRARY_PATH wrappers.
        let dir = std::env::temp_dir().join(format!("elf-test-rp32-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("a");
        std::fs::write(&f, synth_dyn_elf("/tmp/build/binutils/lib", true, false)).unwrap();
        assert_eq!(read_rpath(&f).unwrap().as_deref(), Some("/tmp/build/binutils/lib"));
        set_rpath(&f, "/td/store/glibc/lib").unwrap();
        assert_eq!(read_rpath(&f).unwrap().as_deref(), Some("/td/store/glibc/lib"));
        // legacy DT_RPATH on ELF32 reads back too
        std::fs::write(&f, synth_dyn_elf("/a/b/c", false, false)).unwrap();
        assert_eq!(read_rpath(&f).unwrap().as_deref(), Some("/a/b/c"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn reads_needed_shared_objects() {
        let dir = std::env::temp_dir().join(format!("elf-test-need-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("a");
        // multiple DT_NEEDED, in order
        std::fs::write(&f, synth_needed_elf(&["libc.so.6", "libm.so.6"], true)).unwrap();
        assert_eq!(read_needed(&f).unwrap(), vec!["libc.so.6".to_string(), "libm.so.6".to_string()]);
        // ELF32 reads back too
        std::fs::write(&f, synth_needed_elf(&["ld-linux.so.2"], false)).unwrap();
        assert_eq!(read_needed(&f).unwrap(), vec!["ld-linux.so.2".to_string()]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn runtime_link_search_returns_interp_and_splits_runpath() {
        let dir = std::env::temp_dir().join(format!("elf-test-rls-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("a");
        // A run-path-only dynamic ELF (no interp): the colon-separated entries split out.
        std::fs::write(&f, synth_dyn_elf("/gnu/store/aaa/lib:/gnu/store/bbb/lib", true, true)).unwrap();
        let (interp, dirs, needed) = runtime_link_search(&f).unwrap();
        assert_eq!(interp, None);
        assert_eq!(dirs, vec!["/gnu/store/aaa/lib".to_string(), "/gnu/store/bbb/lib".to_string()]);
        assert!(needed.is_empty());
        // An interp-only ELF (no PT_DYNAMIC): interp out, no run-path.
        std::fs::write(&f, synth_elf("/gnu/store/ccc/lib/ld-linux-x86-64.so.2")).unwrap();
        let (interp, dirs, needed) = runtime_link_search(&f).unwrap();
        assert_eq!(interp.as_deref(), Some("/gnu/store/ccc/lib/ld-linux-x86-64.so.2"));
        assert!(dirs.is_empty());
        assert!(needed.is_empty());
        // A DT_NEEDED name containing a slash is opened as a pathname, without a run-path.
        let absolute_needed = "/td/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-lib/lib/libfoo.so";
        std::fs::write(&f, synth_needed_elf(&[absolute_needed], true)).unwrap();
        let (interp, dirs, needed) = runtime_link_search(&f).unwrap();
        assert_eq!(interp, None);
        assert!(dirs.is_empty());
        assert_eq!(needed, [absolute_needed]);
        // A NON-ELF file (a script, a header) has NO dynamic-linkage search set — this is
        // exactly why a store path named only in such a file (bash-static in glibc's
        // bin/ldd or include/paths.h) never enters the builder's runtime closure (re #469).
        std::fs::write(&f, b"#!/gnu/store/ddd-bash/bin/sh\necho hi\n").unwrap();
        let (interp, dirs, needed) = runtime_link_search(&f).unwrap();
        assert_eq!(interp, None);
        assert!(dirs.is_empty());
        assert!(needed.is_empty());
        // A relocatable ELF object legitimately has no program headers. It is not a
        // loader input by itself and contributes no interpreter or run-path.
        let mut reloc = vec![0u8; 64];
        reloc[0..4].copy_from_slice(EI_MAG);
        reloc[EI_CLASS] = 2;
        reloc[EI_DATA] = 1;
        std::fs::write(&f, reloc).unwrap();
        let (interp, dirs, needed) = runtime_link_search(&f).unwrap();
        assert_eq!(interp, None);
        assert!(dirs.is_empty());
        assert!(needed.is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn bounded_runtime_search_charges_before_each_foreign_allocation() {
        let dir =
            std::env::temp_dir().join(format!("elf-test-bounded-rls-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("a");
        let bytes = synth_needed_elf(&["libalpha.so", "libbeta.so"], true);
        std::fs::write(&file, &bytes).unwrap();
        let limits = RuntimeLinkLimits {
            file_bytes: 1024 * 1024,
            dynamic_entries: 16,
            references: 16,
            string_bytes: 4096,
            aggregate_text_bytes: 4096,
        };
        assert_eq!(
            runtime_link_search_bounded(&file, limits).unwrap().needed,
            ["libalpha.so", "libbeta.so"]
        );
        for limited in [
            RuntimeLinkLimits {
                file_bytes: u64::try_from(bytes.len()).unwrap() - 1,
                ..limits
            },
            RuntimeLinkLimits {
                dynamic_entries: 3,
                ..limits
            },
            RuntimeLinkLimits {
                references: 1,
                ..limits
            },
            RuntimeLinkLimits {
                string_bytes: 3,
                ..limits
            },
            RuntimeLinkLimits {
                aggregate_text_bytes: 12,
                ..limits
            },
        ] {
            assert!(runtime_link_search_bounded(&file, limited).is_err());
        }

        let mut invalid_version = bytes.clone();
        invalid_version[6] = 0;
        std::fs::write(&file, invalid_version).unwrap();
        let error = runtime_link_search_bounded(&file, limits).unwrap_err();
        assert!(error.contains("unsupported version"), "{error}");

        std::fs::write(&file, synth_dyn_elf("/app/lib", false, true)).unwrap();
        assert_eq!(
            runtime_link_search_bounded(&file, limits)
                .unwrap()
                .run_path_kind,
            RuntimeRunPathKind::Rpath
        );
        std::fs::write(&file, synth_dyn_elf("/app/lib", true, true)).unwrap();
        assert_eq!(
            runtime_link_search_bounded(&file, limits)
                .unwrap()
                .run_path_kind,
            RuntimeRunPathKind::Runpath
        );

        let mut dual = synth_dyn_elf("/app/lib", true, true);
        let dynamic_offset = 64 + 2 * 56;
        let third_entry = dynamic_offset + 2 * 16;
        dual[third_entry..third_entry + 8].copy_from_slice(&DT_RPATH.to_le_bytes());
        dual[third_entry + 8..third_entry + 16].copy_from_slice(&1u64.to_le_bytes());
        std::fs::write(&file, dual).unwrap();
        let error = runtime_link_search_bounded(&file, limits).unwrap_err();
        assert!(error.contains("both DT_RPATH and DT_RUNPATH"), "{error}");

        let mut duplicate = synth_dyn_elf("/app/lib", true, true);
        duplicate[third_entry..third_entry + 8].copy_from_slice(&DT_RUNPATH.to_le_bytes());
        duplicate[third_entry + 8..third_entry + 16].copy_from_slice(&1u64.to_le_bytes());
        std::fs::write(&file, duplicate).unwrap();
        let error = runtime_link_search_bounded(&file, limits).unwrap_err();
        assert!(error.contains("duplicate run-path"), "{error}");

        let mut hidden_audit = synth_dyn_elf("/app/lib", true, true);
        hidden_audit[third_entry..third_entry + 8].copy_from_slice(&DT_AUDIT.to_le_bytes());
        let dynamic_ph = 64 + 56;
        put_word(&mut hidden_audit, dynamic_ph + 0x20, 2 * 16, true);
        std::fs::write(&file, hidden_audit).unwrap();
        let error = runtime_link_search_bounded(&file, limits).unwrap_err();
        assert!(error.contains("no DT_NULL terminator"), "{error}");

        let mut audit = synth_dyn_elf("/app/lib", true, true);
        let second_entry = dynamic_offset + 16;
        audit[second_entry..second_entry + 8].copy_from_slice(&DT_AUDIT.to_le_bytes());
        std::fs::write(&file, audit).unwrap();
        let error = runtime_link_search_bounded(&file, limits).unwrap_err();
        assert!(error.contains("unsupported loader object tag"), "{error}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn static_binary_needs_nothing() {
        let dir = std::env::temp_dir().join(format!("elf-test-need0-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("a");
        // a fully static ELF (no PT_DYNAMIC) declares no needed libraries
        std::fs::write(&f, synth_static_elf(true)).unwrap();
        assert!(read_needed(&f).unwrap().is_empty());
        // a dynamic ELF with only a run-path (no DT_NEEDED) also needs nothing
        std::fs::write(&f, synth_dyn_elf("/some/lib", true, true)).unwrap();
        assert!(read_needed(&f).unwrap().is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn assert_static_accepts_a_static_elf() {
        let dir = std::env::temp_dir().join(format!("elf-test-as-ok-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("a");
        // the fully-static shape: no interpreter, no needed libs, no run-path — x86-64…
        std::fs::write(&f, synth_static_elf(true)).unwrap();
        assert!(assert_static(&f).is_ok());
        // …and i686 (the class check is class-independent)
        std::fs::write(&f, synth_static_elf(false)).unwrap();
        assert!(assert_static(&f).is_ok());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn application_executable_requires_x86_64_kind_and_file_backed_entry() {
        let dir = std::env::temp_dir().join(format!("elf-app-exec-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("app");
        let valid = synth_static_elf(true);
        std::fs::write(&file, &valid).unwrap();
        assert!(assert_x86_64_executable(&file).is_ok());
        assert!(assert_x86_64_executable_bounded(
            &file,
            u64::try_from(valid.len()).unwrap()
        )
        .is_ok());
        let error = assert_x86_64_executable_bounded(
            &file,
            u64::try_from(valid.len()).unwrap() - 1,
        )
        .unwrap_err();
        assert!(error.contains("executable ELF limit"), "{error}");

        for (bytes, expected) in [
            ({
                let mut bytes = valid.clone();
                bytes[0x10..0x12].copy_from_slice(&1u16.to_le_bytes());
                bytes
            }, "ET_EXEC or ET_DYN"),
            ({
                let mut bytes = valid.clone();
                bytes[0x12..0x14].copy_from_slice(&3u16.to_le_bytes());
                bytes
            }, "EM_X86_64"),
            ({
                let mut bytes = valid.clone();
                bytes[0x18..0x20].copy_from_slice(&0u64.to_le_bytes());
                bytes
            }, "zero ELF entry point"),
            ({
                let mut bytes = valid.clone();
                bytes[64 + 4..64 + 8].copy_from_slice(&0u32.to_le_bytes());
                bytes
            }, "not covered"),
        ] {
            std::fs::write(&file, &bytes).unwrap();
            let error = assert_x86_64_executable(&file).unwrap_err();
            assert!(error.contains(expected), "{error}");
        }

        let mut malformed_second_load = valid.clone();
        let second = malformed_second_load.len();
        malformed_second_load.resize(second + 56, 0);
        malformed_second_load[0x38..0x3a].copy_from_slice(&2u16.to_le_bytes());
        malformed_second_load[second..second + 4].copy_from_slice(&PT_LOAD.to_le_bytes());
        malformed_second_load[second + 0x20..second + 0x28]
            .copy_from_slice(&1u64.to_le_bytes());
        std::fs::write(&file, malformed_second_load).unwrap();
        let error = assert_x86_64_executable(&file).unwrap_err();
        assert!(error.contains("p_filesz larger than p_memsz"), "{error}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn static_assertion_refuses_overflowing_segment_metadata_without_panicking() {
        let dir = std::env::temp_dir().join(format!("elf-static-ranges-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("app");
        let valid = synth_static_elf(true);

        for (kind, expected) in [
            (PT_INTERP, "PT_INTERP string file range overflows"),
            (PT_DYNAMIC, "PT_DYNAMIC file range overflows"),
        ] {
            let mut malformed = valid.clone();
            let second = malformed.len();
            malformed.resize(second + 56, 0);
            malformed[0x38..0x3a].copy_from_slice(&2u16.to_le_bytes());
            malformed[second..second + 4].copy_from_slice(&kind.to_le_bytes());
            malformed[second + 0x08..second + 0x10]
                .copy_from_slice(&u64::MAX.to_le_bytes());
            malformed[second + 0x20..second + 0x28]
                .copy_from_slice(&1u64.to_le_bytes());
            std::fs::write(&file, malformed).unwrap();
            let error = assert_static(&file).unwrap_err();
            assert!(error.contains(expected), "{error}");
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn assert_static_rejects_dynamic_linkage() {
        let dir = std::env::temp_dir().join(format!("elf-test-as-bad-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("a");
        // (1) a program interpreter (PT_INTERP) is rejected
        std::fs::write(&f, synth_elf("/lib64/ld-linux-x86-64.so.2")).unwrap();
        let err = assert_static(&f).unwrap_err();
        assert!(err.contains("PT_INTERP"), "unexpected error: {err}");
        // (2) a DT_NEEDED shared library is rejected, and the message names it
        std::fs::write(&f, synth_needed_elf(&["libc.so.6"], true)).unwrap();
        let err = assert_static(&f).unwrap_err();
        assert!(err.contains("libc.so.6"), "unexpected error: {err}");
        // (3) a DT_RPATH/DT_RUNPATH run-path is rejected
        std::fs::write(&f, synth_dyn_elf("/gnu/store/lib", true, true)).unwrap();
        let err = assert_static(&f).unwrap_err();
        assert!(err.contains("run-path"), "unexpected error: {err}");
        std::fs::remove_dir_all(&dir).ok();
    }
}

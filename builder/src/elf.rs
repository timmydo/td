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
const SHT_SYMTAB: u32 = 2;
const SHT_STRTAB: u32 = 3;
const SHT_NOTE: u32 = 7;
const SHT_NOBITS: u32 = 8;
const NT_GNU_BUILD_ID: u32 = 3;
const SHF_ALLOC: u64 = 2;
const MAX_SECTION_NAME_TABLE_BYTES: u64 = 16 * 1024 * 1024;
const MAX_SECTION_HEADER_BYTES: usize = 4096;
/// Write a class-width word (u64 on ELF64, low u32 on ELF32) at `off`, little-endian.
pub use td_engine::elf_edit::{set_interp, set_rpath};

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

/// Whether a non-allocated section named `name` is DWARF or a debugger
/// index. Allocated sections such as rustc's `.debug_gdb_scripts` are part of
/// the runtime mapping and are judged by `SHF_ALLOC`, not by name.
fn is_debug_payload(name: &[u8]) -> bool {
    name.starts_with(b".debug_") || name.starts_with(b".zdebug_") || name == b".gdb_index"
}

/// Verify the runtime/debug pair identity and the companion's contents. The
/// runtime retains only allocated/runtime metadata and dynamic symbols; the
/// companion carries the full ordinary symbol table and no DWARF, its line
/// program and string tables included.
pub fn assert_debug_pair(runtime: &Path, debug: &Path) -> Result<(), String> {
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
        if section.flags & SHF_ALLOC == 0 && is_debug_payload(name) {
            return Err(format!(
                "{}: stripped runtime still carries debug section {}",
                runtime.display(),
                String::from_utf8_lossy(name)
            ));
        }
    }
    let mut has_symbols = false;
    for section in &debug_elf.sections {
        let name = debug_elf.section_name(section)?;
        if section.flags & SHF_ALLOC == 0 && is_debug_payload(name) {
            return Err(format!(
                "{}: debug companion carries {}, outside the symbols-only companion policy",
                debug.display(),
                String::from_utf8_lossy(name)
            ));
        }
        if name != b".symtab" && section.kind != SHT_SYMTAB {
            continue;
        }
        if has_symbols {
            return Err(format!(
                "{}: duplicate .symtab section is outside the symbols-only companion policy",
                debug.display()
            ));
        }
        if section.kind != SHT_SYMTAB {
            return Err(format!(
                "{}: .symtab has unsupported section type {}",
                debug.display(),
                section.kind
            ));
        }
        if name != b".symtab" {
            return Err(format!(
                "{}: ordinary symbol table must be named .symtab",
                debug.display()
            ));
        }
        debug_elf
            .validate_symbol_table(*section)
            .map_err(|error| format!("{}: {error}", debug.display()))?;
        has_symbols = true;
    }
    if !has_symbols {
        return Err(format!(
            "{}: debug companion requires .symtab",
            debug.display()
        ));
    }
    Ok(())
}

/// Require an ordinary symbol table, as an unstripped executable keeps.
pub fn assert_symbol_table(path: &Path) -> Result<(), String> {
    let elf = FileElf::open(path)?;
    let mut symbols = elf
        .sections
        .iter()
        .filter(|section| section.kind == SHT_SYMTAB);
    let section = symbols
        .next()
        .ok_or_else(|| format!("{}: ELF has no ordinary symbol table", path.display()))?;
    if symbols.next().is_some() {
        return Err(format!(
            "{}: ELF has more than one ordinary symbol table",
            path.display()
        ));
    }
    elf.validate_symbol_table(*section)
        .map_err(|error| format!("{}: {error}", path.display()))
}

/// Locate the PT_INTERP program-header ENTRY and return `(ph_entry_offset, string_off,
/// string_filesz, is64)`, or `None` if the ELF has no interpreter. Unlike `interp_slot`
/// this yields the ph ENTRY offset so the setter can grow the string (repoint p_offset/
/// p_filesz), not just overwrite it in place.
#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::elf_synth::*;

    const SHT_PROGBITS: u32 = 1;
    const SHF_COMPRESSED: u64 = 0x800;

    pub(crate) fn synth_profiled_elf(
        build_ids: &[[u8; 20]],
        with_lines: bool,
        with_symbols: bool,
    ) -> Vec<u8> {
        let names = b"\0.shstrtab\0.note.gnu.build-id\0.symtab\0.debug_line\0.strtab\0.text\0.bss\0.debug_gdb_scripts\0.debug_line_str\0.debug_str\0.debug_info\0.zdebug_line\0.gdb_index\0";
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
        let shoff = u64le(bytes, 0x28).unwrap() as usize;
        let names_off = u64le(bytes, shoff + 64 + 0x18).unwrap() as usize;
        let names_size = u64le(bytes, shoff + 64 + 0x20).unwrap() as usize;
        let relative = bytes
            .get(names_off..names_off + names_size)
            .unwrap()
            .windows(name.len() + 2)
            .position(|candidate| {
                candidate.first() == Some(&0)
                    && candidate
                        .get(1..=name.len())
                        .is_some_and(|middle| middle == name)
                    && candidate.last() == Some(&0)
            })
            .unwrap()
            + 1;
        profile_section_u32(bytes, section, 0, relative as u32);
    }

    #[test]
    fn companion_requires_one_sha1_id_and_symbols_without_dwarf() {
        let _dir_cleanup = crate::test_temp::TempDir::new("elf-test-profile").unwrap();
        let dir = _dir_cleanup.join("fixture");
        std::fs::create_dir_all(&dir).unwrap();
        let runtime = dir.join("runtime");
        let debug = dir.join("debug");
        let id = [0x5au8; 20];
        std::fs::write(&runtime, synth_profiled_elf(&[id], false, false)).unwrap();
        std::fs::write(&debug, synth_profiled_elf(&[id], false, true)).unwrap();
        assert!(is_runtime_elf(&runtime).unwrap());
        assert_eq!(read_build_id(&runtime).unwrap(), id);
        // The allocated `.debug_gdb_scripts` stays: SHF_ALLOC, not its name,
        // makes it runtime mapping rather than debug payload.
        assert_debug_pair(&runtime, &debug).unwrap();
        assert_symbol_table(&debug).unwrap();
        let err = assert_symbol_table(&runtime).unwrap_err();
        assert!(err.contains("no ordinary symbol table"), "{err}");

        // The line program, its string tables and every other DWARF or
        // debugger-index section are refused, compressed or not.
        std::fs::write(&debug, synth_profiled_elf(&[id], true, true)).unwrap();
        let err = assert_debug_pair(&runtime, &debug).unwrap_err();
        assert!(
            err.contains("carries .debug_line, outside the symbols-only companion policy"),
            "{err}"
        );
        for name in [
            ".debug_line_str",
            ".debug_str",
            ".debug_info",
            ".zdebug_line",
            ".gdb_index",
        ] {
            let mut carrying = synth_profiled_elf(&[id], false, true);
            profile_set_section_name(&mut carrying, 8, name.as_bytes());
            std::fs::write(&debug, carrying).unwrap();
            let err = assert_debug_pair(&runtime, &debug).unwrap_err();
            assert!(err.contains(&format!("carries {name},")), "{name}: {err}");
            let mut compressed = synth_profiled_elf(&[id], false, true);
            profile_set_section_name(&mut compressed, 8, name.as_bytes());
            profile_section_u64(&mut compressed, 8, 0x08, SHF_COMPRESSED);
            std::fs::write(&debug, compressed).unwrap();
            assert!(assert_debug_pair(&runtime, &debug).is_err(), "{name}");
        }

        let mut wrong_names_type = synth_profiled_elf(&[id], false, true);
        profile_section_u32(&mut wrong_names_type, 1, 4, SHT_PROGBITS);
        std::fs::write(&debug, wrong_names_type).unwrap();
        let err = assert_debug_pair(&runtime, &debug).unwrap_err();
        assert!(
            err.contains("section-name table is not SHT_STRTAB"),
            "{err}"
        );

        let mut duplicate = synth_profiled_elf(&[id], false, true);
        profile_duplicate_section_name(&mut duplicate, 3, 7);
        std::fs::write(&debug, duplicate).unwrap();
        let err = assert_debug_pair(&runtime, &debug).unwrap_err();
        assert!(err.contains("duplicate .symtab section"), "{err}");

        let mut renamed = synth_profiled_elf(&[id], false, true);
        profile_set_section_name(&mut renamed, 3, b".text");
        std::fs::write(&debug, renamed).unwrap();
        let err = assert_debug_pair(&runtime, &debug).unwrap_err();
        assert!(
            err.contains("ordinary symbol table must be named .symtab"),
            "{err}"
        );

        let mut mistyped = synth_profiled_elf(&[id], false, true);
        profile_section_u32(&mut mistyped, 3, 4, SHT_PROGBITS);
        std::fs::write(&debug, mistyped).unwrap();
        let err = assert_debug_pair(&runtime, &debug).unwrap_err();
        assert!(
            err.contains(".symtab has unsupported section type 1"),
            "{err}"
        );

        std::fs::write(&debug, synth_profiled_elf(&[id], false, false)).unwrap();
        let err = assert_debug_pair(&runtime, &debug).unwrap_err();
        assert!(err.contains("requires .symtab"), "{err}");

        std::fs::write(&debug, synth_profiled_elf(&[id], false, true)).unwrap();
        std::fs::write(&runtime, synth_profiled_elf(&[id], false, true)).unwrap();
        let err = assert_debug_pair(&runtime, &debug).unwrap_err();
        assert!(err.contains("ordinary symbol table"), "{err}");

        std::fs::write(&runtime, synth_profiled_elf(&[id], true, false)).unwrap();
        let err = assert_debug_pair(&runtime, &debug).unwrap_err();
        assert!(err.contains("still carries debug section"), "{err}");
        std::fs::write(&runtime, synth_profiled_elf(&[id], false, false)).unwrap();

        std::fs::write(&debug, synth_profiled_elf(&[[0xa5; 20]], false, true)).unwrap();
        let err = assert_debug_pair(&runtime, &debug).unwrap_err();
        assert!(err.contains("different GNU build IDs"), "{err}");

        std::fs::write(&debug, synth_profiled_elf(&[], false, true)).unwrap();
        let err = read_build_id(&debug).unwrap_err();
        assert!(err.contains("found 0"), "{err}");

        std::fs::write(&debug, synth_profiled_elf(&[id, id], false, true)).unwrap();
        let err = read_build_id(&debug).unwrap_err();
        assert!(err.contains("found 2"), "{err}");

        let mut malformed = synth_profiled_elf(&[id], false, true);
        profile_section_u64(&mut malformed, 3, 0x20, 0);
        std::fs::write(&debug, &malformed).unwrap();
        let err = assert_debug_pair(&runtime, &debug).unwrap_err();
        assert!(err.contains(".symtab has size 0"), "{err}");

        let mut malformed = synth_profiled_elf(&[id], false, true);
        profile_section_u64(&mut malformed, 3, 0x38, 1);
        std::fs::write(&debug, &malformed).unwrap();
        let err = assert_debug_pair(&runtime, &debug).unwrap_err();
        assert!(err.contains("entry size 1"), "{err}");
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

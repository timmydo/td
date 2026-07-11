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
const PF_R: u32 = 4; // segment readable
const DT_NULL: u64 = 0; // end of the dynamic array
// Backs the pub `read_needed`/`assert_static` DT_NEEDED query; its first in-crate caller is
// the forthcoming td-sh static-build verifier (re #469, this PR), so it reads as dead code
// until then. `pub` items above (read_needed/assert_static) are exempt from the lint; these
// private helpers are not. Drop the allows when the td-sh build wires them in.
#[allow(dead_code)]
const DT_NEEDED: u64 = 1; // .dynstr offset of a required shared-object name
const DT_STRTAB: u64 = 5; // vaddr of the .dynstr string table
const DT_RPATH: u64 = 15; // legacy run-path (string offset into .dynstr)
const DT_RUNPATH: u64 = 29; // run-path, takes precedence over DT_RPATH at load time

fn u16le(b: &[u8], off: usize) -> Result<u16, String> {
    b.get(off..off + 2)
        .map(|s| u16::from_le_bytes([s[0], s[1]]))
        .ok_or_else(|| format!("ELF truncated at u16 offset {off}"))
}
fn u32le(b: &[u8], off: usize) -> Result<u32, String> {
    b.get(off..off + 4)
        .map(|s| u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
        .ok_or_else(|| format!("ELF truncated at u32 offset {off}"))
}
fn u64le(b: &[u8], off: usize) -> Result<u64, String> {
    b.get(off..off + 8)
        .map(|s| u64::from_le_bytes(s.try_into().unwrap()))
        .ok_or_else(|| format!("ELF truncated at u64 offset {off}"))
}

/// Write a class-width word (u64 on ELF64, low u32 on ELF32) at `off`, little-endian.
fn put_word(b: &mut [u8], off: usize, v: u64, is64: bool) -> Result<(), String> {
    if is64 {
        b.get_mut(off..off + 8)
            .ok_or_else(|| format!("ELF truncated writing u64 at {off}"))?
            .copy_from_slice(&v.to_le_bytes());
    } else {
        b.get_mut(off..off + 4)
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
    put_word(b, ph + ph_field(&f, is64), v, is64)
}
/// Write a 4-byte program-header field (`p_type`/`p_flags`, which are u32 in BOTH classes).
fn set_ph_u32(b: &mut [u8], ph: usize, is64: bool, f: PField, v: u32) -> Result<(), String> {
    let off = ph + ph_field(&f, is64);
    b.get_mut(off..off + 4)
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
        let phoff = self.word(off)? as usize;
        let phentsize = u16le(self.b, ents)? as usize;
        let phnum = u16le(self.b, num)? as usize;
        if phentsize < min_ents {
            return Err(format!("implausible e_phentsize {phentsize}"));
        }
        Ok((phoff, phentsize, phnum))
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
            let ph = phoff + i * phentsize;
            if u32le(self.b, ph)? == pt {
                let off = self.word(ph + p_off)? as usize;
                let sz = self.word(ph + p_filesz)? as usize;
                if off + sz > self.b.len() {
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
            let ph = phoff + i * phentsize;
            if u32le(self.b, ph)? != PT_LOAD {
                continue;
            }
            let off = self.word(ph + p_off)?;
            let va = self.word(ph + p_vaddr)?;
            let fsz = self.word(ph + p_filesz)?;
            if vaddr >= va && vaddr < va + fsz {
                return Ok(Some((off + (vaddr - va)) as usize));
            }
        }
        Ok(None)
    }
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
        let ph = phoff + i * phentsize;
        if u32le(b, ph)? == PT_INTERP {
            let off = elf.word(ph + p_off)? as usize;
            let sz = elf.word(ph + p_filesz)? as usize;
            if off + sz > b.len() {
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

fn rpath_slots(b: &[u8]) -> Result<Option<RpathSlots>, String> {
    let elf = Elf::parse(b)?;
    let (doff, dsize) = match elf.segment_slot(PT_DYNAMIC, "PT_DYNAMIC")? {
        None => return Ok(None), // static binary — no dynamic section
        Some(x) => x,
    };
    // Elf64_Dyn is 16 bytes (d_tag u64 @0, d_un u64 @8); Elf32_Dyn is 8 (u32 @0, u32 @4).
    let (entsize, d_un) = if elf.is64 { (16, 8) } else { (8, 4) };
    let mut strtab_vaddr: Option<u64> = None;
    let mut entries: Vec<(u64, u64)> = Vec::new();
    for i in 0..(dsize / entsize) {
        let e = doff + i * entsize;
        let tag = elf.word(e)?;
        let val = elf.word(e + d_un)?;
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
/// td-sh musl-seed contract — has neither a dynamic section nor any needed library.
#[allow(dead_code)] // see DT_NEEDED: backs the pub API, dead until the td-sh verifier calls it
struct NeededSlots {
    strtab_off: usize,  // file offset of .dynstr (DT_STRTAB vaddr mapped through PT_LOAD)
    offsets: Vec<u64>,  // string offset into .dynstr of each DT_NEEDED name
}

#[allow(dead_code)] // see DT_NEEDED: backs the pub API, dead until the td-sh verifier calls it
fn needed_slots(b: &[u8]) -> Result<Option<NeededSlots>, String> {
    let elf = Elf::parse(b)?;
    let (doff, dsize) = match elf.segment_slot(PT_DYNAMIC, "PT_DYNAMIC")? {
        None => return Ok(None), // static binary — no dynamic section
        Some(x) => x,
    };
    // Elf64_Dyn is 16 bytes (d_tag u64 @0, d_un u64 @8); Elf32_Dyn is 8 (u32 @0, u32 @4).
    let (entsize, d_un) = if elf.is64 { (16, 8) } else { (8, 4) };
    let mut strtab_vaddr: Option<u64> = None;
    let mut offsets: Vec<u64> = Vec::new();
    for i in 0..(dsize / entsize) {
        let e = doff + i * entsize;
        let tag = elf.word(e)?;
        let val = elf.word(e + d_un)?;
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
            let raw = &b[off..off + sz];
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
        for (i, slot) in b[off..off + sz].iter_mut().enumerate() {
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
                let e = phoff + i * phentsize;
                match u32le(&b, e)? {
                    PT_NOTE if note.is_none() => note = Some(e),
                    PT_LOAD => {
                        let va = elf.word(e + pv)?;
                        let msz = elf.word(e + pm)?;
                        end = end.max(va + msz);
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
        .unwrap();
    let off = slots.strtab_off + pick.1 as usize;
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
        .map(|(_, v)| slots.strtab_off + *v as usize)
        .collect();
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

/// Read the DT_NEEDED shared-object names of a dynamic ELF — the libraries the loader would
/// pull in at run time. Returns an EMPTY vector for a fully static binary (no PT_DYNAMIC) or a
/// dynamic ELF that declares no needed libraries. This is td's OWN DT_NEEDED query so the
/// td-sh musl-seed verification asserts "this binary links nothing" without shelling out to a
/// host `readelf` (which would itself be host-executable ingress, re #469).
#[allow(dead_code)] // see DT_NEEDED: staged for the td-sh static-build verifier (this PR)
pub fn read_needed(path: &Path) -> Result<Vec<String>, String> {
    let b = std::fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let slots = match needed_slots(&b)? {
        None => return Ok(Vec::new()),
        Some(s) => s,
    };
    let mut names = Vec::with_capacity(slots.offsets.len());
    for o in slots.offsets {
        let off = slots.strtab_off + o as usize;
        let raw = b.get(off..).ok_or("DT_NEEDED string offset past end of file")?;
        let end = raw.iter().position(|&c| c == 0).unwrap_or(raw.len());
        names.push(String::from_utf8_lossy(&raw[..end]).into_owned());
    }
    Ok(names)
}

/// Assert an ELF is FULLY STATIC — no program interpreter (`PT_INTERP`), no `DT_NEEDED`
/// shared libraries, and no `DT_RPATH`/`DT_RUNPATH` run-path. This is the td-sh musl-seed
/// contract (re #469): td-sh is built by the root-seed Rust (the control plane), so a
/// *dynamically* linked td-sh would drag a host `/gnu/store` loader + glibc back in at run
/// time — exactly the host-runtime ingress #469 closes. A `x86_64-unknown-linux-musl` build
/// with a static C runtime has none of these; this check fails the build loudly (naming the
/// offending entry) if a regression reintroduces any of them.
#[allow(dead_code)] // see DT_NEEDED: staged for the td-sh static-build verifier (this PR)
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
mod tests {
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
        let (off, ents, num) = if is64 { (0x20, 0x36, 0x38) } else { (0x1C, 0x2A, 0x2C) };
        put_word(b, off, phoff as u64, is64);
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
        let dyn_size = 3 * dyn_entsize; // DT_STRTAB, DT_RPATH/RUNPATH, DT_NULL
        let strtab_off = dyn_off + dyn_size;
        let rpath_str_off = 1usize; // index 0 is the conventional empty string ("\0")
        let total = strtab_off + 1 + rpath.len() + 1;

        let mut b = vec![0u8; total];
        b[0..4].copy_from_slice(EI_MAG);
        b[EI_CLASS] = if is64 { 2 } else { 1 };
        b[EI_DATA] = 1;
        put_phdr_header(&mut b, ehdr, phentsize, phnum, is64);
        let (p_off, p_vaddr, p_filesz) = ph_field_offsets(is64);

        // PHDR 0: PT_LOAD covering the whole file, identity-mapped.
        let p0 = ehdr;
        b[p0..p0 + 4].copy_from_slice(&PT_LOAD.to_le_bytes());
        put_word(&mut b, p0 + p_off, 0, is64);
        put_word(&mut b, p0 + p_vaddr, 0, is64);
        put_word(&mut b, p0 + p_filesz, total as u64, is64);
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

        b[strtab_off + rpath_str_off..strtab_off + rpath_str_off + rpath.len()]
            .copy_from_slice(rpath.as_bytes());
        b
    }

    // A minimal dynamic ELF whose .dynstr holds each `needed` name, with one DT_NEEDED entry
    // per name (plus DT_STRTAB and the DT_NULL terminator). The single PT_LOAD is identity-
    // mapped, so the DT_STRTAB vaddr equals its file offset. Enough for the DT_NEEDED reader
    // and the static assertion; not a runnable binary.
    fn synth_needed_elf(needed: &[&str], is64: bool) -> Vec<u8> {
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
        put_phdr_header(&mut b, ehdr, phentsize, phnum, is64);
        let (p_off, p_vaddr, p_filesz) = ph_field_offsets(is64);

        // PHDR 0: PT_LOAD covering the whole file, identity-mapped.
        let p0 = ehdr;
        b[p0..p0 + 4].copy_from_slice(&PT_LOAD.to_le_bytes());
        put_word(&mut b, p0 + p_off, 0, is64);
        put_word(&mut b, p0 + p_vaddr, 0, is64);
        put_word(&mut b, p0 + p_filesz, total as u64, is64);
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
    // a fully static, non-dynamic executable (the shape the td-sh musl seed must produce).
    fn synth_static_elf(is64: bool) -> Vec<u8> {
        let (ehdr, phentsize) = if is64 { (64usize, 56usize) } else { (52usize, 32usize) };
        let phnum = 1usize;
        let total = ehdr + phnum * phentsize;
        let mut b = vec![0u8; total];
        b[0..4].copy_from_slice(EI_MAG);
        b[EI_CLASS] = if is64 { 2 } else { 1 };
        b[EI_DATA] = 1;
        put_phdr_header(&mut b, ehdr, phentsize, phnum, is64);
        let (p_off, p_vaddr, p_filesz) = ph_field_offsets(is64);
        let p0 = ehdr;
        b[p0..p0 + 4].copy_from_slice(&PT_LOAD.to_le_bytes());
        put_word(&mut b, p0 + p_off, 0, is64);
        put_word(&mut b, p0 + p_vaddr, 0, is64);
        put_word(&mut b, p0 + p_filesz, total as u64, is64);
        b
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
        // of the file and PT_INTERP is repointed at it (the interp is read from the file at
        // p_offset by the kernel, so no LOAD segment is needed). This is what lets rustc/cargo
        // point at the full hashed /td/store/<hash>-glibc.../ld-linux-x86-64.so.2 loader.
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
        // the td-sh musl-seed shape: no interpreter, no needed libs, no run-path — x86-64…
        std::fs::write(&f, synth_static_elf(true)).unwrap();
        assert!(assert_static(&f).is_ok());
        // …and i686 (the class check is class-independent)
        std::fs::write(&f, synth_static_elf(false)).unwrap();
        assert!(assert_static(&f).is_ok());
        std::fs::remove_dir_all(&dir).ok();
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

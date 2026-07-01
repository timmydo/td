//! Minimal, dependency-free ELF reader/writer — td's OWN replacement for the two
//! `patchelf` features the store-native relink/cleanup needs, so the build path adds NO
//! guix tool (patchelf would come from the host guix). This is deliberately NOT a full
//! patchelf: it only reads and rewrites two strings IN PLACE —
//!   - the program interpreter (`PT_INTERP`), which the upstream-Rust relink retargets to
//!     `/td/store/ld` (12 bytes, SHORTER than `/lib64/ld-linux-x86-64.so.2`, 27 bytes), so
//!     it fits the existing `p_filesz` slot (NUL-padded). No segment growing.
//!   - the run-path (`DT_RUNPATH` / legacy `DT_RPATH`), which makes a toolchain binary
//!     self-sufficient — e.g. retargeting an `ar`/`ranlib` build-dir search path to
//!     `/td/store/...lib` so it finds its shared libc without an `LD_LIBRARY_PATH` wrapper.
//! Both rewrites are in-place only: the new string (plus its NUL) must fit the existing
//! slot. Growing a string (or ADDING a run-path where none exists) would need the full
//! add-a-LOAD-segment / grow-.dynstr dance; if that is ever required, the setter errors
//! loudly rather than corrupting the file — a deliberate, visible boundary, not a silent
//! truncation.
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
const DT_NULL: u64 = 0; // end of the dynamic array
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

/// Rewrite the program interpreter (`PT_INTERP`) string in place. The new path (plus its
/// NUL terminator) must fit in the existing slot; any remaining bytes are NUL-padded.
/// Errors (without modifying the file) if there is no interpreter, or if the new path is
/// too long for the slot — the case that would need real segment growing.
pub fn set_interp(path: &Path, new_interp: &str) -> Result<(), String> {
    let mut b = std::fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let (off, sz) = interp_slot(&b)?
        .ok_or_else(|| format!("{}: no PT_INTERP (not an interpreted executable)", path.display()))?;
    let nb = new_interp.as_bytes();
    if nb.contains(&0) {
        return Err("new interpreter contains a NUL byte".into());
    }
    if nb.len() + 1 > sz {
        return Err(format!(
            "new interpreter {:?} ({} bytes + NUL) does not fit the {}-byte PT_INTERP slot \
             — would need segment growing (out of scope for this minimal rewriter)",
            new_interp,
            nb.len(),
            sz
        ));
    }
    for (i, slot) in b[off..off + sz].iter_mut().enumerate() {
        *slot = if i < nb.len() { nb[i] } else { 0 };
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

#[cfg(test)]
mod tests {
    use super::*;

    // A minimal little-endian ELF buffer with exactly one PT_INTERP program header whose
    // string slot holds `interp` (NUL-terminated). `is64` selects ELFCLASS64 (x86-64) or
    // ELFCLASS32 (i686 — the class the bootstrap toolchain cc1/as/ld actually is). Enough
    // for the reader/writer; not a runnable binary (no sections), which is all this needs.
    fn synth_interp_elf(interp: &str, is64: bool) -> Vec<u8> {
        let (ehdr, phentsize) = if is64 { (64usize, 56usize) } else { (52usize, 32usize) };
        let interp_off = ehdr + phentsize; // string right after the single phdr
        let slot = interp.len() + 1; // include the NUL terminator
        let mut b = vec![0u8; interp_off + slot];
        b[0..4].copy_from_slice(EI_MAG);
        b[EI_CLASS] = if is64 { 2 } else { 1 };
        b[EI_DATA] = 1; // little-endian
        put_phdr_header(&mut b, ehdr, phentsize, 1, is64);
        let (p_off, _p_vaddr, p_filesz) = ph_field_offsets(is64);
        // the single program header: PT_INTERP, p_offset, p_filesz
        b[ehdr..ehdr + 4].copy_from_slice(&PT_INTERP.to_le_bytes());
        put_word(&mut b, ehdr + p_off, interp_off as u64, is64);
        put_word(&mut b, ehdr + p_filesz, slot as u64, is64);
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
    fn refuses_too_long_interp() {
        let dir = std::env::temp_dir().join(format!("elf-test-l-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("a");
        std::fs::write(&f, synth_elf("/lib64/ld.so")).unwrap();
        // a longer path than the original slot must be refused, not silently truncated
        let err = set_interp(&f, "/td/store/aaaaaaaaaaaaaaaaaaaa/ld").unwrap_err();
        assert!(err.contains("does not fit"), "unexpected error: {err}");
        // and the file is unchanged
        assert_eq!(read_interp(&f).unwrap().as_deref(), Some("/lib64/ld.so"));
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
}

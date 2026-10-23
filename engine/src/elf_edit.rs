//! ELF interpreter and run-path rewriting for the pinned Rust toolchain.
#![allow(clippy::indexing_slicing)] // relocated existing writer
use crate::elf::*;
use std::path::Path;
const PT_NOTE: u32 = 4;
const PF_R: u32 = 4;
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

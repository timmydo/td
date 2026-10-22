//! Minimal synthetic ELF images for the ELF reader and writer tests.
//!
//! Compiled only under `cfg(test)`: td-engine declares it as a module and
//! the builder `#[path]`-includes it, so both crates' tests build the same
//! fixtures from one source.

#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used
)]

use crate::elf::{
    DT_NEEDED, DT_NULL, DT_RPATH, DT_RUNPATH, DT_STRTAB, EI_CLASS, EI_DATA, EI_MAG, EM_X86_64,
    ET_DYN, ET_EXEC, PT_DYNAMIC, PT_INTERP, PT_LOAD,
};

const PT_NOTE: u32 = 4;
const PF_X: u32 = 1;
const EV_CURRENT: u32 = 1;

// A minimal little-endian ELF buffer with exactly one PT_INTERP program header whose
// string slot holds `interp` (NUL-terminated). `is64` selects ELFCLASS64 (x86-64) or
// ELFCLASS32 (i686). Enough for the reader/writer; not a runnable binary (no
// sections), which is all this needs.
pub(crate) fn synth_interp_elf(interp: &str, is64: bool) -> Vec<u8> {
    let (ehdr, phentsize) = if is64 {
        (64usize, 56usize)
    } else {
        (52usize, 32usize)
    };
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
pub(crate) fn synth_elf(interp: &str) -> Vec<u8> {
    synth_interp_elf(interp, true)
}

// Write a class-width word (u64 on ELF64, u32 on ELF32) at `off`.
pub(crate) fn put_word(b: &mut [u8], off: usize, v: u64, is64: bool) {
    if is64 {
        b[off..off + 8].copy_from_slice(&v.to_le_bytes());
    } else {
        b[off..off + 4].copy_from_slice(&(v as u32).to_le_bytes());
    }
}
// Fill the e_phoff/e_phentsize/e_phnum header fields for the given class.
pub(crate) fn put_phdr_header(
    b: &mut [u8],
    phoff: usize,
    phentsize: usize,
    phnum: usize,
    is64: bool,
) {
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
pub(crate) fn ph_field_offsets(is64: bool) -> (usize, usize, usize) {
    if is64 {
        (0x08, 0x10, 0x20)
    } else {
        (0x04, 0x08, 0x10)
    }
}

// A minimal ELF with a PT_LOAD (identity-mapped: p_vaddr == p_offset == 0, so a
// DT_STRTAB vaddr equals its file offset) + a PT_DYNAMIC holding DT_STRTAB, one run-path
// entry (DT_RUNPATH if `runpath`, else legacy DT_RPATH), and DT_NULL. The .dynstr is
// `"\0" <rpath> "\0"`. `is64` selects the ELF class. Enough for the run-path reader/writer.
pub(crate) fn synth_dyn_elf(rpath: &str, runpath: bool, is64: bool) -> Vec<u8> {
    let (ehdr, phentsize, dyn_entsize, d_un) = if is64 {
        (64usize, 56usize, 16usize, 8usize)
    } else {
        (52usize, 32usize, 8usize, 4usize)
    };
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
    b[0x12..0x14].copy_from_slice(&(if is64 { EM_X86_64 } else { 3u16 }).to_le_bytes());
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
    put_dyn(
        &mut b,
        1,
        if runpath { DT_RUNPATH } else { DT_RPATH },
        rpath_str_off as u64,
    );
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
    let (ehdr, phentsize, dyn_entsize, d_un) = if is64 {
        (64usize, 56usize, 16usize, 8usize)
    } else {
        (52usize, 32usize, 8usize, 4usize)
    };
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
    b[0x12..0x14].copy_from_slice(&(if is64 { EM_X86_64 } else { 3u16 }).to_le_bytes());
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
pub(crate) fn synth_static_elf(is64: bool) -> Vec<u8> {
    let (ehdr, phentsize) = if is64 {
        (64usize, 56usize)
    } else {
        (52usize, 32usize)
    };
    let phnum = 1usize;
    let total = ehdr + phnum * phentsize;
    let mut b = vec![0u8; total];
    b[0..4].copy_from_slice(EI_MAG);
    b[EI_CLASS] = if is64 { 2 } else { 1 };
    b[EI_DATA] = 1;
    b[6] = 1;
    b[0x10..0x12].copy_from_slice(&ET_EXEC.to_le_bytes());
    b[0x12..0x14].copy_from_slice(&(if is64 { EM_X86_64 } else { 3u16 }).to_le_bytes());
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

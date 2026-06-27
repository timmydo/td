//! Minimal, dependency-free ELF64 PT_INTERP reader/writer — td's OWN replacement for
//! the one `patchelf` feature the rust-store-native relink needs, so the build path adds
//! NO guix tool (patchelf would come from the host guix). This is deliberately NOT a full
//! patchelf: it only reads and rewrites the program interpreter (`PT_INTERP`) string in
//! place, which is all the upstream-Rust relink requires —
//!   - the relink target loader path (`/td/store/ld`, 12 bytes) is SHORTER than the
//!     original (`/lib64/ld-linux-x86-64.so.2`, 27 bytes), so the new string fits in the
//!     existing `p_filesz` slot (NUL-padded). No segment growing, no PHDR relocation.
//!   - the rust binaries' DT_RUNPATH is already `$ORIGIN/../lib` (relative), so the deps
//!     resolve from the tree's own `lib/` and need no rewrite.
//! Growing the interpreter (or rewriting RUNPATH to a longer value) would need the full
//! add-a-LOAD-segment dance; if that is ever required, `set_interp` errors loudly rather
//! than corrupting the file — a deliberate, visible boundary, not a silent truncation.
//!
//! Scope: 32- and 64-bit little-endian ELF (i686 + x86-64) — the bootstrap toolchain is i686. Any
//! other class/endianness is rejected.

use std::path::Path;

// ELF identification (class-independent).
const EI_MAG: &[u8] = b"\x7fELF";
const EI_CLASS: usize = 4; // 1 = ELFCLASS32, 2 = ELFCLASS64
const EI_DATA: usize = 5; // 1 = ELFDATA2LSB
const PT_INTERP: u32 = 3;

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

/// Locate the PT_INTERP program header and return `(file_offset, filesz)` of its
/// interpreter string, or `None` if the ELF has no interpreter (e.g. a shared object).
fn interp_slot(b: &[u8]) -> Result<Option<(usize, usize)>, String> {
    if b.len() < 52 || &b[0..4] != EI_MAG {
        return Err("not an ELF file (bad magic)".into());
    }
    // Both classes are supported: ELFCLASS32 (i686 — the bootstrap toolchain) and ELFCLASS64
    // (x86-64). The header + program-header field offsets differ by class; PT_INTERP is the same.
    let is64 = match b[EI_CLASS] {
        1 => false,
        2 => true,
        c => return Err(format!("unknown ELF class {c} (only ELFCLASS32/64 supported)")),
    };
    if b[EI_DATA] != 1 {
        return Err("not ELFDATA2LSB (only little-endian ELF is supported)".into());
    }
    // (e_phoff, e_phentsize, e_phnum) and the min plausible phentsize, per class.
    let (phoff, phentsize, phnum, min_phentsize) = if is64 {
        (u64le(b, 0x20)? as usize, u16le(b, 0x36)? as usize, u16le(b, 0x38)? as usize, 0x38)
    } else {
        (u32le(b, 0x1C)? as usize, u16le(b, 0x2A)? as usize, u16le(b, 0x2C)? as usize, 0x20)
    };
    if phentsize < min_phentsize {
        return Err(format!("implausible e_phentsize {phentsize}"));
    }
    for i in 0..phnum {
        let ph = phoff + i * phentsize;
        if u32le(b, ph)? == PT_INTERP {
            // p_offset / p_filesz: u64 at +0x08/+0x20 (ELF64) vs u32 at +0x04/+0x10 (ELF32).
            let (off, sz) = if is64 {
                (u64le(b, ph + 0x08)? as usize, u64le(b, ph + 0x20)? as usize)
            } else {
                (u32le(b, ph + 0x04)? as usize, u32le(b, ph + 0x10)? as usize)
            };
            if off + sz > b.len() {
                return Err("PT_INTERP string runs past end of file".into());
            }
            return Ok(Some((off, sz)));
        }
    }
    Ok(None)
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

#[cfg(test)]
mod tests {
    use super::*;

    // A minimal ELF64 LE buffer with exactly one PT_INTERP program header whose string
    // slot holds `interp` (NUL-terminated). Enough for the reader/writer; not a runnable
    // binary (no sections), which is all this unit needs.
    fn synth_elf(interp: &str) -> Vec<u8> {
        let phoff = 64usize;
        let phentsize = 56usize;
        let interp_off = phoff + phentsize; // string right after the single phdr
        let slot = interp.len() + 1; // include the NUL terminator
        let mut b = vec![0u8; interp_off + slot];
        b[0..4].copy_from_slice(EI_MAG);
        b[EI_CLASS] = 2; // ELFCLASS64
        b[EI_DATA] = 1; // little-endian
        b[0x20..0x28].copy_from_slice(&(phoff as u64).to_le_bytes()); // e_phoff
        b[0x36..0x38].copy_from_slice(&(phentsize as u16).to_le_bytes()); // e_phentsize
        b[0x38..0x3a].copy_from_slice(&1u16.to_le_bytes()); // e_phnum
        // the single program header: PT_INTERP, p_offset (+0x08), p_filesz (+0x20)
        b[phoff..phoff + 4].copy_from_slice(&PT_INTERP.to_le_bytes());
        b[phoff + 0x08..phoff + 0x10].copy_from_slice(&(interp_off as u64).to_le_bytes());
        b[phoff + 0x20..phoff + 0x28].copy_from_slice(&(slot as u64).to_le_bytes());
        b[interp_off..interp_off + interp.len()].copy_from_slice(interp.as_bytes());
        b
    }

    // The 32-bit (i686) analogue — the class the bootstrap toolchain (cc1/as/ld) actually is.
    fn synth_elf32(interp: &str) -> Vec<u8> {
        let phoff = 52usize; // ELF32 header is 52 bytes
        let phentsize = 32usize; // ELF32 program-header entry
        let interp_off = phoff + phentsize;
        let slot = interp.len() + 1;
        let mut b = vec![0u8; interp_off + slot];
        b[0..4].copy_from_slice(EI_MAG);
        b[EI_CLASS] = 1; // ELFCLASS32
        b[EI_DATA] = 1; // little-endian
        b[0x1c..0x20].copy_from_slice(&(phoff as u32).to_le_bytes()); // e_phoff (u32)
        b[0x2a..0x2c].copy_from_slice(&(phentsize as u16).to_le_bytes()); // e_phentsize
        b[0x2c..0x2e].copy_from_slice(&1u16.to_le_bytes()); // e_phnum
        // the single program header: PT_INTERP, p_offset (+0x04), p_filesz (+0x10) — all u32
        b[phoff..phoff + 4].copy_from_slice(&PT_INTERP.to_le_bytes());
        b[phoff + 0x04..phoff + 0x08].copy_from_slice(&(interp_off as u32).to_le_bytes());
        b[phoff + 0x10..phoff + 0x14].copy_from_slice(&(slot as u32).to_le_bytes());
        b[interp_off..interp_off + interp.len()].copy_from_slice(interp.as_bytes());
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
        std::fs::write(&f, synth_elf32("/lib/ld-linux.so.2")).unwrap();
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
}

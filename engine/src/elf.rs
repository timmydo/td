//! Read-only ELF inspection shared by the image build and the target.
//!
//! The separate `elf_edit` module retargets the pinned Rust toolchain during
//! builds and first-use downloads (APPLICATIONS.md §G). What lives here only reads: the
//! class-dispatching header parser, the `PT_INTERP`, run-path and `DT_NEEDED`
//! readers, the bounded link search the dynamic-application validator runs
//! over hostile payloads, and the executable and static-shape assertions.
//!
//! Scope: 32- and 64-bit little-endian ELF; application checks are x86-64.

use std::io::Read;
use std::path::Path;

// ELF identification (class-independent).
pub const EI_MAG: &[u8] = b"\x7fELF";
pub const EI_CLASS: usize = 4; // 1 = ELFCLASS32, 2 = ELFCLASS64
pub const EI_DATA: usize = 5; // 1 = ELFDATA2LSB

// Program-header types and dynamic-section tags (class-independent values).
pub const PT_LOAD: u32 = 1;
pub const PT_DYNAMIC: u32 = 2;
pub const PT_INTERP: u32 = 3;
pub(crate) const PF_X: u32 = 1; // segment executable
pub const ET_EXEC: u16 = 2;
pub const ET_DYN: u16 = 3;
pub const EM_X86_64: u16 = 62;
pub(crate) const EM_AARCH64: u16 = 183;
pub(crate) const EV_CURRENT: u32 = 1;
pub const DT_NULL: u64 = 0; // end of the dynamic array
pub const DT_NEEDED: u64 = 1; // .dynstr offset of a required shared-object name
pub const DT_STRTAB: u64 = 5; // vaddr of the .dynstr string table
pub const DT_RPATH: u64 = 15; // legacy run-path (string offset into .dynstr)
pub const DT_RUNPATH: u64 = 29; // run-path, takes precedence over DT_RPATH at load time
pub(crate) const DT_DEPAUDIT: u64 = 0x6fff_fefb;
pub(crate) const DT_AUDIT: u64 = 0x6fff_fefc;
pub(crate) const DT_AUXILIARY: u64 = 0x7fff_fffd;
pub(crate) const DT_FILTER: u64 = 0x7fff_ffff;

/// `(PT_INTERP, run-path entries, DT_NEEDED names)` of one ELF.
pub type LinkReferences = (Option<String>, Vec<String>, Vec<String>);

/// The bytes before the first NUL, or all of them.
fn nul_terminated(raw: &[u8]) -> &[u8] {
    raw.split(|byte| *byte == 0).next().unwrap_or_default()
}

pub fn u16le(b: &[u8], off: usize) -> Result<u16, String> {
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
pub fn u32le(b: &[u8], off: usize) -> Result<u32, String> {
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
pub fn u64le(b: &[u8], off: usize) -> Result<u64, String> {
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

/// A validated little-endian ELF buffer carrying its class. The header + program-header +
/// dynamic-entry field offsets differ between ELFCLASS32 (i686) and ELFCLASS64 (x86-64);
/// every class-dependent access goes through one of these methods so the PT_INTERP and
/// DT_RPATH/DT_RUNPATH paths share a single class dispatch.
pub struct Elf<'a> {
    pub b: &'a [u8],
    pub is64: bool,
}

impl<'a> Elf<'a> {
    pub fn parse(b: &'a [u8]) -> Result<Elf<'a>, String> {
        if b.len() < 52 || b.get(..4) != Some(EI_MAG) {
            return Err("not an ELF file (bad magic)".into());
        }
        let is64 = match b.get(EI_CLASS).copied() {
            Some(1) => false,
            Some(2) => true,
            c => {
                return Err(format!(
                    "unknown ELF class {} (only ELFCLASS32/64 supported)",
                    c.unwrap_or_default()
                ))
            }
        };
        if b.get(EI_DATA).copied() != Some(1) {
            return Err("not ELFDATA2LSB (only little-endian ELF is supported)".into());
        }
        Ok(Elf { b, is64 })
    }

    /// Read a class-width word — u64 on ELF64, u32 (zero-extended) on ELF32.
    pub fn word(&self, off: usize) -> Result<u64, String> {
        if self.is64 {
            u64le(self.b, off)
        } else {
            Ok(u32le(self.b, off)? as u64)
        }
    }

    /// `(e_phoff, e_phentsize, e_phnum)` for the program-header table.
    pub fn phdr_table(&self) -> Result<(usize, usize, usize), String> {
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

    pub fn phdr_offset(
        &self,
        phoff: usize,
        phentsize: usize,
        index: usize,
    ) -> Result<usize, String> {
        index
            .checked_mul(phentsize)
            .and_then(|offset| phoff.checked_add(offset))
            .ok_or_else(|| format!("ELF program-header {index} offset overflow"))
    }

    pub fn field_offset(ph: usize, field: usize, what: &str) -> Result<usize, String> {
        ph.checked_add(field)
            .ok_or_else(|| format!("ELF {what} field offset overflow"))
    }

    /// `(p_offset, p_vaddr, p_filesz)` field offsets within a program-header entry.
    pub fn ph_fields(&self) -> (usize, usize, usize) {
        if self.is64 {
            (0x08, 0x10, 0x20)
        } else {
            (0x04, 0x08, 0x10)
        }
    }

    /// Locate the first program header of type `pt` and return `(file_offset, filesz)` of
    /// the data it points at, or `None` if no such segment exists.
    pub(crate) fn segment_slot(
        &self,
        pt: u32,
        what: &str,
    ) -> Result<Option<(usize, usize)>, String> {
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
    pub(crate) fn vaddr_to_off(&self, vaddr: u64) -> Result<Option<usize>, String> {
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

/// Locate the PT_INTERP program header and return `(file_offset, filesz)` of its
/// interpreter string, or `None` if the ELF has no interpreter (e.g. a shared object).
pub fn interp_slot(b: &[u8]) -> Result<Option<(usize, usize)>, String> {
    Elf::parse(b)?.segment_slot(PT_INTERP, "PT_INTERP string")
}

/// The .dynstr file offset plus the `(tag, string-offset)` of every DT_RPATH/DT_RUNPATH
/// entry, or `None` if the ELF has no PT_DYNAMIC or no run-path entry at all.
pub struct RpathSlots {
    pub strtab_off: usize, // file offset of .dynstr (DT_STRTAB vaddr mapped through PT_LOAD)
    pub entries: Vec<(u64, u64)>, // (DT_RPATH|DT_RUNPATH, string offset into .dynstr)
}

pub(crate) fn terminated_dynamic_entry_count(
    b: &[u8],
    elf: &Elf<'_>,
    offset: usize,
    size: usize,
    entry_size: usize,
    max_dynamic_entries: usize,
) -> Result<usize, String> {
    if !size.is_multiple_of(entry_size) {
        return Err("PT_DYNAMIC size is not aligned to its entry size".into());
    }
    let count = size / entry_size;
    if count > max_dynamic_entries {
        return Err(format!("PT_DYNAMIC exceeds {max_dynamic_entries} entries"));
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

pub fn rpath_slots(b: &[u8]) -> Result<Option<RpathSlots>, String> {
    rpath_slots_with_limit(b, usize::MAX)
}

pub(crate) fn rpath_slots_with_limit(
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
    let entry_count =
        terminated_dynamic_entry_count(b, &elf, doff, dsize, entsize, max_dynamic_entries)?;
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
    Ok(Some(RpathSlots {
        strtab_off,
        entries,
    }))
}

/// The .dynstr file offset plus the `.dynstr` string offset of every DT_NEEDED entry (each
/// names a shared object the loader would pull in at run time), or `None` if the ELF has no
/// PT_DYNAMIC or no DT_NEEDED at all. Mirrors `rpath_slots`: a fully static binary — the
/// static-bootstrap contract — has neither a dynamic section nor any needed library.
pub(crate) struct NeededSlots {
    pub strtab_off: usize, // file offset of .dynstr (DT_STRTAB vaddr mapped through PT_LOAD)
    pub offsets: Vec<u64>, // string offset into .dynstr of each DT_NEEDED name
}

pub(crate) fn needed_slots(b: &[u8]) -> Result<Option<NeededSlots>, String> {
    needed_slots_with_limit(b, usize::MAX)
}

pub(crate) fn needed_slots_with_limit(
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
    let entry_count =
        terminated_dynamic_entry_count(b, &elf, doff, dsize, entsize, max_dynamic_entries)?;
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
    Ok(Some(NeededSlots {
        strtab_off,
        offsets,
    }))
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
            Ok(Some(
                String::from_utf8_lossy(nul_terminated(raw)).into_owned(),
            ))
        }
    }
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
    let raw = b
        .get(off..)
        .ok_or("DT_RPATH/DT_RUNPATH string offset past end of file")?;
    Ok(Some(
        String::from_utf8_lossy(nul_terminated(raw)).into_owned(),
    ))
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
pub fn runtime_link_search(path: &Path) -> Result<LinkReferences, String> {
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
            Some(String::from_utf8_lossy(nul_terminated(raw)).into_owned())
        }
    };
    let mut dirs: Vec<String> = Vec::new();
    if let Some(slots) = rpath_slots(&b)? {
        // Every DT_RPATH and DT_RUNPATH slot (the loader prefers RUNPATH, but a closure over
        // ALL run-path store dirs is the safe superset — it never DROPS a real provider dir).
        for (_tag, v) in &slots.entries {
            let string_offset = usize::try_from(*v)
                .map_err(|_| "DT_RPATH/DT_RUNPATH string offset does not fit this architecture")?;
            let off = slots
                .strtab_off
                .checked_add(string_offset)
                .ok_or("DT_RPATH/DT_RUNPATH string offset overflow")?;
            let raw = b
                .get(off..)
                .ok_or("DT_RPATH/DT_RUNPATH string offset past end of file")?;
            for entry in String::from_utf8_lossy(nul_terminated(raw)).split(':') {
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
pub struct RuntimeLinkLimits {
    pub file_bytes: u64,
    pub dynamic_entries: usize,
    pub references: usize,
    pub string_bytes: usize,
    pub aggregate_text_bytes: usize,
}

#[derive(Clone, Debug)]
pub struct RuntimeLinkSearch {
    pub file_bytes: u64,
    pub kind: RuntimeElfKind,
    pub executable: bool,
    pub interpreter: Option<String>,
    pub run_paths: Vec<String>,
    pub run_path_kind: RuntimeRunPathKind,
    pub needed: Vec<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuntimeRunPathKind {
    None,
    Rpath,
    Runpath,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuntimeElfKind {
    NotElf,
    Executable,
    SharedObject,
    Other(u16),
}

pub(crate) fn validate_runtime_loadable_shape(bytes: &[u8], elf: &Elf<'_>) -> Result<(), String> {
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
        let virtual_address =
            elf.word(Elf::field_offset(offset, 0x10, "PT_LOAD virtual address")?)?;
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

pub(crate) fn bounded_dynamic_string<'a>(
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

pub(crate) fn account_runtime_reference(
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

pub(crate) fn reject_unmodeled_loader_objects(
    bytes: &[u8],
    max_dynamic_entries: usize,
) -> Result<(), String> {
    let elf = Elf::parse(bytes)?;
    let (offset, size) = match elf.segment_slot(PT_DYNAMIC, "PT_DYNAMIC")? {
        Some(slot) => slot,
        None => return Ok(()),
    };
    let (entry_size, value_offset) = if elf.is64 { (16, 8) } else { (8, 4) };
    let entry_count =
        terminated_dynamic_entry_count(bytes, &elf, offset, size, entry_size, max_dynamic_entries)?;
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

pub fn runtime_link_search_bounded(
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
    let mut file =
        std::fs::File::open(path).map_err(|error| format!("read {}: {error}", path.display()))?;
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
    if matches!(
        kind,
        RuntimeElfKind::Executable | RuntimeElfKind::SharedObject
    ) {
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
                return Err(format!("PT_INTERP exceeds {} bytes", limits.string_bytes));
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
            return Err("dynamic application ELF carries both DT_RPATH and DT_RUNPATH".into());
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
            let value =
                bounded_dynamic_string(&bytes, offset, limits, &mut aggregate, "DT_NEEDED")?;
            account_runtime_reference(&mut references, limits)?;
            needed.push(value.to_string());
        }
    }
    let executable = matches!(
        kind,
        RuntimeElfKind::Executable | RuntimeElfKind::SharedObject
    ) && assert_x86_64_executable_bytes(path, &bytes).is_ok();
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

pub(crate) fn needed_names(b: &[u8]) -> Result<Vec<String>, String> {
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
        let raw = b
            .get(off..)
            .ok_or("DT_NEEDED string offset past end of file")?;
        names.push(String::from_utf8_lossy(nul_terminated(raw)).into_owned());
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
pub(crate) fn assert_x86_64_executable_bytes(path: &Path, bytes: &[u8]) -> Result<(), String> {
    assert_64_executable_bytes(path, bytes, EM_X86_64, "EM_X86_64")
}

pub(crate) fn assert_64_executable_bytes(
    path: &Path,
    bytes: &[u8],
    expected_machine: u16,
    name: &str,
) -> Result<(), String> {
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
    if machine != expected_machine {
        return Err(format!(
            "{}: application entry has ELF machine {machine}, expected {name}",
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
    let table_end = phoff.checked_add(table_size).ok_or_else(|| {
        format!(
            "{}: ELF program-header table offset overflow",
            path.display()
        )
    })?;
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
        let virtual_address =
            elf.word(Elf::field_offset(offset, 0x10, "PT_LOAD virtual address")?)?;
        let file_size = elf.word(Elf::field_offset(offset, 0x20, "PT_LOAD file size")?)?;
        let memory_size = elf.word(Elf::field_offset(offset, 0x28, "PT_LOAD memory size")?)?;
        if file_size > memory_size {
            return Err(format!(
                "{}: PT_LOAD has p_filesz larger than p_memsz",
                path.display()
            ));
        }
        let file_end = file_offset
            .checked_add(file_size)
            .ok_or_else(|| format!("{}: PT_LOAD file range overflow", path.display()))?;
        if file_end > bytes.len() as u64 {
            return Err(format!("{}: PT_LOAD runs past end of file", path.display()));
        }
        virtual_address
            .checked_add(memory_size)
            .ok_or_else(|| format!("{}: PT_LOAD memory address range overflow", path.display()))?;
        let file_backed_end = virtual_address.checked_add(file_size).ok_or_else(|| {
            format!(
                "{}: PT_LOAD file-backed address range overflow",
                path.display()
            )
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

pub fn assert_x86_64_executable_bounded(path: &Path, max_bytes: u64) -> Result<(), String> {
    assert_executable_bounded(path, max_bytes, EM_X86_64, "EM_X86_64")
}

pub fn assert_aarch64_executable_bounded(path: &Path, max_bytes: u64) -> Result<(), String> {
    assert_executable_bounded(path, max_bytes, EM_AARCH64, "EM_AARCH64")
}

pub(crate) fn assert_executable_bounded(
    path: &Path,
    max_bytes: u64,
    machine: u16,
    name: &str,
) -> Result<(), String> {
    let file =
        std::fs::File::open(path).map_err(|error| format!("read {}: {error}", path.display()))?;
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
    std::io::Read::read_to_end(&mut file.take(max_bytes.saturating_add(1)), &mut bytes)
        .map_err(|error| format!("read {}: {error}", path.display()))?;
    if u64::try_from(bytes.len()).map_or(true, |read| read > max_bytes) {
        return Err(format!(
            "{} changed while reading and exceeds the {max_bytes}-byte executable ELF limit",
            path.display()
        ));
    }
    assert_64_executable_bytes(path, &bytes, machine, name)
}

/// Assert an ELF is FULLY STATIC — no program interpreter (`PT_INTERP`), no `DT_NEEDED`
/// shared libraries, and no `DT_RPATH`/`DT_RUNPATH` run-path. This is a runtime-provenance
/// contract (re #469): a *dynamically* linked binary drags a host loader + glibc back in at
/// run time — exactly the host-runtime ingress #469 closes. Its consumer is the
/// `Step::AssertStatic` step, which fails a recipe whose static output regresses to a
/// host loader/libc.
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

/// Assert an ELF is a STATIC POSITION-INDEPENDENT executable: everything
/// `assert_static` requires, and `ET_DYN` rather than `ET_EXEC`. The Cargo
/// runner's static mode installs nothing else, so a link that kept its bytes
/// static but fell back to a fixed load address is caught where the binary is
/// installed rather than by a later recipe check.
pub fn assert_static_pie(path: &Path) -> Result<(), String> {
    assert_static(path)?;
    let bytes = std::fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let kind = u16le(&bytes, 0x10)?;
    if kind != ET_DYN {
        return Err(format!(
            "{}: expected a static position-independent executable (ET_DYN) but its ELF type is {kind}",
            path.display()
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::panic,
        clippy::unwrap_used
    )]

    use super::*;
    use crate::elf_synth::*;

    #[test]
    fn reads_interp() {
        let _dir_cleanup = crate::test_temp::TempDir::new("elf-test-r").unwrap();
        let dir = _dir_cleanup.join("fixture");
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("a");
        std::fs::write(&f, synth_elf("/lib64/ld-linux-x86-64.so.2")).unwrap();
        assert_eq!(
            read_interp(&f).unwrap().as_deref(),
            Some("/lib64/ld-linux-x86-64.so.2")
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn rejects_non_elf() {
        let _dir_cleanup = crate::test_temp::TempDir::new("elf-test-n").unwrap();
        let dir = _dir_cleanup.join("fixture");
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("a");
        std::fs::write(
            &f,
            b"not an elf at all, just text padding padding padding padding",
        )
        .unwrap();
        assert!(read_interp(&f).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn reads_runpath_and_legacy_rpath() {
        let _dir_cleanup = crate::test_temp::TempDir::new("elf-test-rp").unwrap();
        let dir = _dir_cleanup.join("fixture");
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
    fn reads_needed_shared_objects() {
        let _dir_cleanup = crate::test_temp::TempDir::new("elf-test-need").unwrap();
        let dir = _dir_cleanup.join("fixture");
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("a");
        // multiple DT_NEEDED, in order
        std::fs::write(&f, synth_needed_elf(&["libc.so.6", "libm.so.6"], true)).unwrap();
        assert_eq!(
            read_needed(&f).unwrap(),
            vec!["libc.so.6".to_string(), "libm.so.6".to_string()]
        );
        // ELF32 reads back too
        std::fs::write(&f, synth_needed_elf(&["ld-linux.so.2"], false)).unwrap();
        assert_eq!(read_needed(&f).unwrap(), vec!["ld-linux.so.2".to_string()]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn runtime_link_search_returns_interp_and_splits_runpath() {
        let _dir_cleanup = crate::test_temp::TempDir::new("elf-test-rls").unwrap();
        let dir = _dir_cleanup.join("fixture");
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("a");
        // A run-path-only dynamic ELF (no interp): the colon-separated entries split out.
        std::fs::write(
            &f,
            synth_dyn_elf("/gnu/store/aaa/lib:/gnu/store/bbb/lib", true, true),
        )
        .unwrap();
        let (interp, dirs, needed) = runtime_link_search(&f).unwrap();
        assert_eq!(interp, None);
        assert_eq!(
            dirs,
            vec![
                "/gnu/store/aaa/lib".to_string(),
                "/gnu/store/bbb/lib".to_string()
            ]
        );
        assert!(needed.is_empty());
        // An interp-only ELF (no PT_DYNAMIC): interp out, no run-path.
        std::fs::write(&f, synth_elf("/gnu/store/ccc/lib/ld-linux-x86-64.so.2")).unwrap();
        let (interp, dirs, needed) = runtime_link_search(&f).unwrap();
        assert_eq!(
            interp.as_deref(),
            Some("/gnu/store/ccc/lib/ld-linux-x86-64.so.2")
        );
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
        let _dir_cleanup = crate::test_temp::TempDir::new("elf-test-bounded-rls").unwrap();
        let dir = _dir_cleanup.join("fixture");
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
        let _dir_cleanup = crate::test_temp::TempDir::new("elf-test-need0").unwrap();
        let dir = _dir_cleanup.join("fixture");
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
        let _dir_cleanup = crate::test_temp::TempDir::new("elf-test-as-ok").unwrap();
        let dir = _dir_cleanup.join("fixture");
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
    fn assert_static_pie_requires_et_dyn_on_top_of_the_static_shape() {
        let _dir_cleanup = crate::test_temp::TempDir::new("elf-test-aspie").unwrap();
        let dir = _dir_cleanup.join("fixture");
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("a");
        // A static ET_EXEC passes the static check and fails the PIE one by type.
        std::fs::write(&f, synth_static_elf(true)).unwrap();
        assert!(assert_static(&f).is_ok());
        let err = assert_static_pie(&f).unwrap_err();
        assert!(err.contains("ELF type is 2"), "{err}");
        // The same bytes as ET_DYN are the static-PIE shape.
        let mut pie = synth_static_elf(true);
        pie[0x10..0x12].copy_from_slice(&ET_DYN.to_le_bytes());
        std::fs::write(&f, pie).unwrap();
        assert!(assert_static_pie(&f).is_ok());
        // Dynamic linkage still fails first, whatever the type says.
        std::fs::write(&f, synth_needed_elf(&["libc.so.6"], true)).unwrap();
        let err = assert_static_pie(&f).unwrap_err();
        assert!(err.contains("dynamically links"), "{err}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn portable_arm_executable_does_not_pass_the_x86_launcher_contract() {
        let _dir_cleanup = crate::test_temp::TempDir::new("elf-arm-exec").unwrap();
        let dir = _dir_cleanup.join("fixture");
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("app");
        let mut valid = synth_static_elf(true);
        std::fs::write(&file, &valid).unwrap();
        let limit = u64::try_from(valid.len()).unwrap();
        assert!(assert_aarch64_executable_bounded(&file, limit).is_err());
        valid[0x12..0x14].copy_from_slice(&EM_AARCH64.to_le_bytes());
        std::fs::write(&file, &valid).unwrap();
        assert!(assert_aarch64_executable_bounded(&file, limit).is_ok());
        assert!(assert_aarch64_executable_bounded(&file, limit - 1).is_err());
        assert!(assert_x86_64_executable(&file).is_err());
        assert!(assert_x86_64_executable_bounded(&file, limit).is_err());
        valid[0x18..0x20].copy_from_slice(&0u64.to_le_bytes());
        std::fs::write(&file, &valid).unwrap();
        assert!(assert_aarch64_executable_bounded(&file, limit)
            .unwrap_err()
            .contains("zero ELF entry"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn application_executable_requires_x86_64_kind_and_file_backed_entry() {
        let _dir_cleanup = crate::test_temp::TempDir::new("elf-app-exec").unwrap();
        let dir = _dir_cleanup.join("fixture");
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("app");
        let valid = synth_static_elf(true);
        std::fs::write(&file, &valid).unwrap();
        assert!(assert_x86_64_executable(&file).is_ok());
        assert!(
            assert_x86_64_executable_bounded(&file, u64::try_from(valid.len()).unwrap()).is_ok()
        );
        let error =
            assert_x86_64_executable_bounded(&file, u64::try_from(valid.len()).unwrap() - 1)
                .unwrap_err();
        assert!(error.contains("executable ELF limit"), "{error}");

        for (bytes, expected) in [
            (
                {
                    let mut bytes = valid.clone();
                    bytes[0x10..0x12].copy_from_slice(&1u16.to_le_bytes());
                    bytes
                },
                "ET_EXEC or ET_DYN",
            ),
            (
                {
                    let mut bytes = valid.clone();
                    bytes[0x12..0x14].copy_from_slice(&3u16.to_le_bytes());
                    bytes
                },
                "EM_X86_64",
            ),
            (
                {
                    let mut bytes = valid.clone();
                    bytes[0x18..0x20].copy_from_slice(&0u64.to_le_bytes());
                    bytes
                },
                "zero ELF entry point",
            ),
            (
                {
                    let mut bytes = valid.clone();
                    bytes[64 + 4..64 + 8].copy_from_slice(&0u32.to_le_bytes());
                    bytes
                },
                "not covered",
            ),
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
        malformed_second_load[second + 0x20..second + 0x28].copy_from_slice(&1u64.to_le_bytes());
        std::fs::write(&file, malformed_second_load).unwrap();
        let error = assert_x86_64_executable(&file).unwrap_err();
        assert!(error.contains("p_filesz larger than p_memsz"), "{error}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn static_assertion_refuses_overflowing_segment_metadata_without_panicking() {
        let _dir_cleanup = crate::test_temp::TempDir::new("elf-static-ranges").unwrap();
        let dir = _dir_cleanup.join("fixture");
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
            malformed[second + 0x08..second + 0x10].copy_from_slice(&u64::MAX.to_le_bytes());
            malformed[second + 0x20..second + 0x28].copy_from_slice(&1u64.to_le_bytes());
            std::fs::write(&file, malformed).unwrap();
            let error = assert_static(&file).unwrap_err();
            assert!(error.contains(expected), "{error}");
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn assert_static_rejects_dynamic_linkage() {
        let _dir_cleanup = crate::test_temp::TempDir::new("elf-test-as-bad").unwrap();
        let dir = _dir_cleanup.join("fixture");
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

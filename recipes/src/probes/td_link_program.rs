//! td-link-test's program, built as a dynamic PIE, a static ET_EXEC and a
//! static PIE. It reads its own file (its first argument) and checks the
//! ELF shape its `td_link_kind` cfg names, its run path and the libraries
//! it needs, that LLD linked it, that a panic unwinds, that a thread runs,
//! and that the proc macro td-link linked expanded inside rustc. Any further
//! argument is a shared object td-link linked, checked the same way.
#![forbid(unsafe_code)]

use std::process::ExitCode;

#[cfg(td_link_kind = "static_exec")]
const ET_EXEC: u16 = 2;
const ET_DYN: u16 = 3;
const PT_LOAD: u32 = 1;
const PT_DYNAMIC: u32 = 2;
const PT_INTERP: u32 = 3;
const DT_NULL: u64 = 0;
const DT_NEEDED: u64 = 1;
const DT_STRTAB: u64 = 5;
const DT_RPATH: u64 = 15;
const DT_RUNPATH: u64 = 29;

/// td glibc's loader, from the recipe; its directory is the run path
/// td-link writes into every dynamic output.
const GLIBC_INTERP: &str = env!("TD_LINK_TEST_INTERP");

/// The libraries a td-link output may need: glibc's, whose loader also
/// serves gcc-runtime's static unwinder's `_dl_find_object`. Never
/// `libgcc_s.so.1`: libgcc is static.
const GLIBC_NEEDED: &[&str] = &["libc.so.6", "ld-linux-x86-64.so.2"];

#[cfg(td_link_kind = "dynamic_pie")]
const KIND: (&str, u16, bool) = ("dynamic PIE", ET_DYN, true);
#[cfg(td_link_kind = "static_exec")]
const KIND: (&str, u16, bool) = ("static ET_EXEC", ET_EXEC, false);
#[cfg(td_link_kind = "static_pie")]
const KIND: (&str, u16, bool) = ("static PIE", ET_DYN, false);

fn add(a: u64, b: u64) -> Result<u64, String> {
    a.checked_add(b)
        .ok_or_else(|| "ELF offset overflows".into())
}

fn mul(a: u64, b: u64) -> Result<u64, String> {
    a.checked_mul(b)
        .ok_or_else(|| "ELF offset overflows".into())
}

/// The dynamic section's run paths and needed libraries.
#[derive(Default)]
struct Dynamic {
    rpath: Option<String>,
    runpath: Option<String>,
    needed: Vec<String>,
}

struct Elf<'a>(&'a [u8]);

impl Elf<'_> {
    fn bytes(&self, at: u64, len: u64) -> Result<&[u8], String> {
        let start = usize::try_from(at).map_err(|e| e.to_string())?;
        let end = start
            .checked_add(usize::try_from(len).map_err(|e| e.to_string())?)
            .ok_or("ELF range overflows")?;
        self.0
            .get(start..end)
            .ok_or_else(|| format!("ELF range {start}..{end} is outside the file"))
    }

    fn array<const N: usize>(&self, at: u64) -> Result<[u8; N], String> {
        let len = u64::try_from(N).map_err(|e| e.to_string())?;
        <[u8; N]>::try_from(self.bytes(at, len)?).map_err(|e| e.to_string())
    }

    fn u16(&self, at: u64) -> Result<u16, String> {
        self.array(at).map(u16::from_le_bytes)
    }

    fn u32(&self, at: u64) -> Result<u32, String> {
        self.array(at).map(u32::from_le_bytes)
    }

    fn u64(&self, at: u64) -> Result<u64, String> {
        self.array(at).map(u64::from_le_bytes)
    }

    fn cstr(&self, at: u64) -> Result<String, String> {
        let start = usize::try_from(at).map_err(|e| e.to_string())?;
        let rest = self.0.get(start..).ok_or("string outside the file")?;
        let len = rest
            .iter()
            .position(|&b| b == 0)
            .ok_or("unterminated string")?;
        let bytes = rest.get(..len).ok_or("string outside the file")?;
        Ok(String::from_utf8_lossy(bytes).into_owned())
    }

    /// (type, offset, vaddr, filesz) of each program header.
    fn segments(&self) -> Result<Vec<(u32, u64, u64, u64)>, String> {
        let (phoff, phentsize, phnum) = (self.u64(32)?, self.u16(54)?, self.u16(56)?);
        (0..u64::from(phnum))
            .map(|i| {
                let ph = add(phoff, mul(i, u64::from(phentsize))?)?;
                Ok((
                    self.u32(ph)?,
                    self.u64(add(ph, 8)?)?,
                    self.u64(add(ph, 16)?)?,
                    self.u64(add(ph, 32)?)?,
                ))
            })
            .collect()
    }

    fn file_offset(&self, vaddr: u64) -> Result<u64, String> {
        for (kind, offset, start, size) in self.segments()? {
            if kind == PT_LOAD && vaddr >= start && vaddr - start < size {
                return add(offset, vaddr - start);
            }
        }
        Err(format!("address {vaddr:#x} is in no loaded segment"))
    }

    fn interpreter(&self) -> Result<Option<String>, String> {
        match self.segments()?.into_iter().find(|s| s.0 == PT_INTERP) {
            Some((_, offset, _, _)) => self.cstr(offset).map(Some),
            None => Ok(None),
        }
    }

    fn dynamic(&self) -> Result<Dynamic, String> {
        let Some((_, offset, _, size)) = self.segments()?.into_iter().find(|s| s.0 == PT_DYNAMIC)
        else {
            return Ok(Dynamic::default());
        };
        let mut entries = Vec::new();
        let mut strtab = None;
        for i in 0..size / 16 {
            let entry = add(offset, mul(i, 16)?)?;
            let (tag, value) = (self.u64(entry)?, self.u64(add(entry, 8)?)?);
            match tag {
                DT_NULL => break,
                DT_STRTAB => strtab = Some(self.file_offset(value)?),
                DT_NEEDED | DT_RPATH | DT_RUNPATH => entries.push((tag, value)),
                _ => {}
            }
        }
        let mut dynamic = Dynamic::default();
        if entries.is_empty() {
            return Ok(dynamic);
        }
        let strtab = strtab.ok_or("dynamic strings without DT_STRTAB")?;
        for (tag, at) in entries {
            let value = self.cstr(add(strtab, at)?)?;
            match tag {
                DT_NEEDED => dynamic.needed.push(value),
                DT_RPATH => dynamic.rpath = Some(value),
                _ => dynamic.runpath = Some(value),
            }
        }
        Ok(dynamic)
    }

    /// The bytes of the named section.
    fn section(&self, name: &str) -> Result<Option<&[u8]>, String> {
        let (shoff, shentsize, shnum, shstrndx) =
            (self.u64(40)?, self.u16(58)?, self.u16(60)?, self.u16(62)?);
        let header = |i: u64| add(shoff, mul(i, u64::from(shentsize))?);
        let names = self.u64(add(header(u64::from(shstrndx))?, 24)?)?;
        for i in 0..u64::from(shnum) {
            let h = header(i)?;
            if self.cstr(add(names, u64::from(self.u32(h)?))?)? == name {
                return self
                    .bytes(self.u64(add(h, 24)?)?, self.u64(add(h, 32)?)?)
                    .map(Some);
            }
        }
        Ok(None)
    }
}

/// One file td-link linked: its type and interpreter, LLD's mark, and, when
/// it is dynamic, td glibc's lib as its one run path, written as DT_RPATH,
/// and only glibc's libraries and `extra` needed.
fn check_file(
    path: &str,
    kind: &str,
    e_type: u16,
    dynamic: bool,
    interp: Option<&str>,
    extra: &[&str],
) -> Result<(), String> {
    let bytes = std::fs::read(path).map_err(|e| format!("read {path}: {e}"))?;
    let elf = Elf(&bytes);
    let found = elf.u16(16)?;
    if found != e_type {
        return Err(format!("{path} is not a {kind}: e_type {found}"));
    }
    let interpreter = elf.interpreter()?;
    if interpreter.as_deref() != interp {
        return Err(format!(
            "{path}: interpreter {interpreter:?}, expected {interp:?}"
        ));
    }
    let section = elf.dynamic()?;
    let glibc_lib = GLIBC_INTERP
        .rsplit_once('/')
        .map(|(dir, _)| dir)
        .ok_or("TD_LINK_TEST_INTERP has no directory")?;
    let expected_rpath = dynamic.then_some(glibc_lib);
    if section.rpath.as_deref() != expected_rpath {
        return Err(format!(
            "{path}: DT_RPATH {:?}, expected {expected_rpath:?}",
            section.rpath
        ));
    }
    if let Some(runpath) = section.runpath {
        return Err(format!("{path}: unexpected DT_RUNPATH {runpath}"));
    }
    let allowed = |name: &String| {
        (dynamic && GLIBC_NEEDED.contains(&name.as_str()))
            || extra.iter().any(|suffix| name.ends_with(suffix))
    };
    if let Some(name) = section.needed.iter().find(|name| !allowed(name)) {
        return Err(format!(
            "{path} needs {name}, outside td glibc and {extra:?}: {:?}",
            section.needed
        ));
    }
    if let Some(missing) = extra
        .iter()
        .find(|suffix| !section.needed.iter().any(|name| name.ends_with(*suffix)))
    {
        return Err(format!(
            "{path} does not need {missing}: {:?}",
            section.needed
        ));
    }
    let comment = elf.section(".comment")?.ok_or("no .comment section")?;
    if !comment.windows(11).any(|w| w == b"Linker: LLD") {
        return Err(format!("{path} was not linked by LLD"));
    }
    println!("TD-LINK-TEST {path}: {kind}, needs {:?}", section.needed);
    Ok(())
}

fn check(args: &[String]) -> Result<(), String> {
    let (own, shared) = args.split_first().ok_or("usage: PROGRAM [SHARED...]")?;
    let (kind, e_type, dynamic) = KIND;
    let interp = dynamic.then_some(GLIBC_INTERP);
    // The dynamic program needs the cdylib, linked by path.
    let extra: &[&str] = if dynamic {
        &["/libtd_link_cdylib.so"]
    } else {
        &[]
    };
    check_file(own, kind, e_type, dynamic, interp, extra)?;
    for library in shared {
        check_file(library, "shared object", ET_DYN, true, None, &[])?;
    }
    // resume_unwind raises through gcc-runtime's unwinder without the
    // panic machinery's message and hook.
    if std::panic::catch_unwind(|| std::panic::resume_unwind(Box::new("td-link-test"))).is_ok() {
        return Err("a panic did not unwind".into());
    }
    if std::thread::spawn(|| 6 * 7).join().unwrap_or(0) != td_link_macro::answer!() {
        return Err("a thread or the proc macro did not answer 42".into());
    }
    println!("TD-LINK-TEST {kind} OK");
    Ok(())
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match check(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("td-link-test: {error}");
            ExitCode::FAILURE
        }
    }
}

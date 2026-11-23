//! What td glibc's loader loads for the closure on this host. The launcher
//! resolved every NEEDED name from the outputs; here the host's own loader
//! lists each program the controller runs and each shared object the server
//! can load, before any of them runs.
use super::{in_store, Closure, PROGRAMS, STORE};
use crate::{
    common::{child, fmt, owned, require, store_basename, Host},
    isolation::files,
    R,
};
use std::{
    fs::{self, File},
    os::unix::fs::FileExt,
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

const ET_DYN: u16 = 3;
const PT_INTERP: u32 = 3;
const HEADER: usize = 64;

fn u16_at(bytes: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(bytes.get(at..at + 2)?.try_into().ok()?))
}
fn u32_at(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
}
fn u64_at(bytes: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_le_bytes(bytes.get(at..at + 8)?.try_into().ok()?))
}

/// An x86-64 ELF64 little-endian header: its type and where its program
/// header table lies (offset, entry size, count). None for anything else.
pub(super) fn header(bytes: &[u8]) -> Option<(u16, u64, usize, usize)> {
    if bytes.get(..4)? != b"\x7fELF" || bytes.get(4..6)? != [2, 1] || u16_at(bytes, 18)? != 62 {
        return None;
    }
    Some((
        u16_at(bytes, 16)?,
        u64_at(bytes, 32)?,
        usize::from(u16_at(bytes, 54)?),
        usize::from(u16_at(bytes, 56)?),
    ))
}

/// PT_INTERP's file offset and size in a program header table.
pub(super) fn interp_segment(table: &[u8], entry: usize) -> Option<(u64, u64)> {
    if entry < 56 {
        return None;
    }
    table.chunks_exact(entry).find_map(|row| {
        (u32_at(row, 0)? == PT_INTERP).then_some((u64_at(row, 8)?, u64_at(row, 32)?))
    })
}

fn read_header(file: &File) -> R<Option<(u16, u64, usize, usize)>> {
    let mut bytes = [0u8; HEADER];
    match file.read_exact_at(&mut bytes, 0) {
        Ok(()) => Ok(header(&bytes)),
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// The interpreter an x86-64 ELF program names, if any.
pub(super) fn interpreter(path: &Path) -> R<Option<String>> {
    let file = File::open(path)?;
    let Some((_, offset, entry, count)) = read_header(&file)? else {
        return Err(fmt(format_args!("{} is not an x86-64 ELF file", path.display()))?.into());
    };
    require(count <= 256 && entry <= 256, "ELF program header table")?;
    let mut table = td_alloc::filled(entry * count, 0u8)?;
    file.read_exact_at(&mut table, offset)?;
    let Some((at, size)) = interp_segment(&table, entry) else {
        return Ok(None);
    };
    require((2..=4096).contains(&size), "ELF interpreter size")?;
    let mut name = td_alloc::filled(usize::try_from(size)?, 0u8)?;
    file.read_exact_at(&mut name, at)?;
    let name = name.strip_suffix(b"\0").ok_or("ELF interpreter")?;
    Ok(Some(owned(std::str::from_utf8(name)?)?))
}

/// The paths an `ld.so --list` report says the loader opened; refuses a
/// library it did not find.
pub(super) fn listed(report: &str) -> R<Vec<&str>> {
    let mut paths = Vec::new();
    for line in report.lines() {
        let line = line.trim();
        if line.is_empty() || line == "statically linked" {
            continue;
        }
        let path = match line.split_once(" => ") {
            Some((name, rest)) => {
                let path = rest.split_once(" (").map_or(rest, |(path, _)| path);
                if !path.starts_with('/') {
                    return Err(fmt(format_args!("td glibc's loader cannot find {name}"))?.into());
                }
                path
            }
            None => {
                let (path, _) = line.split_once(" (").ok_or("unexpected loader listing")?;
                if !path.starts_with('/') {
                    // The kernel's vDSO is mapped into every process.
                    require(
                        path.starts_with("linux-vdso.so."),
                        "unexpected loader listing",
                    )?;
                    continue;
                }
                path
            }
        };
        td_alloc::push(&mut paths, path)?;
    }
    Ok(paths)
}

// Every regular x86-64 shared object under `dir`, debug companions aside.
fn shared_objects(dir: &Path, debug: &Path, out: &mut Vec<PathBuf>) -> R<()> {
    for item in fs::read_dir(dir)? {
        let item = item?;
        let kind = item.file_type()?;
        let path = item.path();
        if kind.is_dir() && path != debug {
            shared_objects(&path, debug, out)?;
        } else if kind.is_file()
            && read_header(&File::open(&path)?)?.is_some_and(|(kind, ..)| kind == ET_DYN)
        {
            td_alloc::push(out, path)?;
        }
    }
    Ok(())
}

/// The store path `/td/store/NAME` holding `path`, which must be `stem`'s.
pub(super) fn store_root(path: &str, stem: &str) -> R<PathBuf> {
    let rest = path
        .strip_prefix("/td/store/")
        .ok_or("not a td store path")?;
    let name = rest.split_once('/').map_or(rest, |(name, _)| name);
    store_basename(name, stem)?;
    child(Path::new(STORE), name)
}

/// td glibc's loader, run with no environment, lists every program in
/// PROGRAMS and every shared object of the PostgreSQL output, and every path
/// it opens lies inside the closure, symbolic links followed.
pub(super) fn check_loading(host: &Host, closure: &Closure) -> R<()> {
    let names = closure.basenames();
    let loader = interpreter(&closure.postgres()?)?.ok_or("the td-built server names no loader")?;
    require(
        in_store(&loader, &[&closure.glibc.basename]),
        "the td-built server's loader is not td glibc's",
    )?;
    let loader = Path::new(&loader);
    files::trusted_binary(loader)?;
    let bin = closure.bin()?;
    let mut objects = Vec::new();
    for program in PROGRAMS {
        let path = child(&bin, program)?;
        require(
            interpreter(&path)?.as_deref() == loader.to_str(),
            "a td-built program names another loader",
        )?;
        td_alloc::push(&mut objects, path)?;
    }
    let output = child(Path::new(STORE), &closure.postgres.basename)?;
    let lib = child(&output, "lib")?;
    shared_objects(&lib, &child(&lib, "debug")?, &mut objects)?;
    for object in &objects {
        let mut list = Command::new(loader);
        list.env_clear().arg("--list").arg(object);
        let out = host.exec(&mut list, &[], Duration::from_secs(40), true)?;
        for path in listed(std::str::from_utf8(&out.stdout)?)? {
            let real = fs::canonicalize(path)?;
            if !in_store(path, &names) || !real.to_str().is_some_and(|r| in_store(r, &names)) {
                return Err(fmt(format_args!(
                    "{} loads {path}, outside the PostgreSQL closure",
                    object.display()
                ))?
                .into());
            }
        }
    }
    Ok(())
}

//! `cpio -t` and `cpio -i` for newc archives (`070701`/`070702`), the format
//! the kernel's `gen_init_cpio` writes. Listing and extraction only: td
//! builds archives with `gen_init_cpio` or the engine writer. Extraction stays
//! under the current directory: a name with a `..` component is refused, a
//! leading `/` dropped, and every ancestor that already exists must be a real
//! directory, so an archive cannot plant a link and then write through it.
//! Device nodes need `mknod(2)`, which this crate does not take, so they are
//! skipped with a warning.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};

use crate::glob;

const USAGE: &str = "usage: cpio -t|-i [-dduv] [-F ARCHIVE] [PATTERN...]";
const HEADER: usize = 110;
const TRAILER: &[u8] = b"TRAILER!!!";
const S_IFMT: u32 = 0o170_000;
const S_IFDIR: u32 = 0o040_000;
const S_IFREG: u32 = 0o100_000;
const S_IFLNK: u32 = 0o120_000;

/// One member as the archive describes it.
pub struct Member<'a> {
    pub name: &'a [u8],
    pub ino: u32,
    pub mode: u32,
    pub nlink: u32,
    pub mtime: u32,
    /// The device the inode was on, which with `ino` names a hard-link group.
    pub dev: (u32, u32),
    pub data: &'a [u8],
}

/// Every member of a newc archive up to its trailer.
pub fn members(archive: &[u8]) -> Result<Vec<Member<'_>>, String> {
    let mut out = Vec::new();
    let mut at = 0usize;
    loop {
        let h = archive
            .get(at..at + HEADER)
            .ok_or_else(|| format!("truncated header at offset {at}"))?;
        let magic = h.get(..6).unwrap_or(&[]);
        if magic != b"070701" && magic != b"070702" {
            return Err(format!("bad magic at offset {at} (newc only)"));
        }
        let field = |i: usize| -> Result<u32, String> {
            let hex = h.get(6 + i * 8..14 + i * 8).unwrap_or(&[]);
            let text =
                std::str::from_utf8(hex).map_err(|_| format!("bad header field at offset {at}"))?;
            u32::from_str_radix(text, 16).map_err(|_| format!("bad header field at offset {at}"))
        };
        let ino = field(0)?;
        let mode = field(1)?;
        let nlink = field(4)?;
        let mtime = field(5)?;
        let dev = (field(7)?, field(8)?);
        let size = field(6)? as usize;
        let namesize = field(11)? as usize;
        let name_start = at + HEADER;
        let name = archive
            .get(name_start..name_start + namesize.saturating_sub(1))
            .ok_or_else(|| format!("truncated name at offset {at}"))?;
        let data_start = align4(name_start + namesize);
        if name == TRAILER {
            return Ok(out);
        }
        let data = archive
            .get(data_start..data_start + size)
            .ok_or_else(|| format!("truncated data for {}", String::from_utf8_lossy(name)))?;
        out.push(Member {
            name,
            ino,
            mode,
            nlink,
            mtime,
            dev,
            data,
        });
        at = align4(data_start + size);
    }
}

fn align4(n: usize) -> usize {
    (n + 3) & !3
}

/// Where an archive name lands under the current directory, or None for a
/// name that would leave it.
fn target(name: &[u8]) -> Option<PathBuf> {
    let text = std::str::from_utf8(name).ok()?;
    let rel = text.trim_start_matches('/');
    if rel.is_empty() || rel == "." {
        return None;
    }
    let path = Path::new(rel);
    if path.components().any(|c| matches!(c, Component::ParentDir)) {
        return None;
    }
    Some(path.to_path_buf())
}

pub fn run(args: &[String]) -> Result<u8, String> {
    let (mut list, mut extract, mut make_dirs, mut unconditional, mut verbose) =
        (false, false, false, false, false);
    let mut file: Option<String> = None;
    let mut patterns: Vec<&str> = Vec::new();
    let mut i = 0;
    while let Some(a) = args.get(i) {
        let s = a.as_str();
        if s == "-F" {
            i += 1;
            file = Some(args.get(i).cloned().ok_or(USAGE)?);
        } else if let Some(f) = s.strip_prefix("-F") {
            file = Some(f.to_string());
        } else if s.starts_with('-') && s.len() > 1 {
            for c in s.chars().skip(1) {
                match c {
                    't' => list = true,
                    'i' => extract = true,
                    'd' => make_dirs = true,
                    'u' => unconditional = true,
                    'v' => verbose = true,
                    // Modification times are always restored, as -m asks.
                    'm' => {}
                    _ => return Err(format!("unrecognised option '-{c}'\n{USAGE}")),
                }
            }
        } else {
            patterns.push(s);
        }
        i += 1;
    }
    // Exactly one of -t and -i.
    if list == extract {
        return Err(USAGE.to_string());
    }
    let mut archive = Vec::new();
    match &file {
        Some(f) => archive = std::fs::read(f).map_err(|e| format!("{f}: {e}"))?,
        None => {
            std::io::stdin()
                .lock()
                .read_to_end(&mut archive)
                .map_err(|e| format!("stdin: {e}"))?;
        }
    }
    let wanted = |name: &[u8]| {
        patterns.is_empty()
            || patterns
                .iter()
                .any(|p| glob::matches(p.as_bytes(), name, false))
    };
    let mut status = 0u8;
    let mut listing = String::new();
    let mut ex = Extract {
        make_dirs,
        unconditional,
        dirs: Vec::new(),
        links: HashMap::new(),
    };
    for m in members(&archive)? {
        if !wanted(m.name) {
            if extract {
                if let Err(e) = ex.skipped(&m) {
                    crate::emit_err(&format!("cpio: {e}\n"));
                    status = 1;
                }
            }
            continue;
        }
        let shown = String::from_utf8_lossy(m.name).into_owned();
        if list {
            listing.push_str(&shown);
            listing.push('\n');
            continue;
        }
        if verbose {
            crate::emit_err(&format!("{shown}\n"));
        }
        let Some(path) = target(m.name) else {
            if m.name != b"." {
                crate::emit_err(&format!(
                    "cpio: {shown}: refusing a name outside the current directory\n"
                ));
                status = 1;
            }
            if let Err(e) = ex.skipped(&m) {
                crate::emit_err(&format!("cpio: {e}\n"));
                status = 1;
            }
            continue;
        };
        if let Err(e) = ex.member(&path, &m) {
            crate::emit_err(&format!("cpio: {shown}: {e}\n"));
            status = 1;
        }
    }
    if list {
        crate::emit(&listing)?;
    }
    Ok(status.max(ex.finish()))
}

struct Extract {
    make_dirs: bool,
    unconditional: bool,
    /// Directory modes and times, applied once every member is in: a 0555
    /// directory set early refuses its own children, and each child written
    /// moves its mtime.
    dirs: Vec<(PathBuf, u32, u32)>,
    /// newc hard-link groups by (dev, ino). GNU's writer puts the data on
    /// the LAST name and gives the others size 0, so a name can arrive
    /// before the bytes it shares.
    links: HashMap<(u32, u32, u32), Group>,
}

#[derive(Default)]
struct Group {
    /// The name holding the data, once written.
    first: Option<PathBuf>,
    /// Names seen before the data, with their mode and mtime.
    waiting: Vec<(PathBuf, u32, u32)>,
}

impl Extract {
    fn member(&mut self, path: &Path, m: &Member) -> Result<(), String> {
        prepare_parent(path, self.make_dirs)?;
        match m.mode & S_IFMT {
            S_IFDIR => {
                match path.symlink_metadata() {
                    Ok(md) if md.is_dir() => {}
                    Ok(_) if self.unconditional => {
                        std::fs::remove_file(path).map_err(|e| e.to_string())?;
                        std::fs::create_dir(path).map_err(|e| e.to_string())?;
                    }
                    Ok(_) => return Err("exists but is not a directory".to_string()),
                    Err(_) => std::fs::create_dir(path).map_err(|e| e.to_string())?,
                }
                self.dirs.push((path.to_path_buf(), m.mode, m.mtime));
                Ok(())
            }
            S_IFREG if m.nlink > 1 => self.linked(path, m),
            S_IFREG => {
                clear(path, self.unconditional)?;
                write_file(path, m.data, m.mode, m.mtime)
            }
            S_IFLNK => {
                clear(path, self.unconditional)?;
                let dest = std::ffi::OsStr::new(
                    std::str::from_utf8(m.data).map_err(|_| "non-UTF-8 link target")?,
                );
                std::os::unix::fs::symlink(dest, path).map_err(|e| e.to_string())
            }
            _ => Err("device or special file skipped (no mknod here)".to_string()),
        }
    }

    fn linked(&mut self, path: &Path, m: &Member) -> Result<(), String> {
        let unconditional = self.unconditional;
        let group = self.links.entry((m.dev.0, m.dev.1, m.ino)).or_default();
        clear(path, unconditional)?;
        if let Some(first) = &group.first {
            return std::fs::hard_link(first, path).map_err(|e| e.to_string());
        }
        if m.data.is_empty() {
            group.waiting.push((path.to_path_buf(), m.mode, m.mtime));
            return Ok(());
        }
        write_file(path, m.data, m.mode, m.mtime)?;
        group.first = Some(path.to_path_buf());
        for (name, _, _) in std::mem::take(&mut group.waiting) {
            std::fs::hard_link(path, &name).map_err(|e| format!("{}: {e}", name.display()))?;
        }
        Ok(())
    }

    /// A member left out, by pattern or refusal, can still carry the data of
    /// a group whose wanted names came before it: GNU's writer puts the data
    /// on the last name. Those names get it, as GNU gives it them.
    fn skipped(&mut self, m: &Member) -> Result<(), String> {
        if m.mode & S_IFMT != S_IFREG || m.nlink <= 1 || m.data.is_empty() {
            return Ok(());
        }
        let Some(group) = self.links.get_mut(&(m.dev.0, m.dev.1, m.ino)) else {
            return Ok(());
        };
        if group.first.is_some() {
            return Ok(());
        }
        let mut waiting = std::mem::take(&mut group.waiting).into_iter();
        let Some((first, mode, mtime)) = waiting.next() else {
            return Ok(());
        };
        write_file(&first, m.data, mode, mtime).map_err(|e| format!("{}: {e}", first.display()))?;
        for (name, _, _) in waiting {
            std::fs::hard_link(&first, &name).map_err(|e| format!("{}: {e}", name.display()))?;
        }
        group.first = Some(first);
        Ok(())
    }

    /// Groups whose data never came (an empty file with several names), then
    /// directory modes and times, deepest first. The status is 1 if any of
    /// it failed.
    fn finish(&mut self) -> u8 {
        let mut status = 0u8;
        let mut fail = |what: &Path, e: &dyn std::fmt::Display| {
            crate::emit_err(&format!("cpio: {}: {e}\n", what.display()));
            status = 1;
        };
        for group in self.links.values_mut() {
            let mut waiting = std::mem::take(&mut group.waiting).into_iter();
            let Some((first, mode, mtime)) = waiting.next() else {
                continue;
            };
            if let Err(e) = write_file(&first, &[], mode, mtime) {
                fail(&first, &e);
                continue;
            }
            for (name, _, _) in waiting {
                if let Err(e) = std::fs::hard_link(&first, &name) {
                    fail(&name, &e);
                }
            }
        }
        self.dirs
            .sort_by_key(|(path, _, _)| std::cmp::Reverse(path.components().count()));
        for (path, mode, mtime) in &self.dirs {
            // Only a real directory: a name swapped for a link since is left.
            if !path.symlink_metadata().is_ok_and(|m| m.is_dir()) {
                continue;
            }
            // Opened first, so a mode that drops the owner's own access
            // cannot shut out the time.
            match std::fs::File::open(path) {
                Ok(dir) => {
                    let _ = dir.set_modified(when(*mtime));
                    if let Err(e) =
                        dir.set_permissions(std::fs::Permissions::from_mode(mode & 0o7777))
                    {
                        fail(path, &e);
                    }
                }
                Err(e) => fail(path, &e),
            }
        }
        status
    }
}

/// Make `path`'s parent usable without following a link out of the tree:
/// each ancestor that exists must be a real directory, and a missing one is
/// made only under -d.
fn prepare_parent(path: &Path, make_dirs: bool) -> Result<(), String> {
    let mut at = PathBuf::new();
    for c in path.parent().map(Path::components).into_iter().flatten() {
        at.push(c);
        match at.symlink_metadata() {
            Ok(m) if m.is_dir() => {}
            Ok(_) => return Err(format!("'{}' exists but is not a directory", at.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && make_dirs => {
                std::fs::create_dir(&at).map_err(|e| format!("{}: {e}", at.display()))?;
            }
            Err(e) => return Err(format!("{}: {e}", at.display())),
        }
    }
    Ok(())
}

/// Clear the name for a non-directory: an existing file or link goes only
/// under -u, and then the name itself goes, never what a link points at.
fn clear(path: &Path, unconditional: bool) -> Result<(), String> {
    match path.symlink_metadata() {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.to_string()),
        Ok(m) if m.is_dir() => Err("a directory is in the way".to_string()),
        Ok(_) if !unconditional => Err("not created: newer or same age version exists".to_string()),
        Ok(_) => std::fs::remove_file(path).map_err(|e| e.to_string()),
    }
}

/// A new file, created rather than opened over, with its mode and time set
/// on the handle.
fn write_file(path: &Path, data: &[u8], mode: u32, mtime: u32) -> Result<(), String> {
    // 0600 until the archive's mode is set, so nothing is readable early.
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(|e| e.to_string())?;
    f.write_all(data).map_err(|e| e.to_string())?;
    f.set_permissions(std::fs::Permissions::from_mode(mode & 0o7777))
        .map_err(|e| e.to_string())?;
    let _ = f.set_modified(when(mtime));
    Ok(())
}

fn when(mtime: u32) -> std::time::SystemTime {
    std::time::UNIX_EPOCH + std::time::Duration::from_secs(u64::from(mtime))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]
    use super::*;

    fn header(out: &mut Vec<u8>, mode: u32, size: usize, name: &str) {
        out.extend_from_slice(b"070701");
        let fields = [
            1,
            mode,
            0,
            0,
            1,
            7,
            size as u32,
            0,
            0,
            0,
            0,
            name.len() as u32 + 1,
            0,
        ];
        for f in fields {
            out.extend_from_slice(format!("{f:08X}").as_bytes());
        }
        out.extend_from_slice(name.as_bytes());
        out.push(0);
        while out.len() % 4 != 0 {
            out.push(0);
        }
    }

    fn archive(entries: &[(&str, u32, &[u8])]) -> Vec<u8> {
        let mut out = Vec::new();
        for (name, mode, data) in entries {
            header(&mut out, *mode, data.len(), name);
            out.extend_from_slice(data);
            while out.len() % 4 != 0 {
                out.push(0);
            }
        }
        header(&mut out, 0, 0, "TRAILER!!!");
        out
    }

    #[test]
    fn reads_members_to_the_trailer() {
        let a = archive(&[
            ("dev", S_IFDIR | 0o755, b""),
            ("init", S_IFREG | 0o755, b"#!/bin/sh\n"),
            ("bin/sh", S_IFLNK | 0o777, b"td-sh"),
        ]);
        let m = members(&a).unwrap();
        let names: Vec<&[u8]> = m.iter().map(|m| m.name).collect();
        assert_eq!(names, [&b"dev"[..], b"init", b"bin/sh"]);
        assert_eq!(m[1].data, b"#!/bin/sh\n");
        assert_eq!(m[1].mtime, 7);
        assert!(members(&a[..a.len() - 20]).is_err());
        assert!(members(b"070707junk").is_err());
    }

    #[test]
    fn names_that_leave_the_directory_are_refused() {
        assert_eq!(target(b"a/b"), Some(PathBuf::from("a/b")));
        assert_eq!(target(b"/etc/passwd"), Some(PathBuf::from("etc/passwd")));
        assert_eq!(target(b"../x"), None);
        assert_eq!(target(b"a/../../x"), None);
        assert_eq!(target(b"."), None);
    }
}

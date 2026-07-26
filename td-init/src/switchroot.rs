//! `switch_root NEWROOT INIT [ARG...]` — the initramfs' last act: make NEWROOT
//! the real root and exec INIT as PID 1 there.
//!
//! `MS_MOVE` + `chroot(2)`, as util-linux and busybox do it. `pivot_root(2)` is
//! deliberately absent from the syscall surface: it fails on the initramfs
//! rootfs, the only place switch_root runs.
//!
//! Everything fallible happens BEFORE the first mount moves — INIT is proven
//! executable inside the new root, and NEWROOT proven a mount point — because a
//! failure after `chroot(2)` is an unrecoverable kernel panic. One window
//! remains, as it does in util-linux: the API mounts move before the root does,
//! so a failed root move strands them under NEWROOT. The old rootfs is freed in
//! that same window — busybox's order — so a failed root move also leaves no
//! rescue shell behind. Everything that can be checked has been by then.

use crate::sys;
use std::collections::VecDeque;
use std::ffi::{CString, OsStr};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The mounts util-linux carries across, in the order it moves them.
const API_MOUNTS: &[&str] = &["/dev", "/proc", "/sys", "/run"];

fn usage() -> String {
    "usage: switch_root NEWROOT INIT [ARG...]".to_string()
}

fn cpath(p: &str) -> Result<CString, String> {
    CString::new(p.as_bytes()).map_err(|_| format!("path '{p}' contains a NUL byte"))
}

fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    match std::fs::metadata(p) {
        Ok(m) => m.is_file() && m.permissions().mode() & 0o111 != 0,
        Err(_) => false,
    }
}

/// The argv[0] to exec INIT under: the operand, named as it reads after the
/// chroot. A multicall INIT dispatches on this, so it must survive the symlink
/// resolution that produced the path actually exec'd.
fn exec_argv0(init: &str) -> String {
    if init.starts_with('/') {
        init.to_string()
    } else {
        format!("/{init}")
    }
}

/// The exec itself: run the VERIFIED path, under the OPERAND's name. Those two
/// deliberately differ (see `run` for why), and dropping the `arg0` is a
/// panicked kernel rather than a cosmetic slip — so it is built here, where a
/// test can watch the call happen instead of only the string it is handed.
fn exec_command(after_chroot: &Path, argv0: &str, rest: &[String]) -> Command {
    let mut cmd = Command::new(after_chroot);
    cmd.arg0(argv0).args(rest);
    cmd
}

/// Which loaders the kernel will consider for this file. The two are not
/// interchangeable: a `#!` script is a valid program, but it is NOT a valid ELF
/// program interpreter — `load_elf_interp` refuses a non-ELF with ELIBBAD.
#[derive(Clone, Copy)]
enum Loader {
    /// Picked by content: an ELF, or a `#!` script.
    Any,
    /// Reached through an ELF's PT_INTERP. Must be an ELF.
    ElfOnly,
}

/// The most this can prove without running it: INIT is a regular executable file
/// AND the kernel has a loader for it — an ELF, or a `#!` script whose
/// interpreter also resolves inside the new root and is itself runnable.
///
/// Mode bits alone are not enough. A `chmod +x` text file, or a script naming an
/// interpreter that exists on the initramfs but not in the new root, passes them
/// and then fails `execve` AFTER the mounts have moved and `chroot` has
/// happened — the one failure this applet cannot report, because by then there
/// is no way back and PID 1 dying is a kernel panic.
fn is_runnable(root: &Path, file: &Path, depth: usize, loader: Loader) -> Result<(), String> {
    // A `#!` line may name an interpreter that is itself a script. This is td's
    // own bound, deliberately one tighter than the kernel's (`fs/exec.c` fails
    // a `recursion_depth > 5` with ELOOP) rather than a mirror of it: refusing
    // a chain the kernel would have run costs a clear message before anything
    // has moved, while accepting one it would refuse costs a panic after the
    // chroot. An init behind five `#!` hops does not exist.
    const MAX_SHEBANG: usize = 4;
    if !is_executable(file) {
        return Err(format!(
            "{}: not an executable file in the new root — refusing to switch",
            file.display()
        ));
    }
    if depth > MAX_SHEBANG {
        return Err(format!(
            "{}: more than {MAX_SHEBANG} '#!' interpreter hops — refusing to switch",
            file.display()
        ));
    }
    let head = read_head(file)?;
    if head.starts_with(ELF_MAGIC) {
        let interp = match (elf_interpreter(file, &head)?, loader) {
            (Some(interp), Loader::Any) => interp,
            // A static ELF: the kernel loads it with no help. So does an ELF
            // reached THROUGH a PT_INTERP — `load_elf_interp` maps that file's
            // segments and never consults a PT_INTERP of its own, so following
            // one here would refuse an init the kernel runs.
            _ => return Ok(()),
        };
        let staged = resolve_in_root(root, &interp).ok_or_else(|| {
            format!(
                "{}: its interpreter {interp} does not resolve inside the new root",
                file.display()
            )
        })?;
        return is_runnable(root, &staged, depth + 1, Loader::ElfOnly);
    }
    // Not an ELF. Whether that is loadable depends on who asked for it.
    if let Loader::ElfOnly = loader {
        return Err(format!(
            "{}: named as an ELF program interpreter but is not an ELF — the kernel refuses that with ELIBBAD, which would happen after the chroot",
            file.display()
        ));
    }
    let interp = shebang_interpreter(&head).ok_or_else(|| {
        format!(
            "{}: neither an ELF nor a '#!' script — the kernel has no loader for it, so exec would fail after the chroot",
            file.display()
        )
    })?;
    let staged = resolve_in_root(root, &interp).ok_or_else(|| {
        format!(
            "{}: its interpreter {interp} does not resolve inside the new root",
            file.display()
        )
    })?;
    // A script's interpreter may itself be a script; the kernel recurses.
    is_runnable(root, &staged, depth + 1, Loader::Any)
}

const ELF_MAGIC: &[u8] = b"\x7fELF";

/// Little-endian scalars out of a header, bounds-checked into `Option`.
fn u16_at(b: &[u8], off: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(off..off + 2)?.try_into().ok()?))
}

fn u32_at(b: &[u8], off: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(off..off + 4)?.try_into().ok()?))
}

fn u64_at(b: &[u8], off: usize) -> Option<u64> {
    Some(u64::from_le_bytes(b.get(off..off + 8)?.try_into().ok()?))
}

/// Validate the ELF header and report the dynamic loader it needs, if any.
///
/// The magic alone proves nothing: a truncated file, a 32-bit or wrong-machine
/// binary, and a relocatable `.o` all carry it and all fail `execve`. So does a
/// dynamic executable whose `PT_INTERP` is absent from the new root — the exact
/// shape a from-source distro produces when a store path moves. Every one of
/// those failures would land AFTER the chroot, so each is a refusal here.
///
/// `Ok(None)` means the kernel needs no interpreter for it.
fn elf_interpreter(file: &Path, head: &[u8]) -> Result<Option<String>, String> {
    use std::io::{Read, Seek, SeekFrom};
    // x86-64 little-endian ELF64 — the same ABI this crate's own syscall numbers
    // are written for, so a mismatch is a binary this kernel will not run.
    const EHDR_LEN: usize = 64;
    const CLASS64: u8 = 2;
    const LSB: u8 = 1;
    const EV_CURRENT: u8 = 1;
    const ET_EXEC: u16 = 2;
    const ET_DYN: u16 = 3;
    const EM_X86_64: u16 = 62;
    const PT_LOAD: u32 = 1;
    const PT_INTERP: u32 = 3;
    const PHENT_LEN: u16 = 56;
    // Both bounds are the kernel's own, not a guess at a reasonable one: a
    // stricter limit here would refuse a binary `execve` accepts, and a refused
    // boot is the failure this whole function exists to avoid. `load_elf_phdrs`
    // caps the table at 64KiB, and PATH_MAX bounds the interpreter name.
    const MAX_PHNUM: u16 = (65536u32 / PHENT_LEN as u32) as u16;
    const PATH_MAX: u64 = 4096;

    let bad = |why: &str| format!("{}: {why} — refusing to switch", file.display());
    let field = |what: &str| bad(&format!("truncated ELF header ({what})"));

    if head.len() < EHDR_LEN {
        return Err(bad("ELF header is truncated"));
    }
    if head.get(4).copied() != Some(CLASS64) {
        return Err(bad("not a 64-bit ELF"));
    }
    if head.get(5).copied() != Some(LSB) {
        return Err(bad("not a little-endian ELF"));
    }
    if head.get(6).copied() != Some(EV_CURRENT) {
        return Err(bad("unknown ELF version"));
    }
    let e_type = u16_at(head, 16).ok_or_else(|| field("e_type"))?;
    if e_type != ET_EXEC && e_type != ET_DYN {
        return Err(bad("not an ELF executable (a relocatable or core file)"));
    }
    let e_machine = u16_at(head, 18).ok_or_else(|| field("e_machine"))?;
    if e_machine != EM_X86_64 {
        return Err(bad("built for another machine"));
    }
    let e_phoff = u64_at(head, 32).ok_or_else(|| field("e_phoff"))?;
    let e_phentsize = u16_at(head, 54).ok_or_else(|| field("e_phentsize"))?;
    let e_phnum = u16_at(head, 56).ok_or_else(|| field("e_phnum"))?;
    if e_phnum == 0 {
        // Nothing to load. The kernel refuses this, so we do.
        return Err(bad("ELF has no program headers"));
    }
    if e_phentsize != PHENT_LEN || e_phnum > MAX_PHNUM {
        return Err(bad("ELF program header table has an unusable shape"));
    }

    let mut f = std::fs::File::open(file).map_err(|e| format!("{}: {e}", file.display()))?;
    f.seek(SeekFrom::Start(e_phoff))
        .map_err(|e| format!("{}: {e}", file.display()))?;
    let mut table = vec![0u8; usize::from(e_phnum) * usize::from(e_phentsize)];
    f.read_exact(&mut table)
        .map_err(|_| bad("ELF program header table is past the end of the file"))?;

    let mut interp = None;
    let mut loadable = false;
    for i in 0..usize::from(e_phnum) {
        let at = i * usize::from(e_phentsize);
        let Some(ph) = table.get(at..at + usize::from(e_phentsize)) else {
            break;
        };
        match u32_at(ph, 0) {
            Some(PT_LOAD) => loadable = true,
            Some(PT_INTERP) if interp.is_none() => {
                let p_offset = u64_at(ph, 8).ok_or_else(|| field("p_offset"))?;
                let p_filesz = u64_at(ph, 32).ok_or_else(|| field("p_filesz"))?;
                // `fs/binfmt_elf.c` verbatim: under two bytes or over PATH_MAX
                // is -ENOEXEC before the segment is even read.
                if !(2..=PATH_MAX).contains(&p_filesz) {
                    return Err(bad("ELF names an interpreter of an implausible length"));
                }
                f.seek(SeekFrom::Start(p_offset))
                    .map_err(|e| format!("{}: {e}", file.display()))?;
                let mut raw = vec![0u8; p_filesz as usize];
                f.read_exact(&mut raw)
                    .map_err(|_| bad("ELF interpreter name is past the end of the file"))?;
                // The kernel tests the LAST byte of the segment, not "is there a
                // NUL somewhere" — `ld.so\0junk` is -ENOEXEC there. Accepting it
                // here would defer that refusal past the chroot, where it is a
                // panicked kernel instead of a message.
                if raw.last() != Some(&0) {
                    return Err(bad("ELF interpreter name is not NUL-terminated"));
                }
                // The name itself is the C string the kernel opens: up to the
                // FIRST NUL. `unwrap_or` cannot bite — the last byte is one.
                let end = raw.iter().position(|b| *b == 0).unwrap_or(0);
                let text = raw.get(..end).unwrap_or(&[]);
                if text.is_empty() {
                    return Err(bad("ELF names an empty interpreter"));
                }
                let name = std::str::from_utf8(text)
                    .map_err(|_| bad("ELF interpreter name is not text"))?;
                interp = Some(name.to_string());
            }
            _ => {}
        }
    }
    // No PT_LOAD means no program image. The kernel does NOT refuse this: it
    // execs successfully and the process dies of SIGSEGV. So the refusal is
    // ours to make — there is no way back once the mount has moved, and a
    // named diagnostic beats a segfaulting PID 1.
    if !loadable {
        return Err(bad("ELF has no loadable segment"));
    }
    Ok(interp)
}

/// The first bytes of a file — enough for the ELF header and a `#!` line.
///
/// `take` + `read_to_end` rather than one `read`: a single read may legally
/// return fewer bytes than asked for, and a short one here would make a valid
/// ELF look truncated and be REFUSED — a machine that does not boot, which is
/// the direction this module must never fail in.
fn read_head(file: &Path) -> Result<Vec<u8>, String> {
    use std::io::Read;
    let f = std::fs::File::open(file).map_err(|e| format!("{}: {e}", file.display()))?;
    let mut head = Vec::with_capacity(HEAD);
    f.take(HEAD as u64)
        .read_to_end(&mut head)
        .map_err(|e| format!("{}: {e}", file.display()))?;
    Ok(head)
}

/// `BINPRM_BUF_SIZE` — the kernel reads exactly this much of a file to decide
/// how to load it, so reading the same amount is what makes the checks here
/// answer the same question the kernel will.
const HEAD: usize = 256;

/// The interpreter named by a `#!` line, if this is one. The kernel takes the
/// text up to the first whitespace as the program, and ends the line at the
/// first newline OR NUL inside its buffer.
fn shebang_interpreter(head: &[u8]) -> Option<String> {
    let rest = head.strip_prefix(b"#!")?;
    let line = match rest.iter().position(|b| *b == b'\n' || *b == 0) {
        Some(end) => rest.get(..end)?,
        // Unterminated. The kernel zero-fills its buffer, so a file SHORTER
        // than the buffer is terminated by that padding and runs — but a full
        // one with no newline is ENOEXEC (`fs/binfmt_script.c` walks off the
        // end of the buffer and gives up). Taking the whole run as a filename
        // there would accept a file the kernel refuses, which is a panic after
        // the chroot instead of this message before it.
        None if head.len() < HEAD => rest,
        None => return None,
    };
    let mut word: &[u8] = &[];
    for candidate in line.split(|b| *b == b' ' || *b == b'\t') {
        if !candidate.is_empty() {
            word = candidate;
            break;
        }
    }
    if word.is_empty() {
        return None;
    }
    std::str::from_utf8(word).ok().map(str::to_string)
}

/// Resolve `path` the way the kernel will AFTER the chroot: with `root` as "/",
/// so an absolute symlink target restarts at `root` instead of escaping into the
/// live root.
///
/// This is not pedantry on td: the new root reaches its programs through
/// absolute `/td/store/<hash>-.../bin/...` symlinks and the store is not mounted
/// in the initramfs, so a plain `metadata(root.join(init))` follows them against
/// the CURRENT root and refuses a good root. The converse is worse — a target
/// that happens to exist in the initramfs passes and is gone after the chroot.
///
/// `..` cannot climb above `root`, matching the kernel. `MAX_HOPS` bounds
/// symlink loops; `None` means "does not resolve", which the caller refuses on.
fn resolve_in_root(root: &Path, path: &str) -> Option<PathBuf> {
    const MAX_HOPS: usize = 40;
    // Bytes, not `str`: a symlink target is whatever the filesystem holds, and a
    // non-UTF-8 one must resolve rather than refuse — for switch_root a refusal
    // is a machine that does not boot.
    let split = |b: &[u8]| -> VecDeque<Vec<u8>> {
        b.split(|c| *c == b'/')
            .filter(|c| !c.is_empty())
            .map(<[u8]>::to_vec)
            .collect()
    };

    let mut pending = split(path.as_bytes());
    let mut resolved = root.to_path_buf();
    let mut hops = 0usize;
    while let Some(component) = pending.pop_front() {
        match component.as_slice() {
            b"." => continue,
            b".." => {
                // `a/file/../b` is ENOTDIR to the kernel, so accepting it here
                // would pass an INIT that then fails to exec after the chroot —
                // the unrecoverable direction.
                if !resolved.is_dir() {
                    return None;
                }
                if resolved != root {
                    resolved.pop();
                }
                continue;
            }
            _ => resolved.push(OsStr::from_bytes(&component)),
        }
        let meta = std::fs::symlink_metadata(&resolved).ok()?;
        if !meta.file_type().is_symlink() {
            continue;
        }
        hops += 1;
        if hops > MAX_HOPS {
            return None;
        }
        let target = std::fs::read_link(&resolved).ok()?;
        let target = target.as_os_str().as_bytes();
        let mut expanded = split(target);
        if target.first() == Some(&b'/') {
            resolved = root.to_path_buf();
        } else {
            resolved.pop();
        }
        expanded.append(&mut pending);
        pending = expanded;
    }
    Some(resolved)
}

/// Whether `path` is itself a mount point.
///
/// The authority is the mount table: `path` canonicalised must appear in it.
/// Comparing `st_dev` against `/` is not sufficient — a plain directory inside
/// some other mount also differs from `/`. The fallback, for a system with no
/// readable `/proc`, is the classic st_dev test against the PARENT. Both answer
/// "no" when they cannot tell, so an unstattable NEWROOT is refused.
fn is_mount_point(path: &Path, mounts: Option<&str>) -> bool {
    use std::os::unix::fs::MetadataExt;
    if let Some(text) = mounts {
        if let Ok(real) = std::fs::canonicalize(path) {
            return mount_points(text).iter().any(|p| Path::new(p) == real);
        }
        return false;
    }
    match (std::fs::metadata(path), std::fs::metadata(path.join(".."))) {
        (Ok(here), Ok(up)) => here.dev() != up.dev(),
        _ => false,
    }
}

/// The mount points listed in `/proc/self/mounts` (field 2 of each line).
///
/// The kernel octal-escapes four characters in that field, so the text is
/// unescaped rather than used raw: the API paths never contain them, but NEWROOT
/// is an arbitrary path now that it is compared against this list. That reading
/// lives in `mount`, which parses the same table for `umount -a`.
/// Lossy on purpose, and only here: these paths are COMPARED against a
/// canonicalised NEWROOT, never handed back to the kernel, so a mangled name can
/// only fail to match its own mount — and a NEWROOT that fails to match is
/// refused, which is the safe direction. `mount`'s own table reading keeps the
/// raw bytes, because `umount -a` acts on them.
fn mount_points(mounts: &str) -> Vec<String> {
    crate::mount::parse_table(mounts.as_bytes())
        .into_iter()
        .map(|e| String::from_utf8_lossy(&e.target).into_owned())
        .collect()
}

/// The (source, target) moves to perform: every API mount that is currently
/// mounted, rebased under `newroot`. Pure, so the selection is testable without
/// a live `/proc`.
fn api_moves(mounts: &str, newroot: &str) -> Vec<(String, String)> {
    let points = mount_points(mounts);
    let base = newroot.trim_end_matches('/');
    let mut out = Vec::new();
    for api in API_MOUNTS {
        if points.iter().any(|p| p == api) {
            out.push(((*api).to_string(), format!("{base}{api}")));
        }
    }
    out
}

/// Root filesystems this applet will empty. An initramfs is `rootfs`, or the
/// `tmpfs`/`ramfs` it is built on; anything else is a real filesystem whose
/// files somebody wants back.
const DISPOSABLE_ROOT: &[&str] = &["rootfs", "ramfs", "tmpfs"];

/// The filesystem type mounted at `/`, from a `/proc/self/mounts` table.
fn root_fstype(mounts: &str) -> Option<String> {
    for entry in crate::mount::parse_table(mounts.as_bytes()) {
        if entry.target == b"/" && !entry.fstype.is_empty() {
            return Some(String::from_utf8_lossy(&entry.fstype).into_owned());
        }
    }
    None
}

/// Delete everything on ONE filesystem, depth first. `dev` is the boundary: a
/// mount point's own stat reports the filesystem mounted THERE, so any path
/// whose device differs is another filesystem's root and is neither entered nor
/// removed. The new root is such a path, which is what makes this safe to run
/// with the new root already mounted underneath.
///
/// Symlinks are removed as links (`symlink_metadata`, never `metadata`) — a
/// followed one would take the walk somewhere it was never asked to go.
/// Failures are skipped: reclaiming most of the memory beats aborting a boot.
fn empty_filesystem(dir: &Path, dev: u64) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if std::os::unix::fs::MetadataExt::dev(&meta) != dev {
            continue;
        }
        if meta.is_dir() {
            empty_filesystem(&path, dev);
            let _ = std::fs::remove_dir(&path);
        } else {
            let _ = std::fs::remove_file(&path);
        }
    }
}

/// Free the initramfs before moving the new root over it.
///
/// The initramfs rootfs CANNOT be unmounted — the kernel documents this — so
/// its extracted contents stay resident for the life of the machine unless they
/// are deleted here. util-linux and busybox both delete them; a switch_root
/// that does not is a permanent memory leak the size of the initramfs.
///
/// It deletes files on the live root, so it is guarded twice, exactly as
/// busybox guards it: the root must be a disposable filesystem type, and the
/// walk never crosses a device boundary. Either guard failing costs the memory
/// and nothing else — the boot carries on.
fn free_old_root(mounts: Option<&str>) {
    let fstype = match mounts.and_then(root_fstype) {
        Some(t) => t,
        None => {
            crate::emit_err(
                "switch_root: no mount table; leaving the old root's memory in use\n",
            );
            return;
        }
    };
    if !DISPOSABLE_ROOT.contains(&fstype.as_str()) {
        crate::emit_err(&format!(
            "switch_root: / is {fstype}, not an initramfs; leaving it intact\n"
        ));
        return;
    }
    let Ok(meta) = std::fs::metadata("/") else {
        return;
    };
    empty_filesystem(Path::new("/"), std::os::unix::fs::MetadataExt::dev(&meta));
}

pub fn run(args: &[String]) -> Result<u8, String> {
    let newroot = args.first().ok_or_else(usage)?;
    let init = args.get(1).ok_or_else(usage)?;
    let rest = args.get(2..).unwrap_or(&[]);

    let root = Path::new(newroot);
    if !root.is_dir() {
        return Err(format!("{newroot}: not a directory"));
    }
    // Both remaining checks run BEFORE the first mount move, so a refusal leaves
    // the running system exactly as it was.
    //
    // INIT is named as it will be AFTER the chroot, so it is resolved with the
    // new root as "/" — see `resolve_in_root`. Refusing here is the difference
    // between an error message and a panicked kernel.
    let staged_init = resolve_in_root(root, init).ok_or_else(|| {
        format!("{init}: does not resolve inside {newroot} — refusing to switch")
    })?;
    is_runnable(root, &staged_init, 0, Loader::Any)?;
    let argv0 = exec_argv0(init);
    // The same file named from inside the new root, for the exec below.
    let after_chroot = match staged_init.strip_prefix(root) {
        Ok(rel) => Path::new("/").join(rel),
        Err(_) => {
            return Err(format!(
                "{}: resolved outside {newroot} — refusing to switch",
                staged_init.display()
            ))
        }
    };

    // Read the mount table BEFORE moving anything: /proc is one of the mounts
    // about to move, and it is unreadable from the moment it does.
    //
    // Read as BYTES: mount points may hold any bytes but `/` and NUL, so
    // `read_to_string` would lose the WHOLE table to one non-UTF-8 mount
    // elsewhere on the system, leaving the new root with no /dev or /proc.
    // Lossy conversion can only damage such a mount's own name; the API paths
    // are ASCII, and a mangled NEWROOT is refused by the check below.
    let mounts = match std::fs::read("/proc/self/mounts") {
        Ok(raw) => Some(String::from_utf8_lossy(&raw).into_owned()),
        Err(e) => {
            crate::emit_err(&format!(
                "switch_root: /proc/self/mounts: {e}; carrying no API mounts across\n"
            ));
            None
        }
    };
    // NEWROOT must be a mount, not a plain directory: `MS_MOVE` of a directory
    // onto / fails, and without this the API mounts would ALREADY have been
    // relocated into it by then — a half-switched system with /dev and /proc
    // buried in a subdirectory of a root that never became one.
    if !is_mount_point(root, mounts.as_deref()) {
        return Err(format!(
            "{newroot}: not a mount point — refusing to switch"
        ));
    }
    let moves = match mounts.as_deref() {
        Some(text) => api_moves(text, newroot),
        None => Vec::new(),
    };
    for (source, target) in &moves {
        // The new root is read-only, so a missing mount point is a fact about
        // the image, not something to repair here.
        if !Path::new(target).is_dir() {
            crate::emit_err(&format!(
                "switch_root: {target} does not exist in the new root; leaving {source} behind\n"
            ));
            continue;
        }
        if let Err(e) = sys::mount(
            &cpath(source)?,
            &cpath(target)?,
            None,
            sys::MS_MOVE,
            None,
        ) {
            crate::emit_err(&format!("switch_root: moving {source} to {target}: {e}\n"));
        }
    }

    std::env::set_current_dir(root).map_err(|e| format!("chdir {newroot}: {e}"))?;
    // Last use of the old root. From here the cwd is the only handle on the new
    // one, which is why the chdir comes first.
    free_old_root(mounts.as_deref());
    // "." rather than NEWROOT: the chdir above has already resolved it, so this
    // is correct whether the caller passed an absolute or a relative path.
    let dot = cpath(".")?;
    sys::mount(&dot, &cpath("/")?, None, sys::MS_MOVE, None)
        .map_err(|e| format!("moving {newroot} to /: {e}"))?;
    sys::chroot(&dot).map_err(|e| format!("chroot: {e}"))?;
    std::env::set_current_dir("/").map_err(|e| format!("chdir /: {e}"))?;

    // The path is the VERIFIED one as it reads after the chroot, not the
    // operand: `Command::new("init")` has no slash, so execvp would PATH-search
    // instead — against whatever PATH the environment happens to carry, or
    // glibc's `/bin:/usr/bin` fallback when it carries none. Neither is the
    // file this applet just verified.
    // argv[0] stays the operand, because the file it names is routinely a
    // multicall dispatching on it — handing `/init` its own store path makes
    // argv[0] `busybox` with no applet, which prints usage and exits as PID 1.
    let mut cmd = exec_command(&after_chroot, &argv0, rest);
    // `exec` replaces this process image and does not return on success.
    Err(format!("exec {}: {}", after_chroot.display(), cmd.exec()))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]
    use super::*;

    const MOUNTS: &str = "\
rootfs / rootfs rw 0 0
devtmpfs /dev devtmpfs rw,nosuid 0 0
proc /proc proc rw,nosuid,nodev,noexec 0 0
sysfs /sys sysfs rw,nosuid,nodev,noexec 0 0
tmpfs /run tmpfs rw,nosuid,nodev 0 0
/dev/loop0 /mnt/root erofs ro 0 0
";

    #[test]
    fn every_mounted_api_path_is_carried_across_and_rebased() {
        let moves = api_moves(MOUNTS, "/mnt/root");
        assert_eq!(
            moves,
            vec![
                ("/dev".to_string(), "/mnt/root/dev".to_string()),
                ("/proc".to_string(), "/mnt/root/proc".to_string()),
                ("/sys".to_string(), "/mnt/root/sys".to_string()),
                ("/run".to_string(), "/mnt/root/run".to_string()),
            ]
        );
    }

    /// An API path that is not mounted must not be moved: `mount(MS_MOVE)` on a
    /// plain directory fails, and the resulting console noise would look like a
    /// real fault during every boot that has no `/run`.
    #[test]
    fn unmounted_api_paths_are_skipped() {
        let partial = "rootfs / rootfs rw 0 0\nproc /proc proc rw 0 0\n";
        assert_eq!(
            api_moves(partial, "/mnt/root"),
            vec![("/proc".to_string(), "/mnt/root/proc".to_string())]
        );
        assert!(api_moves("", "/mnt/root").is_empty());
    }

    /// A trailing slash on NEWROOT must not produce `//dev`.
    #[test]
    fn a_trailing_slash_on_the_new_root_is_normalised() {
        let moves = api_moves("proc /proc proc rw 0 0\n", "/mnt/root/");
        assert_eq!(moves, vec![("/proc".to_string(), "/mnt/root/proc".to_string())]);
    }

    #[test]
    fn mount_points_are_the_second_field() {
        let points = mount_points(MOUNTS);
        assert_eq!(points.first().map(String::as_str), Some("/"));
        assert!(points.iter().any(|p| p == "/mnt/root"));
        // A short or blank line contributes nothing rather than panicking.
        assert!(mount_points("garbage\n\n").is_empty());
        // The kernel escapes the separators it uses; an unescaped compare would
        // read a mounted `/mnt/new root` as unmounted and refuse a valid boot.
        assert_eq!(
            mount_points("/dev/sda1 /mnt/new\\040root ext4 rw 0 0\n"),
            vec!["/mnt/new root".to_string()]
        );
        // Only those four escapes are the kernel's; every other byte is copied
        // through unchanged. Decoding them one `char` at a time would mangle a
        // multi-byte name into one that matches no canonicalised path.
        assert_eq!(
            mount_points("/dev/sda1 /mnt/r\u{e9}al\\040x ext4 rw 0 0\n"),
            vec!["/mnt/r\u{e9}al x".to_string()]
        );
        assert_eq!(
            mount_points("/dev/sda1 /mnt/a\\134b ext4 rw 0 0\n"),
            vec!["/mnt/a\\b".to_string()]
        );
        // A backslash that begins no known escape stays a backslash rather than
        // eating the three bytes after it.
        assert_eq!(
            mount_points("/dev/sda1 /mnt/a\\999 ext4 rw 0 0\n"),
            vec!["/mnt/a\\999".to_string()]
        );
    }

    #[test]
    fn a_missing_operand_is_a_usage_error() {
        assert!(run(&[]).is_err());
        assert!(run(&["/mnt/root".to_string()]).is_err());
    }

    /// The mount-point test, fail-closed. Crucially it must reject a plain
    /// directory that merely sits on a DIFFERENT filesystem than `/` — comparing
    /// st_dev against `/` alone accepted those, and `MS_MOVE` of a non-mount
    /// then failed with the API mounts already relocated into it.
    ///
    /// Note what is NOT tested here, deliberately: a NEWROOT that PASSES every
    /// check would carry `run` straight into moving this process's mounts and
    /// chrooting the test harness. Only refusal paths may be exercised.
    #[test]
    fn only_a_real_mount_point_is_accepted() {
        const MOUNTS: &str = "rootfs / rootfs rw 0 0\n/dev/loop0 /mnt/root erofs ro 0 0\n";
        // In the table => a mount point. Absent => not, however it is spelled.
        assert!(is_mount_point(Path::new("/"), Some(MOUNTS)));
        assert!(!is_mount_point(Path::new("/etc"), Some(MOUNTS)));
        // The case the st_dev-against-/ test got wrong: a plain directory INSIDE
        // another mount. It is not in the table, so it is refused.
        assert!(!is_mount_point(Path::new("/mnt/root/subdir"), Some(MOUNTS)));
        // Unstattable or unresolvable answers "no" rather than switching.
        assert!(!is_mount_point(Path::new("/nonexistent-xyz"), Some(MOUNTS)));
        // Fallback with no mount table: a directory of / is not a mount point.
        assert!(!is_mount_point(Path::new("/etc"), None));
    }

    /// The property that matters on td: `/sbin/init` in a real new root is a
    /// symlink to an ABSOLUTE `/td/store/...` path that does not exist in the
    /// initramfs. Resolution must follow it with the new root as "/", or a good
    /// root is refused.
    #[test]
    fn an_absolute_symlink_resolves_against_the_new_root() {
        let root = std::env::temp_dir().join(format!("td-init-resolve-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let store = root.join("td/store/abc-td-init/bin");
        std::fs::create_dir_all(&store).unwrap();
        std::fs::create_dir_all(root.join("sbin")).unwrap();
        std::fs::write(store.join("td-init"), "#!/bin/sh\n").unwrap();
        std::os::unix::fs::symlink("/td/store/abc-td-init/bin/td-init", root.join("sbin/init"))
            .unwrap();

        // The live root has no /td/store, so this is exactly the case that a
        // resolution against "/" would get wrong.
        assert!(!Path::new("/td/store/abc-td-init/bin/td-init").exists());
        assert_eq!(
            resolve_in_root(&root, "/sbin/init"),
            Some(store.join("td-init"))
        );

        // A relative link resolves next to its own directory, and `..` may not
        // climb out of the new root.
        std::os::unix::fs::symlink("../td/store/abc-td-init/bin/td-init", root.join("sbin/rel"))
            .unwrap();
        assert_eq!(resolve_in_root(&root, "/sbin/rel"), Some(store.join("td-init")));
        std::fs::create_dir_all(root.join("etc")).unwrap();
        assert_eq!(resolve_in_root(&root, "/../../etc"), Some(root.join("etc")));

        // A loop terminates rather than hanging, and a dangling name resolves to
        // nothing at all.
        std::os::unix::fs::symlink("/sbin/loop", root.join("sbin/loop")).unwrap();
        assert_eq!(resolve_in_root(&root, "/sbin/loop"), None);
        assert_eq!(resolve_in_root(&root, "/sbin/absent/deeper"), None);

        let _ = std::fs::remove_dir_all(&root);
    }

    /// A path is bytes, not text. A symlink target the filesystem accepts but
    /// UTF-8 does not must still resolve: refusing it would be a machine that
    /// does not boot, and the store path it points at is perfectly valid.
    #[test]
    fn a_non_utf8_symlink_target_still_resolves() {
        let root = std::env::temp_dir().join(format!("td-init-bytes-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let odd = OsStr::from_bytes(b"\xff\xfe");
        let bin = root.join("td").join(odd).join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::create_dir_all(root.join("sbin")).unwrap();
        std::fs::write(bin.join("real"), "#!/bin/sh\n").unwrap();

        let mut target = std::ffi::OsString::from("/td/");
        target.push(odd);
        target.push("/bin/real");
        std::os::unix::fs::symlink(&target, root.join("sbin/init")).unwrap();

        assert_eq!(resolve_in_root(&root, "/sbin/init"), Some(bin.join("real")));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Mode bits do not prove the kernel can exec a file. Each case here would
    /// otherwise fail `execve` AFTER the mounts moved and the chroot happened —
    /// the one failure switch_root cannot report, because PID 1 dying there is a
    /// kernel panic rather than a message.
    #[test]
    fn only_a_file_the_kernel_can_actually_load_is_accepted() {
        use std::os::unix::fs::PermissionsExt;
        let root = std::env::temp_dir().join(format!("td-init-exec-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("bin")).unwrap();
        let exec = |p: &Path| {
            std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755)).unwrap()
        };

        // A static ELF is loadable.
        let elf = root.join("bin/elf");
        std::fs::write(&elf, elf64(&[])).unwrap();
        exec(&elf);
        assert!(is_runnable(&root, &elf, 0, Loader::Any).is_ok());

        // A chmod +x text file is NOT — the case mode bits alone let through.
        let text = root.join("bin/text");
        std::fs::write(&text, "just some words\n").unwrap();
        exec(&text);
        let e = is_runnable(&root, &text, 0, Loader::Any).unwrap_err();
        assert!(e.contains("neither an ELF nor"), "{e}");

        // A script whose interpreter resolves inside the new root is loadable...
        let good = root.join("bin/good");
        std::fs::write(&good, "#!/bin/elf -x\necho hi\n").unwrap();
        exec(&good);
        std::os::unix::fs::symlink("/bin/elf", root.join("bin/sh")).unwrap();
        assert!(is_runnable(&root, &good, 0, Loader::Any).is_ok());

        // ...and one naming an interpreter that only exists on the LIVE root is
        // not. /bin/sh exists on this host, which is what makes this the real
        // case: resolution must happen inside NEWROOT.
        let bad = root.join("bin/bad");
        std::fs::write(&bad, "#!/usr/bin/env python3\n").unwrap();
        exec(&bad);
        let e = is_runnable(&root, &bad, 0, Loader::Any).unwrap_err();
        assert!(e.contains("does not resolve inside the new root"), "{e}");

        let _ = std::fs::remove_dir_all(&root);
    }

    /// A well-formed ELF64 x86-64 executable header carrying one `PT_LOAD` plus
    /// whatever else the caller asks for. `PT_LOAD` is in the baseline because
    /// an executable without one is not loadable, and every test here that is
    /// not ABOUT that wants a file the kernel would accept.
    fn elf64(phdrs: &[[u8; 56]]) -> Vec<u8> {
        let mut all = vec![phdr(1)]; // PT_LOAD
        all.extend_from_slice(phdrs);
        elf64_raw(&all)
    }

    /// The same, with NO segment added — for the cases that are about the
    /// program header table itself.
    fn elf64_raw(phdrs: &[[u8; 56]]) -> Vec<u8> {
        let mut out = vec![0u8; 64];
        out.splice(0..4, b"\x7fELF".iter().copied());
        out[4] = 2; // ELFCLASS64
        out[5] = 1; // ELFDATA2LSB
        out[6] = 1; // EV_CURRENT
        out.splice(16..18, 2u16.to_le_bytes()); // ET_EXEC
        out.splice(18..20, 62u16.to_le_bytes()); // EM_X86_64
        out.splice(32..40, 64u64.to_le_bytes()); // e_phoff — right after the header
        out.splice(54..56, 56u16.to_le_bytes()); // e_phentsize
        let n = u16::try_from(phdrs.len().max(1)).unwrap();
        out.splice(56..58, n.to_le_bytes()); // e_phnum
        if phdrs.is_empty() {
            out.extend_from_slice(&phdr(0)); // one PT_NULL segment
        }
        for ph in phdrs {
            out.extend_from_slice(ph);
        }
        out
    }

    fn phdr(p_type: u32) -> [u8; 56] {
        let mut ph = [0u8; 56];
        ph[0..4].copy_from_slice(&p_type.to_le_bytes());
        ph
    }

    /// A PT_INTERP program header whose string sits at `offset` in the file.
    fn pt_interp(offset: u64, len: u64) -> [u8; 56] {
        let mut ph = phdr(3); // PT_INTERP
        ph[8..16].copy_from_slice(&offset.to_le_bytes()); // p_offset
        ph[32..40].copy_from_slice(&len.to_le_bytes()); // p_filesz
        ph
    }

    /// The four magic bytes are not a loadability check. Every case here starts
    /// with `\x7fELF`, is `chmod +x`, and would fail `execve` — after the chroot,
    /// where the failure is a panicked kernel rather than this error message.
    #[test]
    fn an_elf_that_this_kernel_cannot_load_is_refused() {
        use std::os::unix::fs::PermissionsExt;
        let root = std::env::temp_dir().join(format!("td-init-elf-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("bin")).unwrap();
        let put = |name: &str, bytes: &[u8]| {
            let p = root.join("bin").join(name);
            std::fs::write(&p, bytes).unwrap();
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
            p
        };
        let why = |p: &Path| is_runnable(&root, p, 0, Loader::Any).unwrap_err();

        // Truncated: the magic is all there is.
        let e = why(&put("stub", b"\x7fELF\x02\x01\x01\x00rest"));
        assert!(e.contains("truncated"), "{e}");

        let mut m32 = elf64(&[]);
        m32[4] = 1; // ELFCLASS32
        assert!(why(&put("m32", &m32)).contains("not a 64-bit ELF"));

        let mut msb = elf64(&[]);
        msb[5] = 2; // ELFDATA2MSB
        assert!(why(&put("msb", &msb)).contains("little-endian"));

        let mut arm = elf64(&[]);
        arm.splice(18..20, 183u16.to_le_bytes()); // EM_AARCH64
        assert!(why(&put("arm", &arm)).contains("another machine"));

        // Not a bound but a contract: every offset below is read at the position
        // EI_VERSION 1 defines. A future version could move them, and reading a
        // v2 header by v1's layout is exactly the misparse this refuses.
        let mut v2 = elf64(&[]);
        v2[6] = 2;
        assert!(why(&put("v2", &v2)).contains("unknown ELF version"));

        // More program headers than the header table can plausibly hold, and an
        // entry size that is not the one every offset below assumes.
        let mut wide = elf64(&[]);
        wide.splice(56..58, 4096u16.to_le_bytes()); // e_phnum past MAX_PHNUM
        assert!(why(&put("wide", &wide)).contains("unusable shape"));
        let mut phent = elf64(&[]);
        phent.splice(54..56, 32u16.to_le_bytes()); // the 32-bit entry size
        assert!(why(&put("phent", &phent)).contains("unusable shape"));

        let mut obj = elf64(&[]);
        obj.splice(16..18, 1u16.to_le_bytes()); // ET_REL
        assert!(why(&put("obj", &obj)).contains("not an ELF executable"));

        // Nothing to map. `execve` fails on it, so accepting it would be the
        // post-chroot panic in a file that passes every header field.
        let e = why(&put("noload", &elf64_raw(&[])));
        assert!(e.contains("no loadable segment"), "{e}");

        // A dynamic executable whose loader is absent from the new root: the
        // shape a store path that moved produces, and the one the shebang path
        // has always caught for scripts.
        let name = b"/lib64/ld-linux-x86-64.so.2\0";
        let mut dyn_missing = elf64(&[pt_interp(176, name.len() as u64)]);
        dyn_missing.extend_from_slice(name);
        let e = why(&put("dyn", &dyn_missing));
        assert!(e.contains("does not resolve inside the new root"), "{e}");

        // The kernel wants the terminator INSIDE p_filesz; without one there is
        // no interpreter name, only bytes we would have to guess an end for.
        let mut unterminated = elf64(&[pt_interp(176, 8)]);
        unterminated.extend_from_slice(b"/lib/ld!");
        let e = why(&put("noterm", &unterminated));
        assert!(e.contains("not NUL-terminated"), "{e}");

        // ...and the kernel tests the LAST byte, not "is there a NUL somewhere".
        // A name with trailing bytes past its terminator is -ENOEXEC there, so
        // reading only up to the first NUL would accept a binary that cannot be
        // exec'd — the refusal deferred to after the chroot.
        let mut trailing = elf64(&[pt_interp(176, 10)]);
        trailing.extend_from_slice(b"/lib/ld\0!!");
        let e = why(&put("trailing", &trailing));
        assert!(e.contains("not NUL-terminated"), "{e}");

        // Under two bytes cannot hold a name and its terminator; the kernel
        // rejects the segment before reading it, and a lone NUL would otherwise
        // parse here as an interpreter named "".
        let mut just_nul = elf64(&[pt_interp(176, 1)]);
        just_nul.extend_from_slice(b"\0");
        let e = why(&put("nulonly", &just_nul));
        assert!(e.contains("implausible length"), "{e}");
        let mut empty_name = elf64(&[pt_interp(176, 2)]);
        empty_name.extend_from_slice(b"\0\0");
        let e = why(&put("emptyname", &empty_name));
        assert!(e.contains("empty interpreter"), "{e}");

        // The table bound is the kernel's 64KiB, so a header count it accepts is
        // never refused here. 1170 entries at 56 bytes is the last legal one.
        let mut most = elf64(&[]);
        most.splice(56..58, 1170u16.to_le_bytes());
        let e = why(&put("most", &most));
        assert!(!e.contains("unusable shape"), "{e}");

        // The same binary boots once its loader IS in the new root.
        std::fs::create_dir_all(root.join("lib64")).unwrap();
        std::fs::write(root.join("lib64/ld-linux-x86-64.so.2"), elf64(&[])).unwrap();
        std::fs::set_permissions(
            root.join("lib64/ld-linux-x86-64.so.2"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        assert!(is_runnable(&root, &root.join("bin/dyn"), 0, Loader::Any).is_ok());

        // ...but only because that loader is an ELF. A PT_INTERP naming a
        // SCRIPT is ELIBBAD to the kernel — `load_elf_interp` accepts no other
        // format — while a plain `#!` program is loaded fine. Running both
        // through one recursion accepted the first, which is the direction that
        // costs a panic after the chroot.
        std::fs::write(root.join("lib64/ld-linux-x86-64.so.2"), b"#!/bin/sh\n").unwrap();
        std::fs::set_permissions(
            root.join("lib64/ld-linux-x86-64.so.2"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        let e = why(&root.join("bin/dyn"));
        assert!(e.contains("ELIBBAD"), "{e}");

        let _ = std::fs::remove_dir_all(&root);
    }

    /// The old rootfs is freed only when the table says `/` is one of the
    /// disposable kinds. This is the guard between "reclaim the initramfs" and
    /// "delete somebody's root filesystem", so it is read off the real format.
    #[test]
    fn only_an_initramfs_root_is_disposable() {
        let table = |t: &str| format!("rootfs / {t} rw 0 0\ndevtmpfs /dev devtmpfs rw 0 0\n");
        for disposable in ["rootfs", "ramfs", "tmpfs"] {
            let text = table(disposable);
            assert_eq!(root_fstype(&text).as_deref(), Some(disposable));
            assert!(DISPOSABLE_ROOT.contains(&disposable));
        }
        for keep in ["erofs", "ext4", "btrfs", "squashfs", "overlay"] {
            let text = table(keep);
            assert_eq!(root_fstype(&text).as_deref(), Some(keep));
            assert!(!DISPOSABLE_ROOT.contains(&keep), "{keep}");
        }
        // `/` is found wherever it sits in the table, not just first...
        let late = "devtmpfs /dev devtmpfs rw 0 0\nrootfs / rootfs rw 0 0\n";
        assert_eq!(root_fstype(late).as_deref(), Some("rootfs"));
        // ...and a table without it yields nothing rather than a wrong answer.
        assert_eq!(root_fstype("devtmpfs /dev devtmpfs rw 0 0\n"), None);
        assert_eq!(root_fstype(""), None);
        // A mount point whose name only DECODES to "/" must not answer for it.
        assert_eq!(root_fstype("x /mnt\\040dir ext4 rw 0 0\n"), None);
    }

    /// The device boundary is the other guard, and the one that keeps the new
    /// root — mounted UNDER the tree being emptied — out of the walk. A `dev`
    /// that matches nothing must leave the tree untouched; the real one must
    /// empty it completely, symlinks included and unfollowed.
    #[test]
    fn the_walk_stops_at_a_device_boundary() {
        use std::os::unix::fs::MetadataExt;
        let base = std::env::temp_dir().join(format!("td-init-free-{}", std::process::id()));
        let build = || {
            let _ = std::fs::remove_dir_all(&base);
            std::fs::create_dir_all(base.join("sub/deeper")).unwrap();
            std::fs::write(base.join("file"), b"x").unwrap();
            std::fs::write(base.join("sub/nested"), b"y").unwrap();
            std::os::unix::fs::symlink("/nowhere", base.join("dangling")).unwrap();
        };

        // A foreign device: every entry is another filesystem's root, so none is
        // entered and none is removed.
        build();
        let dev = std::fs::metadata(&base).unwrap().dev();
        empty_filesystem(&base, dev.wrapping_add(1));
        assert!(base.join("sub/nested").exists());
        assert!(base.join("file").exists());

        // The tree's own device: emptied, but the directory handed in survives —
        // it is the mount point itself, which the caller still needs.
        build();
        // A symlink pointing at a directory is unlinked, never followed into.
        let outside = base.with_extension("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("keep"), b"z").unwrap();
        std::os::unix::fs::symlink(&outside, base.join("escape")).unwrap();
        empty_filesystem(&base, dev);
        assert!(base.is_dir(), "the walk must not remove its own root");
        assert_eq!(std::fs::read_dir(&base).unwrap().count(), 0);
        assert!(outside.join("keep").exists(), "a symlink was followed out");

        let _ = std::fs::remove_dir_all(&base);
        let _ = std::fs::remove_dir_all(&outside);
    }

    /// argv[0] is what a multicall INIT dispatches on. td's `/init` is a symlink
    /// into busybox and `/sbin/init` becomes one into td-init, so exec'ing the
    /// RESOLVED store path under its own name gives argv[0] `busybox`/`td-init`
    /// with no applet argument — usage, exit, and a panicked kernel.
    #[test]
    fn the_operand_survives_as_argv0() {
        assert_eq!(exec_argv0("/sbin/init"), "/sbin/init");
        assert_eq!(exec_argv0("/init"), "/init");
        // Named relative to the new root, it still reads as a post-chroot path.
        assert_eq!(exec_argv0("sbin/init"), "/sbin/init");
        // What matters downstream is the basename the multicall sees.
        assert_eq!(crate::basename(&exec_argv0("/sbin/init")), "init");
    }

    /// ...and it must survive as argv[0] of the exec ACTUALLY built, not just as
    /// a correct string sitting in a variable: a caller that stops passing it is
    /// the same panicked kernel, and testing `exec_argv0` alone cannot see that.
    /// std renders a differing argv[0] as `["program"] "argv0" ...`, so this
    /// reads the two apart; a std formatting change would red it loudly rather
    /// than quietly stop checking.
    #[test]
    fn the_exec_runs_the_resolved_path_under_the_operands_name() {
        let target = Path::new("/td/store/9k4-td-init-1/bin/td-init");
        let cmd = exec_command(target, &exec_argv0("/sbin/init"), &["-q".to_string()]);
        // Positional, not three `contains` checks: which name goes where is the
        // whole property. `Command::new(argv0).arg0(resolved)` — the exact
        // inversion — and `arg(argv0)` instead of `arg0(argv0)`, which shifts the
        // operand into argv[1] and leaves the store path as argv[0], both put all
        // three strings in the output while being the panicked kernel.
        assert_eq!(
            format!("{cmd:?}"),
            "[\"/td/store/9k4-td-init-1/bin/td-init\"] \"/sbin/init\" \"-q\""
        );
    }

    #[test]
    fn the_shebang_line_is_parsed_the_way_the_kernel_parses_it() {
        assert_eq!(shebang_interpreter(b"#!/bin/sh\n"), Some("/bin/sh".into()));
        // Leading blanks are skipped and arguments are not part of the program.
        assert_eq!(shebang_interpreter(b"#!  /bin/sh -x\n"), Some("/bin/sh".into()));
        // A file shorter than the buffer needs no newline: the kernel's buffer
        // is zero-filled, so the padding ends the line and the file runs.
        assert_eq!(shebang_interpreter(b"#!/bin/sh"), Some("/bin/sh".into()));
        // A NUL ends the line exactly as a newline does.
        assert_eq!(shebang_interpreter(b"#!/bin/sh\0junk"), Some("/bin/sh".into()));
        assert_eq!(shebang_interpreter(b"\x7fELF"), None);
        assert_eq!(shebang_interpreter(b"#!\n"), None);
        assert_eq!(shebang_interpreter(b""), None);
    }

    /// A full buffer with no line terminator is ENOEXEC to the kernel, which
    /// stops looking at the end of its 256 bytes. Reading the whole run as a
    /// filename would accept an init that cannot be exec'd — and this module's
    /// one job is to fail BEFORE the mounts move, not after.
    #[test]
    fn an_unterminated_shebang_filling_the_buffer_is_refused() {
        let mut full = b"#!/".to_vec();
        full.resize(HEAD, b'a');
        assert_eq!(full.len(), HEAD);
        assert_eq!(shebang_interpreter(&full), None);
        // One byte short of the buffer is a file that ENDED, so it still runs.
        assert_eq!(
            shebang_interpreter(full.get(..HEAD - 1).unwrap()),
            Some(format!("/{}", "a".repeat(HEAD - 4)))
        );
    }

    /// The fail-early property: an INIT that is not executable inside the new
    /// root must be diagnosed BEFORE any mount moves.
    #[test]
    fn a_new_root_without_a_usable_init_is_refused() {
        let root = std::env::temp_dir().join(format!("td-init-switch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("sbin")).unwrap();
        // A plain file with no execute bit: it RESOLVES, so this exercises the
        // executability test rather than the resolution one.
        std::fs::write(root.join("sbin/init"), "not a program").unwrap();
        let argv = vec![
            root.display().to_string(),
            "/sbin/init".to_string(),
        ];
        let err = run(&argv).unwrap_err();
        assert!(err.contains("not an executable file"), "{err}");
        // An INIT that is not there at all is refused by name.
        let absent = vec![root.display().to_string(), "/sbin/absent".to_string()];
        assert!(run(&absent).unwrap_err().contains("does not resolve inside"));
        // ...and a NEWROOT that is not a directory at all.
        let missing = vec![
            root.join("nope").display().to_string(),
            "/sbin/init".to_string(),
        ];
        assert!(run(&missing).unwrap_err().contains("not a directory"));
        let _ = std::fs::remove_dir_all(&root);
    }
}

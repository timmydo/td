//! Minimal raw x86_64-linux syscall layer: what the build sandbox, the gate
//! runner and the store writer need and safe `std` does not reach — namespaces
//! and mounts, process creation and waiting, ids and resource limits, and a
//! handful of descriptor primitives. Hand-rolled to keep the
//! crate zero-dependency (precedent is the
//! hand-rolled SHA-256; the rung's differential proves behavior, and the drv
//! platform field is checked to be x86_64-linux before any of this runs).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unreachable, clippy::todo, clippy::unimplemented, clippy::indexing_slicing)] // grandfathered: pre-dates the rust-lint rules (AGENTS.md); remove when cleaned
#![allow(unsafe_code)] // confined raw-syscall / low-level layer (UNSAFE.md)

#[cfg(not(all(target_arch = "x86_64", target_os = "linux")))]
compile_error!("td-builder's sandbox is x86_64-linux only (the pinned platform)");

use std::ffi::CStr;
use std::io;

const SYS_READ: usize = 0;
const SYS_CLOSE: usize = 3;
const SYS_IOCTL: usize = 16;
const SYS_PIPE2: usize = 293;
const SYS_SOCKET: usize = 41;
const SYS_GETUID: usize = 102;
const SYS_GETGID: usize = 104;
const SYS_MOUNT: usize = 165;
const SYS_UMOUNT2: usize = 166;
const SYS_PIVOT_ROOT: usize = 155;
const SYS_UNSHARE: usize = 272;
const SYS_FORK: usize = 57;
const SYS_WAIT4: usize = 61;
const SYS_EXIT_GROUP: usize = 231;
const SYS_WRITE: usize = 1;
const SYS_PRCTL: usize = 157;
const SYS_GETPPID: usize = 110;
const SYS_SETPRIORITY: usize = 141;
const SYS_GETPRIORITY: usize = 140;
const SYS_PRLIMIT64: usize = 302;
const SYS_MMAP: usize = 9;
const SYS_RENAMEAT2: usize = 316;

const AT_FDCWD: isize = -100;
const RENAME_NOREPLACE: usize = 1;

/// setpriority/getpriority `which`: act on a single process by PID (0 = self).
const PRIO_PROCESS: usize = 0;

/// RLIMIT_DATA (the data segment: heap + brk + private writable anonymous
/// mmap on Linux ≥ 4.7). The per-build memory backstop caps THIS — it blocks
/// the bulk of a compiler's allocation without counting the large *virtual*
/// address-space reservations Go/Rust make (so it false-trips far less than
/// RLIMIT_AS would). Scope of the resource arg to prlimit64(2).
pub const RLIMIT_DATA: usize = 2;

/// prlimit64 sentinel for "leave this limit unchanged" — RLIM64_INFINITY is
/// only used as a comparison value here; a cap is always finite.
pub const RLIM_INFINITY: u64 = u64::MAX;

/// mmap(2) prot/flags for a private anonymous read/write mapping — what the
/// rlimit behavioral test allocates to prove the cap is load-bearing.
const PROT_READ_WRITE: usize = 0x1 | 0x2;
const MAP_PRIVATE_ANON: usize = 0x2 | 0x20;

const PR_SET_PDEATHSIG: usize = 1;
/// SIGKILL — the parent-death signal the host-sandbox arms (uncatchable, so a
/// wedged inner build cannot ignore it).
pub const SIGKILL: usize = 9;
/// SIGTERM — a graceful termination request (the signal a CI cancel/timeout
/// sends). The sandbox-hardening gate SIGTERMs the top td-builder to prove the
/// PR_SET_PDEATHSIG cascade reaps the inner tree even on a soft kill.
pub const SIGTERM: usize = 15;

// Bring a loopback interface up via SIOCSIFFLAGS on a dgram socket.
const AF_INET: usize = 2;
const SOCK_DGRAM: usize = 2;
const SIOCGIFFLAGS: usize = 0x8913;
const SIOCSIFFLAGS: usize = 0x8914;
const IFF_UP: u16 = 0x1;

/// FICLONE = _IOW(0x94, 9, int): clone a whole file's data (reflink/CoW). This is the
/// asm-generic encoding (x86-64/aarch64/riscv — td's targets); MIPS/PowerPC/SPARC encode
/// `_IOW` differently, where this constant is simply the wrong ioctl and returns an error —
/// `try_reflink` then falls back to a byte copy, so a wrong value degrades, never corrupts.
const FICLONE: usize = 0x4004_9409;

pub const CLONE_NEWNS: usize = 0x0002_0000;
pub const CLONE_NEWUTS: usize = 0x0400_0000;
pub const CLONE_NEWIPC: usize = 0x0800_0000;
pub const CLONE_NEWUSER: usize = 0x1000_0000;
pub const CLONE_NEWPID: usize = 0x2000_0000;
pub const CLONE_NEWNET: usize = 0x4000_0000;

pub const MS_RDONLY: usize = 0x1;
/// APPLICATIONS.md §B.8: a declared `payloadInputs` item binds `ro,noexec`, so
/// "never executed" is refused by the kernel rather than by a scan of the
/// recipe's argv — which a build tool that can see the path defeats by
/// concatenating it, reading it out of a file, or walking the store.
pub const MS_NOEXEC: usize = 0x8;
pub const MS_REMOUNT: usize = 0x20;
pub const MS_BIND: usize = 0x1000;
pub const MS_REC: usize = 0x4000;
pub const MS_PRIVATE: usize = 0x4_0000;

/// umount2(2) flag: detach a busy mount lazily (used to drop the old root
/// after pivot_root).
pub const MNT_DETACH: usize = 0x2;

/// x86_64 syscall ABI: number in rax, args in rdi/rsi/rdx/r10/r8; rcx and
/// r11 are clobbered by the instruction; negative return is -errno.
unsafe fn syscall5(n: usize, a1: usize, a2: usize, a3: usize, a4: usize, a5: usize) -> isize {
    let ret: isize;
    core::arch::asm!(
        "syscall",
        inlateout("rax") n as isize => ret,
        in("rdi") a1,
        in("rsi") a2,
        in("rdx") a3,
        in("r10") a4,
        in("r8") a5,
        out("rcx") _,
        out("r11") _,
        options(nostack)
    );
    ret
}

/// Six-argument variant (adds the 6th arg in r9) — needed by mmap(2), whose
/// last arg is the file offset. x86_64 `sys_mmap` rejects a non-page-aligned
/// offset with EINVAL even for an anonymous mapping, so the offset register
/// must be set explicitly (syscall5 would leave r9 holding garbage).
unsafe fn syscall6(
    n: usize,
    a1: usize,
    a2: usize,
    a3: usize,
    a4: usize,
    a5: usize,
    a6: usize,
) -> isize {
    let ret: isize;
    core::arch::asm!(
        "syscall",
        inlateout("rax") n as isize => ret,
        in("rdi") a1,
        in("rsi") a2,
        in("rdx") a3,
        in("r10") a4,
        in("r8") a5,
        in("r9") a6,
        out("rcx") _,
        out("r11") _,
        options(nostack)
    );
    ret
}

fn check(ret: isize) -> io::Result<()> {
    if ret < 0 {
        Err(io::Error::from_raw_os_error(-ret as i32))
    } else {
        Ok(())
    }
}

pub fn unshare(flags: usize) -> io::Result<()> {
    check(unsafe { syscall5(SYS_UNSHARE, flags, 0, 0, 0, 0) })
}

/// mount(2). `src`/`fstype`/`data` may be None (NULL) — e.g. the
/// MS_REC|MS_PRIVATE propagation change takes none of them; `data` carries
/// fs-specific options like tmpfs `uid=/gid=`.
pub fn mount(
    src: Option<&CStr>,
    target: &CStr,
    fstype: Option<&CStr>,
    flags: usize,
    data: Option<&CStr>,
) -> io::Result<()> {
    let s = src.map_or(std::ptr::null(), CStr::as_ptr);
    let t = fstype.map_or(std::ptr::null(), CStr::as_ptr);
    let d = data.map_or(std::ptr::null(), CStr::as_ptr);
    check(unsafe {
        syscall5(SYS_MOUNT, s as usize, target.as_ptr() as usize, t as usize, flags, d as usize)
    })
}

/// pivot_root(2): make `new_root` the process's root and mount the old root at
/// `put_old`. Both must be directories; `new_root` must be a mount point.
pub fn pivot_root(new_root: &CStr, put_old: &CStr) -> io::Result<()> {
    check(unsafe {
        syscall5(SYS_PIVOT_ROOT, new_root.as_ptr() as usize, put_old.as_ptr() as usize, 0, 0, 0)
    })
}

/// umount2(2): unmount `target` with `flags` (e.g. MNT_DETACH).
pub fn umount2(target: &CStr, flags: usize) -> io::Result<()> {
    check(unsafe { syscall5(SYS_UMOUNT2, target.as_ptr() as usize, flags, 0, 0, 0) })
}

/// Bring the loopback interface up inside the current network namespace —
/// SIOCGIFFLAGS|=IFF_UP, SIOCSIFFLAGS on a dgram socket. A fresh netns starts
/// with `lo` DOWN; `guix shell -C` brings it up, so this matches that posture.
/// Requires CAP_NET_ADMIN in the netns (held as userns root).
pub fn bring_loopback_up() -> io::Result<()> {
    let fd = unsafe { syscall5(SYS_SOCKET, AF_INET, SOCK_DGRAM, 0, 0, 0) };
    if fd < 0 {
        return Err(io::Error::from_raw_os_error(-fd as i32));
    }
    let fd = fd as usize;
    // struct ifreq: char ifr_name[16] then a union whose first member is the
    // short ifr_flags (at offset 16). 40 bytes is the x86_64 size.
    let mut ifr = [0u8; 40];
    ifr[0] = b'l';
    ifr[1] = b'o';
    let close_fd = || unsafe {
        syscall5(SYS_CLOSE, fd, 0, 0, 0, 0);
    };
    let g = unsafe { syscall5(SYS_IOCTL, fd, SIOCGIFFLAGS, ifr.as_mut_ptr() as usize, 0, 0) };
    if g < 0 {
        close_fd();
        return Err(io::Error::from_raw_os_error(-g as i32));
    }
    let flags = u16::from_ne_bytes([ifr[16], ifr[17]]) | IFF_UP;
    ifr[16..18].copy_from_slice(&flags.to_ne_bytes());
    let s = unsafe { syscall5(SYS_IOCTL, fd, SIOCSIFFLAGS, ifr.as_mut_ptr() as usize, 0, 0) };
    close_fd();
    check(s)
}

/// Reflink (copy-on-write clone) the whole of `src_fd` into `dst_fd` via
/// ioctl(FICLONE). `dst_fd` must be a freshly-created, empty, writable regular
/// file on the SAME filesystem as `src_fd`. On success the two files share data
/// extents copy-on-write: an independent inode whose later writes copy rather
/// than propagate. Errors (EXDEV cross-device, EOPNOTSUPP/ENOTTY on a fs without
/// reflink) are returned so the caller can fall back to a byte copy.
pub fn reflink(dst_fd: i32, src_fd: i32) -> io::Result<()> {
    check(unsafe { syscall5(SYS_IOCTL, dst_fd as usize, FICLONE, src_fd as usize, 0, 0) })
}

/// Atomically rename `old` to `new` only when `new` is absent. Stable `std`
/// exposes only replacement rename, which is unsafe for publishing a completed
/// directory after a long materialization race window.
pub fn rename_noreplace(old: &CStr, new: &CStr) -> io::Result<()> {
    check(unsafe {
        syscall5(
            SYS_RENAMEAT2,
            AT_FDCWD as usize,
            old.as_ptr() as usize,
            AT_FDCWD as usize,
            new.as_ptr() as usize,
            RENAME_NOREPLACE,
        )
    })
}

/// Write a diagnostic line to fd 2 (stderr) via the raw write(2) syscall —
/// async-signal-safe, unlike `eprintln!` whose lock can deadlock in the
/// post-fork `host_shell` child. Best-effort; a short/failed write is ignored.
/// Used to label which sandbox setup step failed, since std collapses a
/// `pre_exec` error into a generic "spawning <cmd>: <errno>".
pub fn warn(msg: &[u8]) {
    unsafe {
        syscall5(SYS_WRITE, 2, msg.as_ptr() as usize, msg.len(), 0, 0);
    }
}

pub fn getuid() -> u32 {
    // Cannot fail per the man page.
    unsafe { syscall5(SYS_GETUID, 0, 0, 0, 0, 0) as u32 }
}

/// getppid(2) — the parent PID. Used to close the PR_SET_PDEATHSIG race: if the
/// parent already died before set_pdeathsig ran, getppid() reports the reaper
/// (1 or a subreaper) instead of the expected parent, so the child can bail
/// rather than run orphaned. (Meaningful only in the SAME pid namespace as the
/// parent; across a pid-ns boundary the kernel reports 0.)
pub fn getppid() -> i64 {
    unsafe { syscall5(SYS_GETPPID, 0, 0, 0, 0, 0) as i64 }
}

/// prctl(PR_SET_PDEATHSIG, sig): ask the kernel to deliver `sig` to THIS process
/// when its parent dies. The host-sandbox arms SIGKILL at every fork level so a
/// killed td-builder (CI cancellation, a timeout, Ctrl-C) cascades: the
/// PID-namespace parent dies → the PID-1 child is SIGKILLed → the kernel tears
/// the whole PID namespace down, reaping every descendant. Without it the inner
/// build + its mounts are orphaned and keep running. NB the flag is RESET to 0
/// across fork(2), so each forked level must re-arm it.
pub fn set_pdeathsig(sig: usize) -> io::Result<()> {
    check(unsafe { syscall5(SYS_PRCTL, PR_SET_PDEATHSIG, sig, 0, 0, 0) })
}

const O_CLOEXEC: usize = 0o2_000_000;
pub(crate) const O_NONBLOCK: usize = 0o4000;
const EINTR: i32 = 4;

/// pipe2(2), O_CLOEXEC|O_NONBLOCK — the sandbox's parent-liveness channel.
///
/// PR_SET_PDEATHSIG is armed AFTER a fork, so a parent that dies before the
/// child is first scheduled is never signalled and the child runs on orphaned.
/// The child closes the write end, so the read end reports EOF exactly when
/// every OTHER holder is gone — which is what `getppid` cannot answer across a
/// PID-namespace boundary, where the kernel reports 0. O_CLOEXEC because the
/// question is asked before the exec and must not outlive it; O_NONBLOCK
/// because a live parent must answer "alive" rather than block forever.
pub fn pipe_liveness() -> io::Result<(i32, i32)> {
    let mut fds: [i32; 2] = [-1, -1];
    check(unsafe {
        syscall5(
            SYS_PIPE2,
            fds.as_mut_ptr() as usize,
            O_CLOEXEC | O_NONBLOCK,
            0,
            0,
            0,
        )
    })?;
    Ok((fds[0], fds[1]))
}

/// close(2).
pub fn close(fd: i32) -> io::Result<()> {
    check(unsafe { syscall5(SYS_CLOSE, fd as usize, 0, 0, 0, 0) })
}

/// Is any write end of `fd` still open? `Ok(false)` is EOF — every writer is
/// gone, so the parent this channel watches has died. Nothing ever WRITES to
/// it, so a live parent shows as EAGAIN on a non-blocking read. Any OTHER
/// errno is returned rather than folded into `false`: the caller acts on this
/// bit by killing itself, and an unreadable channel is not a dead parent.
pub fn pipe_peer_open(fd: i32) -> io::Result<bool> {
    let mut byte = [0u8; 1];
    loop {
        let ret = unsafe { syscall5(SYS_READ, fd as usize, byte.as_mut_ptr() as usize, 1, 0, 0) };
        if ret == 0 {
            return Ok(false);
        }
        if ret > 0 {
            return Ok(true);
        }
        let errno = -ret as i32;
        if errno == EWOULDBLOCK {
            return Ok(true);
        }
        if errno != EINTR {
            return Err(io::Error::from_raw_os_error(errno));
        }
    }
}

pub fn getgid() -> u32 {
    unsafe { syscall5(SYS_GETGID, 0, 0, 0, 0, 0) as u32 }
}

/// flock(2) LOCK_EX|LOCK_NB — shared per-user scheduler tokens.
/// A slot is an exclusively-flocked file; the kernel releases the lock when the
/// holding process exits (even on SIGKILL), so a crashed gate can never leak a slot
/// — the property that makes the cross-agent pool safe without a reaper.
const SYS_FLOCK: usize = 73;
const LOCK_EX: usize = 2;
const LOCK_NB: usize = 4;
const EWOULDBLOCK: i32 = 11;

/// kill(2) to a whole PROCESS GROUP (negative pid). Gate containment combines
/// this atomic ordinary-tree kill with a `/proc` descendant snapshot so phases
/// that created another process group are covered too.
const SYS_KILL: usize = 62;

/// PRIVATE: every signal the builder sends leaves through `kill_recorded`, so a
/// site that kills has to say why and the reason reaches the audit record.
fn kill_process_group(pgid: u32, sig: usize) -> io::Result<()> {
    // Group 0 would name the sender's own group, and a value past `pid_t`
    // would wrap: neither is a target, so neither reaches the kernel.
    if pgid == 0 || i32::try_from(pgid).is_err() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{pgid} is not a process group id"),
        ));
    }
    let neg = -(i64::from(pgid));
    check(unsafe { syscall5(SYS_KILL, neg as usize, sig, 0, 0, 0) })
}

/// kill(2) to a single PID (positive) — the sandbox-hardening gate SIGTERMs the
/// top td-builder (proving PR_SET_PDEATHSIG reaps the inner tree) and SIGKILLs
/// any stray marker process left behind on a failure path. Distinct from
/// `kill_process_group`, which targets a whole group via a negative pid.
/// PRIVATE for the same reason as `kill_process_group`.
fn kill_pid(pid: i64, sig: usize) -> io::Result<()> {
    // 0 and negatives name a group or everything, and a value past `pid_t`
    // wraps negative: a pid target that is not a positive `pid_t` reaches
    // no one.
    if pid <= 0 || i32::try_from(pid).is_err() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{pid} is not a process id"),
        ));
    }
    check(unsafe { syscall5(SYS_KILL, pid as usize, sig, 0, 0, 0) })
}

/// What a recorded kill is aimed at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KillTarget {
    /// One process, by pid.
    Pid(i64),
    /// A whole process group, by its id: `kill(2)` with the negated pid.
    Group(u32),
}

/// The audit file's directory, `$HOME/.td/kill-audit`; `None` without a
/// HOME. Its own directory, so `host-sandbox` can bind exactly it read-write
/// and the gate runner inside the check sandbox records to the host's file.
pub fn kill_audit_dir() -> Option<std::path::PathBuf> {
    std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(|home| std::path::PathBuf::from(home).join(".td/kill-audit"))
}

/// The audit file, `$HOME/.td/kill-audit/log`.
pub fn kill_audit_path() -> Option<std::path::PathBuf> {
    kill_audit_dir().map(|dir| dir.join("log"))
}

/// Create `kill_audit_dir()` under an EXISTING `~/.td` and return it.
/// td-builder never creates `~/.td` for this: a build sandbox's stand-in
/// home has none, and a record written there would be an undeclared write
/// into the build that nobody could read back. Used by the recorder and by
/// `host-sandbox`, which binds the directory into the check sandbox.
pub fn ensure_kill_audit_dir() -> Option<std::path::PathBuf> {
    let dir = kill_audit_dir()?;
    create_audit_dir(&dir).then_some(dir)
}

/// `dir` exists afterwards, created mode 0700 when its parent already does.
fn create_audit_dir(dir: &std::path::Path) -> bool {
    use std::os::unix::fs::DirBuilderExt as _;
    if !dir.parent().is_some_and(std::path::Path::is_dir) {
        return false;
    }
    let _ = std::fs::DirBuilder::new().mode(0o700).create(dir);
    dir.is_dir()
}

/// Send `sig` to `target` and record it: one line naming the time, this
/// process and its verb, the signal, the target, the reason, whether the
/// signal was accepted, and the target's command line as it stood just
/// before. The line goes to stderr and is appended to `kill_audit_path()`.
/// The only work before the signal is one bounded `/proc` read, in memory;
/// the record comes after it, so recording can neither delay nor fail the
/// kill, and an unwritable file loses the line, not the signal. The append
/// is an ordinary synchronous write to a per-user file, so what a stalled
/// filesystem under it could delay is the caller's own cleanup after the
/// kill, never the kill.
pub fn kill_recorded(target: KillTarget, sig: usize, reason: &str) -> io::Result<()> {
    let cmdline = target_cmdline(target);
    let result = send(target, sig);
    record_kill(target, sig, &cmdline, reason, &result);
    result
}

/// One decision fanned out over several targets, a tree kill: every command
/// line is read first, every signal is sent next, and only then is anything
/// recorded, so nothing but signals stands between the first member and the
/// last. Results in the callers' order.
pub fn kill_all_recorded(kills: &[(KillTarget, String)], sig: usize) -> Vec<io::Result<()>> {
    let cmdlines: Vec<String> = kills.iter().map(|(target, _)| target_cmdline(*target)).collect();
    let results: Vec<io::Result<()>> = kills.iter().map(|(target, _)| send(*target, sig)).collect();
    for (((target, reason), cmdline), result) in kills.iter().zip(&cmdlines).zip(&results) {
        record_kill(*target, sig, cmdline, reason, result);
    }
    results
}

fn send(target: KillTarget, sig: usize) -> io::Result<()> {
    match target {
        KillTarget::Pid(pid) => kill_pid(pid, sig),
        KillTarget::Group(pgid) => kill_process_group(pgid, sig),
    }
}

/// `Child::kill` (SIGKILL to the child's pid) with the same record.
pub fn kill_child_recorded(child: &mut std::process::Child, reason: &str) -> io::Result<()> {
    let target = KillTarget::Pid(i64::from(child.id()));
    let cmdline = target_cmdline(target);
    let result = child.kill();
    record_kill(target, SIGKILL, &cmdline, reason, &result);
    result
}

/// The audit line for one kill. The command line is the one field the target
/// chose, so it comes last, and every character that could break or hide a
/// line becomes a space: a target can neither end its record early, start
/// another, nor reorder what a reader sees.
fn kill_audit_line(
    target: KillTarget,
    sig: usize,
    cmdline: &str,
    reason: &str,
    result: &io::Result<()>,
) -> String {
    let epoch = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // `args_os`, not `args`: the latter panics on a non-UTF-8 argument, and
    // this runs on cleanup paths that may not.
    let verb = std::env::args_os()
        .nth(1)
        .map(|arg| arg.to_string_lossy().into_owned())
        .unwrap_or_default();
    let signal = match sig {
        SIGKILL => "SIGKILL".to_string(),
        SIGTERM => "SIGTERM".to_string(),
        other => format!("signal {other}"),
    };
    let (kind, id, label) = match target {
        KillTarget::Pid(pid) => ("pid", pid, "cmdline"),
        KillTarget::Group(pgid) => ("pgid", i64::from(pgid), "leader cmdline"),
    };
    let outcome = match result {
        Ok(()) => "sent".to_string(),
        Err(e) => format!("not sent: {e}"),
    };
    format!(
        "{epoch}s td-builder[{} {verb}] {signal} {kind} {id} because {reason}; {outcome}; {label}: {cmdline}",
        std::process::id()
    )
    .chars()
    .map(|c| if hides_or_breaks_a_line(c) { ' ' } else { c })
    .collect()
}

/// A character a record may not carry: a control (every newline is one) or
/// a Unicode format or separator character (bidi overrides and isolates,
/// zero-width joiners and spaces, the line and paragraph separators, the
/// interlinear annotations, the invisible tag block) that a viewer honours
/// without it being a control.
fn hides_or_breaks_a_line(c: char) -> bool {
    c.is_control()
        || matches!(
            c,
            '\u{061C}'
                | '\u{180E}'
                | '\u{200B}'..='\u{200F}'
                | '\u{2028}'..='\u{202E}'
                | '\u{2060}'..='\u{206F}'
                | '\u{FEFF}'
                | '\u{FFF9}'..='\u{FFFB}'
                | '\u{E0000}'..='\u{E007F}'
        )
}

/// The target's command line as `/proc` shows it now, NULs read as spaces and
/// capped, or a note that it could not be read: a process already gone, or a
/// group whose leader's `/proc` entry is unreadable, is still worth a record.
fn target_cmdline(target: KillTarget) -> String {
    use std::io::Read as _;
    const CAP: usize = 200;
    let pid = match target {
        KillTarget::Pid(pid) => pid,
        KillTarget::Group(pgid) => i64::from(pgid),
    };
    // Bounded: a command line may run to ARG_MAX, and the memory-budget arm
    // of a watchdog is the wrong place to read megabytes into the sender.
    let mut bytes = Vec::with_capacity(CAP + 1);
    let read = std::fs::File::open(format!("/proc/{pid}/cmdline"))
        .and_then(|file| file.take(CAP as u64 + 1).read_to_end(&mut bytes));
    if read.is_err() {
        return "<cmdline unreadable>".to_string();
    }
    if bytes.is_empty() {
        return "<no cmdline: zombie or mid-exec>".to_string();
    }
    let text: String = String::from_utf8_lossy(&bytes)
        .chars()
        .map(|c| if c == '\0' { ' ' } else { c })
        .collect();
    let mut capped = text.trim_end().to_string();
    if capped.len() > CAP {
        let mut end = CAP;
        while !capped.is_char_boundary(end) {
            end = end.saturating_sub(1);
        }
        capped.truncate(end);
        capped.push('…');
    }
    capped
}

fn record_kill(target: KillTarget, sig: usize, cmdline: &str, reason: &str, result: &io::Result<()>) {
    use std::io::Write as _;
    let line = kill_audit_line(target, sig, cmdline, reason, result);
    // Not `eprintln!`: that panics when stderr is gone, and this runs on
    // cleanup paths whose stderr may be.
    let _ = writeln!(io::stderr().lock(), "{line}");
    // A test build records into the sink, never the user's audit file: a
    // `cargo test` must not write the host's `~/.td/kill-audit/log`, and a
    // test reads the line a kill produced from the sink. An expression rather
    // than an attribute, because the confinement tests below split this
    // file's shipped part at its first test-only attribute.
    if cfg!(test) {
        if let Ok(mut sink) = kill_audit_sink().lock() {
            sink.push(line);
        }
    } else if let Some(path) = kill_audit_path() {
        append_kill_audit_line(&path, &line);
    }
}

/// Append one line to the audit file, creating it mode 0600 and its directory
/// under an existing `~/.td` (see `ensure_kill_audit_dir`). Every failure is
/// swallowed: see `kill_recorded`.
fn append_kill_audit_line(path: &std::path::Path, line: &str) {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    if !path.parent().is_some_and(create_audit_dir) {
        return;
    }
    let opened = std::fs::OpenOptions::new().append(true).create(true).mode(0o600).open(path);
    if let Ok(mut file) = opened {
        // The line and its newline in one buffer, which for a short record
        // on a regular file is one write(2): `writeln!` on a bare `File`
        // makes two, and two senders appending at once could fuse their
        // records between them.
        let _ = file.write_all(format!("{line}\n").as_bytes());
    }
}

/// The in-process record a test build writes instead of the audit file. Empty
/// and unread in a shipped build.
pub(crate) fn kill_audit_sink() -> &'static std::sync::Mutex<Vec<String>> {
    static SINK: std::sync::OnceLock<std::sync::Mutex<Vec<String>>> = std::sync::OnceLock::new();
    SINK.get_or_init(|| std::sync::Mutex::new(Vec::new()))
}

/// Try to take an exclusive, non-blocking flock on FD. `Ok(true)` = acquired (held
/// until the fd closes); `Ok(false)` = another process holds it; `Err` = real failure.
pub fn flock_try_exclusive(fd: i32) -> io::Result<bool> {
    let ret = unsafe { syscall5(SYS_FLOCK, fd as usize, LOCK_EX | LOCK_NB, 0, 0, 0) };
    if ret == 0 {
        return Ok(true);
    }
    let errno = -ret as i32;
    if errno == EWOULDBLOCK {
        return Ok(false);
    }
    Err(io::Error::from_raw_os_error(errno))
}

/// Take an exclusive flock on FD, blocking until it is available.
pub fn flock_exclusive(fd: i32) -> io::Result<()> {
    check(unsafe { syscall5(SYS_FLOCK, fd as usize, LOCK_EX, 0, 0, 0) })
}

/// fork(2): returns the child PID in the parent and 0 in the child. The
/// host-sandbox forks AFTER unshare(CLONE_NEWUSER|CLONE_NEWPID) so the child is
/// PID 1 of the fresh PID namespace (the namespace's first process), which then
/// mounts a private /proc reflecting that namespace — matching `guix shell -C`'s
/// child-is-pid1 model so nested containers can create their own PID ns + /proc.
pub fn fork() -> io::Result<i64> {
    let ret = unsafe { syscall5(SYS_FORK, 0, 0, 0, 0, 0) };
    if ret < 0 {
        Err(io::Error::from_raw_os_error(-ret as i32))
    } else {
        Ok(ret as i64)
    }
}

/// wait4(2) on a specific PID with no options and no rusage; returns the raw
/// wait status the kernel fills, decoded by the caller (WIFEXITED/WEXITSTATUS:
/// `status & 0x7f == 0` means exited with `(status >> 8) & 0xff`).
pub fn waitpid(pid: i64) -> io::Result<i32> {
    let mut status: i32 = 0;
    let ret = unsafe {
        syscall5(SYS_WAIT4, pid as usize, &mut status as *mut i32 as usize, 0, 0, 0)
    };
    if ret < 0 {
        Err(io::Error::from_raw_os_error(-ret as i32))
    } else {
        Ok(status)
    }
}

const SYS_WAITID: usize = 247;
const P_PID: usize = 1;
const WEXITED: usize = 0x0000_0004;
const WNOWAIT: usize = 0x0100_0000;

/// waitid(2) with WNOWAIT: block until `pid` has TERMINATED — exited or been
/// killed, the second being the case this exists for — but leave it waitable so
/// the pid stays allocated until something reaps it. A process-group id IS its
/// leader's pid, so a REAPING wait frees that number as it returns, and a
/// `kill(-pgid, …)` racing just behind one can reach whatever group the kernel
/// handed the number to next. Measured: with the leader a zombie,
/// `kill(-pgid, 0)` returns 0; after the reap, ESRCH.
///
/// `infop` is NULL, which the kernel accepts and which skips the siginfo_t copy
/// entirely — so unlike every other pointer-taking call in this file there is no
/// buffer whose length could be got wrong. WEXITED alone (no WSTOPPED, no
/// WCONTINUED) is what keeps a stop or a continue from returning here; the
/// caller reaps with `Child::wait` and takes the status from there.
pub fn wait_exited_no_reap(pid: u32) -> io::Result<()> {
    loop {
        let ret = unsafe { syscall5(SYS_WAITID, P_PID, pid as usize, 0, WEXITED | WNOWAIT, 0) };
        if ret >= 0 {
            return Ok(());
        }
        let errno = -ret as i32;
        if errno != EINTR {
            return Err(io::Error::from_raw_os_error(errno));
        }
    }
}

/// exit_group(2): terminate the whole process immediately with `code`. The
/// host-sandbox's PID-namespace PARENT uses this to propagate its PID-1 child's
/// exit status WITHOUT returning into std's post-fork exec path — there must be
/// exactly one exec (the PID-1 child's), and no second sync-pipe write.
/// setpriority(2) on the calling process (`which=PRIO_PROCESS, who=0`). `prio` is
/// the absolute nice value (-20..=19); larger = lower scheduling priority. An
/// unprivileged caller may only RAISE niceness — trying to lower it fails with
/// EPERM/EACCES, which callers treat as "already nice enough". Scheduling-only:
/// build OUTPUT is unaffected, so reproducibility is intact.
pub fn set_self_priority(prio: i32) -> io::Result<()> {
    check(unsafe { syscall5(SYS_SETPRIORITY, PRIO_PROCESS, 0, prio as isize as usize, 0, 0) })
}

/// getpriority(2) for the calling process, as the nice value (-20..=19). The raw
/// syscall returns `20 - nice` to keep the success range non-negative (a real
/// error is the usual `-errno`); we undo that bias.
pub fn get_self_priority() -> io::Result<i32> {
    let ret = unsafe { syscall5(SYS_GETPRIORITY, PRIO_PROCESS, 0, 0, 0, 0) };
    if ret < 0 {
        Err(io::Error::from_raw_os_error(-ret as i32))
    } else {
        Ok(20 - ret as i32)
    }
}

/// prlimit64(2) on the calling process (`pid=0`) for `resource`, reading the
/// current (soft, hard) pair. Always available (the modern resource-limit
/// syscall), so no setrlimit/getrlimit split.
pub fn get_rlimit(resource: usize) -> io::Result<(u64, u64)> {
    let mut old: [u64; 2] = [0, 0];
    check(unsafe {
        syscall5(SYS_PRLIMIT64, 0, resource, 0, old.as_mut_ptr() as usize, 0)
    })?;
    Ok((old[0], old[1]))
}

/// prlimit64(2): set the calling process's (soft, hard) limit for `resource`.
/// An unprivileged caller may LOWER the hard limit (and set soft ≤ hard) but
/// not raise the hard limit — callers cap, so that direction is the norm. The
/// limit is inherited across fork(2) and execve(2), so setting it on the build
/// child before it forks the PID-1 builder caps the whole build tree.
/// Purely a resource ceiling — build OUTPUT is unaffected, so reproducibility
/// is intact (a build over the cap FAILS, it does not produce different bytes).
pub fn set_rlimit(resource: usize, soft: u64, hard: u64) -> io::Result<()> {
    let new: [u64; 2] = [soft, hard];
    check(unsafe {
        syscall5(SYS_PRLIMIT64, 0, resource, new.as_ptr() as usize, 0, 0)
    })
}

/// mmap(2) a private anonymous read/write region of `len` bytes. Returns the
/// raw syscall result: a valid address (≥ 0 as isize) on success, or `-errno`
/// (e.g. `-ENOMEM` when an rlimit blocks the mapping) on failure. Offset is
/// passed as 0 (x86_64 `sys_mmap` EINVALs a non-page-aligned offset even for an
/// anonymous mapping, so it cannot be left as register garbage — syscall6).
/// Async-signal-safe (no allocator), which is why the rlimit behavioral test
/// probes the cap with this rather than a heap allocation in a forked child.
pub fn mmap_anon(len: usize) -> isize {
    // fd = -1 (usize::MAX) for an anonymous mapping; offset 0.
    unsafe { syscall6(SYS_MMAP, 0, len, PROT_READ_WRITE, MAP_PRIVATE_ANON, usize::MAX, 0) }
}

pub fn exit_group(code: i32) -> ! {
    unsafe {
        syscall5(SYS_EXIT_GROUP, code as usize, 0, 0, 0, 0);
    }
    // exit_group never returns; satisfy the ! type if the kernel ever did.
    loop {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::ffi::OsStrExt;

    #[test]
    fn rename_noreplace_never_replaces_an_existing_directory() {
        let root = std::env::temp_dir().join(format!(
            "td-builder-rename-noreplace-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir(&root).unwrap();
        let old = root.join("old");
        let new = root.join("new");
        std::fs::create_dir(&old).unwrap();
        std::fs::create_dir(&new).unwrap();
        let old_c = std::ffi::CString::new(old.as_os_str().as_bytes()).unwrap();
        let new_c = std::ffi::CString::new(new.as_os_str().as_bytes()).unwrap();

        let error = rename_noreplace(&old_c, &new_c).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert!(old.is_dir());
        assert!(new.is_dir());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rename_noreplace_contract_is_value_and_caller_pinned() {
        assert_eq!(SYS_RENAMEAT2, 316);
        assert_eq!(AT_FDCWD, -100);
        assert_eq!(RENAME_NOREPLACE, 1);
        let shipped_sys = shipped_part(include_str!("sys.rs"));
        assert_eq!(shipped_sys.matches("SYS_RENAMEAT2").count(), 2);
        assert_eq!(shipped_sys.matches("RENAME_NOREPLACE").count(), 2);
        assert_eq!(shipped_sys.matches("pub fn rename_noreplace(").count(), 1);
        let compact: String = shipped_sys
            .chars()
            .filter(|character| !character.is_whitespace())
            .collect();
        assert!(compact.contains(
            "syscall5(SYS_RENAMEAT2,AT_FDCWDasusize,old.as_ptr()asusize,AT_FDCWDasusize,new.as_ptr()asusize,RENAME_NOREPLACE,)"
        ));

        fn count_production_calls(path: &std::path::Path) -> usize {
            let mut count = 0usize;
            for entry in std::fs::read_dir(path).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    count += count_production_calls(&path);
                } else if path.extension().and_then(|value| value.to_str()) == Some("rs") {
                    let source = std::fs::read_to_string(path).unwrap();
                    count += shipped_part(&source).matches("rename_noreplace").count();
                }
            }
            count
        }
        let source_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        assert_eq!(count_production_calls(&source_root), 2);
    }

    #[test]
    fn getuid_matches_proc_status() {
        // Cross-check the raw syscall against the kernel's own report.
        let status = std::fs::read_to_string("/proc/self/status").unwrap();
        let uid_line = status.lines().find(|l| l.starts_with("Uid:")).unwrap();
        let real: u32 = uid_line.split_whitespace().nth(1).unwrap().parse().unwrap();
        assert_eq!(getuid(), real);
    }

    #[test]
    fn bad_mount_reports_errno() {
        // Mounting onto a path that does not exist must surface ENOENT, not
        // a bogus success — proves the -errno decoding.
        let target = std::ffi::CString::new("/no/such/td-builder/mount/point").unwrap();
        let err = mount(None, &target, None, MS_REC | MS_PRIVATE, None).unwrap_err();
        assert_eq!(err.raw_os_error(), Some(2 /* ENOENT */));
    }

    #[test]
    fn set_rlimit_data_round_trips() {
        // Lower the SOFT data limit to a finite value below the (typically
        // infinite) hard limit and read it back. Proves prlimit64 set+get are
        // real syscalls, not stubs — a no-op would leave the original soft
        // limit, which we assert against. RLIMIT_DATA is per-PROCESS and the
        // cargo-test harness runs every #[test] in this one process, so pick
        // a target far above any test's heap footprint (a 1 GiB target once
        // aborted the xz real-tarball decode running on a sibling thread) and
        // restore the original soft limit afterwards.
        let (orig_soft, hard) = get_rlimit(RLIMIT_DATA).expect("getrlimit");
        let ceiling = if hard == RLIM_INFINITY {
            1u64 << 36 // 64 GiB — finite, but never binding for a test
        } else {
            hard.min(1u64 << 36)
        };
        // Strictly BELOW the current soft limit, or the readback below stops
        // discriminating: run-capped (builder/src/run_capped.rs) now sets soft
        // AND hard to this crate's ceiling before exec, so `orig_soft == hard`
        // and a `set_rlimit` stubbed to `Ok(())` would satisfy
        // `soft_after == target` without issuing a syscall at all. One page
        // below keeps the limit as non-binding as it was.
        let target = ceiling.min(orig_soft.saturating_sub(4096));
        assert!(
            target < orig_soft,
            "the target must sit below the inherited soft limit or a no-op passes"
        );
        set_rlimit(RLIMIT_DATA, target, hard).expect("lowering the soft data limit must succeed");
        let (soft_after, hard_after) = get_rlimit(RLIMIT_DATA).expect("getrlimit");
        assert_eq!(soft_after, target, "soft data limit should be exactly the set value");
        assert_eq!(hard_after, hard, "hard limit must be unchanged");
        set_rlimit(RLIMIT_DATA, orig_soft, hard)
            .expect("restoring the soft data limit must succeed");
    }

    #[test]
    fn rlimit_data_caps_anon_mmap() {
        // The cap is load-bearing: a child with a small RLIMIT_DATA cannot map a
        // large private-anon region, while an uncapped child can. Done in forked
        // children so the limit and the (async-signal-safe) mmap stay isolated
        // from the test harness — no allocator runs in the child, so there is no
        // fork-in-a-threaded-program malloc hazard.
        const BIG: usize = 256 * 1024 * 1024; // 256 MiB
        let run = |cap: Option<u64>| -> i32 {
            let pid = fork().expect("fork");
            if pid == 0 {
                if let Some(c) = cap {
                    // Cap both soft and hard well below BIG; ignore the result —
                    // failing to cap would just make the assertion below catch it.
                    let _ = set_rlimit(RLIMIT_DATA, c, c);
                }
                let r = mmap_anon(BIG);
                // exit 0 = mapping SUCCEEDED, 1 = mapping FAILED (capped).
                exit_group(if r >= 0 { 0 } else { 1 });
            }
            let status = waitpid(pid).expect("waitpid");
            assert_eq!(status & 0x7f, 0, "child should exit normally, not be signalled");
            (status >> 8) & 0xff
        };
        // "Uncapped" means only that THIS test sets no cap; the child still
        // inherits whatever the process has. run-capped
        // (builder/src/run_capped.rs) now gives the test binary a finite
        // ceiling, and `TD_RUN_CAPPED_MIB` can lower it further — below BIG,
        // this half would red about a property it is not testing. Skip it
        // there rather than report a misleading failure; the capped half below
        // is the assertion that carries the claim.
        let (ambient_soft, _) = get_rlimit(RLIMIT_DATA).expect("getrlimit");
        let headroom = BIG as u64 + 64 * 1024 * 1024;
        if ambient_soft >= headroom {
            assert_eq!(run(None), 0, "an uncapped child must be able to map {BIG} bytes");
        }
        assert_eq!(
            run(Some(32 * 1024 * 1024)),
            1,
            "a child capped at 32 MiB RLIMIT_DATA must fail to map {BIG} bytes"
        );
    }

    #[test]
    fn a_liveness_pipe_reports_eof_only_once_every_writer_is_gone() {
        // The sandbox reads this ONE bit to decide whether its parent outlived
        // the fork, so both answers have to be real: an open write end must not
        // read as death (that would abort every healthy sandbox), and a closed
        // one must not read as life (that is the orphan this exists to catch).
        let (r, w) = pipe_liveness().expect("pipe2");
        assert!(pipe_peer_open(r).expect("read"), "an open write end must read as alive");
        close(w).expect("close");
        assert!(!pipe_peer_open(r).expect("read"), "the last writer closing must read as EOF");
        // EOF is level-triggered, so asking twice must not consume the answer.
        assert!(!pipe_peer_open(r).expect("read"), "EOF must stay EOF on a second ask");
        // The THIRD answer, and the dangerous one: an unreadable channel is not
        // a dead parent. Folding every errno into `Ok(false)` would satisfy both
        // asserts above while making pid 1 kill every sandbox it cannot ask.
        assert_eq!(
            pipe_peer_open(i32::MAX).unwrap_err().raw_os_error(),
            Some(9 /* EBADF */),
            "an unreadable channel must report its errno, not read as a dead parent"
        );
        close(r).expect("close");
        // Never a descriptor that was once live: this harness runs tests in
        // parallel threads of one process, so a freed fd number can be handed to
        // another thread between the two calls — and closing it would break that
        // test rather than this one.
        assert_eq!(
            close(i32::MAX).unwrap_err().raw_os_error(),
            Some(9 /* EBADF */),
            "close must reach the kernel — a stub returning Ok would pass the asserts above"
        );
    }

    #[test]
    fn a_no_reap_wait_leaves_the_pid_reserved_for_a_later_reap() {
        // The gate watchdog signals a process GROUP by its leader's pid, so the
        // wait that decides it may stop must not release that number. Both
        // halves are asserted: the wait returns only once the child has exited,
        // and the child is still there to reap afterwards.
        let pid = fork().expect("fork");
        if pid == 0 {
            // Alive for long enough that a stub which returned without issuing
            // anything would find the child still running, and read R/S below
            // rather than Z. Without the pause a stub passes whenever the child
            // wins the race to exit, which it usually does.
            std::thread::sleep(std::time::Duration::from_millis(100));
            exit_group(7);
        }
        wait_exited_no_reap(pid as u32).expect("waitid(WNOWAIT)");
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
        // Field 3 is the state, and comm (field 2) can itself hold ')' and
        // spaces, so read from the LAST one.
        let after_comm = stat.rsplit(')').next().unwrap_or("");
        assert_eq!(
            after_comm.split_whitespace().next(),
            Some("Z"),
            "the child must have exited and still hold its pid: {stat}"
        );
        let status = waitpid(pid).expect("the pid must still be reapable");
        assert_eq!(status & 0x7f, 0, "child should exit normally, not be signalled");
        assert_eq!((status >> 8) & 0xff, 7, "the exit code must survive the no-reap wait");
        // The control: the REAP is what frees the pid, which is what makes
        // waiting without one worth doing.
        assert_eq!(
            waitpid(pid).unwrap_err().raw_os_error(),
            Some(10 /* ECHILD */),
            "a second wait must find nothing — the reap released it"
        );
    }

    #[test]
    fn set_self_priority_raises_niceness() {
        // Raising niceness is always permitted for the calling user, so a +2 bump
        // must actually move the value the kernel reports. Proves both syscalls
        // (the round-trip would pass on a no-op stub only if `before == after`,
        // which the +2 request rules out unless we were already pinned at 19).
        let before = get_self_priority().expect("getpriority");
        let target = (before + 2).min(19);
        set_self_priority(target).expect("raising niceness must succeed");
        let after = get_self_priority().expect("getpriority");
        assert_eq!(after, target, "niceness should be exactly the raised target");
    }

    /// The shipped part of a source file: everything before the first line
    /// that IS a test-only attribute. A mention of that attribute in a
    /// comment does not end the shipped part, so prose cannot shorten a
    /// pin's scan.
    fn shipped_part(source: &str) -> &str {
        let mut offset = 0;
        for line in source.split_inclusive('\n') {
            if line.trim_start().starts_with("#[cfg(test)]") {
                break;
            }
            offset += line.len();
        }
        &source[..offset]
    }

    fn sleeper_command() -> std::process::Command {
        let mut command = std::process::Command::new("sleep");
        command.arg("300").stdin(std::process::Stdio::null());
        command
    }

    /// Wait until `child` has exec'd, so its cmdline is its own rather than
    /// this test binary's, which is what `/proc` shows between fork and exec.
    /// Bounded, and the child does not outlive a failed wait.
    fn await_exec(mut child: std::process::Child) -> std::process::Child {
        for _ in 0..500 {
            let cmdline = std::fs::read(format!("/proc/{}/cmdline", child.id())).unwrap_or_default();
            if cmdline.starts_with(b"sleep\0300\0") {
                return child;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let _ = kill_child_recorded(&mut child, "test cleanup: the sleeper never exec'd");
        let _ = child.wait();
        panic!("the sleeper never exec'd");
    }

    fn spawn_sleeper() -> std::process::Child {
        await_exec(sleeper_command().spawn().unwrap())
    }

    /// A sleeper leading its own process group, so a group kill reaches it
    /// and nothing else.
    fn spawn_group_sleeper() -> std::process::Child {
        use std::os::unix::process::CommandExt as _;
        await_exec(sleeper_command().process_group(0).spawn().unwrap())
    }

    /// The recorded line carrying `token`, found among the records other
    /// tests in this binary write concurrently.
    fn recorded_line(token: &str) -> String {
        let lines = kill_audit_sink().lock().unwrap().clone();
        lines
            .iter()
            .find(|line| line.contains(token))
            .cloned()
            .unwrap_or_else(|| panic!("no record containing {token:?} among {lines:?}"))
    }

    fn signal_of(status: &std::process::ExitStatus) -> Option<i32> {
        std::os::unix::process::ExitStatusExt::signal(status)
    }

    /// A pid no process can hold: one past `pid_max`. Signalling it is safe
    /// and fails with ESRCH, unlike re-signalling a reaped pid the kernel may
    /// already have handed to a bystander.
    fn impossible_pid() -> i64 {
        let pid_max: i64 = std::fs::read_to_string("/proc/sys/kernel/pid_max")
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        pid_max + 1
    }

    /// One recorded kill: the signal lands, and the line names the time, this
    /// process, the signal, the target, the reason, the outcome and the
    /// target's command line, in that order.
    #[test]
    fn a_recorded_kill_signals_and_leaves_one_named_line() {
        let mut child = spawn_sleeper();
        let pid = i64::from(child.id());
        let reason = format!("test: proving the record for sleeper {pid}");
        kill_recorded(KillTarget::Pid(pid), SIGTERM, &reason).unwrap();
        let status = child.wait().unwrap();
        assert_eq!(
            signal_of(&status),
            Some(SIGTERM as i32),
            "the sleeper must die of the recorded signal"
        );
        let line = recorded_line(&reason);
        assert!(
            line.ends_with(&format!(" SIGTERM pid {pid} because {reason}; sent; cmdline: sleep 300")),
            "{line}"
        );
        let verb = std::env::args_os()
            .nth(1)
            .map(|arg| arg.to_string_lossy().into_owned())
            .unwrap_or_default();
        assert!(
            line.contains(&format!(" td-builder[{} {verb}] ", std::process::id())),
            "{line}"
        );
        let epoch: u64 = line.split('s').next().unwrap().parse().unwrap();
        assert!(epoch > 1_700_000_000, "{line}");

        // A target that is not there still leaves a line, and says the signal
        // was not accepted rather than pretending it was.
        let nobody = impossible_pid();
        let reason = format!("test: nobody holds pid {nobody}");
        assert!(kill_recorded(KillTarget::Pid(nobody), SIGTERM, &reason).is_err());
        let line = recorded_line(&reason);
        assert!(
            line.contains("; not sent: ") && line.ends_with("; cmdline: <cmdline unreadable>"),
            "{line}"
        );
    }

    /// A group target signals the whole group, and the line says whose
    /// command line it shows.
    #[test]
    fn a_recorded_group_kill_names_the_group_and_its_leader() {
        let mut child = spawn_group_sleeper();
        let pgid = child.id();
        let reason = format!("test: proving the group record for {pgid}");
        kill_recorded(KillTarget::Group(pgid), SIGTERM, &reason).unwrap();
        let status = child.wait().unwrap();
        assert_eq!(signal_of(&status), Some(SIGTERM as i32));
        let line = recorded_line(&reason);
        assert!(
            line.ends_with(&format!(
                " SIGTERM pgid {pgid} because {reason}; sent; leader cmdline: sleep 300"
            )),
            "{line}"
        );
    }

    /// A kill fanned out over several targets records every member, in the
    /// order the signals went, each with its own reason.
    #[test]
    fn a_fanned_out_kill_records_every_member_in_order() {
        let mut leader = spawn_group_sleeper();
        let mut lone = spawn_sleeper();
        let token = format!("test: fanning out over {} and {}", leader.id(), lone.id());
        let kills = vec![
            (KillTarget::Group(leader.id()), token.clone()),
            (KillTarget::Pid(i64::from(lone.id())), format!("{token}; descendant")),
        ];
        let results = kill_all_recorded(&kills, SIGKILL);
        assert!(results.iter().all(Result::is_ok), "{results:?}");
        assert_eq!(signal_of(&leader.wait().unwrap()), Some(SIGKILL as i32));
        assert_eq!(signal_of(&lone.wait().unwrap()), Some(SIGKILL as i32));
        let lines = kill_audit_sink().lock().unwrap().clone();
        let members: Vec<&String> = lines.iter().filter(|line| line.contains(&token)).collect();
        assert_eq!(members.len(), 2, "{members:?}");
        assert!(
            members[0].ends_with(&format!(
                " SIGKILL pgid {} because {token}; sent; leader cmdline: sleep 300",
                leader.id()
            )),
            "{members:?}"
        );
        assert!(
            members[1].ends_with(&format!(
                " SIGKILL pid {} because {token}; descendant; sent; cmdline: sleep 300",
                lone.id()
            )),
            "{members:?}"
        );
    }

    /// `Child::kill` goes through the same record, as SIGKILL to the pid.
    #[test]
    fn a_recorded_child_kill_is_the_same_line() {
        let mut child = spawn_sleeper();
        let pid = i64::from(child.id());
        let reason = format!("test: the child helper for sleeper {pid}");
        kill_child_recorded(&mut child, &reason).unwrap();
        let status = child.wait().unwrap();
        assert_eq!(signal_of(&status), Some(SIGKILL as i32));
        let line = recorded_line(&reason);
        assert!(
            line.ends_with(&format!(" SIGKILL pid {pid} because {reason}; sent; cmdline: sleep 300")),
            "{line}"
        );
    }

    /// The target chose its command line; the line does not let it choose
    /// where its record ends or what a reader takes for the next one.
    #[test]
    fn a_target_cannot_forge_a_record_through_its_command_line() {
        let forged = "sleep 300\n0s td-builder[1 x] SIGKILL pid 1 because forged; sent; \
                      cmdline: \u{202E}sh\x1b[2K\t\u{2028}";
        let line = kill_audit_line(KillTarget::Pid(7), SIGKILL, forged, "why", &Ok(()));
        assert_eq!(line.lines().count(), 1, "{line:?}");
        assert!(!line.chars().any(hides_or_breaks_a_line), "{line:?}");
        let (head, tail) = line.split_once("; cmdline: ").unwrap();
        assert!(head.ends_with(" SIGKILL pid 7 because why; sent"), "{head}");
        assert_eq!(
            tail,
            "sleep 300 0s td-builder[1 x] SIGKILL pid 1 because forged; sent; cmdline:  sh [2K  "
        );
    }

    /// A target outside `pid_t`, or one that would name the sender's own
    /// group or everything, reaches no one and is recorded as not sent.
    #[test]
    fn a_target_outside_pid_range_is_refused_and_recorded() {
        for (target, what) in [
            (KillTarget::Pid(0), "pid 0"),
            (KillTarget::Pid(-1), "pid -1"),
            (KillTarget::Pid(i64::from(i32::MAX) + 1), "a pid past pid_t"),
            (KillTarget::Group(0), "pgid 0"),
            (KillTarget::Group(u32::MAX), "a pgid past pid_t"),
        ] {
            // Signal 0 checks and kills nothing, so a guard that failed here
            // could not take this test binary's own group with it.
            let reason = format!("test: refusing {what}");
            let sent = kill_recorded(target, 0, &reason);
            assert_eq!(sent.map_err(|e| e.kind()), Err(io::ErrorKind::InvalidInput), "{what}");
            assert!(recorded_line(&reason).contains("; not sent: "), "{what}");
        }
    }

    /// The file sits in its own directory under `~/.td`, which td-builder
    /// creates but never `~/.td` itself, so a home without one (a build
    /// sandbox's) gets no file. Created private, appended to, never
    /// truncated.
    #[test]
    fn the_audit_file_is_created_only_under_an_existing_td_dir() {
        use std::os::unix::fs::PermissionsExt as _;
        if let Some(home) = std::env::var_os("HOME").filter(|home| !home.is_empty()) {
            let dir = kill_audit_dir().unwrap();
            assert_eq!(dir, std::path::PathBuf::from(home).join(".td/kill-audit"));
            assert_eq!(kill_audit_path().unwrap(), dir.join("log"));
        }
        let root = std::env::temp_dir()
            .join(format!("td-kill-audit-file-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir(&root).unwrap();
        let file = root.join(".td/kill-audit/log");
        append_kill_audit_line(&file, "lost");
        assert!(!root.join(".td").exists(), "no `.td` may be created for a record");
        std::fs::create_dir(root.join(".td")).unwrap();
        append_kill_audit_line(&file, "first");
        append_kill_audit_line(&file, "second");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "first\nsecond\n");
        let dir_mode = std::fs::metadata(root.join(".td/kill-audit")).unwrap().permissions().mode();
        assert_eq!(dir_mode & 0o777, 0o700);
        let file_mode = std::fs::metadata(&file).unwrap().permissions().mode();
        assert_eq!(file_mode & 0o777, 0o600);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A call of a `kill` method or associated function on this line: `.kill`
    /// or `::kill` followed, after any whitespace, by its parenthesis, so
    /// neither a reformatting nor the `Child::kill(&mut c)` form slips past.
    fn calls_kill(line: &str) -> bool {
        for needle in [".kill", "::kill"] {
            let mut rest = line;
            while let Some(at) = rest.find(needle) {
                rest = &rest[at + needle.len()..];
                if rest.trim_start().starts_with('(') {
                    return true;
                }
            }
        }
        false
    }

    /// The raw `kill(2)` wrappers are private, so the compiler already refuses
    /// an unrecorded signal from another module; `Child::kill` is `std`'s and
    /// needs this pin. Every `kill(` call in the crate, in any Rust file under
    /// its manifest directory, is the one inside `kill_child_recorded`.
    #[test]
    fn every_kill_in_the_crate_is_recorded() {
        fn child_kills(path: &std::path::Path) -> Vec<String> {
            let mut out = Vec::new();
            for entry in std::fs::read_dir(path).unwrap() {
                let path = entry.unwrap().path();
                let name = path.file_name().and_then(|value| value.to_str());
                if path.is_dir() {
                    if name != Some("target") {
                        out.extend(child_kills(&path));
                    }
                } else if path.extension().and_then(|value| value.to_str()) == Some("rs")
                    && name != Some("sys.rs")
                {
                    let source = std::fs::read_to_string(&path).unwrap();
                    for (index, line) in source.lines().enumerate() {
                        if calls_kill(line) {
                            out.push(format!("{}:{}: {}", path.display(), index + 1, line.trim()));
                        }
                    }
                }
            }
            out
        }
        assert!(calls_kill("let _ = child.kill();"));
        assert!(calls_kill("child.kill ()"));
        assert!(calls_kill("std::process::Child::kill(&mut child)"));
        assert!(!calls_kill("kill_child_recorded(&mut child, why)"));
        assert!(!calls_kill("crate::sys::kill_recorded(target, SIGKILL, why)"));
        assert_eq!(
            shipped_part("a\n// mentions #[cfg(test)] in prose\nb\n#[cfg(test)]\nmod tests {}\n"),
            "a\n// mentions #[cfg(test)] in prose\nb\n"
        );
        let crate_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let stray = child_kills(crate_root);
        assert!(stray.is_empty(), "unrecorded Child::kill calls: {stray:#?}");
        let shipped_sys = shipped_part(include_str!("sys.rs"));
        // The split is at the attribute, not at this file's mention of it:
        // the shipped part reaches this file's last shipped items.
        assert!(shipped_sys.contains("fn kill_audit_sink(") && shipped_sys.contains("fn exit_group("));
        assert!(!shipped_sys.contains("mod tests"));
        assert_eq!(shipped_sys.lines().filter(|line| calls_kill(line)).count(), 1);
        assert_eq!(shipped_sys.matches("fn kill_pid(").count(), 1);
        assert_eq!(shipped_sys.matches("fn kill_process_group(").count(), 1);
        assert!(!shipped_sys.contains("pub fn kill_pid("));
        assert!(!shipped_sys.contains("pub fn kill_process_group("));
    }
}

//! Minimal raw x86_64-linux syscall layer: unshare(2), mount(2), getuid(2),
//! getgid(2) — exactly what the build sandbox needs. Hand-rolled to keep the
//! crate zero-dependency (precedent is the
//! hand-rolled SHA-256; the rung's differential proves behavior, and the drv
//! platform field is checked to be x86_64-linux before any of this runs).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unreachable, clippy::todo, clippy::unimplemented, clippy::indexing_slicing)] // grandfathered: pre-dates the rust-lint rules (AGENTS.md); remove when cleaned
#![allow(unsafe_code)] // confined raw-syscall / low-level layer (AGENTS.md)

#[cfg(not(all(target_arch = "x86_64", target_os = "linux")))]
compile_error!("td-builder's sandbox is x86_64-linux only (the pinned platform)");

use std::ffi::CStr;
use std::io;

const SYS_CLOSE: usize = 3;
const SYS_IOCTL: usize = 16;
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

pub const CLONE_NEWNS: usize = 0x0002_0000;
pub const CLONE_NEWUTS: usize = 0x0400_0000;
pub const CLONE_NEWIPC: usize = 0x0800_0000;
pub const CLONE_NEWUSER: usize = 0x1000_0000;
pub const CLONE_NEWPID: usize = 0x2000_0000;
pub const CLONE_NEWNET: usize = 0x4000_0000;

pub const MS_RDONLY: usize = 0x1;
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

pub fn getgid() -> u32 {
    unsafe { syscall5(SYS_GETGID, 0, 0, 0, 0, 0) as u32 }
}

/// flock(2) LOCK_EX|LOCK_NB — the gate runner's machine-wide slot pool (gates.rs).
/// A slot is an exclusively-flocked file; the kernel releases the lock when the
/// holding process exits (even on SIGKILL), so a crashed gate can never leak a slot
/// — the property that makes the cross-agent pool safe without a reaper.
const SYS_FLOCK: usize = 73;
const LOCK_EX: usize = 2;
const LOCK_NB: usize = 4;
const EWOULDBLOCK: i32 = 11;

/// kill(2) to a whole PROCESS GROUP (negative pid) — the gate-tree memory
/// watchdog's enforcement (gates.rs): SIGKILL the gate's process group when its
/// aggregate RSS crosses the tree budget.
const SYS_KILL: usize = 62;

pub fn kill_process_group(pgid: u32, sig: usize) -> io::Result<()> {
    let neg = -(i64::from(pgid));
    check(unsafe { syscall5(SYS_KILL, neg as usize, sig, 0, 0, 0) })
}

/// kill(2) to a single PID (positive) — the sandbox-hardening gate SIGTERMs the
/// top td-builder (proving PR_SET_PDEATHSIG reaps the inner tree) and SIGKILLs
/// any stray marker process left behind on a failure path. Distinct from
/// `kill_process_group`, which targets a whole group via a negative pid.
pub fn kill_pid(pid: i64, sig: usize) -> io::Result<()> {
    check(unsafe { syscall5(SYS_KILL, pid as usize, sig, 0, 0, 0) })
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
        // limit, which we assert against. Lowering only the soft limit and only
        // for THIS test process is harmless: RLIMIT_DATA blocks future heap
        // GROWTH, and 1 GiB is far above the test's footprint.
        let (_, hard) = get_rlimit(RLIMIT_DATA).expect("getrlimit");
        let target = 1u64 << 30; // 1 GiB, comfortably under the hard limit
        assert!(hard == RLIM_INFINITY || hard >= target, "hard limit too low for the test");
        set_rlimit(RLIMIT_DATA, target, hard).expect("lowering the soft data limit must succeed");
        let (soft_after, hard_after) = get_rlimit(RLIMIT_DATA).expect("getrlimit");
        assert_eq!(soft_after, target, "soft data limit should be exactly the set value");
        assert_eq!(hard_after, hard, "hard limit must be unchanged");
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
        assert_eq!(run(None), 0, "an uncapped child must be able to map {BIG} bytes");
        assert_eq!(
            run(Some(32 * 1024 * 1024)),
            1,
            "a child capped at 32 MiB RLIMIT_DATA must fail to map {BIG} bytes"
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
}

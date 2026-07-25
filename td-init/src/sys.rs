//! The confined raw-syscall layer — the whole `unsafe` surface of this crate.
//!
//! The crate root `#![deny(unsafe_code)]`s and exactly one item here carries a
//! scoped `#[allow]`: `syscall5`, the `syscall`-instruction body copied verbatim
//! from `builder/src/sys.rs`/`td-kexec`. Everything else in the crate — and every
//! other function in this module — is ordinary safe Rust. This is the THIRD
//! target-side unsafe exception AGENTS.md records, after td-kexec (two syscalls)
//! and td-netd (one ioctl).
//!
//! The crate-level `deny` catches an added `unsafe`, but it is not the whole
//! guarantee: re-leveling that lint is not itself `unsafe`, so a module
//! `#![warn(...)]` demotes it and `global_asm!` — which needs no `unsafe` block
//! — then compiles. `main.rs`'s confinement tests are what close that, and they
//! are the durable enforcement here.
//!
//! The amended surface is exactly the eight syscalls below, one per boot-glue
//! applet requirement that safe `std` does not expose. A NINTH is a reviewed
//! amendment, not an edit; `main.rs`'s confinement test asserts the roster.
//! Notably absent: `pivot_root(2)` (it fails on the initramfs rootfs, so
//! switch_root moves the mount instead, as util-linux and busybox do),
//! `fork`/`execve` (`Command` plus the SAFE `CommandExt::exec` cover both), and
//! `dup2` (`Stdio::from(File)` makes exec wire the console onto 0/1/2).

use std::ffi::CStr;
use std::io;
use std::os::fd::RawFd;
use std::ptr;

#[cfg(not(all(target_arch = "x86_64", target_os = "linux")))]
compile_error!("td-init is x86_64-linux only (raw syscall ABI)");

// The amended eight (x86_64 syscall numbers).
const SYS_IOCTL: usize = 16;
const SYS_WAIT4: usize = 61;
const SYS_SETSID: usize = 112;
const SYS_CHROOT: usize = 161;
const SYS_SYNC: usize = 162;
const SYS_MOUNT: usize = 165;
const SYS_REBOOT: usize = 169;
const SYS_SETHOSTNAME: usize = 170;

/// The single raw-syscall entry point (x86_64 SysV syscall ABI), copied from
/// `builder/src/sys.rs`. Its body is the ONLY `unsafe` in the crate. The scoped
/// `#[allow]` under the crate `#![deny(unsafe_code)]` covers where `unsafe` may
/// appear, not what may be passed here — this fn is safe to CALL, so its
/// confinement is module privacy plus the eight typed wrappers below being its
/// only callers. Syscalls taking fewer than five arguments pass 0 in the unused
/// registers, which the kernel ignores.
#[inline]
#[allow(unsafe_code)]
fn syscall5(n: usize, a1: usize, a2: usize, a3: usize, a4: usize, a5: usize) -> isize {
    let ret: isize;
    // SAFETY: the `syscall` instruction clobbers rcx/r11 and returns in rax;
    // the args are plain integers or a pointer-as-usize whose pointee the caller
    // keeps live across the call. No memory is aliased beyond the kernel's read
    // (a NUL-terminated path, a hostname byte range) or write (one `i32` status).
    // `options(nomem)` is deliberately ABSENT, and load-bearing by its absence:
    // wait4's status is written by the kernel through one of those pointers, and
    // promising the compiler this asm touches no memory would let it keep a
    // stale `status` across the call.
    unsafe {
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
            options(nostack),
        );
    }
    ret
}

/// Turn a raw syscall return into a `Result`, mirroring `sys.rs::check`.
fn check(ret: isize) -> io::Result<isize> {
    if ret < 0 {
        Err(io::Error::from_raw_os_error(-ret as i32))
    } else {
        Ok(ret)
    }
}

// ── reboot(2) ───────────────────────────────────────────────────────────────

// linux/reboot.h.
const LINUX_REBOOT_MAGIC1: usize = 0xfee1_dead;
const LINUX_REBOOT_MAGIC2: usize = 0x2812_1969;
pub const REBOOT_RESTART: usize = 0x0123_4567;
pub const REBOOT_HALT: usize = 0xcdef_0123;
pub const REBOOT_POWER_OFF: usize = 0x4321_fedc;

/// `reboot(2)`. On success the kernel does not return, so a returned `Ok` is
/// itself an anomaly the caller reports.
pub fn reboot(cmd: usize) -> io::Result<()> {
    check(syscall5(SYS_REBOOT, LINUX_REBOOT_MAGIC1, LINUX_REBOOT_MAGIC2, cmd, 0, 0)).map(|_| ())
}

/// `sync(2)` — flush every filesystem before power-down. It cannot fail and has
/// no `std` equivalent (`File::sync_all` covers one handle, not the system).
pub fn sync() {
    let _ = syscall5(SYS_SYNC, 0, 0, 0, 0, 0);
}

// ── switch_root's two syscalls ──────────────────────────────────────────────

/// `MS_MOVE` (linux/mount.h) — the ONLY mount flag this crate ever passes.
const MS_MOVE: usize = 0x2000;

/// `mount(source, target, NULL, MS_MOVE, NULL)`: relocate an existing mount.
/// No other mount operation is reachable — the filesystem type and data
/// arguments are pinned to NULL here, not chosen by a caller.
pub fn move_mount(source: &CStr, target: &CStr) -> io::Result<()> {
    check(syscall5(
        SYS_MOUNT,
        source.as_ptr() as usize,
        target.as_ptr() as usize,
        0,
        MS_MOVE,
        0,
    ))
    .map(|_| ())
}

/// `chroot(2)`.
pub fn chroot(path: &CStr) -> io::Result<()> {
    check(syscall5(SYS_CHROOT, path.as_ptr() as usize, 0, 0, 0, 0)).map(|_| ())
}

// ── hostname(1)'s setter ────────────────────────────────────────────────────

/// `sethostname(2)`. Deliberately a syscall rather than a write to
/// `/proc/sys/kernel/hostname`: sysinit sets the hostname before `/proc` is
/// necessarily mounted, and this is the flag gap that keeps `hostname` on
/// busybox (uutils ships no `hostname` at all).
pub fn sethostname(name: &[u8]) -> io::Result<()> {
    check(syscall5(
        SYS_SETHOSTNAME,
        name.as_ptr() as usize,
        name.len(),
        0,
        0,
        0,
    ))
    .map(|_| ())
}

// ── cttyhack's two syscalls ─────────────────────────────────────────────────

/// `setsid(2)` — become a session leader. `TIOCSCTTY` is EPERM for anyone else,
/// and init's children are not session leaders, which is why cttyhack exists.
pub fn setsid() -> io::Result<i32> {
    check(syscall5(SYS_SETSID, 0, 0, 0, 0, 0)).map(|pid| pid as i32)
}

/// `ioctl(fd, TIOCSCTTY, 0)` — claim `fd`'s terminal as the controlling one.
/// The request number is pinned here, so no other ioctl is reachable.
///
/// The argument is pinned to 0 as well, which is the whole "do not steal" rule
/// expressed where it cannot be got wrong: 1 would make a `CAP_SYS_ADMIN` caller
/// take the terminal from the session that holds it, and a terminal still held
/// is one whose session is alive (the kernel releases it when the leader exits).
/// With 0 that case is EPERM, and the caller carries on without a terminal.
pub fn set_controlling_tty(fd: RawFd) -> io::Result<()> {
    const TIOCSCTTY: usize = 0x540e;
    check(syscall5(SYS_IOCTL, fd as usize, TIOCSCTTY, NO_STEAL, 0, 0)).map(|_| ())
}

/// `TIOCSCTTY`'s argument, and the only value of it this crate may pass. Named
/// so it is assertable: nothing observable distinguishes a steal from a success,
/// so a `1` here would change who owns an operator's terminal with every test
/// still green. `main.rs`'s confinement test pins this line.
const NO_STEAL: usize = 0;

// ── init's reaper ───────────────────────────────────────────────────────────

/// `(pid_t)-1` — reap ANY child. Written as the cast rather than `usize::MAX` so
/// the register value is obviously the -1 the kernel truncates to a `pid_t`.
const PID_ANY: usize = -1isize as usize;
const WNOHANG: usize = 1;
const ECHILD: i32 = 10;
const EINTR: i32 = 4;

/// What one `wait4(2)` call observed.
#[derive(Debug, PartialEq, Eq)]
pub enum Reaped {
    /// A child was reaped, with its raw wait status.
    Child { pid: i32, status: i32 },
    /// `WNOHANG` and no child has exited yet.
    NotYet,
    /// ECHILD — this process has no children at all.
    NoChildren,
}

/// `wait4(-1, &status, options, NULL)`. PID 1 must reap the ORPHANS the kernel
/// reparents onto it, which `Child::wait` (a targeted `waitpid`) cannot see —
/// that is the whole reason this syscall is in the surface.
pub fn wait_any(nohang: bool) -> io::Result<Reaped> {
    let mut status: i32 = 0;
    let opts = if nohang { WNOHANG } else { 0 };
    let ret = syscall5(
        SYS_WAIT4,
        PID_ANY,
        ptr::addr_of_mut!(status) as usize,
        opts,
        0,
        0,
    );
    classify(ret, status)
}

/// What a `wait4(2)` return means. Split from the syscall so the classification
/// — the part with the judgement in it — is reachable from a test.
fn classify(ret: isize, status: i32) -> io::Result<Reaped> {
    if ret > 0 {
        return Ok(Reaped::Child {
            pid: ret as i32,
            status,
        });
    }
    if ret == 0 {
        return Ok(Reaped::NotYet);
    }
    if ret == -(ECHILD as isize) {
        return Ok(Reaped::NoChildren);
    }
    // EINTR is not a fault to report. It needs no signal handler to occur — a
    // SIGSTOP/SIGCONT or a debugger attach is enough — and PID 1's caller logs
    // every error to the console, so reporting it would spam the console ten
    // times a second for as long as the interruption lasts.
    if ret == -(EINTR as isize) {
        return Ok(Reaped::NotYet);
    }
    Err(io::Error::from_raw_os_error(-ret as i32))
}

// ── wait-status decoding (pure arithmetic, no syscall) ──────────────────────

/// `WIFEXITED`/`WEXITSTATUS`.
pub fn exit_code(status: i32) -> Option<i32> {
    if status & 0x7f == 0 {
        Some((status >> 8) & 0xff)
    } else {
        None
    }
}

/// `WIFSIGNALED`/`WTERMSIG`. `0x7f` in the low bits is WIFSTOPPED, which cannot
/// reach us: `wait_any` never passes `WUNTRACED`.
pub fn term_signal(status: i32) -> Option<i32> {
    let sig = status & 0x7f;
    if sig != 0 && sig != 0x7f {
        Some(sig)
    } else {
        None
    }
}

/// Human-readable form of a raw wait status, for init's console log.
pub fn status_text(status: i32) -> String {
    if let Some(code) = exit_code(status) {
        return format!("exit {code}");
    }
    if let Some(sig) = term_signal(status) {
        return format!("signal {sig}");
    }
    format!("status {status:#x}")
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]
    use super::*;

    /// The raw encodings the kernel actually hands back: `status >> 8` for a
    /// normal exit, the low seven bits for a fatal signal.
    #[test]
    fn a_normal_exit_decodes_to_its_code() {
        assert_eq!(exit_code(0), Some(0));
        assert_eq!(exit_code(1 << 8), Some(1));
        assert_eq!(exit_code(127 << 8), Some(127));
        assert_eq!(term_signal(0), None);
        assert_eq!(status_text(2 << 8), "exit 2");
    }

    #[test]
    fn a_fatal_signal_decodes_to_its_number() {
        // SIGKILL (9) with no core flag.
        assert_eq!(term_signal(9), Some(9));
        assert_eq!(exit_code(9), None);
        assert_eq!(status_text(9), "signal 9");
        // The core-dumped bit (0x80) must not leak into the signal number.
        assert_eq!(term_signal(11 | 0x80), Some(11));
    }

    /// The four outcomes PID 1's reap loop turns on. The EINTR arm is the one
    /// with a decision in it: without it a debugger attach or a SIGSTOP/SIGCONT
    /// — neither of which needs a signal handler to reach us — becomes an error
    /// the loop reports to the console many times a second.
    #[test]
    fn a_wait_return_classifies_into_the_outcomes_the_reap_loop_expects() {
        assert_eq!(
            classify(42, 3 << 8).unwrap(),
            Reaped::Child {
                pid: 42,
                status: 3 << 8
            }
        );
        assert_eq!(classify(0, 0).unwrap(), Reaped::NotYet);
        assert_eq!(classify(-(ECHILD as isize), 0).unwrap(), Reaped::NoChildren);
        assert_eq!(classify(-(EINTR as isize), 0).unwrap(), Reaped::NotYet);
        // Anything else is a genuine fault and is surfaced as one.
        assert_eq!(classify(-22, 0).unwrap_err().raw_os_error(), Some(22)); // EINVAL
    }

    /// A stopped child is neither an exit nor a termination. `wait_any` cannot
    /// produce one, so `status_text` falls through to the raw form rather than
    /// misreporting it as a signal death.
    #[test]
    fn a_stopped_status_is_reported_raw() {
        let stopped = 0x7f | (19 << 8);
        assert_eq!(exit_code(stopped), None);
        assert_eq!(term_signal(stopped), None);
        assert!(status_text(stopped).starts_with("status "));
    }
}

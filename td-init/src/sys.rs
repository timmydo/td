//! The confined raw-syscall layer — the whole `unsafe` surface of this crate.
//!
//! The crate root `#![deny(unsafe_code)]`s and exactly one item here carries a
//! scoped `#[allow]`: `syscall5`, the `syscall`-instruction body copied verbatim
//! from `builder/src/sys.rs`/`td-kexec`. Everything else in the crate — and every
//! other function in this module — is ordinary safe Rust. This is the THIRD
//! target-side unsafe exception UNSAFE.md records, after td-kexec (two syscalls)
//! and td-netd (one ioctl).
//!
//! The crate-level `deny` catches an added `unsafe`, but it is not the whole
//! guarantee: re-leveling that lint is not itself `unsafe`, so a module
//! `#![warn(...)]` demotes it and `global_asm!` — which needs no `unsafe` block
//! — then compiles. `main.rs`'s confinement tests are what close that, and they
//! are the durable enforcement here.
//!
//! The amended surface is exactly the ten syscalls below, one per boot-glue
//! applet requirement that safe `std` does not expose. An ELEVENTH is a reviewed
//! amendment, not an edit; `main.rs`'s confinement test asserts the roster.
//! `ioctl(2)` is the one with TWO permitted requests — `TIOCSCTTY` for
//! cttyhack and `LOOP_SET_FD` for losetup — and both are pinned by value, so
//! widening the roster is as reviewable as adding a syscall to it.
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

// The amended ten (x86_64 syscall numbers).
const SYS_IOCTL: usize = 16;
const SYS_MKNOD: usize = 133;
const SYS_WAIT4: usize = 61;
const SYS_SETSID: usize = 112;
const SYS_CHROOT: usize = 161;
const SYS_SYNC: usize = 162;
const SYS_MOUNT: usize = 165;
const SYS_UMOUNT2: usize = 166;
const SYS_REBOOT: usize = 169;
const SYS_SETHOSTNAME: usize = 170;

/// The single raw-syscall entry point (x86_64 SysV syscall ABI), copied from
/// `builder/src/sys.rs`. Its body is the ONLY `unsafe` in the crate. The scoped
/// `#[allow]` under the crate `#![deny(unsafe_code)]` covers where `unsafe` may
/// appear, not what may be passed here — this fn is safe to CALL, so its
/// confinement is module privacy plus the typed wrappers below being its
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

// ── the filesystem pair (mount/umount applets, and switch_root's move) ──────

/// The `mount(2)` flags this crate may set (linux/mount.h). The list IS the
/// surface, and `mount.rs` is the only module that composes from it — through
/// its option table, the `-r`/`-w` shorthands for the bit that table already
/// owns, and `umount -r`'s read-only remount. `switch_root` names `MS_MOVE` and
/// nothing else. Both halves are asserted in `main.rs`: the roster is exactly
/// this list, and no other module may name one.
///
/// Until the `mount`/`umount` applets landed, `MS_MOVE` was the only one — the
/// amendment that widened this is the same one that added `umount2(2)`, and it
/// is what took the last busybox job off td's boot path.
pub const MS_RDONLY: usize = 0x1;
pub const MS_NOSUID: usize = 0x2;
pub const MS_NODEV: usize = 0x4;
pub const MS_NOEXEC: usize = 0x8;
pub const MS_SYNCHRONOUS: usize = 0x10;
pub const MS_REMOUNT: usize = 0x20;
pub const MS_NOATIME: usize = 0x400;
pub const MS_NODIRATIME: usize = 0x800;
pub const MS_BIND: usize = 0x1000;
pub const MS_MOVE: usize = 0x2000;
pub const MS_RELATIME: usize = 0x0020_0000;

/// `mount(source, target, fstype, flags, data)`.
///
/// `fstype` and `data` are `Option` rather than `&CStr` because NULL is not a
/// spelling of the empty string here: the kernel reads a non-NULL `data` as an
/// option string the filesystem must accept, and every flag-only operation
/// (`MS_MOVE`, `MS_REMOUNT`) passes NULL for both.
pub fn mount(
    source: &CStr,
    target: &CStr,
    fstype: Option<&CStr>,
    flags: usize,
    data: Option<&CStr>,
) -> io::Result<()> {
    check(syscall5(
        SYS_MOUNT,
        source.as_ptr() as usize,
        target.as_ptr() as usize,
        nullable(fstype),
        flags,
        nullable(data),
    ))
    .map(|_| ())
}

/// A string argument the kernel accepts NULL for. Kept as a named helper so the
/// two `Option` arguments above cannot be spelled differently from each other.
fn nullable(s: Option<&CStr>) -> usize {
    match s {
        Some(c) => c.as_ptr() as usize,
        None => 0,
    }
}

/// `umount2(2)`'s flags. `MNT_EXPIRE` and `UMOUNT_NOFOLLOW` are deliberately
/// absent: neither applet offers them, so neither is reachable.
pub const MNT_FORCE: usize = 0x1;
pub const MNT_DETACH: usize = 0x2;

/// `umount2(target, flags)`. `umount(2)` proper takes no flags and is a strict
/// subset, so this one call serves both — `flags` of 0 IS `umount(2)`.
pub fn umount(target: &CStr, flags: usize) -> io::Result<()> {
    check(syscall5(SYS_UMOUNT2, target.as_ptr() as usize, flags, 0, 0, 0)).map(|_| ())
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

// ── mknod's one node ───────────────────────────────────────────────────────

/// `mknod(path, mode, dev)`.
///
/// `dev` is a plain `usize` here rather than a major/minor pair, and that is
/// deliberate: the packing is a decision about which driver the kernel routes to,
/// so it belongs in `mknod.rs` next to the readback that CHECKS it, not hidden in
/// a wrapper where a wrong shift would be invisible. `mode` carries the node type
/// in its top bits; `mknod.rs` is the only composer of both, the same split the
/// `mount` flags use.
/// `mode` is a `usize` for register uniformity, but the kernel parameter is a
/// 16-bit `umode_t`: anything above 0xffff is dropped. `mknod.rs` composes only
/// `S_IFBLK | 0o600`, which fits.
pub fn mknod(path: &CStr, mode: usize, dev: usize) -> io::Result<()> {
    check(syscall5(SYS_MKNOD, path.as_ptr() as usize, mode, dev, 0, 0)).map(|_| ())
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

// ── losetup's one request ───────────────────────────────────────────────────

/// `ioctl(loop_fd, LOOP_SET_FD, backing_fd)` — attach a backing file to a loop
/// device. The SECOND request this crate's `ioctl` is restricted to.
///
/// `LOOP_SET_FD` rather than the newer `LOOP_CONFIGURE`, and that choice is the
/// safety argument: `LOOP_CONFIGURE` takes a `struct loop_config` — a `__u32`,
/// a nested `loop_info64`, and eight reserved `__u64`s — that this crate would
/// have to lay out by hand and pass as a pointer, and a field at the wrong
/// offset is a kernel operation nobody can see from the call site.
/// `LOOP_SET_FD`'s argument is the backing descriptor itself, an integer, so
/// there is no layout to get wrong.
///
/// Read-only is not a flag here, which is BETTER than passing one: the kernel
/// marks the loop read-only when the backing file was opened without write
/// access, so `losetup`'s `-r` cannot be forgotten — it is a property of the
/// descriptor td-boot verified and handed over. `losetup.rs` reads it back out
/// of sysfs and refuses rather than trusting that.
pub fn attach_loop(loop_fd: RawFd, backing_fd: RawFd) -> io::Result<()> {
    const LOOP_SET_FD: usize = 0x4c00;
    check(syscall5(
        SYS_IOCTL,
        loop_fd as usize,
        LOOP_SET_FD,
        backing_fd as usize,
        0,
        0,
    ))
    .map(|_| ())
}

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

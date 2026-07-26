//! The confined raw-syscall layer — the whole `unsafe` surface of this crate.
//!
//! The crate root `#![deny(unsafe_code)]`s and exactly one item here carries a
//! scoped `#[allow]`: `syscall2`, the `syscall`-instruction body copied from
//! `td-init/src/sys.rs` (itself copied from `builder/src/sys.rs`). Everything
//! else in the crate — and every other function in this module — is ordinary
//! safe Rust. This is the FOURTH target-side unsafe exception AGENTS.md records,
//! after td-kexec (two syscalls), td-netd (one ioctl), and td-init (eight).
//!
//! The amended surface is exactly the THREE credential-setting syscalls below.
//! They are here rather than behind `CommandExt::uid()/gid()/groups()` for two
//! reasons THREAT-MODEL.md §2 states in full: `groups` is unstable on the pinned
//! stable rustc, so the only reachable `std` behaviour drops every supplementary
//! group; and `std` applies them in a forked child, where td-login can never
//! read back what actually took. Doing them in this process is what makes the
//! post-condition check possible, and the post-condition check is the whole
//! defence.
//!
//! Deliberately NOT in the surface: `getuid`/`getgid`/`getgroups` (the same
//! answers come from `/proc/self/status`, which the post-condition check has to
//! read anyway), `setresuid`/`setreuid` (a second way to set the same thing is a
//! second way to get it wrong), `execve` (safe `CommandExt::exec` covers it), and
//! `umask`. A FOURTH syscall is a reviewed amendment, not an edit; `main.rs`'s
//! confinement test asserts the roster.

use std::io;

#[cfg(not(all(target_arch = "x86_64", target_os = "linux")))]
compile_error!("td-login is x86_64-linux only (raw syscall ABI)");

// The amended three (x86_64 syscall numbers).
const SYS_SETUID: usize = 105;
const SYS_SETGID: usize = 106;
const SYS_SETGROUPS: usize = 116;

/// Linux's `NGROUPS_MAX`. The kernel rejects a longer list with EINVAL; refusing
/// it here means the diagnostic names the account database rather than an errno.
const NGROUPS_MAX: usize = 65536;

/// The single raw-syscall entry point (x86_64 SysV syscall ABI). Its body is the
/// ONLY `unsafe` in the crate. The scoped `#[allow]` under the crate
/// `#![deny(unsafe_code)]` covers where `unsafe` may appear, not what may be
/// passed here — this fn is safe to CALL, so its confinement is module privacy
/// plus the three typed wrappers below being its only callers. All three
/// syscalls take at most two arguments; the unused register is 0, which the
/// kernel ignores.
#[inline]
#[allow(unsafe_code)]
fn syscall2(n: usize, a1: usize, a2: usize) -> isize {
    let ret: isize;
    // SAFETY: the `syscall` instruction clobbers rcx/r11 and returns in rax. The
    // arguments are a plain integer and, for setgroups, a pointer-as-usize whose
    // pointee the caller keeps live across the call. The kernel only READS that
    // range (`a2` gids); nothing here is written through a pointer, so unlike
    // td-init's wait4 there is no stale-value hazard.
    unsafe {
        core::arch::asm!(
            "syscall",
            inlateout("rax") n as isize => ret,
            in("rdi") a1,
            in("rsi") a2,
            out("rcx") _,
            out("r11") _,
            options(nostack),
        );
    }
    ret
}

/// Turn a raw syscall return into a `Result`, mirroring `td-init/src/sys.rs`.
fn check(ret: isize) -> io::Result<()> {
    if ret < 0 {
        Err(io::Error::from_raw_os_error(-ret as i32))
    } else {
        Ok(())
    }
}

/// `setgroups(2)` — replace the supplementary group set. FIRST of the three, and
/// the one a partial switch drops: it needs `CAP_SETGID`, which `setuid(2)` is
/// about to take away.
///
/// An empty list is refused rather than passed through. `setgroups(0, NULL)`
/// (clear every group) is a legitimate kernel operation, but it is not one
/// td-login has a caller for — `Credentials` always carries the primary gid — so
/// an empty slice reaching here means the set was computed wrong, and clearing
/// on the strength of a bug is how a session ends up with credentials nobody
/// intended.
pub fn setgroups(list: &[u32]) -> io::Result<()> {
    if list.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "refusing to set an empty supplementary group list",
        ));
    }
    if list.len() > NGROUPS_MAX {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "supplementary group list exceeds NGROUPS_MAX",
        ));
    }
    check(syscall2(
        SYS_SETGROUPS,
        list.len(),
        list.as_ptr() as usize,
    ))
}

/// `setgid(2)` — the primary group. SECOND: still privileged, and still before
/// the uid drop.
pub fn setgid(gid: u32) -> io::Result<()> {
    check(syscall2(SYS_SETGID, gid as usize, 0))
}

/// `setuid(2)` — LAST, because it is the call that removes the privilege the
/// other two require. Called from root it sets the real, effective and saved
/// uids together, which is what `creds::apply`'s post-condition asserts.
pub fn setuid(uid: u32) -> io::Result<()> {
    check(syscall2(SYS_SETUID, uid as usize, 0))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]
    use super::*;

    /// The two refusals that happen before any syscall. Both are about the list
    /// being wrong rather than the kernel being unwilling, so they must not
    /// reach `syscall2` at all — a test that ran them as root would otherwise
    /// clear the runner's groups.
    #[test]
    fn an_unusable_group_list_is_refused_before_the_syscall() {
        let empty = setgroups(&[]).unwrap_err();
        assert_eq!(empty.kind(), io::ErrorKind::InvalidInput);
        let huge = vec![0u32; NGROUPS_MAX + 1];
        assert_eq!(
            setgroups(&huge).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
    }

    /// The wrappers must not renumber. A transposed pair here is silent — every
    /// call still "succeeds" — and sets the wrong credential.
    #[test]
    fn the_amended_three_keep_their_x86_64_numbers() {
        assert_eq!(SYS_SETUID, 105);
        assert_eq!(SYS_SETGID, 106);
        assert_eq!(SYS_SETGROUPS, 116);
    }

    /// An errno round-trips as itself: `apply` reports which of the three failed
    /// by its `io::Error`, so a mangled sign would report EPERM as success.
    #[test]
    fn a_negative_return_becomes_its_errno() {
        assert!(check(0).is_ok());
        assert_eq!(check(-1).unwrap_err().raw_os_error(), Some(1)); // EPERM
        assert_eq!(check(-22).unwrap_err().raw_os_error(), Some(22)); // EINVAL
    }
}

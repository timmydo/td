//! The confined raw-syscall layer — the whole `unsafe` surface of this crate.
//!
//! The crate root `#![deny(unsafe_code)]`s and exactly one item here carries a
//! scoped `#[allow]`: `syscall3`, the `syscall`-instruction body copied from
//! `td-init/src/sys.rs`. Everything else in the crate — and every other function
//! in this module — is ordinary safe Rust. This is the SEVENTH target-side unsafe
//! exception AGENTS.md records.
//!
//! The surface is ONE syscall, `ioctl(2)`, with exactly THREE permitted requests,
//! all of them about the shape and mode of a terminal this process already has
//! open: `TCGETS` and `TCSETS` (read and set the line discipline, so `less` can
//! take a keystroke without waiting for Enter) and `TIOCGWINSZ` (how many rows and
//! columns, so a page is a screenful rather than a guess). A FOURTH request, or a
//! second syscall, is a reviewed amendment; `main.rs`'s confinement tests assert
//! the roster, the request values, the assembly body and the callers.
//!
//! Deliberately NOT here: `TIOCSWINSZ` (nothing should resize an operator's
//! terminal), `TCSETSW`/`TCSETSF` (they drain or flush pending output, and a pager
//! has no business discarding what another process wrote), `TIOCSTI` (it injects
//! input into a terminal, which is the classic escape from a restricted session),
//! and `isatty`, which `std::io::IsTerminal` already answers safely.
//!
//! The termios and winsize buffers are OPAQUE BYTES here. This module never
//! decides what a field means — it hands the kernel a buffer and gives one back —
//! so the layout knowledge lives in exactly one place (`term.rs`), next to the
//! read-back that checks it.

use std::io;
use std::os::fd::RawFd;

#[cfg(not(all(target_arch = "x86_64", target_os = "linux")))]
compile_error!("td-util's terminal layer is x86_64-linux only (raw syscall ABI)");

const SYS_IOCTL: usize = 16;

/// `struct termios` as the x86_64 kernel lays it out: four `u32` flag words, a
/// `c_line` byte, then `NCCS` = 19 control characters.
pub const TERMIOS_LEN: usize = 36;

/// `struct winsize`: four `u16` — rows, columns, and two pixel fields td ignores.
pub const WINSIZE_LEN: usize = 8;

/// The single raw-syscall entry point (x86_64 SysV syscall ABI), copied from
/// `td-init/src/sys.rs`. Its body is the ONLY `unsafe` in the crate. The scoped
/// `#[allow]` covers where `unsafe` may appear, not what may be passed here — this
/// fn is safe to CALL, so its confinement is module privacy plus the three typed
/// wrappers below being its only callers.
#[inline]
#[allow(unsafe_code)]
fn syscall3(n: usize, a1: usize, a2: usize, a3: usize) -> isize {
    let ret: isize;
    // SAFETY: the `syscall` instruction clobbers rcx/r11 and returns in rax; the
    // args are plain integers or a pointer-as-usize whose pointee the caller keeps
    // live and correctly sized across the call. `options(nomem)` is deliberately
    // ABSENT and load-bearing by its absence: TCGETS and TIOCGWINSZ have the
    // kernel WRITE through one of those pointers, and promising the compiler this
    // asm touches no memory would let it keep a stale buffer across the call.
    unsafe {
        core::arch::asm!(
            "syscall",
            inlateout("rax") n as isize => ret,
            in("rdi") a1,
            in("rsi") a2,
            in("rdx") a3,
            out("rcx") _,
            out("r11") _,
            options(nostack),
        );
    }
    ret
}

/// Turn a raw syscall return into a `Result`, mirroring `td-init`'s `check`.
fn check(ret: isize) -> io::Result<()> {
    if ret < 0 {
        Err(io::Error::from_raw_os_error(-ret as i32))
    } else {
        Ok(())
    }
}

/// `ioctl(fd, TCGETS, &mut termios)` — read the current line discipline.
pub fn termios_get(fd: RawFd, out: &mut [u8; TERMIOS_LEN]) -> io::Result<()> {
    const TCGETS: usize = 0x5401;
    check(syscall3(
        SYS_IOCTL,
        fd as usize,
        TCGETS,
        out.as_mut_ptr() as usize,
    ))
}

/// `ioctl(fd, TCSETS, &termios)` — set it, effective immediately.
///
/// `TCSETS` rather than `TCSETSW`/`TCSETSF`: those wait for pending output to
/// drain, or discard pending input, and a pager changing its own read mode has no
/// business doing either to whatever else is writing to the terminal.
pub fn termios_set(fd: RawFd, termios: &[u8; TERMIOS_LEN]) -> io::Result<()> {
    const TCSETS: usize = 0x5402;
    check(syscall3(
        SYS_IOCTL,
        fd as usize,
        TCSETS,
        termios.as_ptr() as usize,
    ))
}

/// `ioctl(fd, TIOCGWINSZ, &mut winsize)` — the terminal's size.
///
/// Read-only by choice: `TIOCSWINSZ` is the setter and is not in this surface,
/// because nothing td ships has a reason to resize an operator's terminal.
pub fn window_size(fd: RawFd, out: &mut [u8; WINSIZE_LEN]) -> io::Result<()> {
    const TIOCGWINSZ: usize = 0x5413;
    check(syscall3(
        SYS_IOCTL,
        fd as usize,
        TIOCGWINSZ,
        out.as_mut_ptr() as usize,
    ))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use std::os::fd::AsRawFd;

    /// The syscall is really ISSUED, and really fails for a non-terminal.
    ///
    /// Every other assertion about this module is about source TEXT; a wrapper
    /// that returned `Ok(())` without issuing anything would satisfy all of them.
    /// A regular file is not a terminal, so the kernel answers ENOTTY — which
    /// proves the request reached it.
    #[test]
    fn the_ioctl_is_issued_and_the_kernel_answers() {
        let path = std::env::temp_dir().join(format!("td-util-sys-{}", std::process::id()));
        let f = std::fs::File::create(&path).unwrap();
        let mut buf = [0u8; TERMIOS_LEN];
        let err = termios_get(f.as_raw_fd(), &mut buf).unwrap_err();
        assert_eq!(
            err.raw_os_error(),
            Some(25),
            "a regular file must answer ENOTTY (25), not {err}"
        );
        let mut win = [0u8; WINSIZE_LEN];
        assert!(window_size(f.as_raw_fd(), &mut win).is_err());
        let _ = std::fs::remove_file(&path);
    }

    /// A bad descriptor is EBADF, so the fd argument lands in the right register.
    #[test]
    fn the_descriptor_argument_reaches_the_kernel() {
        let mut buf = [0u8; TERMIOS_LEN];
        let err = termios_get(-1, &mut buf).unwrap_err();
        assert_eq!(err.raw_os_error(), Some(9), "expected EBADF, got {err}");
    }
}

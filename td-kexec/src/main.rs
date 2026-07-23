//! td-kexec — the guest-side kexec helper for td's image-based boot.
//!
//! It performs exactly two raw Linux x86_64 syscalls and nothing else:
//!   * `kexec_file_load(2)` (#320) — stage the selected kernel + initramfs
//!   * `reboot(2)` (#169) with `LINUX_REBOOT_CMD_KEXEC` — jump into it
//!
//! Payload-hash verification is the shim's job (busybox `sha256sum`), NOT this
//! program's, so this stays a two-syscall surface. This is the ONLY `unsafe` in
//! td outside `builder/src/sys.rs` — a recorded amendment to the confinement
//! rule (AGENTS.md). Unlike builder's crate-level `#![allow(unsafe_code)]`, the
//! confinement here is compiler-enforced: the crate `#![deny(unsafe_code)]`s and
//! only `syscall5` carries a scoped `#[allow]`, so any other `unsafe` reds.
//!
//! Usage: `td-kexec <kernel> <initramfs|-> <cmdline>`
//!   `<initramfs>` == "-" boots with no initramfs (`KEXEC_FILE_NO_INITRAMFS`).
#![deny(unsafe_code)]

use std::ffi::{CString, OsStr, OsString};
use std::fs::File;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::process::ExitCode;

#[cfg(not(all(target_arch = "x86_64", target_os = "linux")))]
compile_error!("td-kexec is x86_64-linux only (raw syscall ABI)");

const SYS_KEXEC_FILE_LOAD: usize = 320;
const SYS_REBOOT: usize = 169;

// reboot(2) magics + the kexec command (linux/reboot.h).
const LINUX_REBOOT_MAGIC1: usize = 0xfee1_dead;
const LINUX_REBOOT_MAGIC2: usize = 0x2812_1969;
const LINUX_REBOOT_CMD_KEXEC: usize = 0x4558_4543;

// kexec_file_load(2) flags (linux/kexec.h).
const KEXEC_FILE_NO_INITRAMFS: usize = 0x0000_0004;

/// The single raw-syscall entry point (x86_64 SysV syscall ABI), copied from
/// `builder/src/sys.rs`. This function's body is the ONLY `unsafe` in the crate;
/// the scoped `#[allow]` (under the crate `#![deny(unsafe_code)]`) is the
/// compiler-enforced confinement — an `unsafe` anywhere else fails the build.
#[inline]
#[allow(unsafe_code)]
fn syscall5(n: usize, a1: usize, a2: usize, a3: usize, a4: usize, a5: usize) -> isize {
    let ret: isize;
    // SAFETY: the `syscall` instruction clobbers rcx/r11 and returns in rax;
    // the args are plain integers or a pointer-as-usize whose pointee the caller
    // keeps live across the call. No memory is aliased beyond the kernel's read.
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
fn check(ret: isize) -> std::io::Result<isize> {
    if ret < 0 {
        Err(std::io::Error::from_raw_os_error(-ret as i32))
    } else {
        Ok(ret)
    }
}

fn usage_err() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::InvalidInput,
        "usage: td-kexec <kernel> <initramfs|-> <cmdline>",
    )
}

fn parse_args<I: Iterator<Item = OsString>>(
    mut args: I,
) -> std::io::Result<(OsString, OsString, OsString)> {
    let kernel = args.next().ok_or_else(usage_err)?;
    let initramfs = args.next().ok_or_else(usage_err)?;
    let cmdline = args.next().ok_or_else(usage_err)?;
    if args.next().is_some() {
        return Err(usage_err());
    }
    Ok((kernel, initramfs, cmdline))
}

fn run() -> std::io::Result<()> {
    // args_os()/OsString, not args()/String: kernel and initramfs are OS byte
    // paths, and the String iterator PANICS on a non-UTF-8 argument — a valid
    // path is not a reason to panic.
    let (kernel, initramfs, cmdline) = parse_args(std::env::args_os().skip(1))?;

    let kernel_file = File::open(&kernel)?;

    // The initramfs File must outlive the syscall so its fd stays valid; bind it
    // in an outer scope. "-" means boot with no initramfs.
    let initrd_file;
    let (initrd_fd, flags): (i32, usize) = if initramfs.as_os_str() == OsStr::new("-") {
        (-1, KEXEC_FILE_NO_INITRAMFS)
    } else {
        initrd_file = File::open(&initramfs)?;
        (initrd_file.as_raw_fd(), 0)
    };

    // The kernel copies `cmdline_len` bytes and requires the last be NUL, so pass
    // the length WITH the terminator.
    let cmdline_c = CString::new(cmdline.as_bytes()).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "cmdline contains an interior NUL byte",
        )
    })?;
    let cmdline_bytes = cmdline_c.as_bytes_with_nul();

    // kexec_file_load(kernel_fd, initrd_fd, cmdline_len, cmdline_ptr, flags)
    check(syscall5(
        SYS_KEXEC_FILE_LOAD,
        kernel_file.as_raw_fd() as usize,
        initrd_fd as usize,
        cmdline_bytes.len(),
        cmdline_bytes.as_ptr() as usize,
        flags,
    ))?;

    // reboot(magic1, magic2, LINUX_REBOOT_CMD_KEXEC, NULL) — jumps into the
    // staged image and does not return on success.
    check(syscall5(
        SYS_REBOOT,
        LINUX_REBOOT_MAGIC1,
        LINUX_REBOOT_MAGIC2,
        LINUX_REBOOT_CMD_KEXEC,
        0,
        0,
    ))?;

    Err(std::io::Error::other(
        "reboot(LINUX_REBOOT_CMD_KEXEC) returned without booting the staged image",
    ))
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            // Fallible write, not eprintln!, which PANICS if the stderr write
            // fails (e.g. EPIPE); the error path must never panic.
            let _ = writeln!(std::io::stderr(), "td-kexec: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(xs: &[&str]) -> std::vec::IntoIter<OsString> {
        xs.iter().map(|s| OsString::from(*s)).collect::<Vec<_>>().into_iter()
    }

    #[test]
    fn parse_requires_exactly_three_args() {
        assert!(parse_args(args(&["k"])).is_err());
        assert!(parse_args(args(&["k", "i"])).is_err());
        assert!(parse_args(args(&["k", "i", "c"])).is_ok());
    }

    #[test]
    fn parse_rejects_a_fourth_arg() {
        assert!(parse_args(args(&["k", "i", "c", "extra"])).is_err());
    }

    #[test]
    fn parse_preserves_the_three_values() {
        let (k, i, c) = parse_args(args(&["/boot/bzImage", "-", "console=ttyS0"])).unwrap();
        assert_eq!(k, OsString::from("/boot/bzImage"));
        assert_eq!(i, OsString::from("-"));
        assert_eq!(c, OsString::from("console=ttyS0"));
    }

    #[test]
    fn check_maps_negative_to_errno() {
        assert_eq!(check(-2).unwrap_err().raw_os_error(), Some(2));
        assert_eq!(check(0).unwrap(), 0);
        assert_eq!(check(5).unwrap(), 5);
    }

    #[test]
    fn cmdline_with_interior_nul_is_rejected() {
        assert!(CString::new("bad\0cmdline").is_err());
    }
}

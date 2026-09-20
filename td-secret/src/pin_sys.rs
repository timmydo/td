//! Bounded terminal mode and input readiness for the manual token check.

use std::io;
use std::os::fd::{AsRawFd, BorrowedFd};

#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
compile_error!("PIN terminal requires Linux x86-64");

const SYS_IOCTL: usize = 16;
const SYS_POLL: usize = 7;
const SYS_PRCTL: usize = 157;
const TCGETS: usize = 0x5401;
const TCSETSF: usize = 0x5404;

#[allow(unsafe_code)]
fn syscall3(n: usize, a: usize, b: usize, c: usize) -> isize {
    let result: isize;
    // SAFETY: only the wrappers below call this instruction, retaining the
    // borrowed descriptor and exact kernel-sized buffers throughout the call.
    unsafe {
        core::arch::asm!(
            "syscall", inlateout("rax") n as isize => result,
            in("rdi") a, in("rsi") b, in("rdx") c,
            in("r10") 0usize, in("r8") 0usize, in("r9") 0usize,
            out("rcx") _, out("r11") _, options(nostack),
        );
    }
    result
}

fn checked(result: isize) -> io::Result<()> {
    if result < 0 {
        Err(io::Error::from_raw_os_error(-result as i32))
    } else {
        Ok(())
    }
}

pub(super) fn protect_process() -> io::Result<()> {
    checked(syscall3(SYS_PRCTL, 4, 0, 0))?; // PR_SET_DUMPABLE(0)
    if syscall3(SYS_PRCTL, 3, 0, 0) != 0 {
        // PR_GET_DUMPABLE
        return Err(io::Error::other("token check remains dumpable"));
    }
    Ok(())
}

pub(super) fn mode(fd: BorrowedFd<'_>) -> io::Result<[u8; 36]> {
    let mut bytes = [0; 36];
    checked(syscall3(
        SYS_IOCTL,
        fd.as_raw_fd() as usize,
        TCGETS,
        bytes.as_mut_ptr() as usize,
    ))?;
    Ok(bytes)
}

pub(super) fn set_mode(fd: BorrowedFd<'_>, bytes: &[u8; 36]) -> io::Result<()> {
    checked(syscall3(
        SYS_IOCTL,
        fd.as_raw_fd() as usize,
        TCSETSF,
        bytes.as_ptr() as usize,
    ))
}

#[repr(C)]
struct Poll {
    fd: i32,
    events: i16,
    revents: i16,
}
const _: [(); 8] = [(); std::mem::size_of::<Poll>()];

pub(super) fn readable(fd: BorrowedFd<'_>) -> io::Result<bool> {
    let mut poll = Poll {
        fd: fd.as_raw_fd(),
        events: 1,
        revents: 0,
    };
    let result = syscall3(SYS_POLL, std::ptr::from_mut(&mut poll) as usize, 1, 50);
    if result == -4 {
        return Ok(false);
    }
    checked(result)?;
    if poll.revents & !1 != 0 {
        return Err(io::Error::other("PIN terminal disconnected"));
    }
    Ok(poll.revents == 1)
}

#[cfg(test)]
mod tests {
    #[test]
    fn process_dump_protection_is_verified_in_an_owned_child() {
        const MARKER: &str = "TD_PIN_DUMP_FIXTURE";
        if std::env::var_os(MARKER).is_some() {
            super::protect_process().unwrap();
            assert_eq!(super::syscall3(super::SYS_PRCTL, 3, 0, 0), 0);
            return;
        }
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "pin_sys::tests::process_dump_protection_is_verified_in_an_owned_child",
            ])
            .env(MARKER, "1")
            .status()
            .unwrap();
        assert!(status.success());
    }

    #[test]
    fn terminal_kernel_layout_and_requests_are_fixed() {
        use super::*;
        assert_eq!(
            (SYS_IOCTL, SYS_POLL, TCGETS, TCSETSF),
            (16, 7, 0x5401, 0x5404)
        );
        assert_eq!(std::mem::size_of::<Poll>(), 8);
        assert_eq!(std::mem::offset_of!(Poll, fd), 0);
        assert_eq!(std::mem::offset_of!(Poll, events), 4);
        assert_eq!(std::mem::offset_of!(Poll, revents), 6);
        let source = include_str!("pin_sys.rs")
            .split("#[cfg(test)]")
            .next()
            .unwrap();
        let fingerprint = source.bytes().fold(0xcbf29ce484222325u64, |hash, byte| {
            (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3)
        });
        assert_eq!(
            fingerprint, 0xf197c43440f89e6d,
            "manual PIN syscall surface changed"
        );
        assert_eq!(source.matches("syscall3(").count(), 6);
        assert_eq!(source.matches("core::arch::asm!").count(), 1);
        assert_eq!(source.matches("#[allow(unsafe_code)]").count(), 1);
        assert!(source.contains("let mut bytes = [0; 36];"));
        assert!(source.contains("std::ptr::from_mut(&mut poll) as usize, 1, 50"));
    }
}

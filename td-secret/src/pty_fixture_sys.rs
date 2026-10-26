//! A test-only PTY allocator for the PIN terminal fixtures: unlock a fresh
//! `/dev/ptmx` master and open its peer. It is compiled only under
//! `cfg(test)`, never into the production td-secret binary.
use std::io;
use std::os::fd::{AsRawFd, FromRawFd};

#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
compile_error!("the PTY fixture requires Linux x86-64");

const SYS_IOCTL: usize = 16;
const TIOCSPTLCK: usize = 0x4004_5431;
const TIOCGPTPEER: usize = 0x5441;
/// O_RDWR | O_NOCTTY | O_CLOEXEC.
const PEER_FLAGS: usize = 0x80102;

#[allow(unsafe_code)]
fn ioctl(fd: i32, request: usize, data: usize) -> isize {
    let ret: isize;
    // SAFETY: the x86-64 ioctl registers; callers keep any referenced buffer
    // live for the call.
    unsafe {
        core::arch::asm!(
            "syscall",
            inlateout("rax") SYS_IOCTL as isize => ret,
            in("rdi") fd as usize,
            in("rsi") request,
            in("rdx") data,
            out("rcx") _,
            out("r11") _,
            options(nostack),
        );
    }
    ret
}

fn check(ret: isize) -> io::Result<isize> {
    if ret < 0 {
        Err(io::Error::from_raw_os_error(-ret as i32))
    } else {
        Ok(ret)
    }
}

/// Unlock `master` and return its peer, opened read-write without
/// becoming a controlling terminal.
pub fn open_pty_peer(master: std::os::fd::BorrowedFd<'_>) -> io::Result<std::fs::File> {
    let unlocked: i32 = 0;
    check(ioctl(
        master.as_raw_fd(),
        TIOCSPTLCK,
        std::ptr::from_ref(&unlocked) as usize,
    ))?;
    let descriptor = check(ioctl(master.as_raw_fd(), TIOCGPTPEER, PEER_FLAGS))?;
    let descriptor =
        i32::try_from(descriptor).map_err(|_| io::Error::other("invalid PTY descriptor"))?;
    Ok(adopt(descriptor))
}

#[allow(unsafe_code)]
fn adopt(descriptor: i32) -> std::fs::File {
    // SAFETY: the successful peer ioctl installed this owned descriptor.
    unsafe { std::fs::File::from_raw_fd(descriptor) }
}

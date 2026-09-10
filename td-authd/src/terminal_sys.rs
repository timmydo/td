//! Fixed Linux PTY, terminal-mode and bounded two-descriptor polling calls.
use std::io;
use std::os::fd::{AsRawFd, FromRawFd};

#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
compile_error!("terminal relay requires Linux x86-64");

const SYS_IOCTL: usize = 16;
const SYS_POLL: usize = 7;

#[inline]
#[allow(unsafe_code)]
fn syscall5(n: usize, a1: usize, a2: usize, a3: usize, a4: usize, a5: usize) -> isize {
    let ret: isize;
    // SAFETY: the wrapper supplies the x86-64 syscall registers. Callers keep
    // every referenced terminal and poll buffer live for the call.
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

fn value(ret: isize) -> io::Result<usize> {
    if ret < 0 {
        Err(io::Error::from_raw_os_error(-ret as i32))
    } else {
        Ok(ret as usize)
    }
}

fn check(ret: isize) -> io::Result<()> {
    value(ret).map(|_| ())
}

const TIOCSPTLCK: usize = 0x4004_5431;
const TIOCGPTPEER: usize = 0x5441;
const TIOCGWINSZ: usize = 0x5413;
const TIOCSWINSZ: usize = 0x5414;
const TCGETS: usize = 0x5401;
const TCSETS: usize = 0x5402;

#[repr(usize)]
enum RelayRequest {
    Unlock = TIOCSPTLCK,
    Peer = TIOCGPTPEER,
    ReadSize = TIOCGWINSZ,
    WriteSize = TIOCSWINSZ,
    ReadMode = TCGETS,
    WriteMode = TCSETS,
}

fn relay_ioctl(
    fd: std::os::fd::BorrowedFd<'_>,
    operation: RelayRequest,
    data: usize,
) -> io::Result<isize> {
    let request = operation as usize;
    let result = syscall5(SYS_IOCTL, fd.as_raw_fd() as usize, request, data, 0, 0);
    check(result)?;
    Ok(result)
}

pub fn relay_pty_peer(master: std::os::fd::BorrowedFd<'_>) -> io::Result<std::fs::File> {
    let unlocked: i32 = 0;
    relay_ioctl(
        master,
        RelayRequest::Unlock,
        std::ptr::from_ref(&unlocked) as usize,
    )?;
    let descriptor = relay_ioctl(master, RelayRequest::Peer, 0x80102)?;
    let descriptor =
        i32::try_from(descriptor).map_err(|_| io::Error::other("invalid PTY descriptor"))?;
    Ok(adopt(descriptor))
}

#[allow(unsafe_code)]
fn adopt(descriptor: i32) -> std::fs::File {
    // SAFETY: the successful peer ioctl installed this owned descriptor.
    unsafe { std::fs::File::from_raw_fd(descriptor) }
}

pub fn relay_window_get(fd: std::os::fd::BorrowedFd<'_>) -> io::Result<[u16; 4]> {
    let mut size = [0u16; 4];
    relay_ioctl(fd, RelayRequest::ReadSize, size.as_mut_ptr() as usize)?;
    Ok(size)
}
pub fn relay_window_set(fd: std::os::fd::BorrowedFd<'_>, size: &[u16; 4]) -> io::Result<()> {
    relay_ioctl(fd, RelayRequest::WriteSize, size.as_ptr() as usize)?;
    Ok(())
}
pub fn relay_termios_get(fd: std::os::fd::BorrowedFd<'_>) -> io::Result<[u8; 36]> {
    let mut state = [0u8; 36];
    relay_ioctl(fd, RelayRequest::ReadMode, state.as_mut_ptr() as usize)?;
    Ok(state)
}
pub fn relay_termios_set(fd: std::os::fd::BorrowedFd<'_>, state: &[u8; 36]) -> io::Result<()> {
    relay_ioctl(fd, RelayRequest::WriteMode, state.as_ptr() as usize)?;
    Ok(())
}

#[repr(C)]
struct RelayPoll {
    fd: i32,
    events: i16,
    revents: i16,
}
const _: [(); 8] = [(); std::mem::size_of::<RelayPoll>()];

pub fn relay_poll(
    input: i32,
    master: std::os::fd::BorrowedFd<'_>,
    write: bool,
) -> io::Result<(bool, i16)> {
    let mut polls = [
        RelayPoll {
            fd: input,
            events: 1,
            revents: 0,
        },
        RelayPoll {
            fd: master.as_raw_fd(),
            events: 1 | if write { 4 } else { 0 },
            revents: 0,
        },
    ];
    let result = syscall5(SYS_POLL, polls.as_mut_ptr() as usize, 2, 100, 0, 0);
    if result == -4 {
        return Ok((false, 0));
    }
    check(result)?;
    let [input, master] = polls;
    if (input.revents | master.revents) & 0x20 != 0 {
        return Err(io::Error::other("relay poll descriptor is invalid"));
    }
    Ok((input.revents & 0x19 != 0, master.revents))
}

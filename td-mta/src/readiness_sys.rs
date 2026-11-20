//! Borrowed Linux socket readiness under a finite monotonic deadline.
use std::{
    io,
    net::TcpStream,
    os::{fd::AsRawFd, unix::net::UnixStream},
    time::Instant,
};

#[derive(Clone, Copy, Default)]
pub(crate) struct Interest {
    pub read: bool,
    pub write: bool,
}

#[repr(C)]
struct PollFd {
    fd: i32,
    events: i16,
    revents: i16,
}

const IN: i16 = 1;
const OUT: i16 = 4;
const INVALID: i16 = 32;

pub(crate) enum Connected<'a> {
    Tcp(&'a TcpStream),
    Unix(&'a UnixStream),
}

pub(crate) enum Ready {
    Timeout,
    Socket,
    Wake,
}

pub(crate) fn wait(
    socket: Connected<'_>,
    interest: Interest,
    wake: Option<&UnixStream>,
    until: Instant,
) -> io::Result<Ready> {
    let mut fds = [
        PollFd {
            fd: match socket {
                Connected::Tcp(s) => s.as_raw_fd(),
                Connected::Unix(s) => s.as_raw_fd(),
            },
            events: if interest.read { IN } else { 0 } | if interest.write { OUT } else { 0 },
            revents: 0,
        },
        PollFd {
            fd: wake.map_or(-1, AsRawFd::as_raw_fd),
            events: IN,
            revents: 0,
        },
    ];
    loop {
        let left = until.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Ok(Ready::Timeout);
        }
        let mut timeout = [
            i64::try_from(left.as_secs()).map_err(|_| io::ErrorKind::InvalidInput)?,
            i64::from(left.subsec_nanos()),
        ];
        match ppoll(&mut fds, &mut timeout) {
            -4 => continue, // EINTR
            0 => return Ok(Ready::Timeout),
            1 | 2 => {
                let [socket, wake] = &fds;
                if (socket.revents | wake.revents) & INVALID != 0 {
                    return Err(io::ErrorKind::InvalidInput.into());
                }
                // Hangup or error also releases the waiter to do the actual I/O.
                return Ok(if wake.revents != 0 {
                    Ready::Wake
                } else {
                    Ready::Socket
                });
            }
            value => {
                let errno = value
                    .checked_neg()
                    .and_then(|v| i32::try_from(v).ok())
                    .filter(|v| *v > 0)
                    .ok_or(io::ErrorKind::InvalidData)?;
                return Err(io::Error::from_raw_os_error(errno));
            }
        }
    }
}

#[allow(unsafe_code)]
fn ppoll(fds: &mut [PollFd; 2], timeout: &mut [i64; 2]) -> isize {
    // Two live Linux pollfd records and one writable __kernel_timespec.
    // ppoll neither owns the descriptors nor retains either pointer.
    let result: isize;
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    unsafe {
        std::arch::asm!(
            "syscall", inlateout("rax") 271usize => result,
            in("rdi") fds.as_mut_ptr(), in("rsi") 2usize,
            in("rdx") timeout.as_mut_ptr(), in("r10") 0usize, in("r8") 8usize,
            lateout("rcx") _, lateout("r11") _, options(nostack),
        );
    }
    #[cfg(all(target_os = "linux", target_arch = "aarch64"))]
    unsafe {
        std::arch::asm!(
            "svc 0", in("x8") 73usize,
            inlateout("x0") fds.as_mut_ptr() => result, in("x1") 2usize,
            in("x2") timeout.as_mut_ptr(), in("x3") 0usize, in("x4") 8usize,
            options(nostack),
        );
    }
    result
}

const _: () = assert!(std::mem::size_of::<PollFd>() == 8 && std::mem::align_of::<PollFd>() == 4);

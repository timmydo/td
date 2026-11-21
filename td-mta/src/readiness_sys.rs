//! Borrowed Linux socket readiness under a finite monotonic deadline.
use std::{
    io,
    net::{TcpListener, TcpStream},
    os::{
        fd::{AsFd, BorrowedFd},
        unix::net::{UnixListener, UnixStream},
    },
    time::Instant,
};

#[derive(Clone, Copy, Default)]
pub(crate) struct Interest {
    pub read: bool,
    pub write: bool,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct PollFd<'a> {
    fd: BorrowedFd<'a>,
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

impl<'a> PollFd<'a> {
    fn readable(fd: BorrowedFd<'a>) -> Self {
        Self {
            fd,
            events: IN,
            revents: 0,
        }
    }
    fn connected(socket: Connected<'a>, interest: Interest) -> Self {
        let fd = match socket {
            Connected::Tcp(s) => s.as_fd(),
            Connected::Unix(s) => s.as_fd(),
        };
        Self {
            fd,
            events: if interest.read { IN } else { 0 } | if interest.write { OUT } else { 0 },
            revents: 0,
        }
    }
}

const HTTPS_FDS: usize =
    1 + crate::config::listener::MAX_LISTENERS + crate::limits::MAX_HTTPS_CONNECTIONS;

pub(crate) struct PollSet<'a> {
    fds: [PollFd<'a>; HTTPS_FDS],
    used: usize,
}

impl<'a> PollSet<'a> {
    pub(crate) fn new(wake: &'a UnixStream) -> Self {
        Self {
            fds: [PollFd::readable(wake.as_fd()); HTTPS_FDS],
            used: 1,
        }
    }

    fn add(&mut self, fd: PollFd<'a>) -> io::Result<()> {
        *self
            .fds
            .get_mut(self.used)
            .ok_or(io::ErrorKind::InvalidInput)? = fd;
        self.used += 1;
        Ok(())
    }

    pub(crate) fn add_tcp_listener(&mut self, listener: &'a TcpListener) -> io::Result<()> {
        self.add(PollFd::readable(listener.as_fd()))
    }

    pub(crate) fn add_unix_listener(&mut self, listener: &'a UnixListener) -> io::Result<()> {
        self.add(PollFd::readable(listener.as_fd()))
    }

    pub(crate) fn add_connected(
        &mut self,
        socket: Connected<'a>,
        interest: Interest,
    ) -> io::Result<()> {
        self.add(PollFd::connected(socket, interest))
    }

    pub(crate) fn wait(&mut self, until: Instant) -> io::Result<()> {
        let active = self
            .fds
            .get_mut(..self.used)
            .ok_or(io::ErrorKind::InvalidInput)?;
        wait_until(active, until).map(|_| ())
    }
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
    let mut socket = PollFd::connected(socket, interest);
    if let Some(receiver) = wake {
        let mut fds = [socket, PollFd::readable(receiver.as_fd())];
        if !wait_until(&mut fds, until)? {
            return Ok(Ready::Timeout);
        }
        let [_, wake] = fds;
        return Ok(if wake.revents != 0 {
            Ready::Wake
        } else {
            Ready::Socket
        });
    }
    let ready = wait_until(std::slice::from_mut(&mut socket), until)?;
    Ok(if ready { Ready::Socket } else { Ready::Timeout })
}

fn wait_until(fds: &mut [PollFd<'_>], until: Instant) -> io::Result<bool> {
    loop {
        let left = until.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Ok(false);
        }
        let mut timeout = [
            i64::try_from(left.as_secs()).map_err(|_| io::ErrorKind::InvalidInput)?,
            i64::from(left.subsec_nanos()),
        ];
        match ppoll(fds, &mut timeout) {
            -4 => continue, // EINTR
            0 => return Ok(false),
            count if count > 0 => {
                if count as usize > fds.len() {
                    return Err(io::ErrorKind::InvalidData.into());
                }
                if fds.iter().any(|fd| fd.revents & INVALID != 0) {
                    return Err(io::ErrorKind::InvalidInput.into());
                }
                // Hangup or error also releases the waiter to do the actual I/O.
                return Ok(true);
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
fn ppoll(fds: &mut [PollFd<'_>], timeout: &mut [i64; 2]) -> isize {
    // The slice holds live Linux pollfd records borrowing their sockets.
    // ppoll neither owns the descriptors nor retains either pointer.
    let result: isize;
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    unsafe {
        std::arch::asm!(
            "syscall", inlateout("rax") 271usize => result,
            in("rdi") fds.as_mut_ptr(), in("rsi") fds.len(),
            in("rdx") timeout.as_mut_ptr(), in("r10") 0usize, in("r8") 8usize,
            lateout("rcx") _, lateout("r11") _, options(nostack),
        );
    }
    #[cfg(all(target_os = "linux", target_arch = "aarch64"))]
    unsafe {
        std::arch::asm!(
            "svc 0", in("x8") 73usize,
            inlateout("x0") fds.as_mut_ptr() => result, in("x1") fds.len(),
            in("x2") timeout.as_mut_ptr(), in("x3") 0usize, in("x4") 8usize,
            options(nostack),
        );
    }
    result
}

const _: () =
    assert!(std::mem::size_of::<PollFd<'_>>() == 8 && std::mem::align_of::<PollFd<'_>>() == 4);

#[cfg(test)]
#[path = "readiness_tests.rs"]
mod tests;

//! Linux x86-64 private-channel transport; roster section 16.

use std::io;
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;

#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
compile_error!("td-authd requires Linux x86-64");

const SYS_POLL: usize = 7;
const SYS_RECVMSG: usize = 47;
const SYS_SETSOCKOPT: usize = 54;
const SYS_GETSOCKOPT: usize = 55;
const SOL_SOCKET: usize = 1;
const SO_PEERCRED: usize = 17;
const SO_PASSCRED: usize = 16;
const SO_PASSPIDFD: usize = 76;
const SCM_RIGHTS: i32 = 1;
const SCM_CREDENTIALS: i32 = 2;
const SCM_PIDFD: i32 = 4;
const MSG_CTRUNC: i32 = 8;
const MSG_CMSG_CLOEXEC: usize = 0x4000_0000;
const POLLIN: i16 = 1;
const CONTROL: usize = 128;
const HEADER: usize = 16;

#[repr(align(8))]
struct Control([u8; CONTROL]);

#[repr(C)]
struct IoVec {
    base: *mut u8,
    len: usize,
}

#[repr(C)]
struct MsgHdr {
    name: *mut u8,
    name_len: u32,
    iov: *mut IoVec,
    iov_len: usize,
    control: *mut u8,
    control_len: usize,
    flags: i32,
}

#[repr(C)]
struct PollFd {
    fd: i32,
    events: i16,
    revents: i16,
}

const _: [(); 16] = [(); std::mem::size_of::<IoVec>()];
const _: [(); 56] = [(); std::mem::size_of::<MsgHdr>()];
const _: [(); 48] = [(); std::mem::offset_of!(MsgHdr, flags)];
const _: [(); 8] = [(); std::mem::size_of::<PollFd>()];

#[allow(unsafe_code)]
fn syscall5(number: usize, a1: usize, a2: usize, a3: usize, a4: usize, a5: usize) -> isize {
    let result: isize;
    // SAFETY: private wrappers hold every correctly sized allocation for the
    // synchronous kernel call. No borrowed pointer or descriptor escapes it.
    unsafe {
        core::arch::asm!(
            "syscall",
            inlateout("rax") number as isize => result,
            in("rdi") a1,
            in("rsi") a2,
            in("rdx") a3,
            in("r10") a4,
            in("r8") a5,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    result
}

#[allow(unsafe_code)]
fn adopt(fd: i32) -> OwnedFd {
    // SAFETY: harvest calls this once for each freshly installed nonnegative
    // descriptor. It never adopts a number from message-body bytes.
    unsafe { OwnedFd::from_raw_fd(fd) }
}

fn result(value: isize) -> io::Result<usize> {
    if value < 0 {
        return Err(io::Error::from_raw_os_error((-value) as i32));
    }
    Ok(value as usize)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Credentials {
    pub pid: i32,
    pub uid: u32,
    pub gid: u32,
}

/// Only the creator query uses connection-time credentials.
pub(super) fn prepare(stream: &UnixStream) -> io::Result<Credentials> {
    let enabled: i32 = 1;
    for option in [SO_PASSCRED, SO_PASSPIDFD] {
        result(syscall5(
            SYS_SETSOCKOPT,
            stream.as_raw_fd() as usize,
            SOL_SOCKET,
            option,
            (&enabled as *const i32) as usize,
            std::mem::size_of::<i32>(),
        ))?;
    }
    let mut words = [0u32; 3];
    let mut length: u32 = 12;
    result(syscall5(
        SYS_GETSOCKOPT,
        stream.as_raw_fd() as usize,
        SOL_SOCKET,
        SO_PEERCRED,
        words.as_mut_ptr() as usize,
        (&mut length as *mut u32) as usize,
    ))?;
    let [pid, uid, gid] = words;
    if length != 12 || pid == 0 || pid > i32::MAX as u32 {
        return Err(io::Error::other("invalid channel creator credentials"));
    }
    Ok(Credentials {
        pid: pid as i32,
        uid,
        gid,
    })
}

pub(super) fn alive(pidfd: BorrowedFd<'_>) -> io::Result<()> {
    let mut poll = PollFd {
        fd: pidfd.as_raw_fd(),
        events: POLLIN,
        revents: 0,
    };
    let count = result(syscall5(
        SYS_POLL,
        (&mut poll as *mut PollFd) as usize,
        1,
        0,
        0,
        0,
    ))?;
    if count != 0 || poll.revents != 0 {
        return Err(io::Error::other("channel peer is no longer alive"));
    }
    Ok(())
}

pub(super) struct Sender {
    pub credentials: Credentials,
    pub pidfd: OwnedFd,
}

pub(super) fn receive(stream: &UnixStream, bytes: &mut [u8]) -> io::Result<(usize, Sender)> {
    if bytes.is_empty() {
        return Err(io::Error::other("empty channel receive"));
    }
    let mut control = Control([0; CONTROL]);
    let mut iov = IoVec {
        base: bytes.as_mut_ptr(),
        len: bytes.len(),
    };
    let mut message = MsgHdr {
        name: std::ptr::null_mut(),
        name_len: 0,
        iov: &mut iov,
        iov_len: 1,
        control: control.0.as_mut_ptr(),
        control_len: CONTROL,
        flags: 0,
    };
    let count = result(syscall5(
        SYS_RECVMSG,
        stream.as_raw_fd() as usize,
        (&mut message as *mut MsgHdr) as usize,
        MSG_CMSG_CLOEXEC,
        0,
        0,
    ))?;
    let records = harvest(
        control
            .0
            .get(..message.control_len.min(CONTROL))
            .unwrap_or(&[]),
    );
    admit(count, message.control_len, message.flags, records)
}

fn admit(
    count: usize,
    control_len: usize,
    flags: i32,
    records: (Option<Credentials>, Vec<OwnedFd>, bool),
) -> io::Result<(usize, Sender)> {
    let (credentials, mut pidfds, valid) = records;
    if count == 0 {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "channel disconnected",
        ));
    }
    if !valid || flags & MSG_CTRUNC != 0 || control_len > CONTROL {
        return Err(io::Error::other(
            "invalid or truncated channel ancillary data",
        ));
    }
    let credentials = credentials.ok_or_else(|| io::Error::other("missing sender credentials"))?;
    if pidfds.len() != 1 {
        return Err(io::Error::other("channel needs one sender pidfd"));
    }
    let pidfd = pidfds
        .pop()
        .ok_or_else(|| io::Error::other("missing sender pidfd"))?;
    Ok((count, Sender { credentials, pidfd }))
}

fn word(bytes: &[u8], at: usize) -> Option<i32> {
    bytes
        .get(at..at.checked_add(4)?)?
        .try_into()
        .ok()
        .map(i32::from_ne_bytes)
}

fn harvest(bytes: &[u8]) -> (Option<Credentials>, Vec<OwnedFd>, bool) {
    let mut credentials = None;
    let mut pidfds = Vec::new();
    let mut valid = true;
    let mut at = 0usize;
    while at < bytes.len() {
        let Some(head) = bytes.get(at..at.saturating_add(HEADER)) else {
            valid = false;
            break;
        };
        let Some(length) = head
            .get(..8)
            .and_then(|b| b.try_into().ok())
            .map(usize::from_ne_bytes)
        else {
            valid = false;
            break;
        };
        let Some(end) = at
            .checked_add(length)
            .filter(|end| length >= HEADER && *end <= bytes.len())
        else {
            valid = false;
            break;
        };
        let Some(payload) = bytes.get(at + HEADER..end) else {
            valid = false;
            break;
        };
        let kind = word(head, 12);
        if word(head, 8) != Some(SOL_SOCKET as i32) {
            valid = false;
        } else if matches!(kind, Some(SCM_RIGHTS | SCM_PIDFD)) {
            if kind == Some(SCM_RIGHTS) {
                valid = false;
            }
            if payload.len() % 4 != 0 {
                valid = false;
            }
            if kind == Some(SCM_PIDFD) && payload.len() != 4 {
                valid = false;
            }
            for raw in payload.as_chunks::<4>().0 {
                let Some(fd) = word(raw, 0).filter(|fd| *fd >= 0) else {
                    valid = false;
                    continue;
                };
                let fd = adopt(fd);
                if kind == Some(SCM_PIDFD) {
                    pidfds.push(fd);
                } else {
                    valid = false;
                    drop(fd);
                }
            }
        } else if kind == Some(SCM_CREDENTIALS) {
            if payload.len() == 12 {
                let parsed = word(payload, 0).filter(|pid| *pid > 0).and_then(|pid| {
                    Some(Credentials {
                        pid,
                        uid: word(payload, 4)? as u32,
                        gid: word(payload, 8)? as u32,
                    })
                });
                match parsed {
                    Some(value) if credentials.is_none() => credentials = Some(value),
                    _ => valid = false,
                }
            } else {
                valid = false;
            }
        } else {
            valid = false;
        }
        let Some(next) = end.checked_add(7).map(|n| n & !7) else {
            valid = false;
            break;
        };
        at = next.min(bytes.len());
    }
    (credentials, pidfds, valid)
}

#[cfg(test)]
#[path = "../tests/sys.rs"]
mod tests;

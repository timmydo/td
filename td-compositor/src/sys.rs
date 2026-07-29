use std::fmt;
use std::fs::File;
use std::io;
use std::io::Write;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;

const SYS_CLOSE: usize = 3;
const SYS_SENDMSG: usize = 46;
const SYS_RECVMSG: usize = 47;
const SOL_SOCKET: i32 = 1;
const SCM_RIGHTS: i32 = 1;
const MSG_CTRUNC: i32 = 0x08;
const MSG_CMSG_CLOEXEC: i32 = 0x4000_0000;
const ERRNO_EINTR: isize = -4;
#[cfg(test)]
const ERRNO_ECONNABORTED: isize = -103;
#[cfg(test)]
const ERRNO_ECONNRESET: isize = -104;
const CMSG_HEADER: usize = 16;
const CMSG_ALIGN: usize = 8;
const CONTROL_CAPACITY: usize = 1024;

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

fn raw_errno(value: isize) -> Option<io::Error> {
    if value >= 0 {
        None
    } else {
        let raw = value
            .checked_neg()
            .and_then(|number| i32::try_from(number).ok())
            .unwrap_or(i32::MAX);
        Some(io::Error::from_raw_os_error(raw))
    }
}

fn errno_result(value: isize, operation: &str) -> Result<usize, String> {
    if let Some(error) = raw_errno(value) {
        return Err(format!("{operation}: {error}"));
    }
    usize::try_from(value).map_err(|_| format!("{operation}: invalid result {value}"))
}

#[allow(unsafe_code)]
fn syscall3(number: usize, a1: usize, a2: usize, a3: usize) -> isize {
    let result: isize;
    unsafe {
        core::arch::asm!(
            "syscall",
            inlateout("rax") number as isize => result,
            in("rdi") a1,
            in("rsi") a2,
            in("rdx") a3,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack, preserves_flags),
        );
    }
    result
}

fn close_raw(fd: RawFd) -> Result<(), String> {
    if fd < 0 {
        return Err(format!("refusing to close invalid descriptor {fd}"));
    }
    errno_result(syscall3(SYS_CLOSE, fd as usize, 0, 0), "close")?;
    Ok(())
}

fn align_cmsg(value: usize) -> Result<usize, String> {
    value
        .checked_add(CMSG_ALIGN - 1)
        .map(|sum| sum & !(CMSG_ALIGN - 1))
        .ok_or_else(|| "ancillary length overflow".to_string())
}

fn read_usize(bytes: &[u8]) -> Result<usize, String> {
    let raw: [u8; std::mem::size_of::<usize>()] = bytes
        .get(..std::mem::size_of::<usize>())
        .ok_or_else(|| "truncated ancillary usize".to_string())?
        .try_into()
        .map_err(|_| "truncated ancillary usize".to_string())?;
    Ok(usize::from_ne_bytes(raw))
}

fn read_i32(bytes: &[u8]) -> Result<i32, String> {
    let raw: [u8; 4] = bytes
        .get(..4)
        .ok_or_else(|| "truncated ancillary i32".to_string())?
        .try_into()
        .map_err(|_| "truncated ancillary i32".to_string())?;
    Ok(i32::from_ne_bytes(raw))
}

fn close_all(fds: &[RawFd]) {
    for fd in fds {
        let _ = close_raw(*fd);
    }
}

fn parse_fds(control: &[u8]) -> Result<Vec<RawFd>, String> {
    let mut fds = Vec::new();
    let mut offset = 0usize;
    while offset < control.len() {
        let remaining = control
            .get(offset..)
            .ok_or_else(|| "ancillary offset escaped buffer".to_string())?;
        if remaining.len() < CMSG_HEADER {
            if remaining.iter().all(|byte| *byte == 0) {
                break;
            }
            close_all(&fds);
            return Err("truncated ancillary header".into());
        }
        let length = match read_usize(remaining) {
            Ok(value) => value,
            Err(error) => {
                close_all(&fds);
                return Err(error);
            }
        };
        if length < CMSG_HEADER || length > remaining.len() {
            close_all(&fds);
            return Err(format!("invalid ancillary length {length}"));
        }
        let level = match remaining
            .get(8..12)
            .ok_or_else(|| "missing cmsg level".to_string())
        {
            Ok(bytes) => match read_i32(bytes) {
                Ok(value) => value,
                Err(error) => {
                    close_all(&fds);
                    return Err(error);
                }
            },
            Err(error) => {
                close_all(&fds);
                return Err(error);
            }
        };
        let kind = match remaining
            .get(12..16)
            .ok_or_else(|| "missing cmsg type".to_string())
        {
            Ok(bytes) => match read_i32(bytes) {
                Ok(value) => value,
                Err(error) => {
                    close_all(&fds);
                    return Err(error);
                }
            },
            Err(error) => {
                close_all(&fds);
                return Err(error);
            }
        };
        if level != SOL_SOCKET || kind != SCM_RIGHTS {
            close_all(&fds);
            return Err(format!(
                "unsupported ancillary message level={level} type={kind}"
            ));
        }
        let data = match remaining.get(CMSG_HEADER..length) {
            Some(value) => value,
            None => {
                close_all(&fds);
                return Err("ancillary data escaped message".into());
            }
        };
        if data.is_empty() || data.len() % 4 != 0 {
            close_all(&fds);
            return Err(format!("invalid SCM_RIGHTS payload length {}", data.len()));
        }
        for raw in data.as_chunks::<4>().0 {
            match read_i32(raw) {
                Ok(fd) if fd >= 0 => fds.push(fd),
                Ok(fd) => {
                    close_all(&fds);
                    return Err(format!("received invalid descriptor {fd}"));
                }
                Err(error) => {
                    close_all(&fds);
                    return Err(error);
                }
            }
        }
        let advance = match align_cmsg(length) {
            Ok(value) => value,
            Err(error) => {
                close_all(&fds);
                return Err(error);
            }
        };
        offset = match offset.checked_add(advance) {
            Some(value) => value,
            None => {
                close_all(&fds);
                return Err("ancillary offset overflow".into());
            }
        };
        if offset > control.len() {
            if length == remaining.len() {
                break;
            }
            close_all(&fds);
            return Err("aligned ancillary message escaped buffer".into());
        }
    }
    Ok(fds)
}

pub struct Received {
    pub count: usize,
    pub fds: Vec<RawFd>,
}

#[derive(Debug, Eq, PartialEq)]
pub enum ReceiveError {
    Disconnected,
    Failure(String),
}

impl fmt::Display for ReceiveError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ReceiveError::Disconnected => formatter.write_str("recvmsg: Wayland peer disconnected"),
            ReceiveError::Failure(error) => formatter.write_str(error),
        }
    }
}

pub fn write_peer_disconnected(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::BrokenPipe | io::ErrorKind::ConnectionReset
    )
}

fn receive_result(value: isize) -> Result<usize, ReceiveError> {
    if let Some(error) = raw_errno(value) {
        if error.kind() == io::ErrorKind::ConnectionReset {
            return Err(ReceiveError::Disconnected);
        }
        return Err(ReceiveError::Failure(format!("recvmsg: {error}")));
    }
    usize::try_from(value)
        .map_err(|_| ReceiveError::Failure(format!("recvmsg: invalid result {value}")))
}

pub fn recv_with_fds(stream: &UnixStream, bytes: &mut [u8]) -> Result<Received, ReceiveError> {
    if bytes.is_empty() {
        return Err(ReceiveError::Failure("recv buffer is empty".into()));
    }
    let mut control = [0u8; CONTROL_CAPACITY];
    let mut iov = IoVec {
        base: bytes.as_mut_ptr(),
        len: bytes.len(),
    };
    let mut message = MsgHdr {
        name: std::ptr::null_mut(),
        name_len: 0,
        iov: &mut iov,
        iov_len: 1,
        control: control.as_mut_ptr(),
        control_len: control.len(),
        flags: 0,
    };
    let count = loop {
        let result = syscall3(
            SYS_RECVMSG,
            stream.as_raw_fd() as usize,
            (&mut message as *mut MsgHdr) as usize,
            MSG_CMSG_CLOEXEC as usize,
        );
        if result != ERRNO_EINTR {
            break receive_result(result)?;
        }
    };
    let control_len = message.control_len.min(control.len());
    let fds =
        parse_fds(control.get(..control_len).ok_or_else(|| {
            ReceiveError::Failure("kernel returned invalid ancillary length".into())
        })?)
        .map_err(ReceiveError::Failure)?;
    if message.flags & MSG_CTRUNC != 0 {
        close_all(&fds);
        return Err(ReceiveError::Failure(
            "ancillary descriptor data was truncated".into(),
        ));
    }
    Ok(Received { count, fds })
}

pub fn send_with_fd(stream: &UnixStream, bytes: &[u8], fd: RawFd) -> Result<(), String> {
    if bytes.is_empty() {
        return Err("refusing descriptor-only Wayland message".into());
    }
    if fd < 0 {
        return Err(format!("refusing to send invalid descriptor {fd}"));
    }
    let mut control = [0u8; 24];
    let cmsg_len = 20usize;
    let len_bytes = cmsg_len.to_ne_bytes();
    control
        .get_mut(..8)
        .ok_or_else(|| "control header is too small".to_string())?
        .copy_from_slice(&len_bytes);
    control
        .get_mut(8..12)
        .ok_or_else(|| "control header is too small".to_string())?
        .copy_from_slice(&SOL_SOCKET.to_ne_bytes());
    control
        .get_mut(12..16)
        .ok_or_else(|| "control header is too small".to_string())?
        .copy_from_slice(&SCM_RIGHTS.to_ne_bytes());
    control
        .get_mut(16..20)
        .ok_or_else(|| "control data is too small".to_string())?
        .copy_from_slice(&fd.to_ne_bytes());

    let mut iov = IoVec {
        base: bytes.as_ptr() as *mut u8,
        len: bytes.len(),
    };
    let message = MsgHdr {
        name: std::ptr::null_mut(),
        name_len: 0,
        iov: &mut iov,
        iov_len: 1,
        control: control.as_mut_ptr(),
        control_len: control.len(),
        flags: 0,
    };
    let sent = errno_result(
        syscall3(
            SYS_SENDMSG,
            stream.as_raw_fd() as usize,
            (&message as *const MsgHdr) as usize,
            0,
        ),
        "sendmsg",
    )?;
    if sent == 0 || sent > bytes.len() {
        return Err(format!("sendmsg returned invalid byte count {sent}"));
    }
    if sent < bytes.len() {
        let tail = bytes
            .get(sent..)
            .ok_or_else(|| "sendmsg byte count escaped message".to_string())?;
        let mut borrowed = stream;
        borrowed
            .write_all(tail)
            .map_err(|e| format!("write Wayland message tail: {e}"))?;
    }
    Ok(())
}

pub fn duplicate_received(fd: RawFd) -> Result<File, String> {
    if fd < 0 {
        return Err(format!("invalid received descriptor {fd}"));
    }
    let result =
        File::open(format!("/proc/self/fd/{fd}")).map_err(|e| format!("duplicate fd {fd}: {e}"));
    let close = close_raw(fd);
    match (result, close) {
        (Ok(file), Ok(())) => Ok(file),
        (Err(error), Ok(())) => Err(error),
        (Ok(_), Err(error)) => Err(error),
        (Err(open), Err(close)) => Err(format!("{open}; {close}")),
    }
}

pub fn discard_received(fds: &[RawFd]) {
    close_all(fds);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Read;
    use std::os::fd::AsRawFd;
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQ: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn descriptor_round_trip_preserves_bytes_and_file() {
        let (left, right) = UnixStream::pair().unwrap();
        let path = std::env::temp_dir().join(format!(
            "td-compositor-fd-test-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        fs::write(&path, b"pixels").unwrap();
        let source = File::open(&path).unwrap();
        send_with_fd(&left, b"wayland", source.as_raw_fd()).unwrap();
        let mut bytes = [0u8; 32];
        let received = recv_with_fds(&right, &mut bytes).unwrap();
        assert_eq!(received.count, 7);
        assert_eq!(bytes.get(..7).unwrap(), b"wayland");
        assert_eq!(received.fds.len(), 1);
        let mut duplicate = duplicate_received(*received.fds.first().unwrap()).unwrap();
        let mut content = Vec::new();
        duplicate.read_to_end(&mut content).unwrap();
        assert_eq!(content, b"pixels");
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn write_peer_departure_error_kinds_are_explicit() {
        for kind in [io::ErrorKind::BrokenPipe, io::ErrorKind::ConnectionReset] {
            assert!(write_peer_disconnected(&io::Error::from(kind)), "{kind:?}");
        }
        for kind in [
            io::ErrorKind::ConnectionAborted,
            io::ErrorKind::NotConnected,
            io::ErrorKind::PermissionDenied,
            io::ErrorKind::UnexpectedEof,
        ] {
            assert!(!write_peer_disconnected(&io::Error::from(kind)), "{kind:?}");
        }
    }

    #[test]
    fn recvmsg_connection_reset_is_a_typed_disconnect() {
        assert_eq!(
            receive_result(ERRNO_ECONNRESET),
            Err(ReceiveError::Disconnected)
        );
        assert!(matches!(
            receive_result(ERRNO_ECONNABORTED),
            Err(ReceiveError::Failure(_))
        ));
    }
}

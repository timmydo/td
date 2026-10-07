//! `SCM_RIGHTS` over a borrowed `UnixStream`: the message headers, the
//! bounded ancillary parser and the retry loops around one `recvmsg` and one
//! `sendmsg`. No raw surface of its own: it is a child of a raw module, which
//! supplies `recvmsg`, `sendmsg` and `close_raw` (and with them its own
//! flags), `raw_errno` and its `CONTROL_CAPACITY`. td-secret's `sys.rs`
//! (UNSAFE.md §12) and td-compositor's (§6) both mount this one file.

use super::{close_raw, raw_errno, recvmsg, sendmsg, CONTROL_CAPACITY};
use std::fmt;
use std::io::{self, Write};
use std::os::fd::RawFd;
use std::os::unix::net::UnixStream;

const SOL_SOCKET: i32 = 1;
const SCM_RIGHTS: i32 = 1;
const MSG_CTRUNC: i32 = 0x08;
const ERRNO_EINTR: isize = -4;
const CMSG_HEADER: usize = 16;
const CMSG_ALIGN: usize = 8;

#[repr(align(8))]
struct ControlBuffer<const N: usize>([u8; N]);

#[repr(C)]
struct IoVec {
    base: *mut u8,
    len: usize,
}

#[repr(C)]
pub(super) struct MsgHdr {
    name: *mut u8,
    name_len: u32,
    iov: *mut IoVec,
    iov_len: usize,
    control: *mut u8,
    control_len: usize,
    flags: i32,
}

fn close_all(fds: &[RawFd]) {
    for fd in fds {
        let _ = close_raw(*fd);
    }
}

/// Closes descriptor numbers a `Received` carried that nothing adopted.
pub fn discard_received(fds: &[RawFd]) {
    close_all(fds);
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

/// Every `SCM_RIGHTS` descriptor in `control`, or every one closed and the
/// first refusal. A content refusal is recorded while the framing stays
/// walkable, so later rights are found and closed; a structural error
/// returns at once, closing what was collected.
fn parse_fds(control: &[u8]) -> Result<Vec<RawFd>, String> {
    let mut fds = Vec::new();
    let mut refusal = None;
    let mut offset = 0usize;
    while offset < control.len() {
        let remaining = match control.get(offset..) {
            Some(value) => value,
            None => {
                close_all(&fds);
                return Err("ancillary offset escaped buffer".into());
            }
        };
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
        let level = match remaining.get(8..12).map(read_i32) {
            Some(Ok(value)) => value,
            Some(Err(error)) => {
                close_all(&fds);
                return Err(error);
            }
            None => {
                close_all(&fds);
                return Err("missing cmsg level".into());
            }
        };
        let kind = match remaining.get(12..16).map(read_i32) {
            Some(Ok(value)) => value,
            Some(Err(error)) => {
                close_all(&fds);
                return Err(error);
            }
            None => {
                close_all(&fds);
                return Err("missing cmsg type".into());
            }
        };
        let Some(data) = remaining.get(CMSG_HEADER..length) else {
            close_all(&fds);
            return Err("ancillary data escaped message".into());
        };
        if level != SOL_SOCKET || kind != SCM_RIGHTS {
            refusal.get_or_insert_with(|| {
                format!("unsupported ancillary message level={level} type={kind}")
            });
        } else {
            if data.is_empty() || data.len() % 4 != 0 {
                refusal.get_or_insert_with(|| {
                    format!("invalid SCM_RIGHTS payload length {}", data.len())
                });
            }
            for raw in data.as_chunks::<4>().0 {
                let fd = i32::from_ne_bytes(*raw);
                if fd >= 0 {
                    fds.push(fd);
                } else {
                    refusal.get_or_insert_with(|| format!("received invalid descriptor {fd}"));
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
    if let Some(error) = refusal {
        close_all(&fds);
        Err(error)
    } else {
        Ok(fds)
    }
}

pub struct Received {
    pub count: usize,
    pub fds: Vec<RawFd>,
}

#[derive(Debug, Eq, PartialEq)]
pub enum ReceiveError {
    Disconnected,
    TimedOut,
    Failure(String),
}

impl fmt::Display for ReceiveError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Disconnected => formatter.write_str("recvmsg: peer disconnected"),
            Self::TimedOut => formatter.write_str("recvmsg: receive timed out"),
            Self::Failure(error) => formatter.write_str(error),
        }
    }
}

fn receive_result(value: isize) -> Result<usize, ReceiveError> {
    if let Some(error) = raw_errno(value) {
        if error.kind() == io::ErrorKind::ConnectionReset {
            return Err(ReceiveError::Disconnected);
        }
        if matches!(
            error.kind(),
            io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
        ) {
            return Err(ReceiveError::TimedOut);
        }
        return Err(ReceiveError::Failure(format!("recvmsg: {error}")));
    }
    usize::try_from(value)
        .map_err(|_| ReceiveError::Failure(format!("recvmsg: invalid result {value}")))
}

/// One message into `bytes` and the descriptors that came with it, parsed
/// from at most `CONTROL_CAPACITY` ancillary bytes. Truncated ancillary data
/// is refused, its descriptors closed.
pub fn recv_with_fds(stream: &UnixStream, bytes: &mut [u8]) -> Result<Received, ReceiveError> {
    if bytes.is_empty() {
        return Err(ReceiveError::Failure("recv buffer is empty".into()));
    }
    let mut control = ControlBuffer([0u8; CONTROL_CAPACITY]);
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
        control_len: control.0.len(),
        flags: 0,
    };
    let count = loop {
        let result = recvmsg(stream, &mut message);
        if result != ERRNO_EINTR {
            break receive_result(result)?;
        }
    };
    let control_len = message.control_len.min(control.0.len());
    let fds = parse_fds(
        control
            .0
            .get(..control_len)
            .ok_or_else(|| ReceiveError::Failure("invalid ancillary length".into()))?,
    )
    .map_err(ReceiveError::Failure)?;
    if message.flags & MSG_CTRUNC != 0 {
        close_all(&fds);
        return Err(ReceiveError::Failure(
            "ancillary descriptor data was truncated".into(),
        ));
    }
    Ok(Received { count, fds })
}

fn sendmsg_result(value: isize) -> io::Result<Option<usize>> {
    if let Some(error) = raw_errno(value) {
        return if error.kind() == io::ErrorKind::Interrupted {
            Ok(None)
        } else {
            Err(error)
        };
    }
    usize::try_from(value)
        .map(Some)
        .map_err(|_| io::Error::other(format!("sendmsg returned invalid result {value}")))
}

/// `bytes` with `fd`, the descriptor sent once with the head and any tail
/// through the safe stream.
pub fn send_with_fd(stream: &UnixStream, bytes: &[u8], fd: RawFd) -> io::Result<()> {
    let sent = send_prefix_with_fd(stream, bytes, fd)?;
    if sent < bytes.len() {
        let tail = bytes
            .get(sent..)
            .ok_or_else(|| io::Error::other("sendmsg byte count escaped message"))?;
        let mut borrowed = stream;
        borrowed.write_all(tail)?;
    }
    Ok(())
}

/// The one `sendmsg` that carries `fd` with the head of `bytes`, returning how
/// many bytes it took. A caller that bounds its writes in time finishes the
/// tail itself; the descriptor has left once this returns a count.
pub fn send_prefix_with_fd(stream: &UnixStream, bytes: &[u8], fd: RawFd) -> io::Result<usize> {
    if bytes.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "refusing a descriptor-only message",
        ));
    }
    if fd < 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("refusing to send invalid descriptor {fd}"),
        ));
    }
    let mut control = ControlBuffer([0u8; 24]);
    control
        .0
        .get_mut(..8)
        .ok_or_else(|| io::Error::other("control length field is absent"))?
        .copy_from_slice(&20usize.to_ne_bytes());
    control
        .0
        .get_mut(8..12)
        .ok_or_else(|| io::Error::other("control level field is absent"))?
        .copy_from_slice(&SOL_SOCKET.to_ne_bytes());
    control
        .0
        .get_mut(12..16)
        .ok_or_else(|| io::Error::other("control type field is absent"))?
        .copy_from_slice(&SCM_RIGHTS.to_ne_bytes());
    control
        .0
        .get_mut(16..20)
        .ok_or_else(|| io::Error::other("control descriptor field is absent"))?
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
        control: control.0.as_mut_ptr(),
        control_len: control.0.len(),
        flags: 0,
    };
    let sent = loop {
        if let Some(sent) = sendmsg_result(sendmsg(stream, &message))? {
            break sent;
        }
    };
    if sent == 0 || sent > bytes.len() {
        return Err(io::Error::other(format!(
            "sendmsg returned invalid byte count {sent}"
        )));
    }
    Ok(sent)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{File, OpenOptions};
    use std::os::fd::{AsRawFd, IntoRawFd};
    use std::os::unix::fs::MetadataExt;

    /// Which file this is, in terms that survive being unlinked.
    fn file_identity(file: &File) -> (u64, u64) {
        let metadata = file.metadata().unwrap();
        (metadata.dev(), metadata.ino())
    }

    /// Which file descriptor NUMBER `raw` names now, if any.
    ///
    /// `stat`, never `open`: once a number is closed it belongs to the process
    /// again and a parallel test may hold it, so it can name anything that
    /// suite has open — a socket in td-secret's, a character device in td-compositor's.
    /// Opening one of those can block and reading one can never end. Stat
    /// opens nothing and reads nothing, so it can do neither, and it answers
    /// the only question a closed descriptor raises.
    fn identity_of_number(raw: RawFd) -> Option<(u64, u64)> {
        std::fs::metadata(format!("/proc/self/fd/{raw}"))
            .ok()
            .map(|metadata| (metadata.dev(), metadata.ino()))
    }

    fn rejected_control_closes_its_descriptors(
        name: &str,
        mut control: Vec<u8>,
        fd_offsets: &[usize],
    ) -> String {
        let mut handed_over = Vec::new();
        for (index, fd_offset) in fd_offsets.iter().enumerate() {
            let file = tempfile(&format!("{name}-{index}"));
            let identity = file_identity(&file);
            // A second handle, kept until the check is done. The parser owns
            // and closes `raw`, which would drop the last reference to an
            // already-unlinked file and free its inode — and a freed inode
            // number can be handed straight to the next file created, which is
            // the one thing that could make this identity name something else.
            let pin = file.try_clone().unwrap();
            let raw = file.into_raw_fd();
            control[*fd_offset..*fd_offset + 4].copy_from_slice(&raw.to_ne_bytes());
            handed_over.push((raw, identity, pin));
        }

        let error = parse_fds(&control).unwrap_err();
        for (raw, identity, pin) in handed_over {
            // A parallel test may already hold `raw`, so its availability
            // settles nothing. What must be true is that the number no longer
            // names the file the parser had to close.
            // The pin is a second handle on the same file, so its own number
            // must still name that file. A negative below means nothing if the
            // oracle is mute or answers with somebody else's.
            assert_eq!(
                identity_of_number(pin.as_raw_fd()),
                Some(identity),
                "the pin must still name its own file"
            );
            assert_ne!(
                identity_of_number(raw),
                Some(identity),
                "the parser must close the descriptor it refused"
            );
            drop(pin);
        }
        error
    }

    #[test]
    fn malformed_ancillary_headers_are_bounded() {
        assert!(parse_fds(&[1; CMSG_HEADER - 1]).is_err());
        let mut header = [0u8; CMSG_HEADER];
        header[..8].copy_from_slice(&15usize.to_ne_bytes());
        assert!(parse_fds(&header).is_err());
    }

    #[test]
    fn unsupported_ancillary_still_closes_later_rights() {
        let mut control = vec![0u8; 40];
        control[..8].copy_from_slice(&16usize.to_ne_bytes());
        control[8..12].copy_from_slice(&SOL_SOCKET.to_ne_bytes());
        control[12..16].copy_from_slice(&99i32.to_ne_bytes());
        control[16..24].copy_from_slice(&20usize.to_ne_bytes());
        control[24..28].copy_from_slice(&SOL_SOCKET.to_ne_bytes());
        control[28..32].copy_from_slice(&SCM_RIGHTS.to_ne_bytes());
        assert_eq!(
            rejected_control_closes_its_descriptors("unsupported", control, &[32]),
            "unsupported ancillary message level=1 type=99"
        );
    }

    #[test]
    fn invalid_rights_payload_still_closes_later_rights() {
        let mut control = vec![0u8; 48];
        control[..8].copy_from_slice(&17usize.to_ne_bytes());
        control[8..12].copy_from_slice(&SOL_SOCKET.to_ne_bytes());
        control[12..16].copy_from_slice(&SCM_RIGHTS.to_ne_bytes());
        control[24..32].copy_from_slice(&20usize.to_ne_bytes());
        control[32..36].copy_from_slice(&SOL_SOCKET.to_ne_bytes());
        control[36..40].copy_from_slice(&SCM_RIGHTS.to_ne_bytes());
        assert_eq!(
            rejected_control_closes_its_descriptors("width", control, &[40]),
            "invalid SCM_RIGHTS payload length 1"
        );
    }

    #[test]
    fn invalid_descriptor_still_closes_later_rights() {
        let mut control = vec![0u8; 48];
        control[..8].copy_from_slice(&20usize.to_ne_bytes());
        control[8..12].copy_from_slice(&SOL_SOCKET.to_ne_bytes());
        control[12..16].copy_from_slice(&SCM_RIGHTS.to_ne_bytes());
        control[16..20].copy_from_slice(&(-1i32).to_ne_bytes());
        control[24..32].copy_from_slice(&20usize.to_ne_bytes());
        control[32..36].copy_from_slice(&SOL_SOCKET.to_ne_bytes());
        control[36..40].copy_from_slice(&SCM_RIGHTS.to_ne_bytes());
        assert_eq!(
            rejected_control_closes_its_descriptors("negative", control, &[40]),
            "received invalid descriptor -1"
        );
    }

    #[test]
    fn invalid_descriptor_between_rights_closes_both_neighbors() {
        let mut control = vec![0u8; 32];
        control[..8].copy_from_slice(&28usize.to_ne_bytes());
        control[8..12].copy_from_slice(&SOL_SOCKET.to_ne_bytes());
        control[12..16].copy_from_slice(&SCM_RIGHTS.to_ne_bytes());
        control[20..24].copy_from_slice(&(-1i32).to_ne_bytes());
        assert_eq!(
            rejected_control_closes_its_descriptors("neighbors", control, &[16, 24]),
            "received invalid descriptor -1"
        );
    }

    #[test]
    fn the_first_ancillary_refusal_is_the_diagnostic() {
        let mut control = [0u8; 32];
        control[..8].copy_from_slice(&16usize.to_ne_bytes());
        control[8..12].copy_from_slice(&SOL_SOCKET.to_ne_bytes());
        control[12..16].copy_from_slice(&99i32.to_ne_bytes());
        control[16..24].copy_from_slice(&16usize.to_ne_bytes());
        control[24..28].copy_from_slice(&SOL_SOCKET.to_ne_bytes());
        control[28..32].copy_from_slice(&100i32.to_ne_bytes());
        assert_eq!(
            parse_fds(&control).unwrap_err(),
            "unsupported ancillary message level=1 type=99"
        );
    }

    #[test]
    fn structural_error_overrides_a_pending_content_refusal() {
        let mut control = [0u8; 20];
        control[..8].copy_from_slice(&16usize.to_ne_bytes());
        control[8..12].copy_from_slice(&SOL_SOCKET.to_ne_bytes());
        control[12..16].copy_from_slice(&99i32.to_ne_bytes());
        control[16..].fill(1);
        assert_eq!(
            parse_fds(&control).unwrap_err(),
            "truncated ancillary header"
        );
    }

    fn tempfile(name: &str) -> File {
        let path = std::env::temp_dir().join(format!("td-scm-{name}-{}", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap();
        std::fs::remove_file(path).unwrap();
        file
    }

    const ERRNO_EBADF: isize = -9;
    const ERRNO_EAGAIN: isize = -11;
    const ERRNO_ECONNABORTED: isize = -103;
    const ERRNO_ECONNRESET: isize = -104;

    #[test]
    fn interrupted_sendmsg_is_retried_but_other_errors_are_not() {
        assert_eq!(sendmsg_result(ERRNO_EINTR).unwrap(), None);
        assert!(sendmsg_result(ERRNO_EBADF).is_err());
        assert_eq!(sendmsg_result(7).unwrap(), Some(7));
    }

    #[test]
    fn ancillary_buffers_are_aligned_for_message_headers() {
        let control = ControlBuffer([0u8; 24]);
        assert_eq!((control.0.as_ptr() as usize) % CMSG_ALIGN, 0);
    }

    #[test]
    fn recvmsg_connection_reset_is_a_typed_disconnect() {
        assert_eq!(
            receive_result(ERRNO_ECONNRESET),
            Err(ReceiveError::Disconnected)
        );
        assert_eq!(receive_result(ERRNO_EAGAIN), Err(ReceiveError::TimedOut));
        assert!(matches!(
            receive_result(ERRNO_ECONNABORTED),
            Err(ReceiveError::Failure(_))
        ));
    }
}

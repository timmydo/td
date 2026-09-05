//! Linux x86-64 descriptor transport; roster section 14.

use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;

#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
compile_error!("the editor window transport requires Linux x86-64");

const SYS_SENDMSG: usize = 46;
const SYS_RECVMSG: usize = 47;
const SYS_FCNTL: usize = 72;
const F_DUPFD_CLOEXEC: usize = 1030;
const SOL_SOCKET: i32 = 1;
const SCM_RIGHTS: i32 = 1;
const MSG_CTRUNC: i32 = 8;
const MSG_NOSIGNAL: usize = 0x4000;
const MSG_CMSG_CLOEXEC: usize = 0x4000_0000;
const HEADER: usize = 16;
const CONTROL: usize = 128;

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

const _: [(); 16] = [(); std::mem::size_of::<IoVec>()];
const _: [(); 56] = [(); std::mem::size_of::<MsgHdr>()];
const _: [(); 48] = [(); std::mem::offset_of!(MsgHdr, flags)];

#[allow(unsafe_code)]
fn syscall3(number: usize, a1: usize, a2: usize, a3: usize) -> isize {
    let result: isize;
    // SAFETY: private callers keep every named allocation live for the call.
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

#[allow(unsafe_code)]
fn adopt(fd: i32) -> OwnedFd {
    // SAFETY: only freshly installed, nonnegative kernel descriptors reach
    // this private site. No other owner has been constructed for them.
    unsafe { OwnedFd::from_raw_fd(fd) }
}

fn result(value: isize) -> io::Result<usize> {
    if value < 0 {
        return Err(io::Error::from_raw_os_error((-value) as i32));
    }
    Ok(value as usize)
}

/// Duplicate, never adopt or close, the environment's borrowed descriptor.
pub(super) fn inherited(fd: i32) -> io::Result<UnixStream> {
    if fd < 3 {
        return Err(io::Error::other(
            "WAYLAND_SOCKET must name a descriptor >= 3",
        ));
    }
    let value = result(syscall3(SYS_FCNTL, fd as usize, F_DUPFD_CLOEXEC, 3))?;
    // F_DUPFD_CLOEXEC returns a new int descriptor or a negative errno.
    let owned = adopt(value as i32);
    let stream = UnixStream::from(owned);
    stream.peer_addr()?;
    Ok(stream)
}

fn header(iov: &mut IoVec, control: &mut Control, length: usize) -> MsgHdr {
    MsgHdr {
        name: std::ptr::null_mut(),
        name_len: 0,
        iov,
        iov_len: 1,
        control: control.0.as_mut_ptr(),
        control_len: length,
        flags: 0,
    }
}

/// No admitted preview event carries a descriptor. Still receive ancillary
/// data so unexpected installed descriptors are closed before refusal.
pub(super) fn receive(stream: &UnixStream, bytes: &mut [u8]) -> io::Result<usize> {
    let (count, fds, ancillary) = receive_packet(stream, bytes)?;
    drop(fds);
    if ancillary {
        return Err(io::Error::other("unexpected Wayland ancillary data"));
    }
    Ok(count)
}

fn receive_packet(
    stream: &UnixStream,
    bytes: &mut [u8],
) -> io::Result<(usize, Vec<OwnedFd>, bool)> {
    if bytes.is_empty() {
        return Err(io::Error::other("empty Wayland receive buffer"));
    }
    let mut control = Control([0; CONTROL]);
    let mut iov = IoVec {
        base: bytes.as_mut_ptr(),
        len: bytes.len(),
    };
    let mut message = header(&mut iov, &mut control, CONTROL);
    let count = result(syscall3(
        SYS_RECVMSG,
        stream.as_raw_fd() as usize,
        (&mut message as *mut MsgHdr) as usize,
        MSG_CMSG_CLOEXEC,
    ))?;
    let used = message.control_len.min(CONTROL);
    let fds = harvest(control.0.get(..used).unwrap_or(&[]));
    if message.flags & MSG_CTRUNC != 0 {
        return Err(io::Error::other("truncated Wayland ancillary data"));
    }
    Ok((count, fds, used != 0))
}

fn harvest(bytes: &[u8]) -> Vec<OwnedFd> {
    let mut fds = Vec::new();
    let mut at = 0usize;
    while let Some(head) = bytes.get(at..at.saturating_add(HEADER)) {
        let Some(length) = head
            .get(..8)
            .and_then(|v| v.try_into().ok())
            .map(usize::from_ne_bytes)
        else {
            break;
        };
        let Some(end) = at
            .checked_add(length)
            .filter(|end| length >= HEADER && *end <= bytes.len())
        else {
            break;
        };
        let word = |offset| {
            head.get(offset..offset + 4)
                .and_then(|v| v.try_into().ok())
                .map(i32::from_ne_bytes)
        };
        if word(8) == Some(SOL_SOCKET) && word(12) == Some(SCM_RIGHTS) {
            if let Some(payload) = bytes.get(at + HEADER..end) {
                for raw in payload.as_chunks::<4>().0 {
                    let fd = i32::from_ne_bytes(*raw);
                    if fd >= 0 {
                        fds.push(adopt(fd));
                    }
                }
            }
        }
        // Continue after unknown ancillary kinds: later rights are installed
        // too. Structural corruption cannot establish another boundary.
        let Some(next) = end.checked_add(7).map(|n| n & !7) else {
            break;
        };
        at = next;
    }
    fds
}

#[cfg(test)]
pub(super) fn receive_for_test(
    stream: &UnixStream,
    bytes: &mut [u8],
) -> io::Result<(usize, Vec<OwnedFd>)> {
    let (count, fds, _) = receive_packet(stream, bytes)?;
    Ok((count, fds))
}

/// Send exactly one borrowed pool file. A short write's suffix carries no fd.
pub(super) fn send_pool(stream: &UnixStream, bytes: &[u8], file: &File) -> io::Result<usize> {
    if bytes.is_empty() {
        return Err(io::Error::other("empty pool request"));
    }
    let mut control = Control([0; CONTROL]);
    for (offset, value) in [
        (0, 20u32),
        (8, SOL_SOCKET as u32),
        (12, SCM_RIGHTS as u32),
        (16, file.as_raw_fd() as u32),
    ] {
        if let Some(slot) = control.0.get_mut(offset..offset + 4) {
            slot.copy_from_slice(&value.to_ne_bytes());
        }
    }
    let mut iov = IoVec {
        base: bytes.as_ptr() as *mut u8,
        len: bytes.len(),
    };
    let mut message = header(&mut iov, &mut control, 24);
    result(syscall3(
        SYS_SENDMSG,
        stream.as_raw_fd() as usize,
        (&mut message as *mut MsgHdr) as usize,
        MSG_NOSIGNAL,
    ))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::os::fd::IntoRawFd;

    fn endpoint_file() -> (UnixStream, File) {
        let (a, b) = UnixStream::pair().unwrap();
        a.set_read_timeout(Some(std::time::Duration::from_secs(1)))
            .unwrap();
        (a, File::from(OwnedFd::from(b)))
    }

    #[test]
    fn inherited_fd_is_duplicated_and_original_is_never_closed() {
        let (a, mut b) = UnixStream::pair().unwrap();
        let mut duplicate = inherited(a.as_raw_fd()).unwrap();
        let flags = std::fs::read_to_string(format!("/proc/self/fdinfo/{}", duplicate.as_raw_fd()))
            .unwrap();
        let flags = flags
            .lines()
            .find_map(|l| l.strip_prefix("flags:\t"))
            .unwrap();
        assert_ne!(u32::from_str_radix(flags, 8).unwrap() & 0o2000000, 0);
        duplicate.write_all(b"x").unwrap();
        let mut byte = [0];
        b.read_exact(&mut byte).unwrap();
        assert_eq!(byte, [b'x']);
        drop(duplicate);
        (&a).write_all(b"y").unwrap();
        b.read_exact(&mut byte).unwrap();
        assert_eq!(byte, [b'y']);
        assert!(inherited(-1).is_err());
        assert!(inherited(0).is_err());
        assert!(inherited(i32::MAX).is_err());
        let regular = File::open("/dev/null").unwrap();
        assert!(inherited(regular.as_raw_fd()).is_err());
    }

    #[test]
    fn unexpected_received_descriptor_is_closed_before_error() {
        let (a, b) = UnixStream::pair().unwrap();
        let (mut peer, file) = endpoint_file();
        assert_eq!(send_pool(&a, b"bytes", &file).unwrap(), 5);
        drop(file);
        assert!(receive(&b, &mut [0; 32])
            .unwrap_err()
            .to_string()
            .contains("ancillary"));
        assert_eq!(peer.read(&mut [0]).unwrap(), 0, "received endpoint leaked");
    }

    #[test]
    fn harvest_keeps_walking_after_unknown_kind_and_closes_all_owned_fds() {
        let (mut a, file_a) = endpoint_file();
        let (mut b, file_b) = endpoint_file();
        let mut bytes = vec![0; 64];
        bytes[..8].copy_from_slice(&16usize.to_ne_bytes()); // unknown record
        bytes[16..24].copy_from_slice(&28usize.to_ne_bytes());
        bytes[24..28].copy_from_slice(&SOL_SOCKET.to_ne_bytes());
        bytes[28..32].copy_from_slice(&SCM_RIGHTS.to_ne_bytes());
        bytes[32..36].copy_from_slice(&file_a.into_raw_fd().to_ne_bytes());
        bytes[36..40].copy_from_slice(&(-1i32).to_ne_bytes());
        bytes[40..44].copy_from_slice(&file_b.into_raw_fd().to_ne_bytes());
        bytes[48..56].copy_from_slice(&usize::MAX.to_ne_bytes()); // structural stop
        let fds = harvest(&bytes);
        assert_eq!(fds.len(), 2);
        drop(fds);
        assert_eq!(a.read(&mut [0]).unwrap(), 0);
        assert_eq!(b.read(&mut [0]).unwrap(), 0);
    }

    #[test]
    fn kernel_truncation_closes_both_delivered_and_undelivered_rights() {
        let (a, b) = UnixStream::pair().unwrap();
        let mut peers = Vec::new();
        let mut files = Vec::new();
        for _ in 0..40 {
            let (peer, file) = endpoint_file();
            peers.push(peer);
            files.push(file);
        }
        let mut control = [0u64; 22]; // aligned 176-byte CMSG_LEN(40 fds)
        control[0] = 176;
        control[1] = (SCM_RIGHTS as u64) << 32 | SOL_SOCKET as u64;
        for (i, pair) in files.chunks(2).enumerate() {
            control[i + 2] = pair[0].as_raw_fd() as u64 | (pair[1].as_raw_fd() as u64) << 32;
        }
        let byte = *b"x";
        let mut iov = IoVec {
            base: byte.as_ptr() as *mut u8,
            len: 1,
        };
        let mut message = MsgHdr {
            name: std::ptr::null_mut(),
            name_len: 0,
            iov: &mut iov,
            iov_len: 1,
            control: control.as_mut_ptr() as *mut u8,
            control_len: 176,
            flags: 0,
        };
        assert_eq!(
            result(syscall3(
                SYS_SENDMSG,
                a.as_raw_fd() as usize,
                (&mut message as *mut MsgHdr) as usize,
                MSG_NOSIGNAL
            ))
            .unwrap(),
            1
        );
        drop(files);
        assert!(receive(&b, &mut [0; 8])
            .unwrap_err()
            .to_string()
            .contains("truncated"));
        for mut peer in peers {
            assert_eq!(peer.read(&mut [0]).unwrap(), 0);
        }
    }

    #[test]
    fn byte_only_eof_and_disconnect_are_results() {
        let (mut a, b) = UnixStream::pair().unwrap();
        a.write_all(b"abc").unwrap();
        let mut bytes = [0; 3];
        assert_eq!(receive(&b, &mut bytes).unwrap(), 3);
        assert_eq!(&bytes, b"abc");
        drop(a);
        assert_eq!(receive(&b, &mut bytes).unwrap(), 0);
        assert!(send_pool(&b, b"x", &File::open("/dev/null").unwrap()).is_err());
        assert!(receive(&b, &mut []).is_err());
    }
}

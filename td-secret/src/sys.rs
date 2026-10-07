//! The raw half of the credential-descriptor transport (UNSAFE.md §12):
//! one `syscall` instruction carrying `recvmsg`, `sendmsg` and `close`, and
//! one adoption. The message headers, the ancillary parser and the retry
//! loops are `scm.rs`, a safe child td-compositor's raw module mounts too.

use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::net::UnixStream;

#[path = "scm.rs"]
mod scm;
#[allow(
    unused_imports,
    reason = "td-secret, td-portal and td-open each use part of the transport"
)]
pub use scm::{discard_received, recv_with_fds, send_with_fd, ReceiveError, Received};

const SYS_CLOSE: usize = 3;
const SYS_SENDMSG: usize = 46;
const SYS_RECVMSG: usize = 47;
const MSG_CMSG_CLOEXEC: i32 = 0x4000_0000;
const MSG_NOSIGNAL: i32 = 0x4000;
const CONTROL_CAPACITY: usize = 128;

#[allow(unsafe_code)]
fn syscall5(number: usize, a1: usize, a2: usize, a3: usize, a4: usize, a5: usize) -> isize {
    let result: isize;
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
            options(nostack, preserves_flags),
        );
    }
    result
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

fn close_raw(fd: RawFd) -> Result<(), String> {
    if fd < 0 {
        return Err(format!("refusing to close invalid descriptor {fd}"));
    }
    if let Some(error) = raw_errno(syscall5(SYS_CLOSE, fd as usize, 0, 0, 0, 0)) {
        return Err(format!("close received descriptor: {error}"));
    }
    Ok(())
}

/// `recvmsg(2)` into `message`, every received descriptor close-on-exec.
fn recvmsg(stream: &UnixStream, message: &mut scm::MsgHdr) -> isize {
    syscall5(
        SYS_RECVMSG,
        stream.as_raw_fd() as usize,
        (message as *mut scm::MsgHdr) as usize,
        MSG_CMSG_CLOEXEC as usize,
        0,
        0,
    )
}

/// `sendmsg(2)` of `message`; a departed peer is an error, not a signal.
fn sendmsg(stream: &UnixStream, message: &scm::MsgHdr) -> isize {
    syscall5(
        SYS_SENDMSG,
        stream.as_raw_fd() as usize,
        (message as *const scm::MsgHdr) as usize,
        MSG_NOSIGNAL as usize,
        0,
        0,
    )
}

/// Consume one fresh SCM_RIGHTS descriptor; callers remove its raw owner first.
#[allow(unsafe_code)]
pub fn take_received(fd: RawFd) -> Result<File, String> {
    if fd < 0 {
        return Err(format!("invalid received descriptor {fd}"));
    }
    // SAFETY: callers pass one live descriptor just installed by recvmsg,
    // removed from its sole disposal queue. File now owns its only close.
    Ok(unsafe { File::from_raw_fd(fd) })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::OpenOptions;
    use std::io::Write;
    use std::io::{Read, Seek, SeekFrom};
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
    /// suite has open — a socket here, a character device in td-compositor's.
    /// Opening one of those can block and reading one can never end. Stat
    /// opens nothing and reads nothing, so it can do neither, and it answers
    /// the only question a closed descriptor raises.
    fn identity_of_number(raw: RawFd) -> Option<(u64, u64)> {
        std::fs::metadata(format!("/proc/self/fd/{raw}"))
            .ok()
            .map(|metadata| (metadata.dev(), metadata.ino()))
    }

    #[test]
    fn one_descriptor_crosses_and_is_owned_then_closed() {
        let (sender, receiver) = UnixStream::pair().unwrap();
        let mut file = tempfile("descriptor");
        file.write_all(b"portal").unwrap();
        file.seek(SeekFrom::Start(0)).unwrap();
        send_with_fd(&sender, b"frame", file.as_raw_fd()).unwrap();
        let mut bytes = [0u8; 16];
        let mut received = recv_with_fds(&receiver, &mut bytes).unwrap();
        assert_eq!(&bytes[..received.count], b"frame");
        assert_eq!(received.fds.len(), 1);
        let mut duplicate = take_received(received.fds.pop().unwrap()).unwrap();
        let mut contents = String::new();
        duplicate.read_to_string(&mut contents).unwrap();
        assert_eq!(contents, "portal");
    }

    fn tempfile(name: &str) -> File {
        let path = std::env::temp_dir().join(format!("td-portal-{name}-{}", std::process::id()));
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

    #[test]
    fn received_ownership_preserves_the_descriptor_and_revoked_inode_access() {
        use std::os::unix::fs::{FileExt, PermissionsExt};
        let (sender, receiver) = UnixStream::pair().unwrap();
        let mut file = tempfile("exact-owner");
        file.write_all(b"portal").unwrap();
        file.seek(SeekFrom::Start(3)).unwrap();
        file.set_permissions(std::fs::Permissions::from_mode(0o000))
            .unwrap();
        send_with_fd(&sender, b"x", file.as_raw_fd()).unwrap();
        let mut byte = [0];
        let mut received = recv_with_fds(&receiver, &mut byte).unwrap();
        let raw = received.fds.pop().unwrap();
        let owned = take_received(raw).unwrap();
        assert_eq!(owned.as_raw_fd(), raw);
        assert_eq!(file_identity(&owned), file_identity(&file));
        let mut bytes = [0; 6];
        owned.read_exact_at(&mut bytes, 0).unwrap();
        assert_eq!(&bytes, b"portal");
        assert_eq!(file.stream_position().unwrap(), 3);
        drop(owned);
        assert_ne!(identity_of_number(raw), Some(file_identity(&file)));
        assert!(take_received(-1).is_err());
    }
}

//! Linux account authentication for the local registrar stream.
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;

#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
compile_error!("the VM registrar requires Linux x86-64");

const SYS_GETSOCKOPT: usize = 55;
const SOL_SOCKET: usize = 1;
const SO_PEERCRED: usize = 17;
const UCRED_BYTES: u32 = 12;
const _: () = assert!(std::mem::size_of::<[u32; 3]>() == UCRED_BYTES as usize);

#[allow(unsafe_code)]
pub(super) fn peer_uid(stream: &UnixStream) -> io::Result<u32> {
    let mut credential = [u32::MAX; 3];
    let mut length = UCRED_BYTES;
    let result: isize;
    // SAFETY: the borrowed stream and both correctly sized writable buffers
    // remain live for the synchronous, fixed-option kernel call.
    unsafe {
        core::arch::asm!(
            "syscall",
            inlateout("rax") SYS_GETSOCKOPT as isize => result,
            in("rdi") stream.as_raw_fd() as usize,
            in("rsi") SOL_SOCKET,
            in("rdx") SO_PEERCRED,
            in("r10") credential.as_mut_ptr(),
            in("r8") &mut length as *mut u32,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack, preserves_flags),
        );
    }
    if result != 0 {
        let errno = result.checked_neg().and_then(|n| i32::try_from(n).ok());
        return Err(match errno {
            Some(errno) if errno > 0 => io::Error::from_raw_os_error(errno),
            _ => io::Error::other("invalid peer credential syscall result"),
        });
    }
    decode(credential, length)
}

fn decode(credential: [u32; 3], length: u32) -> io::Result<u32> {
    let [pid, uid, _gid] = credential;
    if length != UCRED_BYTES || pid == 0 || pid > i32::MAX as u32 || uid == u32::MAX {
        return Err(io::Error::other("invalid peer credential result"));
    }
    Ok(uid)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn credentials_come_from_the_connected_kernel_stream() -> io::Result<()> {
        use std::os::unix::fs::MetadataExt;
        let (a, b) = UnixStream::pair()?;
        let uid = std::fs::metadata("/proc/self")?.uid();
        assert_eq!(peer_uid(&a)?, uid);
        assert_eq!(peer_uid(&b)?, uid);
        Ok(())
    }

    #[test]
    fn short_or_unmapped_credentials_never_authenticate() {
        assert!(decode([1, 0, 0], 8).is_err());
        assert!(decode([0, 0, 0], 12).is_err());
        assert!(decode([u32::MAX, 0, 0], 12).is_err());
        assert!(decode([1, u32::MAX, 0], 12).is_err());
        assert_eq!(decode([1, 1000, 998], 12).ok(), Some(1000));
    }
}

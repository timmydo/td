//! Linux directory lookup and effective UID; raw requests are pinned by confinement.
#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
compile_error!("td-mta storage currently requires Linux x86-64");

use std::{
    ffi::CStr,
    fs::File,
    io,
    os::fd::{AsRawFd, FromRawFd},
};

#[repr(C)]
struct OpenHow {
    flags: u64,
    mode: u64,
    resolve: u64,
}
const _: () = assert!(std::mem::size_of::<OpenHow>() == 24);

const OPENAT2: usize = 437;
const GETEUID: usize = 107;
const O_DIRECTORY: u64 = 0o200000;
const O_CLOEXEC: u64 = 0o2000000;
const RESOLVE_BENEATH: u64 = 0x08;
const RESOLVE_NO_SYMLINKS: u64 = 0x04;

/// Borrows both inputs through return; owns the newly installed descriptor once.
#[allow(unsafe_code)]
pub(super) fn open_directory(parent: &File, name: &CStr) -> io::Result<File> {
    let how = OpenHow {
        flags: O_DIRECTORY | O_CLOEXEC,
        mode: 0,
        resolve: RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS,
    };
    let result: isize;
    // SAFETY: live borrowed fd, terminated name and initialized 24-byte request.
    // Linux reads the inputs synchronously; syscall clobbers rcx and r11.
    unsafe {
        std::arch::asm!(
            "syscall",
            inlateout("rax") OPENAT2 => result,
            in("rdi") parent.as_raw_fd(),
            in("rsi") name.as_ptr(),
            in("rdx") &how,
            in("r10") std::mem::size_of::<OpenHow>(),
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    if result < 0 {
        return Err(kernel_error(result));
    }
    // SAFETY: successful openat2 returns one new nonnegative int descriptor,
    // including zero. No operation can fail between that return and adoption.
    Ok(unsafe { File::from_raw_fd(result as i32) })
}

/// Reads this process's effective UID without selecting or changing credentials.
#[allow(unsafe_code)]
pub(super) fn effective_uid() -> io::Result<u32> {
    let result: isize;
    // SAFETY: geteuid takes no arguments and writes no user memory.
    unsafe {
        std::arch::asm!(
            "syscall",
            inlateout("rax") GETEUID => result,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack, nomem),
        );
    }
    if result < 0 {
        return Err(kernel_error(result));
    }
    u32::try_from(result).map_err(|_| io::ErrorKind::InvalidData.into())
}

fn kernel_error(result: isize) -> io::Error {
    match result
        .checked_neg()
        .and_then(|value| i32::try_from(value).ok())
    {
        Some(code) if (1..=4095).contains(&code) => io::Error::from_raw_os_error(code),
        _ => io::ErrorKind::InvalidData.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_conversion_is_total_and_refuses_values_outside_the_errno_abi() {
        for code in [1, 13, 4095] {
            assert_eq!(kernel_error(-code).raw_os_error(), Some(code as i32));
        }
        for value in [isize::MIN, -4096, 0, 1, isize::MAX] {
            assert_eq!(kernel_error(value).kind(), io::ErrorKind::InvalidData);
        }
    }
}

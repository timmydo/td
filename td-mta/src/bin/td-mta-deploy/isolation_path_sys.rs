//! Metadata-only opens for the deployment clock transfer on Linux.
use std::ffi::{c_char, c_int};
use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd};

const PATH: c_int = 0o10000000;
const CLOEXEC: c_int = 0o2000000;
#[cfg(target_arch = "x86_64")]
const NOFOLLOW: c_int = 0o400000;
#[cfg(target_arch = "aarch64")]
const NOFOLLOW: c_int = 0o100000;
#[cfg(target_arch = "x86_64")]
const DIRECTORY: c_int = 0o200000;
#[cfg(target_arch = "aarch64")]
const DIRECTORY: c_int = 0o40000;

// std's OpenOptions masks O_ACCMODE from custom_flags, including O_PATH on musl.
#[allow(unsafe_code)]
fn open(parent: &File, name: &str, directory: bool) -> io::Result<File> {
    unsafe extern "C" {
        fn openat(dirfd: c_int, path: *const c_char, flags: c_int, ...) -> c_int;
    }
    let bytes = name.as_bytes();
    if bytes.is_empty()
        || bytes.len() > 255
        || matches!(name, "." | "..")
        || bytes.contains(&b'/')
        || bytes.contains(&0)
    {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    let mut component = [0u8; 256];
    component
        .get_mut(..bytes.len())
        .ok_or(io::ErrorKind::InvalidInput)?
        .copy_from_slice(bytes);
    let flags = PATH | CLOEXEC | NOFOLLOW | if directory { DIRECTORY } else { 0 };
    // The parent is borrowed throughout the call and the component is terminated.
    // No create flag, fourth argument, pointer retention, or data access.
    let fd = unsafe { openat(parent.as_raw_fd(), component.as_ptr().cast(), flags) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // A successful openat returns a fresh descriptor owned solely by this File.
    Ok(unsafe { File::from_raw_fd(fd) })
}

pub(super) fn directory(parent: &File, component: &str) -> io::Result<File> {
    open(parent, component, true)
}
pub(super) fn journal(parent: &File) -> io::Result<File> {
    open(parent, "queue-clock", false)
}

#[cfg(test)]
#[path = "tests/isolation_path.rs"]
mod tests;

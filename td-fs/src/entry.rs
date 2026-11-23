//! Linux x86-64 descriptor-bound file syscalls (UNSAFE.md §14).

use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;

#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
compile_error!("the editor file syscalls require Linux x86-64");

const SYS_FLISTXATTR: usize = 196;
const SYS_RENAMEAT2: usize = 316;
const RENAME_NOREPLACE: usize = 1;

#[allow(unsafe_code)]
fn syscall5(number: usize, a1: usize, a2: usize, a3: usize, a4: usize, a5: usize) -> isize {
    let result: isize;
    // SAFETY: private callers keep every named allocation live for the call.
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

fn syscall3(number: usize, a1: usize, a2: usize, a3: usize) -> isize {
    syscall5(number, a1, a2, a3, 0, 0)
}

/// Move between two borrowed directories with literal basenames, never replacing.
pub fn rename_entry(
    parent: &File,
    from: &std::ffi::OsStr,
    destination: &File,
    to: &std::ffi::OsStr,
) -> io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let leaf = |name: &std::ffi::OsStr| {
        let bytes = name.as_bytes();
        if bytes.is_empty()
            || bytes.len() > 4096
            || bytes == b"."
            || bytes == b".."
            || bytes.contains(&b'/')
        {
            return Err(io::Error::other("rename requires a literal basename"));
        }
        std::ffi::CString::new(bytes).map_err(|_| io::Error::other("NUL in rename basename"))
    };
    let from = leaf(from)?;
    let to = leaf(to)?;
    result(syscall5(
        SYS_RENAMEAT2,
        parent.as_raw_fd() as usize,
        from.as_ptr() as usize,
        destination.as_raw_fd() as usize,
        to.as_ptr() as usize,
        RENAME_NOREPLACE,
    ))
    .map(|_| ())
}

fn result(value: isize) -> io::Result<usize> {
    if value < 0 {
        return Err(io::Error::from_raw_os_error((-value) as i32));
    }
    Ok(value as usize)
}

/// Query list size only: no caller pointer, name, value or mutation.
pub fn has_attributes(file: &File) -> io::Result<bool> {
    result(syscall3(SYS_FLISTXATTR, file.as_raw_fd() as usize, 0, 0)).map(|size| size != 0)
}

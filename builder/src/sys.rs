//! Minimal raw x86_64-linux syscall layer: unshare(2), mount(2), getuid(2),
//! getgid(2) — exactly what the build sandbox needs. Hand-rolled to keep the
//! crate zero-dependency (plan/td-builder.md Q4: precedent is the
//! hand-rolled SHA-256; the rung's differential proves behavior, and the drv
//! platform field is checked to be x86_64-linux before any of this runs).

#[cfg(not(all(target_arch = "x86_64", target_os = "linux")))]
compile_error!("td-builder's sandbox is x86_64-linux only (the pinned platform)");

use std::ffi::CStr;
use std::io;

const SYS_GETUID: usize = 102;
const SYS_GETGID: usize = 104;
const SYS_MOUNT: usize = 165;
const SYS_UNSHARE: usize = 272;

pub const CLONE_NEWNS: usize = 0x0002_0000;
pub const CLONE_NEWUTS: usize = 0x0400_0000;
pub const CLONE_NEWIPC: usize = 0x0800_0000;
pub const CLONE_NEWUSER: usize = 0x1000_0000;
pub const CLONE_NEWNET: usize = 0x4000_0000;

pub const MS_BIND: usize = 0x1000;
pub const MS_REC: usize = 0x4000;
pub const MS_PRIVATE: usize = 0x4_0000;

/// x86_64 syscall ABI: number in rax, args in rdi/rsi/rdx/r10/r8; rcx and
/// r11 are clobbered by the instruction; negative return is -errno.
unsafe fn syscall5(n: usize, a1: usize, a2: usize, a3: usize, a4: usize, a5: usize) -> isize {
    let ret: isize;
    core::arch::asm!(
        "syscall",
        inlateout("rax") n as isize => ret,
        in("rdi") a1,
        in("rsi") a2,
        in("rdx") a3,
        in("r10") a4,
        in("r8") a5,
        out("rcx") _,
        out("r11") _,
        options(nostack)
    );
    ret
}

fn check(ret: isize) -> io::Result<()> {
    if ret < 0 {
        Err(io::Error::from_raw_os_error(-ret as i32))
    } else {
        Ok(())
    }
}

pub fn unshare(flags: usize) -> io::Result<()> {
    check(unsafe { syscall5(SYS_UNSHARE, flags, 0, 0, 0, 0) })
}

/// mount(2). `src`/`fstype` may be None (NULL) — e.g. the MS_REC|MS_PRIVATE
/// propagation change takes neither. `data` is always NULL here.
pub fn mount(src: Option<&CStr>, target: &CStr, fstype: Option<&CStr>, flags: usize) -> io::Result<()> {
    let s = src.map_or(std::ptr::null(), CStr::as_ptr);
    let t = fstype.map_or(std::ptr::null(), CStr::as_ptr);
    check(unsafe {
        syscall5(SYS_MOUNT, s as usize, target.as_ptr() as usize, t as usize, flags, 0)
    })
}

pub fn getuid() -> u32 {
    // Cannot fail per the man page.
    unsafe { syscall5(SYS_GETUID, 0, 0, 0, 0, 0) as u32 }
}

pub fn getgid() -> u32 {
    unsafe { syscall5(SYS_GETGID, 0, 0, 0, 0, 0) as u32 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn getuid_matches_proc_status() {
        // Cross-check the raw syscall against the kernel's own report.
        let status = std::fs::read_to_string("/proc/self/status").unwrap();
        let uid_line = status.lines().find(|l| l.starts_with("Uid:")).unwrap();
        let real: u32 = uid_line.split_whitespace().nth(1).unwrap().parse().unwrap();
        assert_eq!(getuid(), real);
    }

    #[test]
    fn bad_mount_reports_errno() {
        // Mounting onto a path that does not exist must surface ENOENT, not
        // a bogus success — proves the -errno decoding.
        let target = std::ffi::CString::new("/no/such/td-builder/mount/point").unwrap();
        let err = mount(None, &target, None, MS_REC | MS_PRIVATE).unwrap_err();
        assert_eq!(err.raw_os_error(), Some(2 /* ENOENT */));
    }
}

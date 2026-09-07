//! Fixed Linux x86-64 operations for the portal's read-only file grant.

use std::io;
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd};

const SYS_UNSHARE: usize = 272;
const SYS_OPEN_TREE: usize = 428;
const SYS_MOVE_MOUNT: usize = 429;
const SYS_MOUNT_SETATTR: usize = 442;
const CLONE_NEWUSER: usize = 0x1000_0000;
const AT_EMPTY_PATH: usize = 0x1000;
const OPEN_TREE_FLAGS: usize = 1 | 0x80000 | AT_EMPTY_PATH;
const PORTAL_ATTRIBUTES: u64 = 0x100000 | 1 | 2 | 4 | 8;
const MOVE_FLAGS: usize = 4 | 0x40;

#[repr(C)]
struct MountAttr {
    set: u64,
    clear: u64,
    propagation: u64,
    namespace: u64,
}

const _: [(); 32] = [(); std::mem::size_of::<MountAttr>()];
const _: [(); 8] = [(); std::mem::align_of::<MountAttr>()];
const _: [(); 0] = [(); std::mem::offset_of!(MountAttr, set)];
const _: [(); 8] = [(); std::mem::offset_of!(MountAttr, clear)];
const _: [(); 16] = [(); std::mem::offset_of!(MountAttr, propagation)];
const _: [(); 24] = [(); std::mem::offset_of!(MountAttr, namespace)];

#[allow(unsafe_code)]
fn syscall5(number: usize, a1: usize, a2: usize, a3: usize, a4: usize, a5: usize) -> isize {
    let value: isize;
    // SAFETY: fixed wrappers retain the borrowed descriptors and correctly
    // sized pointers through each synchronous call; no pointer escapes it.
    unsafe {
        core::arch::asm!(
            "syscall",
            inlateout("rax") number as isize => value,
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
    value
}

fn result(value: isize) -> io::Result<usize> {
    if value < 0 {
        return Err(io::Error::from_raw_os_error((-value) as i32));
    }
    Ok(value as usize)
}

#[allow(unsafe_code)]
fn adopt(fd: i32) -> OwnedFd {
    // SAFETY: clone_directory calls this exactly once for the new nonnegative
    // descriptor returned by open_tree, before any other owner exists.
    unsafe { OwnedFd::from_raw_fd(fd) }
}

pub(crate) fn new_user_namespace() -> io::Result<()> {
    result(syscall5(SYS_UNSHARE, CLONE_NEWUSER, 0, 0, 0, 0)).map(|_| ())
}

pub(crate) fn clone_directory(source: BorrowedFd<'_>) -> io::Result<OwnedFd> {
    let value = result(syscall5(
        SYS_OPEN_TREE,
        source.as_raw_fd() as usize,
        c"".as_ptr() as usize,
        OPEN_TREE_FLAGS,
        0,
        0,
    ))?;
    let fd = i32::try_from(value)
        .map_err(|_| io::Error::other("mount descriptor exceeds Linux descriptor range"))?;
    Ok(adopt(fd))
}

pub(crate) fn portal_attributes(
    mount: BorrowedFd<'_>,
    namespace: BorrowedFd<'_>,
) -> io::Result<()> {
    let attributes = MountAttr {
        set: PORTAL_ATTRIBUTES,
        clear: 0,
        propagation: 0,
        namespace: namespace.as_raw_fd() as u64,
    };
    result(syscall5(
        SYS_MOUNT_SETATTR,
        mount.as_raw_fd() as usize,
        c"".as_ptr() as usize,
        AT_EMPTY_PATH,
        &attributes as *const MountAttr as usize,
        std::mem::size_of::<MountAttr>(),
    ))
    .map(|_| ())
}

pub(crate) fn publish(mount: BorrowedFd<'_>, target: BorrowedFd<'_>) -> io::Result<()> {
    result(syscall5(
        SYS_MOVE_MOUNT,
        mount.as_raw_fd() as usize,
        c"".as_ptr() as usize,
        target.as_raw_fd() as usize,
        c"".as_ptr() as usize,
        MOVE_FLAGS,
    ))
    .map(|_| ())
}

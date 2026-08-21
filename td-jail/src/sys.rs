//! Raw x86-64 Linux calls that safe `std` cannot express.

use std::ffi::CStr;
use std::io;

#[cfg(not(all(target_arch = "x86_64", target_os = "linux")))]
compile_error!("td-jail is x86_64-linux only (raw syscall ABI)");

const SYS_CLOSE: usize = 3;
const SYS_CAPGET: usize = 125;
const SYS_CAPSET: usize = 126;
const SYS_PIVOT_ROOT: usize = 155;
const SYS_PRCTL: usize = 157;
const SYS_MOUNT: usize = 165;
const SYS_UMOUNT2: usize = 166;
const SYS_UNSHARE: usize = 272;

const CLONE_NEWNS: usize = 0x0002_0000;
const CLONE_NEWUTS: usize = 0x0400_0000;
const CLONE_NEWUSER: usize = 0x1000_0000;
const CLONE_NEWPID: usize = 0x2000_0000;
const CLONE_NEWNET: usize = 0x4000_0000;

const BASE_NAMESPACE_FLAGS: usize = CLONE_NEWUSER | CLONE_NEWNS | CLONE_NEWPID | CLONE_NEWUTS;
const ISOLATED_NETWORK_FLAGS: usize = BASE_NAMESPACE_FLAGS | CLONE_NEWNET;

pub const MS_RDONLY: usize = 0x1;
pub const MS_NOSUID: usize = 0x2;
pub const MS_NODEV: usize = 0x4;
pub const MS_NOEXEC: usize = 0x8;
pub const MS_REMOUNT: usize = 0x20;
pub const MS_BIND: usize = 0x1000;
pub const MS_REC: usize = 0x4000;
pub const MS_PRIVATE: usize = 0x4_0000;
pub const MNT_DETACH: usize = 0x2;

pub const CAP_SETPCAP: u32 = 8;
pub const CAP_SYS_ADMIN: u32 = 21;

const LINUX_CAPABILITY_VERSION_3: u32 = 0x2008_0522;
const PR_CAPBSET_READ: usize = 23;
const PR_CAPBSET_DROP: usize = 24;
const PR_CAP_AMBIENT: usize = 47;
const PR_CAP_AMBIENT_IS_SET: usize = 1;
const PR_CAP_AMBIENT_RAISE: usize = 2;
const PR_CAP_AMBIENT_CLEAR_ALL: usize = 4;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CapabilitySets {
    pub effective: u64,
    pub permitted: u64,
    pub inheritable: u64,
}

#[repr(C)]
struct CapabilityHeader {
    version: u32,
    pid: i32,
}

#[derive(Clone, Copy)]
#[repr(C)]
struct CapabilityData {
    effective: u32,
    permitted: u32,
    inheritable: u32,
}

#[repr(C)]
struct CapabilityDataPair {
    low: CapabilityData,
    high: CapabilityData,
}

impl CapabilityDataPair {
    fn from_sets(sets: CapabilitySets) -> Self {
        Self {
            low: CapabilityData {
                effective: sets.effective as u32,
                permitted: sets.permitted as u32,
                inheritable: sets.inheritable as u32,
            },
            high: CapabilityData {
                effective: (sets.effective >> 32) as u32,
                permitted: (sets.permitted >> 32) as u32,
                inheritable: (sets.inheritable >> 32) as u32,
            },
        }
    }

    fn sets(&self) -> CapabilitySets {
        CapabilitySets {
            effective: u64::from(self.low.effective) | (u64::from(self.high.effective) << 32),
            permitted: u64::from(self.low.permitted) | (u64::from(self.high.permitted) << 32),
            inheritable: u64::from(self.low.inheritable) | (u64::from(self.high.inheritable) << 32),
        }
    }
}

/// The only raw-syscall instruction in td-jail.
#[inline]
#[allow(unsafe_code)]
fn syscall5(n: usize, a1: usize, a2: usize, a3: usize, a4: usize, a5: usize) -> isize {
    let ret: isize;
    // SAFETY: the wrapper supplies the x86-64 syscall registers. Callers keep
    // every referenced C string and capability buffer live for the call.
    unsafe {
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
            options(nostack),
        );
    }
    ret
}

fn value(ret: isize) -> io::Result<usize> {
    if ret < 0 {
        Err(io::Error::from_raw_os_error(-ret as i32))
    } else {
        Ok(ret as usize)
    }
}

fn check(ret: isize) -> io::Result<()> {
    value(ret).map(|_| ())
}

pub fn close(fd: u32) -> io::Result<()> {
    check(syscall5(SYS_CLOSE, fd as usize, 0, 0, 0, 0))
}

pub fn unshare_namespaces(isolate_network: bool) -> io::Result<()> {
    let flags = match isolate_network {
        false => BASE_NAMESPACE_FLAGS,
        true => ISOLATED_NETWORK_FLAGS,
    };
    check(syscall5(SYS_UNSHARE, flags, 0, 0, 0, 0))
}

pub fn mount(
    source: Option<&CStr>,
    target: &CStr,
    filesystem: Option<&CStr>,
    flags: usize,
    data: Option<&CStr>,
) -> io::Result<()> {
    let source = source.map_or(std::ptr::null(), CStr::as_ptr);
    let filesystem = filesystem.map_or(std::ptr::null(), CStr::as_ptr);
    let data = data.map_or(std::ptr::null(), CStr::as_ptr);
    check(syscall5(
        SYS_MOUNT,
        source as usize,
        target.as_ptr() as usize,
        filesystem as usize,
        flags,
        data as usize,
    ))
}

pub fn pivot_root(new_root: &CStr, put_old: &CStr) -> io::Result<()> {
    check(syscall5(
        SYS_PIVOT_ROOT,
        new_root.as_ptr() as usize,
        put_old.as_ptr() as usize,
        0,
        0,
        0,
    ))
}

pub fn umount_detach(target: &CStr) -> io::Result<()> {
    check(syscall5(
        SYS_UMOUNT2,
        target.as_ptr() as usize,
        MNT_DETACH,
        0,
        0,
        0,
    ))
}

pub fn capabilities() -> io::Result<CapabilitySets> {
    let mut header = CapabilityHeader {
        version: LINUX_CAPABILITY_VERSION_3,
        pid: 0,
    };
    let mut data = CapabilityDataPair::from_sets(CapabilitySets {
        effective: 0,
        permitted: 0,
        inheritable: 0,
    });
    check(syscall5(
        SYS_CAPGET,
        std::ptr::from_mut(&mut header) as usize,
        std::ptr::from_mut(&mut data) as usize,
        0,
        0,
        0,
    ))?;
    if header.version != LINUX_CAPABILITY_VERSION_3 {
        return Err(io::Error::other(
            "capget did not retain Linux capability ABI version 3",
        ));
    }
    Ok(data.sets())
}

pub fn set_capabilities(sets: CapabilitySets) -> io::Result<()> {
    let header = CapabilityHeader {
        version: LINUX_CAPABILITY_VERSION_3,
        pid: 0,
    };
    let data = CapabilityDataPair::from_sets(sets);
    check(syscall5(
        SYS_CAPSET,
        std::ptr::from_ref(&header) as usize,
        std::ptr::from_ref(&data) as usize,
        0,
        0,
        0,
    ))
}

pub fn clear_ambient_capabilities() -> io::Result<()> {
    check(syscall5(
        SYS_PRCTL,
        PR_CAP_AMBIENT,
        PR_CAP_AMBIENT_CLEAR_ALL,
        0,
        0,
        0,
    ))
}

pub fn raise_ambient_sys_admin() -> io::Result<()> {
    check(syscall5(
        SYS_PRCTL,
        PR_CAP_AMBIENT,
        PR_CAP_AMBIENT_RAISE,
        CAP_SYS_ADMIN as usize,
        0,
        0,
    ))
}

pub fn ambient_capability(capability: u32) -> io::Result<bool> {
    bool_result(syscall5(
        SYS_PRCTL,
        PR_CAP_AMBIENT,
        PR_CAP_AMBIENT_IS_SET,
        capability as usize,
        0,
        0,
    ))
}

pub fn drop_bounding_capability(capability: u32) -> io::Result<()> {
    check(syscall5(
        SYS_PRCTL,
        PR_CAPBSET_DROP,
        capability as usize,
        0,
        0,
        0,
    ))
}

pub fn bounding_capability(capability: u32) -> io::Result<bool> {
    bool_result(syscall5(
        SYS_PRCTL,
        PR_CAPBSET_READ,
        capability as usize,
        0,
        0,
        0,
    ))
}

fn bool_result(ret: isize) -> io::Result<bool> {
    match value(ret)? {
        0 => Ok(false),
        1 => Ok(true),
        other => Err(io::Error::other(format!(
            "capability readback returned {other}, not zero or one"
        ))),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use std::os::fd::IntoRawFd;

    fn status_capability(key: &str) -> u64 {
        let status = std::fs::read_to_string("/proc/self/status").unwrap();
        let value = status
            .lines()
            .find_map(|line| line.strip_prefix(key))
            .unwrap()
            .trim();
        u64::from_str_radix(value, 16).unwrap()
    }

    #[test]
    fn namespace_flag_sets_are_exact() {
        assert_eq!(BASE_NAMESPACE_FLAGS, 0x3402_0000);
        assert_eq!(ISOLATED_NETWORK_FLAGS, 0x7402_0000);
    }

    #[test]
    fn capability_constants_are_exact() {
        assert_eq!(LINUX_CAPABILITY_VERSION_3, 0x2008_0522);
        assert_eq!(CAP_SETPCAP, 8);
        assert_eq!(CAP_SYS_ADMIN, 21);
        assert_eq!(PR_CAPBSET_READ, 23);
        assert_eq!(PR_CAPBSET_DROP, 24);
        assert_eq!(PR_CAP_AMBIENT, 47);
        assert_eq!(PR_CAP_AMBIENT_IS_SET, 1);
        assert_eq!(PR_CAP_AMBIENT_RAISE, 2);
        assert_eq!(PR_CAP_AMBIENT_CLEAR_ALL, 4);
        assert_eq!(std::mem::size_of::<CapabilityHeader>(), 8);
        assert_eq!(std::mem::size_of::<CapabilityData>(), 12);
        assert_eq!(std::mem::size_of::<CapabilityDataPair>(), 24);
    }

    #[test]
    fn capget_and_prctl_readbacks_agree_with_proc() {
        let sets = capabilities().unwrap();
        assert_eq!(sets.effective, status_capability("CapEff:"));
        assert_eq!(sets.permitted, status_capability("CapPrm:"));
        assert_eq!(sets.inheritable, status_capability("CapInh:"));
        assert_eq!(
            ambient_capability(CAP_SYS_ADMIN).unwrap(),
            status_capability("CapAmb:") & (1_u64 << CAP_SYS_ADMIN) != 0
        );
        assert_eq!(
            bounding_capability(CAP_SYS_ADMIN).unwrap(),
            status_capability("CapBnd:") & (1_u64 << CAP_SYS_ADMIN) != 0
        );
    }

    #[test]
    fn close_owns_a_transferred_descriptor_and_preserves_ebadf() {
        let (reader, writer) = io::pipe().unwrap();
        let fd = writer.into_raw_fd();
        close(fd as u32).unwrap();
        assert_eq!(close(fd as u32).unwrap_err().raw_os_error(), Some(9));
        drop(reader);
    }

    #[test]
    fn check_preserves_errno() {
        assert!(check(0).is_ok());
        assert_eq!(check(-1).unwrap_err().raw_os_error(), Some(1));
    }
}

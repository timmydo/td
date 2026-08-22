//! Raw x86-64 Linux calls that safe `std` cannot express.

use std::ffi::CStr;
use std::io;
use std::net::{Ipv4Addr, UdpSocket};
use std::os::fd::AsRawFd;

#[cfg(not(all(target_arch = "x86_64", target_os = "linux")))]
compile_error!("td-jail is x86_64-linux only (raw syscall ABI)");

const SYS_CLOSE: usize = 3;
const SYS_IOCTL: usize = 16;
const SYS_WAIT4: usize = 61;
const SYS_CAPGET: usize = 125;
const SYS_CAPSET: usize = 126;
const SYS_PIVOT_ROOT: usize = 155;
const SYS_PRCTL: usize = 157;
const SYS_MOUNT: usize = 165;
const SYS_UMOUNT2: usize = 166;
const SYS_UNSHARE: usize = 272;
const SYS_SECCOMP: usize = 317;

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
const PR_SET_PDEATHSIG: usize = 1;
const PR_GET_DUMPABLE: usize = 3;
const PR_SET_DUMPABLE: usize = 4;
const PR_CAPBSET_READ: usize = 23;
const PR_CAPBSET_DROP: usize = 24;
const PR_SET_NO_NEW_PRIVS: usize = 38;
const PR_GET_NO_NEW_PRIVS: usize = 39;
const PR_CAP_AMBIENT: usize = 47;
const PR_CAP_AMBIENT_IS_SET: usize = 1;
const PR_CAP_AMBIENT_RAISE: usize = 2;
const PR_CAP_AMBIENT_CLEAR_ALL: usize = 4;

const PID_ANY: usize = -1_isize as usize;
const WNOHANG: usize = 1;
const EINTR: i32 = 4;
const ECHILD: i32 = 10;
const SECCOMP_SET_MODE_FILTER: usize = 1;
pub(crate) const SECCOMP_MAX_FILTER_INSNS: usize = 4096;
const SIOCGIFFLAGS: usize = 0x8913;
const SIOCSIFFLAGS: usize = 0x8914;
const IFF_UP: i16 = 0x1;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CapabilitySets {
    pub effective: u64,
    pub permitted: u64,
    pub inheritable: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Reaped {
    Child { pid: i32, status: i32 },
    NotYet,
    NoChildren,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(C)]
pub(crate) struct SockFilter {
    pub(crate) code: u16,
    pub(crate) jt: u8,
    pub(crate) jf: u8,
    pub(crate) k: u32,
}

#[repr(C)]
struct SockFprog {
    len: u16,
    filter: *const SockFilter,
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

#[repr(C, align(8))]
struct IfreqFlags {
    name: [u8; 16],
    flags: i16,
    padding: [u8; 22],
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

pub fn wait_any(nohang: bool) -> io::Result<Reaped> {
    let mut status = 0_i32;
    let options = if nohang { WNOHANG } else { 0 };
    let ret = syscall5(
        SYS_WAIT4,
        PID_ANY,
        std::ptr::from_mut(&mut status) as usize,
        options,
        0,
        0,
    );
    classify_wait(ret, status)
}

fn classify_wait(ret: isize, status: i32) -> io::Result<Reaped> {
    if ret > 0 {
        return Ok(Reaped::Child {
            pid: ret as i32,
            status,
        });
    }
    if ret == 0 || ret == -(EINTR as isize) {
        return Ok(Reaped::NotYet);
    }
    if ret == -(ECHILD as isize) {
        return Ok(Reaped::NoChildren);
    }
    Err(io::Error::from_raw_os_error(-ret as i32))
}

pub fn unshare_namespaces(isolate_network: bool) -> io::Result<()> {
    let flags = match isolate_network {
        false => BASE_NAMESPACE_FLAGS,
        true => ISOLATED_NETWORK_FLAGS,
    };
    check(syscall5(SYS_UNSHARE, flags, 0, 0, 0, 0))
}

fn loopback_request() -> IfreqFlags {
    let mut name = [0_u8; 16];
    name[0] = b'l';
    name[1] = b'o';
    IfreqFlags {
        name,
        flags: 0,
        padding: [0; 22],
    }
}

fn read_interface_flags(fd: u32, value: &mut IfreqFlags) -> io::Result<()> {
    check(syscall5(
        SYS_IOCTL,
        fd as usize,
        SIOCGIFFLAGS,
        std::ptr::from_mut(value) as usize,
        0,
        0,
    ))
}

fn write_interface_flags(fd: u32, value: &mut IfreqFlags) -> io::Result<()> {
    check(syscall5(
        SYS_IOCTL,
        fd as usize,
        SIOCSIFFLAGS,
        std::ptr::from_mut(value) as usize,
        0,
        0,
    ))
}

pub fn bring_up_loopback() -> io::Result<()> {
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?;
    let fd = u32::try_from(socket.as_raw_fd())
        .map_err(|_| io::Error::other("loopback ioctl socket has a negative descriptor"))?;
    let mut request = loopback_request();
    read_interface_flags(fd, &mut request)?;
    request.flags |= IFF_UP;
    write_interface_flags(fd, &mut request)?;

    let mut readback = loopback_request();
    read_interface_flags(fd, &mut readback)?;
    if readback.flags & IFF_UP == 0 {
        return Err(io::Error::other(
            "loopback interface did not read back as up",
        ));
    }
    Ok(())
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

pub fn set_no_new_privileges() -> io::Result<()> {
    check(syscall5(SYS_PRCTL, PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0))
}

pub fn no_new_privileges() -> io::Result<bool> {
    strict_bool(
        syscall5(SYS_PRCTL, PR_GET_NO_NEW_PRIVS, 0, 0, 0, 0),
        "no-new-privileges readback",
    )
}

pub fn set_parent_death_signal() -> io::Result<()> {
    const SIGKILL: usize = 9;
    check(syscall5(SYS_PRCTL, PR_SET_PDEATHSIG, SIGKILL, 0, 0, 0))
}

pub fn set_dumpable(dumpable: bool) -> io::Result<()> {
    check(syscall5(
        SYS_PRCTL,
        PR_SET_DUMPABLE,
        usize::from(dumpable),
        0,
        0,
        0,
    ))
}

pub fn dumpable() -> io::Result<bool> {
    strict_bool(
        syscall5(SYS_PRCTL, PR_GET_DUMPABLE, 0, 0, 0, 0),
        "dumpable readback",
    )
}

pub fn install_seccomp_filter(instructions: &[SockFilter]) -> io::Result<()> {
    if instructions.len() > SECCOMP_MAX_FILTER_INSNS {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "seccomp filter exceeds the kernel instruction-count limit",
        ));
    }
    let len = u16::try_from(instructions.len()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "seccomp filter exceeds the kernel instruction-count ABI",
        )
    })?;
    if len == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "seccomp filter must contain an instruction",
        ));
    }
    let program = SockFprog {
        len,
        filter: instructions.as_ptr(),
    };
    check(syscall5(
        SYS_SECCOMP,
        SECCOMP_SET_MODE_FILTER,
        0,
        std::ptr::from_ref(&program) as usize,
        0,
        0,
    ))
}

fn bool_result(ret: isize) -> io::Result<bool> {
    strict_bool(ret, "capability readback")
}

fn strict_bool(ret: isize, name: &str) -> io::Result<bool> {
    match value(ret)? {
        0 => Ok(false),
        1 => Ok(true),
        other => Err(io::Error::other(format!(
            "{name} returned {other}, not zero or one"
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
        assert_eq!(PR_SET_PDEATHSIG, 1);
        assert_eq!(PR_GET_DUMPABLE, 3);
        assert_eq!(PR_SET_DUMPABLE, 4);
        assert_eq!(PR_CAPBSET_READ, 23);
        assert_eq!(PR_CAPBSET_DROP, 24);
        assert_eq!(PR_SET_NO_NEW_PRIVS, 38);
        assert_eq!(PR_GET_NO_NEW_PRIVS, 39);
        assert_eq!(PR_CAP_AMBIENT, 47);
        assert_eq!(PR_CAP_AMBIENT_IS_SET, 1);
        assert_eq!(PR_CAP_AMBIENT_RAISE, 2);
        assert_eq!(PR_CAP_AMBIENT_CLEAR_ALL, 4);
        assert_eq!(std::mem::size_of::<CapabilityHeader>(), 8);
        assert_eq!(std::mem::size_of::<CapabilityData>(), 12);
        assert_eq!(std::mem::size_of::<CapabilityDataPair>(), 24);
        assert_eq!(std::mem::size_of::<SockFilter>(), 8);
        assert_eq!(std::mem::size_of::<SockFprog>(), 16);
        assert_eq!(std::mem::size_of::<IfreqFlags>(), 40);
        assert_eq!(std::mem::align_of::<IfreqFlags>(), 8);
        assert_eq!(SECCOMP_SET_MODE_FILTER, 1);
        assert_eq!(SECCOMP_MAX_FILTER_INSNS, 4096);
        assert_eq!(SYS_IOCTL, 16);
        assert_eq!(SIOCGIFFLAGS, 0x8913);
        assert_eq!(SIOCSIFFLAGS, 0x8914);
        assert_eq!(IFF_UP, 1);
        assert_eq!(PID_ANY, usize::MAX);
        assert_eq!(WNOHANG, 1);
    }

    #[test]
    fn loopback_ioctl_request_is_exact() {
        let request = loopback_request();
        assert_eq!(request.name.get(..3), Some(b"lo\0".as_slice()));
        assert!(request.name.get(3..).unwrap().iter().all(|byte| *byte == 0));
        assert_eq!(request.flags, 0);
        assert!(request.padding.iter().all(|byte| *byte == 0));
    }

    #[test]
    fn seccomp_wrapper_refuses_more_than_the_kernel_instruction_limit() {
        let instruction = SockFilter {
            code: 0x06,
            jt: 0,
            jf: 0,
            k: 0x7fff_0000,
        };
        let oversized = vec![instruction; SECCOMP_MAX_FILTER_INSNS + 1];
        assert!(install_seccomp_filter(&oversized).is_err());
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

    #[test]
    fn wait_results_cover_child_poll_interrupt_and_empty_table() {
        assert_eq!(
            classify_wait(7, 3 << 8).unwrap(),
            Reaped::Child {
                pid: 7,
                status: 3 << 8
            }
        );
        assert_eq!(classify_wait(0, 0).unwrap(), Reaped::NotYet);
        assert_eq!(classify_wait(-(EINTR as isize), 0).unwrap(), Reaped::NotYet);
        assert_eq!(
            classify_wait(-(ECHILD as isize), 0).unwrap(),
            Reaped::NoChildren
        );
        assert_eq!(classify_wait(-22, 0).unwrap_err().raw_os_error(), Some(22));
    }
}

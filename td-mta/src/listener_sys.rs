//! Linux IPv6-only listening sockets; IPv4 retains std's binding path.
use std::{
    io,
    net::{SocketAddr, TcpListener},
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
};

#[cfg(not(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
)))]
compile_error!("mail listeners require x86-64 or aarch64 Linux");

#[cfg(target_arch = "x86_64")]
const SOCKET: usize = 41;
#[cfg(target_arch = "aarch64")]
const SOCKET: usize = 198;
#[cfg(target_arch = "x86_64")]
const SETSOCKOPT: usize = 54;
#[cfg(target_arch = "aarch64")]
const SETSOCKOPT: usize = 208;
#[cfg(target_arch = "x86_64")]
const BIND: usize = 49;
#[cfg(target_arch = "aarch64")]
const BIND: usize = 200;
#[cfg(target_arch = "x86_64")]
const LISTEN: usize = 50;
#[cfg(target_arch = "aarch64")]
const LISTEN: usize = 201;

#[repr(C)]
struct Address {
    family: u16,
    port: u16,
    flow: u32,
    octets: [u8; 16],
    scope: u32,
}
const _: () = assert!(std::mem::size_of::<Address>() == 28);

pub(super) fn bind(address: SocketAddr) -> io::Result<TcpListener> {
    let SocketAddr::V6(address) = address else {
        return TcpListener::bind(address);
    };
    if address.scope_id() != 0 || address.flowinfo() != 0 || address.ip().to_ipv4_mapped().is_some()
    {
        return Err(io::Error::from(io::ErrorKind::InvalidInput));
    }
    let descriptor = checked(call(SOCKET, 10, 1 | 0x80000, 0, 0, 0))?;
    let descriptor = i32::try_from(descriptor).map_err(|_| io::ErrorKind::InvalidData)?;
    let descriptor = adopt(descriptor);
    let fd = descriptor.as_raw_fd() as usize;
    let enabled = 1i32;
    // Set reuse and IPv6-only before bind; never infer family policy from sysctl.
    checked(call(
        SETSOCKOPT,
        fd,
        1,
        2,
        (&enabled as *const i32) as usize,
        4,
    ))?;
    checked(call(
        SETSOCKOPT,
        fd,
        41,
        26,
        (&enabled as *const i32) as usize,
        4,
    ))?;
    let address = Address {
        family: 10,
        port: address.port().to_be(),
        flow: 0,
        octets: address.ip().octets(),
        scope: 0,
    };
    checked(call(
        BIND,
        fd,
        (&address as *const Address) as usize,
        28,
        0,
        0,
    ))?;
    checked(call(LISTEN, fd, 128, 0, 0, 0))?;
    Ok(TcpListener::from(descriptor))
}

fn checked(value: isize) -> io::Result<isize> {
    if value >= 0 {
        return Ok(value);
    }
    let error = value
        .checked_neg()
        .and_then(|value| i32::try_from(value).ok())
        .ok_or(io::ErrorKind::InvalidData)?;
    Err(io::Error::from_raw_os_error(error))
}

#[allow(unsafe_code)]
fn adopt(descriptor: i32) -> OwnedFd {
    // Only bind calls this, once, for its fresh successful socket result.
    unsafe { OwnedFd::from_raw_fd(descriptor) }
}

#[allow(unsafe_code)]
fn call(number: usize, a: usize, b: usize, c: usize, d: usize, e: usize) -> isize {
    let result: isize;
    // The sole caller supplies live fixed-layout address/option storage.
    // These operations retain no pointers; OwnedFd closes every error path.
    #[cfg(target_arch = "x86_64")]
    unsafe {
        std::arch::asm!(
            "syscall", inlateout("rax") number => result,
            in("rdi") a, in("rsi") b, in("rdx") c, in("r10") d, in("r8") e,
            lateout("rcx") _, lateout("r11") _, options(nostack),
        );
    }
    #[cfg(target_arch = "aarch64")]
    unsafe {
        std::arch::asm!(
            "svc 0", in("x8") number, inlateout("x0") a => result,
            in("x1") b, in("x2") c, in("x3") d, in("x4") e,
            options(nostack),
        );
    }
    result
}

#[cfg(test)]
#[path = "listener_sys_tests.rs"]
mod tests;

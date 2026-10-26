//! The confined raw-syscall surface (UNSAFE.md §2). `syscall6`'s body is the
//! ONLY `unsafe` in the crate; the scoped `#[allow]` under the crate's
//! `#![deny(unsafe_code)]` is the compiler-enforced confinement. Every other
//! function here passes integers, or pointers as integers to buffers it keeps
//! live across the call, and pins the request values UNSAFE.md names.

use std::io;
use std::os::fd::RawFd;
use std::time::Duration;

#[cfg(not(all(target_arch = "x86_64", target_os = "linux")))]
compile_error!("td-netd is x86_64-linux only (raw syscall ABI)");

const SYS_CLOSE: usize = 3;
const SYS_POLL: usize = 7;
const SYS_IOCTL: usize = 16;
const SYS_SOCKET: usize = 41;
const SYS_SENDTO: usize = 44;
const SYS_RECVFROM: usize = 45;
const SYS_SETSOCKOPT: usize = 54;

/// The single raw-syscall entry point (x86_64 SysV syscall ABI).
#[inline]
#[allow(unsafe_code)]
fn syscall6(n: usize, a1: usize, a2: usize, a3: usize, a4: usize, a5: usize, a6: usize) -> isize {
    let ret: isize;
    // SAFETY: the `syscall` instruction clobbers rcx/r11 and returns in rax; the
    // args are plain integers or pointers-as-usize whose pointees the caller
    // keeps live, and sized as the call's length arguments say, across the
    // call. No memory is aliased beyond the kernel's access to those buffers.
    unsafe {
        core::arch::asm!(
            "syscall",
            inlateout("rax") n as isize => ret,
            in("rdi") a1,
            in("rsi") a2,
            in("rdx") a3,
            in("r10") a4,
            in("r8") a5,
            in("r9") a6,
            out("rcx") _,
            out("r11") _,
            options(nostack),
        );
    }
    ret
}

/// Turn a raw syscall return into a `Result`.
fn check(ret: isize) -> io::Result<isize> {
    if ret < 0 {
        Err(io::Error::from_raw_os_error(-ret as i32))
    } else {
        Ok(ret)
    }
}

/// `ioctl(fd, request, argp)` for the interface-config requests
/// (SIOCGIFFLAGS/SIOCSIFFLAGS/ADDR/NETMASK, SIOCGIFHWADDR, SIOCADDRT/DELRT) on an
/// AF_INET datagram socket. `argp` points at a caller-owned buffer of the
/// request's size, kept live across the call.
pub fn ioctl(fd: RawFd, request: usize, argp: *mut u8) -> io::Result<()> {
    check(syscall6(
        SYS_IOCTL,
        fd as usize,
        request,
        argp as usize,
        0,
        0,
        0,
    ))
    .map(|_| ())
}

// ── Terminal echo for the passphrase prompt ─────────────────────────────────

const TCGETS: usize = 0x5401;
const TCSETS: usize = 0x5402;
/// `sizeof(struct termios)` as the kernel copies it for TCGETS/TCSETS on
/// x86_64: four 32-bit flag words, the line discipline and 19 control chars.
const TERMIOS_LEN: usize = 36;
const C_LFLAG: usize = 12;
const ECHO: u32 = 0o10;

/// Turn standard input's echo off or back on. `Ok(false)` when standard
/// input is not a terminal (ENOTTY), so the caller reads a pipe unchanged.
/// Only descriptor 0 and only TCGETS/TCSETS reach the kernel.
pub fn stdin_echo(enabled: bool) -> io::Result<bool> {
    let mut termios = [0u8; TERMIOS_LEN];
    match check(syscall6(
        SYS_IOCTL,
        0,
        TCGETS,
        termios.as_mut_ptr() as usize,
        0,
        0,
        0,
    )) {
        Ok(_) => {}
        Err(e) if e.raw_os_error() == Some(25) => return Ok(false),
        Err(e) => return Err(e),
    }
    let flags = termios
        .get(C_LFLAG..C_LFLAG + 4)
        .and_then(|b| <[u8; 4]>::try_from(b).ok())
        .map(u32::from_ne_bytes)
        .ok_or_else(|| io::Error::other("short termios"))?;
    let flags = if enabled { flags | ECHO } else { flags & !ECHO };
    if let Some(slot) = termios.get_mut(C_LFLAG..C_LFLAG + 4) {
        slot.copy_from_slice(&flags.to_ne_bytes());
    }
    check(syscall6(
        SYS_IOCTL,
        0,
        TCSETS,
        termios.as_ptr() as usize,
        0,
        0,
        0,
    ))?;
    Ok(true)
}

// ── The generic netlink socket nl80211 rides ────────────────────────────────

const AF_NETLINK: usize = 16;
/// SOCK_RAW | SOCK_CLOEXEC.
const SOCK_RAW_CLOEXEC: usize = 3 | 0o2000000;
const NETLINK_GENERIC: usize = 16;
const SOL_NETLINK: usize = 270;
const NETLINK_ADD_MEMBERSHIP: usize = 1;
/// MSG_DONTWAIT | MSG_TRUNC: never block (the caller polled first), and
/// report a datagram's real length so a truncated one is refused.
const RECV_FLAGS: usize = 0x40 | 0x20;
/// `sizeof(struct sockaddr_nl)`.
const SOCKADDR_NL_LEN: usize = 12;

/// One owned AF_NETLINK/NETLINK_GENERIC socket. Its descriptor never leaves
/// this module; it is closed on drop. Sends go to the kernel (no address,
/// so the unconnected socket's default destination, port 0), and a receive
/// discards any datagram whose sender is not the kernel.
pub struct GenlSocket {
    fd: RawFd,
}

impl GenlSocket {
    pub fn open() -> io::Result<Self> {
        let fd = check(syscall6(
            SYS_SOCKET,
            AF_NETLINK,
            SOCK_RAW_CLOEXEC,
            NETLINK_GENERIC,
            0,
            0,
            0,
        ))?;
        let fd = RawFd::try_from(fd).map_err(|_| io::Error::other("socket returned a bad fd"))?;
        Ok(Self { fd })
    }

    /// Join generic netlink multicast group `group` (nl80211's "mlme" and
    /// "scan"), the one socket option this module sets.
    pub fn join_group(&self, group: u32) -> io::Result<()> {
        let value = group.to_ne_bytes();
        check(syscall6(
            SYS_SETSOCKOPT,
            self.fd as usize,
            SOL_NETLINK,
            NETLINK_ADD_MEMBERSHIP,
            value.as_ptr() as usize,
            value.len(),
            0,
        ))
        .map(|_| ())
    }

    /// Send one request to the kernel.
    pub fn send(&self, message: &[u8]) -> io::Result<()> {
        let sent = check(syscall6(
            SYS_SENDTO,
            self.fd as usize,
            message.as_ptr() as usize,
            message.len(),
            0,
            0,
            0,
        ))?;
        if sent as usize != message.len() {
            return Err(io::Error::other("netlink send was short"));
        }
        Ok(())
    }

    /// Receive one datagram without blocking. `Ok(None)` when none is
    /// queued or the one received was not the kernel's; a datagram longer
    /// than `buf` is an error.
    pub fn recv(&self, buf: &mut [u8]) -> io::Result<Option<usize>> {
        let mut address = [0u8; SOCKADDR_NL_LEN];
        let mut address_len = (SOCKADDR_NL_LEN as u32).to_ne_bytes();
        let got = match check(syscall6(
            SYS_RECVFROM,
            self.fd as usize,
            buf.as_mut_ptr() as usize,
            buf.len(),
            RECV_FLAGS,
            address.as_mut_ptr() as usize,
            address_len.as_mut_ptr() as usize,
        )) {
            Ok(n) => n as usize,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(None),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => return Ok(None),
            Err(e) => return Err(e),
        };
        if got > buf.len() {
            return Err(io::Error::other(format!(
                "netlink datagram of {got} bytes exceeds the {} byte buffer",
                buf.len()
            )));
        }
        let family = address
            .get(..2)
            .and_then(|b| <[u8; 2]>::try_from(b).ok())
            .map(u16::from_ne_bytes);
        let port = address
            .get(4..8)
            .and_then(|b| <[u8; 4]>::try_from(b).ok())
            .map(u32::from_ne_bytes);
        if u32::from_ne_bytes(address_len) as usize != SOCKADDR_NL_LEN
            || family != Some(AF_NETLINK as u16)
            || port != Some(0)
        {
            return Ok(None);
        }
        Ok(Some(got))
    }

    pub fn fd(&self) -> RawFd {
        self.fd
    }
}

impl Drop for GenlSocket {
    fn drop(&mut self) {
        let _ = syscall6(SYS_CLOSE, self.fd as usize, 0, 0, 0, 0, 0);
    }
}

// ── poll over at most two descriptors ───────────────────────────────────────

const POLLIN: i16 = 0x1;

/// Wait up to `timeout` for either descriptor to become readable (or to
/// report an error or hangup, which a read then surfaces). Returns which
/// were ready. EINTR is an early, empty return.
pub fn poll2(
    first: Option<RawFd>,
    second: Option<RawFd>,
    timeout: Duration,
) -> io::Result<(bool, bool)> {
    // struct pollfd { int fd; short events; short revents; }; a negative fd
    // is ignored by the kernel.
    let mut fds = [0u8; 16];
    for (slot, fd) in fds.as_chunks_mut::<8>().0.iter_mut().zip([first, second]) {
        slot[..4].copy_from_slice(&fd.unwrap_or(-1).to_ne_bytes());
        slot[4..6].copy_from_slice(&POLLIN.to_ne_bytes());
    }
    let millis = usize::try_from(timeout.as_millis().min(i32::MAX as u128)).unwrap_or(0);
    match check(syscall6(
        SYS_POLL,
        fds.as_mut_ptr() as usize,
        2,
        millis,
        0,
        0,
        0,
    )) {
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::Interrupted => return Ok((false, false)),
        Err(e) => return Err(e),
    }
    let ready = |slot: &[u8; 8]| i16::from_ne_bytes([slot[6], slot[7]]) != 0;
    let [a, b] = fds.as_chunks::<8>().0 else {
        return Ok((false, false));
    };
    Ok((first.is_some() && ready(a), second.is_some() && ready(b)))
}

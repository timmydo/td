//! Linux x86-64 PTY device, session and foreground-relay syscalls;
//! UNSAFE.md section 29. One function-scoped syscall instruction carries
//! `ioctl` pinned to five PTY and two termios requests, `setsid` for a PTY
//! child, and `rt_sigprocmask`/`rt_sigtimedwait` over one compiled signal
//! set for the foreground relay; one function-scoped adoption site owns
//! the slave descriptor the kernel installs, and one function-scoped
//! pre-exec hook makes a PTY child lead a session on its terminal. Safe
//! `std` opens the master and owns every close. Nothing here is reachable
//! from another crate: the crate root is the only caller, and a consumer
//! that needs a raw surface of its own gets its own roster entry.

use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::process::Command;

#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
compile_error!("the td-pty raw layer requires Linux x86-64");

const SYS_IOCTL: usize = 16;
const SYS_SETSID: usize = 112;
const TIOCSPTLCK: usize = 0x4004_5431;
const TIOCGPTPEER: usize = 0x5441;
const TIOCSWINSZ: usize = 0x5414;
const TIOCGWINSZ: usize = 0x5413;
const TIOCSCTTY: usize = 0x540e;
/// `O_RDWR | O_NOCTTY | O_CLOEXEC`: the slave belongs to the child, never
/// to this process, and does not leak across an unrelated exec.
const PTY_PEER_FLAGS: usize = 0o2 | 0o400 | 0o2_000_000;

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

#[allow(unsafe_code)]
fn adopt(fd: i32) -> OwnedFd {
    // SAFETY: only freshly installed, nonnegative kernel descriptors reach
    // this private site. No other owner has been constructed for them.
    unsafe { OwnedFd::from_raw_fd(fd) }
}

fn result(value: isize) -> io::Result<usize> {
    if value < 0 {
        return Err(io::Error::from_raw_os_error((-value) as i32));
    }
    Ok(value as usize)
}

/// Unlock a freshly opened `/dev/ptmx` master: a pointer to a zero `int`.
pub(crate) fn unlock_pty(master: &File) -> io::Result<()> {
    let unlocked: i32 = 0;
    result(syscall3(
        SYS_IOCTL,
        master.as_raw_fd() as usize,
        TIOCSPTLCK,
        (&unlocked as *const i32) as usize,
    ))
    .map(|_| ())
}

/// The master's slave, by descriptor rather than by `/dev/pts` name: the
/// flags are an immediate and the kernel installs a new descriptor.
pub(crate) fn pty_peer(master: &File) -> io::Result<File> {
    let value = result(syscall3(
        SYS_IOCTL,
        master.as_raw_fd() as usize,
        TIOCGPTPEER,
        PTY_PEER_FLAGS,
    ))?;
    let fd = i32::try_from(value).map_err(|_| io::Error::other("peer descriptor out of range"))?;
    Ok(File::from(adopt(fd)))
}

/// Publish a `struct winsize`, four native `u16` words in kernel order.
pub(crate) fn set_window_size(terminal: &File, words: [u16; 4]) -> io::Result<()> {
    result(syscall3(
        SYS_IOCTL,
        terminal.as_raw_fd() as usize,
        TIOCSWINSZ,
        (&words as *const [u16; 4]) as usize,
    ))
    .map(|_| ())
}

/// In the child `spawn` is starting, between fork and exec: lead a new
/// session, then take descriptor zero -- the PTY slave, which `std` has
/// already put there -- as its controlling terminal, argument zero because
/// this session holds none yet. Two raw syscalls and nothing that allocates
/// or locks, which is all a pre-exec hook may do. An error fails the spawn.
fn lead_session_on_stdin() -> io::Result<()> {
    result(syscall3(SYS_SETSID, 0, 0, 0))?;
    result(syscall3(SYS_IOCTL, 0, TIOCSCTTY, 0)).map(|_| ())
}

/// Make `command`'s child lead a session on the terminal at its standard
/// input, so the line discipline's signals and job control reach it. The
/// caller has put that terminal on the command's stdin: the hook claims
/// descriptor zero, whatever it holds.
#[allow(unsafe_code)]
pub(crate) fn lead_session(command: &mut Command) {
    // SAFETY: the hook runs in the forked child before exec and makes only
    // the two raw syscalls above: no allocation, lock or shared state.
    unsafe {
        command.pre_exec(lead_session_on_stdin);
    }
}

/// Read a terminal's `struct winsize` back into four words.
pub(crate) fn window_size(terminal: &File) -> io::Result<[u16; 4]> {
    let mut words = [0u16; 4];
    result(syscall3(
        SYS_IOCTL,
        terminal.as_raw_fd() as usize,
        TIOCGWINSZ,
        (&mut words as *mut [u16; 4]) as usize,
    ))?;
    Ok(words)
}

// Terminal relay operations; the borrowed terminal is never adopted.
const TCGETS: usize = 0x5401;
const TCSETS: usize = 0x5402;
const SYS_RT_SIGPROCMASK: usize = 14;
const SYS_RT_SIGTIMEDWAIT: usize = 128;
const RELAY_SIGNALS: u64 = (1 << 0) | (1 << 1) | (1 << 2) | (1 << 14) | (1 << 19);

pub(crate) fn terminal_attributes(terminal: &File) -> io::Result<[u8; 36]> {
    let mut bytes = [0u8; 36];
    result(syscall3(
        SYS_IOCTL,
        terminal.as_raw_fd() as usize,
        TCGETS,
        bytes.as_mut_ptr() as usize,
    ))?;
    Ok(bytes)
}

pub(crate) fn set_terminal_attributes(terminal: &File, bytes: &[u8; 36]) -> io::Result<()> {
    result(syscall3(
        SYS_IOCTL,
        terminal.as_raw_fd() as usize,
        TCSETS,
        bytes.as_ptr() as usize,
    ))
    .map(|_| ())
}

pub(crate) fn block_relay_signals() -> io::Result<u64> {
    let mut previous = 0u64;
    result(syscall5(
        SYS_RT_SIGPROCMASK,
        0,
        (&RELAY_SIGNALS as *const u64) as usize,
        (&mut previous as *mut u64) as usize,
        8,
        0,
    ))?;
    Ok(previous)
}

pub(crate) fn restore_relay_signals(previous: u64) -> io::Result<()> {
    result(syscall5(
        SYS_RT_SIGPROCMASK,
        2,
        (&previous as *const u64) as usize,
        0,
        8,
        0,
    ))
    .map(|_| ())
}

pub(crate) fn relay_signal() -> io::Result<Option<u8>> {
    let timeout = [0i64; 2];
    match result(syscall5(
        SYS_RT_SIGTIMEDWAIT,
        (&RELAY_SIGNALS as *const u64) as usize,
        0,
        timeout.as_ptr() as usize,
        8,
        0,
    )) {
        Ok(signal) => Ok(u8::try_from(signal).ok()),
        Err(e) if matches!(e.raw_os_error(), Some(11 | 4)) => Ok(None),
        Err(e) => Err(e),
    }
}

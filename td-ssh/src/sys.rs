//! The whole raw Linux boundary of td-ssh (UNSAFE.md section 27): one syscall
//! instruction, value-pinned requests, and nothing a caller can aim elsewhere.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::Command;

#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
compile_error!("td-ssh's raw layer is x86-64 Linux only");

const SYS_IOCTL: usize = 16;
const SYS_UMASK: usize = 95;
const SYS_SETUID: usize = 105;
const SYS_SETGID: usize = 106;
const SYS_SETGROUPS: usize = 116;
const SYS_SETSID: usize = 112;
const SYS_PRCTL: usize = 157;
const SYS_SECCOMP: usize = 317;

const TCGETS: usize = 0x5401;
const TCSETS: usize = 0x5402;
const TIOCGWINSZ: usize = 0x5413;
const TIOCSWINSZ: usize = 0x5414;
const TIOCSCTTY: usize = 0x540e;
const TIOCGPTN: usize = 0x8004_5430;
const TIOCSPTLCK: usize = 0x4004_5431;

const PR_SET_DUMPABLE: usize = 4;
const PR_SET_NO_NEW_PRIVS: usize = 38;
const SECCOMP_SET_MODE_FILTER: usize = 1;
const O_NOCTTY: i32 = 0o400;
const NGROUPS_MAX: usize = 65536;
const UMASK_PROBE: usize = 0o077;
const UMASK_FLOOR: usize = 0o022;

/// The kernel's `struct termios`, which is what `TCGETS` fills: not glibc's.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(C)]
pub struct Termios {
    pub iflag: u32,
    pub oflag: u32,
    pub cflag: u32,
    pub lflag: u32,
    pub line: u8,
    pub cc: [u8; 19],
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(C)]
pub struct Winsize {
    pub rows: u16,
    pub columns: u16,
    pub width: u16,
    pub height: u16,
}

const ICANON: u32 = 0o2;
const ECHO: u32 = 0o10;
const ECHONL: u32 = 0o100;
const ISIG: u32 = 0o1;
const IEXTEN: u32 = 0o100000;
const IGNBRK: u32 = 0o1;
const BRKINT: u32 = 0o2;
const PARMRK: u32 = 0o10;
const ISTRIP: u32 = 0o40;
const INLCR: u32 = 0o100;
const IGNCR: u32 = 0o200;
const ICRNL: u32 = 0o400;
const IXON: u32 = 0o2000;
const OPOST: u32 = 0o1;
const CSIZE: u32 = 0o60;
const PARENB: u32 = 0o400;
const CS8: u32 = 0o60;
const VTIME: usize = 5;
const VMIN: usize = 6;

impl Termios {
    /// What `cfmakeraw` does.
    pub fn raw(mut self) -> Self {
        self.iflag &= !(IGNBRK | BRKINT | PARMRK | ISTRIP | INLCR | IGNCR | ICRNL | IXON);
        self.oflag &= !OPOST;
        self.lflag &= !(ECHO | ECHONL | ICANON | ISIG | IEXTEN);
        self.cflag &= !(CSIZE | PARENB);
        self.cflag |= CS8;
        if let Some(slot) = self.cc.get_mut(VMIN) {
            *slot = 1;
        }
        if let Some(slot) = self.cc.get_mut(VTIME) {
            *slot = 0;
        }
        self
    }
}

/// The module's one syscall instruction.
#[allow(unsafe_code)]
fn syscall3(number: usize, a1: usize, a2: usize, a3: usize) -> io::Result<usize> {
    let result: isize;
    // SAFETY: every caller passes a live descriptor and either a value or a
    // pointer to a live exclusively borrowed object of the size that request
    // uses. The instruction clobbers only what is declared.
    unsafe {
        core::arch::asm!(
            "syscall",
            inlateout("rax") number as isize => result,
            in("rdi") a1,
            in("rsi") a2,
            in("rdx") a3,
            // Zero, not whatever they held: prctl refuses a nonzero argument it
            // does not use.
            in("r10") 0usize,
            in("r8") 0usize,
            in("r9") 0usize,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    if result < 0 {
        Err(io::Error::from_raw_os_error(
            i32::try_from(-result).unwrap_or(i32::MAX),
        ))
    } else {
        Ok(result.unsigned_abs())
    }
}

fn raw(fd: BorrowedFd<'_>) -> usize {
    fd.as_raw_fd().unsigned_abs() as usize
}

/// The terminal's settings, or an error if `fd` is not a terminal.
pub fn terminal_attributes(fd: impl AsFd) -> io::Result<Termios> {
    let mut termios = Termios::default();
    syscall3(
        SYS_IOCTL,
        raw(fd.as_fd()),
        TCGETS,
        std::ptr::from_mut(&mut termios) as usize,
    )?;
    Ok(termios)
}

pub fn set_terminal_attributes(fd: impl AsFd, termios: &Termios) -> io::Result<()> {
    syscall3(
        SYS_IOCTL,
        raw(fd.as_fd()),
        TCSETS,
        std::ptr::from_ref(termios) as usize,
    )?;
    Ok(())
}

pub fn window_size(fd: impl AsFd) -> io::Result<Winsize> {
    let mut size = Winsize::default();
    syscall3(
        SYS_IOCTL,
        raw(fd.as_fd()),
        TIOCGWINSZ,
        std::ptr::from_mut(&mut size) as usize,
    )?;
    Ok(size)
}

pub fn is_terminal(fd: impl AsFd) -> bool {
    terminal_attributes(fd).is_ok()
}

pub fn set_window_size(fd: impl AsFd, size: &Winsize) -> io::Result<()> {
    syscall3(
        SYS_IOCTL,
        raw(fd.as_fd()),
        TIOCSWINSZ,
        std::ptr::from_ref(size) as usize,
    )?;
    Ok(())
}

/// A new PTY: its master, and the unlocked slave path to open for a child. No
/// terminal becomes this process's controlling terminal.
pub fn open_pty() -> io::Result<(File, PathBuf)> {
    let master = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(O_NOCTTY)
        .open("/dev/ptmx")?;
    let unlock: i32 = 0;
    syscall3(
        SYS_IOCTL,
        raw(master.as_fd()),
        TIOCSPTLCK,
        std::ptr::from_ref(&unlock) as usize,
    )?;
    let mut number: u32 = 0;
    syscall3(
        SYS_IOCTL,
        raw(master.as_fd()),
        TIOCGPTN,
        std::ptr::from_mut(&mut number) as usize,
    )?;
    Ok((master, PathBuf::from(format!("/dev/pts/{number}"))))
}

/// Opens a PTY slave without acquiring it: the child does, in `lead_session`.
pub fn open_pty_slave(path: &std::path::Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(O_NOCTTY)
        .open(path)
}

/// Starts `command` as a session leader and, with `controlling_tty`, makes its
/// stdin (a PTY slave) the controlling terminal.
#[allow(unsafe_code)]
pub fn lead_session(command: &mut Command, controlling_tty: bool) {
    // SAFETY: the hook runs in the forked child before exec and makes only raw
    // syscalls: no allocation, lock or shared state.
    unsafe {
        command.pre_exec(move || {
            syscall3(SYS_SETSID, 0, 0, 0)?;
            if controlling_tty {
                syscall3(SYS_IOCTL, 0, TIOCSCTTY, 0)?;
            }
            Ok(())
        });
    }
}

/// `setgroups(2)`. First of the three: it needs the privilege the others shed.
pub fn setgroups(list: &[u32]) -> io::Result<()> {
    if list.is_empty() || list.len() > NGROUPS_MAX {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "supplementary group list has an impossible length",
        ));
    }
    syscall3(SYS_SETGROUPS, list.len(), list.as_ptr() as usize, 0)?;
    Ok(())
}

/// `setgid(2)`. Second, and still before the uid drop.
pub fn setgid(gid: u32) -> io::Result<()> {
    syscall3(SYS_SETGID, gid as usize, 0, 0)?;
    Ok(())
}

/// `setuid(2)`. Last: from root it sets the real, effective and saved uids.
pub fn setuid(uid: u32) -> io::Result<()> {
    syscall3(SYS_SETUID, uid as usize, 0, 0)?;
    Ok(())
}

/// Tells the kernel this process and its descendants gain no privilege by exec.
pub fn set_no_new_privs() -> io::Result<()> {
    syscall3(SYS_PRCTL, PR_SET_NO_NEW_PRIVS, 1, 0)?;
    Ok(())
}

/// A non-dumpable process cannot be traced, or have its descriptors reached, by
/// processes of the same uid.
pub fn set_not_dumpable() -> io::Result<()> {
    syscall3(SYS_PRCTL, PR_SET_DUMPABLE, 0, 0)?;
    Ok(())
}

/// Keeps whatever mask was inherited and adds group and other write to it, as
/// OpenSSH's daemon does. `umask(2)` reports only by setting, so reading it takes
/// a strict probe value in between.
pub fn raise_umask_floor() -> io::Result<()> {
    let inherited = syscall3(SYS_UMASK, UMASK_PROBE, 0, 0)?;
    syscall3(SYS_UMASK, inherited | UMASK_FLOOR, 0, 0)?;
    Ok(())
}

/// One classic BPF instruction, as the kernel reads it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(C)]
pub struct SockFilter {
    pub code: u16,
    pub jump_true: u8,
    pub jump_false: u8,
    pub k: u32,
}

#[repr(C)]
struct SockFprog {
    len: u16,
    filter: *const SockFilter,
}

/// Installs a seccomp filter on this thread. Requires `set_no_new_privs` first
/// for an unprivileged caller; every stage calls it first regardless.
pub fn install_seccomp_filter(filter: &[SockFilter]) -> io::Result<()> {
    let program = SockFprog {
        len: u16::try_from(filter.len())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "filter is too long"))?,
        filter: filter.as_ptr(),
    };
    syscall3(
        SYS_SECCOMP,
        SECCOMP_SET_MODE_FILTER,
        0,
        std::ptr::from_ref(&program) as usize,
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn the_kernel_termios_layout_is_what_the_requests_expect() {
        assert_eq!(std::mem::size_of::<Termios>(), 36);
        assert_eq!(std::mem::size_of::<Winsize>(), 8);
    }

    #[test]
    fn raw_mode_matches_cfmakeraw_for_the_flags_that_matter() {
        let start = Termios {
            iflag: ICRNL | IXON,
            oflag: OPOST,
            cflag: PARENB,
            lflag: ECHO | ICANON | ISIG,
            ..Termios::default()
        };
        let raw = start.raw();
        assert_eq!(raw.iflag, 0);
        assert_eq!(raw.oflag, 0);
        assert_eq!(raw.lflag, 0);
        assert_eq!(raw.cflag, CS8);
        assert_eq!(raw.cc[VMIN], 1);
        assert_eq!(raw.cc[VTIME], 0);
    }

    #[test]
    fn a_regular_file_is_not_a_terminal() {
        let file = std::fs::File::open("/proc/self/status").unwrap();
        assert!(!is_terminal(&file));
        assert!(terminal_attributes(&file).is_err());
    }
}

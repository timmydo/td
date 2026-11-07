//! The whole raw Linux boundary of td-ssh (UNSAFE.md section 27): one syscall
//! instruction, value-pinned requests, and nothing a caller can aim elsewhere.

use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd};

#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
compile_error!("td-ssh's raw layer is x86-64 Linux only");

const SYS_IOCTL: usize = 16;
const TCGETS: usize = 0x5401;
const TCSETS: usize = 0x5402;
const TIOCGWINSZ: usize = 0x5413;

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

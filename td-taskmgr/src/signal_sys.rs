//! The complete process-signal boundary recorded in UNSAFE.md §20.
use crate::actions::Signal;
use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;

#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
compile_error!("the process-signal adapter requires Linux x86-64");

pub(crate) fn probe(directory: &File) -> io::Result<()> {
    invoke(directory, None)
}
pub(crate) fn send(directory: &File, signal: Signal) -> io::Result<()> {
    invoke(directory, Some(signal))
}
fn number(signal: Signal) -> i64 {
    match signal {
        Signal::Hangup => 1,
        Signal::Interrupt => 2,
        Signal::Quit => 3,
        Signal::Illegal => 4,
        Signal::Trap => 5,
        Signal::Abort => 6,
        Signal::Bus => 7,
        Signal::Arithmetic => 8,
        Signal::Kill => 9,
        Signal::User1 => 10,
        Signal::Segmentation => 11,
        Signal::User2 => 12,
        Signal::Pipe => 13,
        Signal::Alarm => 14,
        Signal::Terminate => 15,
        Signal::StackFault => 16,
        Signal::Child => 17,
        Signal::Continue => 18,
        Signal::Stop => 19,
        Signal::TerminalStop => 20,
        Signal::TerminalInput => 21,
        Signal::TerminalOutput => 22,
        Signal::Urgent => 23,
        Signal::CpuLimit => 24,
        Signal::FileLimit => 25,
        Signal::VirtualAlarm => 26,
        Signal::Profile => 27,
        Signal::Window => 28,
        Signal::Io => 29,
        Signal::Power => 30,
        Signal::System => 31,
    }
}
#[allow(unsafe_code)]
fn invoke(directory: &File, signal: Option<Signal>) -> io::Result<()> {
    let mut result = 424i64;
    // A live borrowed directory stays owned by the caller; no pointers enter.
    unsafe {
        std::arch::asm!(
            "syscall",
            inlateout("rax") result,
            in("rdi") i64::from(directory.as_raw_fd()),
            in("rsi") signal.map(number).unwrap_or(0),
            in("rdx") 0usize,
            in("r10") 0usize,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    if result == 0 {
        Ok(())
    } else if (-4095..0).contains(&result) {
        Err(io::Error::from_raw_os_error((-result) as i32))
    } else {
        Err(io::Error::other("unexpected process-signal result"))
    }
}

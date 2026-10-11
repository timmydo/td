//! Signals only the retained child owned by the process fixture.
use std::{io, process::Child};

#[derive(Clone, Copy)]
pub enum Signal {
    Interrupt,
    Terminate,
}

#[allow(unsafe_code)]
pub fn send(child: &Child, signal: Signal) -> io::Result<()> {
    let pid = child.id() as usize;
    let signal = match signal {
        Signal::Interrupt => 2usize,
        Signal::Terminate => 15usize,
    };
    let result: isize;
    #[cfg(target_arch = "x86_64")]
    unsafe {
        std::arch::asm!(
            "syscall",
            inlateout("rax") 62usize => result,
            in("rdi") pid, in("rsi") signal,
            lateout("rcx") _, lateout("r11") _,
            options(nostack),
        );
    }
    #[cfg(target_arch = "aarch64")]
    unsafe {
        std::arch::asm!(
            "svc 0",
            in("x8") 129usize,
            inlateout("x0") pid => result,
            in("x1") signal,
            options(nostack),
        );
    }
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::other(format!("signal child: {result}")))
    }
}

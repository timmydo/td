//! Linux process termination requests, consumed synchronously by serve.
use std::io;

#[cfg(not(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
)))]
compile_error!("mail termination signals require x86-64 or aarch64 Linux");

const TERMINATION: u64 = (1 << (2 - 1)) | (1 << (15 - 1));
#[cfg(target_arch = "x86_64")]
const BLOCK: usize = 14;
#[cfg(target_arch = "aarch64")]
const BLOCK: usize = 135;
#[cfg(target_arch = "x86_64")]
const WAIT: usize = 128;
#[cfg(target_arch = "aarch64")]
const WAIT: usize = 137;

/// Called by serve before it starts any worker. The process exits after serve;
/// the mask remains blocked through worker teardown and is never restored.
pub(super) fn block() -> io::Result<()> {
    let result = call(BLOCK, 0, (&TERMINATION as *const u64) as usize, 0, 8);
    match result {
        0 => Ok(()),
        code => Err(error(code)),
    }
}

/// Zero timeout: consume at most one pending SIGINT or SIGTERM per control turn.
pub(super) fn requested() -> io::Result<bool> {
    let timeout = [0i64; 2];
    let result = call(
        WAIT,
        (&TERMINATION as *const u64) as usize,
        0,
        timeout.as_ptr() as usize,
        8,
    );
    match result {
        2 | 15 => Ok(true),
        -11 | -4 => Ok(false), // EAGAIN or interruption by an unrelated signal.
        code => Err(error(code)),
    }
}

fn error(code: isize) -> io::Error {
    match code.checked_neg().and_then(|code| i32::try_from(code).ok()) {
        Some(code) if code > 0 => io::Error::from_raw_os_error(code),
        _ => io::Error::from(io::ErrorKind::InvalidData),
    }
}

#[allow(unsafe_code)]
fn call(number: usize, a: usize, b: usize, c: usize, d: usize) -> isize {
    let result: isize;
    // The only callers pass live fixed-size signal/timeout storage, no output
    // pointers, and the kernel retains none of those pointers after returning.
    #[cfg(target_arch = "x86_64")]
    unsafe {
        std::arch::asm!(
            "syscall",
            inlateout("rax") number => result,
            in("rdi") a, in("rsi") b, in("rdx") c, in("r10") d,
            lateout("rcx") _, lateout("r11") _,
            options(nostack),
        );
    }
    #[cfg(target_arch = "aarch64")]
    unsafe {
        std::arch::asm!(
            "svc 0",
            in("x8") number,
            inlateout("x0") a => result,
            in("x1") b, in("x2") c, in("x3") d,
            options(nostack),
        );
    }
    result
}

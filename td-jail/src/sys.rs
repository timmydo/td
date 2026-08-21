//! Raw x86-64 Linux calls that safe `std` cannot express.

use std::io;

#[cfg(not(all(target_arch = "x86_64", target_os = "linux")))]
compile_error!("td-jail is x86_64-linux only (raw syscall ABI)");

const SYS_UNSHARE: usize = 272;

const CLONE_NEWNS: usize = 0x0002_0000;
const CLONE_NEWUTS: usize = 0x0400_0000;
const CLONE_NEWUSER: usize = 0x1000_0000;
const CLONE_NEWPID: usize = 0x2000_0000;
const CLONE_NEWNET: usize = 0x4000_0000;

const BASE_NAMESPACE_FLAGS: usize = CLONE_NEWUSER | CLONE_NEWNS | CLONE_NEWPID | CLONE_NEWUTS;
const ISOLATED_NETWORK_FLAGS: usize = BASE_NAMESPACE_FLAGS | CLONE_NEWNET;

/// The only raw-syscall instruction in td-jail.
#[inline]
#[allow(unsafe_code)]
fn syscall5(n: usize, a1: usize, a2: usize, a3: usize, a4: usize, a5: usize) -> isize {
    let ret: isize;
    // SAFETY: every argument is an integer. The instruction has no pointer
    // operands in this landing, but `nomem` stays absent for the later surface.
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

fn check(ret: isize) -> io::Result<()> {
    if ret < 0 {
        Err(io::Error::from_raw_os_error(-ret as i32))
    } else {
        Ok(())
    }
}

pub fn unshare_namespaces(isolate_network: bool) -> io::Result<()> {
    let flags = match isolate_network {
        false => BASE_NAMESPACE_FLAGS,
        true => ISOLATED_NETWORK_FLAGS,
    };
    check(syscall5(SYS_UNSHARE, flags, 0, 0, 0, 0))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn namespace_flag_sets_are_exact() {
        assert_eq!(BASE_NAMESPACE_FLAGS, 0x3402_0000);
        assert_eq!(ISOLATED_NETWORK_FLAGS, 0x7402_0000);
    }

    #[test]
    fn check_preserves_errno() {
        assert!(check(0).is_ok());
        assert_eq!(check(-1).unwrap_err().raw_os_error(), Some(1));
    }
}

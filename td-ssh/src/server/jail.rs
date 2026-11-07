//! The confinement of every stage that faces hostile bytes or holds the host
//! key: empty chroot, locked identity, `no_new_privs`, a kill-process allowlist
//! (DESIGN.md section 4).

use super::credentials::{switch, Credentials};
use crate::sys::{self, SockFilter};
use std::fs::{self, File};
use std::os::unix::fs::MetadataExt;

pub const PRIVSEP_PATH: &str = "/run/sshd-empty";
pub const PRIVSEP_UID: u32 = 995;
pub const PRIVSEP_GID: u32 = 995;

const AUDIT_ARCH_X86_64: u32 = 0xc000_003e;
const SECCOMP_RET_KILL_PROCESS: u32 = 0x8000_0000;
const SECCOMP_RET_ALLOW: u32 = 0x7fff_0000;
const BPF_LD_W_ABS: u16 = 0x20;
const BPF_JMP_JEQ_K: u16 = 0x15;
const BPF_JMP_JGE_K: u16 = 0x35;
const BPF_JMP_JSET_K: u16 = 0x45;
const BPF_RET_K: u16 = 0x06;
const OFFSET_NR: u32 = 0;
const OFFSET_ARCH: u32 = 4;
/// The third argument's low word: `prot` for both `mmap` and `mprotect`.
const OFFSET_ARG2_LOW: u32 = 32;
const X32_SYSCALL_BIT: u32 = 0x4000_0000;
const PROT_EXEC: u32 = 4;
const GROUP_OTHER_WRITE: u32 = 0o022;

const SYS_READ: u32 = 0;
const SYS_WRITE: u32 = 1;
const SYS_CLOSE: u32 = 3;
const SYS_MMAP: u32 = 9;
const SYS_MPROTECT: u32 = 10;
const SYS_MUNMAP: u32 = 11;
const SYS_BRK: u32 = 12;
const SYS_RT_SIGPROCMASK: u32 = 14;
const SYS_RT_SIGRETURN: u32 = 15;
const SYS_SCHED_YIELD: u32 = 24;
const SYS_MREMAP: u32 = 25;
const SYS_MADVISE: u32 = 28;
const SYS_EXIT: u32 = 60;
const SYS_SIGALTSTACK: u32 = 131;
const SYS_FUTEX: u32 = 202;
const SYS_CLOCK_GETTIME: u32 = 228;
const SYS_CLOCK_NANOSLEEP: u32 = 230;
const SYS_EXIT_GROUP: u32 = 231;
const SYS_GETRANDOM: u32 = 318;

/// Nothing here opens, forks, execs, signals or changes identity.
const ALLOWED: &[u32] = &[
    SYS_READ,
    SYS_WRITE,
    SYS_CLOSE,
    SYS_MUNMAP,
    SYS_BRK,
    SYS_RT_SIGPROCMASK,
    SYS_RT_SIGRETURN,
    SYS_SCHED_YIELD,
    SYS_MREMAP,
    SYS_MADVISE,
    SYS_EXIT,
    SYS_SIGALTSTACK,
    SYS_FUTEX,
    SYS_CLOCK_GETTIME,
    SYS_CLOCK_NANOSLEEP,
    SYS_EXIT_GROUP,
    SYS_GETRANDOM,
];

/// Mappings stay legal, but none may become executable.
const NEVER_EXECUTABLE: &[u32] = &[SYS_MMAP, SYS_MPROTECT];

fn statement(code: u16, k: u32) -> SockFilter {
    SockFilter {
        code,
        jump_true: 0,
        jump_false: 0,
        k,
    }
}

fn jump(code: u16, k: u32, jump_true: u8, jump_false: u8) -> SockFilter {
    SockFilter {
        code,
        jump_true,
        jump_false,
        k,
    }
}

/// The program, built here so its shape can be read and tested. Each entry
/// falls through to the next unless its number matched, and every path ends in
/// a return, so no jump is longer than one entry's own length.
pub fn filter() -> Vec<SockFilter> {
    let mut program = vec![
        statement(BPF_LD_W_ABS, OFFSET_ARCH),
        jump(BPF_JMP_JEQ_K, AUDIT_ARCH_X86_64, 1, 0),
        statement(BPF_RET_K, SECCOMP_RET_KILL_PROCESS),
        statement(BPF_LD_W_ABS, OFFSET_NR),
        jump(BPF_JMP_JGE_K, X32_SYSCALL_BIT, 0, 1),
        statement(BPF_RET_K, SECCOMP_RET_KILL_PROCESS),
    ];
    for number in ALLOWED {
        program.push(jump(BPF_JMP_JEQ_K, *number, 0, 1));
        program.push(statement(BPF_RET_K, SECCOMP_RET_ALLOW));
    }
    for number in NEVER_EXECUTABLE {
        // On a match, load prot and judge it: A is not needed again either way.
        program.push(jump(BPF_JMP_JEQ_K, *number, 0, 4));
        program.push(statement(BPF_LD_W_ABS, OFFSET_ARG2_LOW));
        program.push(jump(BPF_JMP_JSET_K, PROT_EXEC, 1, 0));
        program.push(statement(BPF_RET_K, SECCOMP_RET_ALLOW));
        program.push(statement(BPF_RET_K, SECCOMP_RET_KILL_PROCESS));
    }
    program.push(statement(BPF_RET_K, SECCOMP_RET_KILL_PROCESS));
    program
}

/// What the chroot must be for the empty jail to be one: root's, a directory, and
/// nobody else's to fill. A link is no directory.
fn jail_directory_passes(is_dir: bool, uid: u32, mode: u32) -> bool {
    is_dir && uid == 0 && mode & GROUP_OTHER_WRITE == 0
}

fn check_jail_directory() -> Result<(), String> {
    let metadata =
        fs::symlink_metadata(PRIVSEP_PATH).map_err(|error| format!("{PRIVSEP_PATH}: {error}"))?;
    if !jail_directory_passes(metadata.is_dir(), metadata.uid(), metadata.mode()) {
        return Err(format!(
            "{PRIVSEP_PATH} must be a directory only root can write"
        ));
    }
    Ok(())
}

/// Applies everything, in the one safe order. `status` is opened before this
/// and closed here, since the filter leaves it nothing further to read for.
/// `drop_identity` is false only in unprivileged single-user mode (DESIGN.md
/// section 4), which has nothing to shed but keeps the filter.
pub fn enter(mut status: File, drop_identity: bool) -> Result<(), String> {
    if drop_identity {
        check_jail_directory()?;
        std::os::unix::fs::chroot(PRIVSEP_PATH)
            .map_err(|error| format!("chroot {PRIVSEP_PATH}: {error}"))?;
        std::env::set_current_dir("/").map_err(|error| format!("chdir /: {error}"))?;
        switch(
            &Credentials::new(PRIVSEP_UID, PRIVSEP_GID, &[]),
            &mut status,
        )?;
        // The kernel clears this at a uid change, depending on a sysctl: not a thing
        // to rely on. Single-user mode, with nothing to shed, stays open to its uid,
        // which is the test that looks at what the jail holds.
        sys::set_not_dumpable().map_err(|error| format!("not dumpable: {error}"))?;
    }
    drop(status);
    sys::set_no_new_privs().map_err(|error| format!("no_new_privs: {error}"))?;
    sys::install_seccomp_filter(&filter()).map_err(|error| format!("seccomp: {error}"))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::indexing_slicing, clippy::unreachable)]
    use super::*;

    /// A tiny interpreter: the BPF the kernel would run, over one syscall.
    fn verdict(arch: u32, number: u32, prot: u32) -> u32 {
        let program = filter();
        let mut accumulator = 0u32;
        let mut pc = 0usize;
        loop {
            let instruction = program[pc];
            pc += 1;
            match instruction.code {
                BPF_LD_W_ABS => {
                    accumulator = match instruction.k {
                        OFFSET_NR => number,
                        OFFSET_ARCH => arch,
                        OFFSET_ARG2_LOW => prot,
                        other => unreachable!("unexpected load offset {other}"),
                    }
                }
                BPF_JMP_JEQ_K | BPF_JMP_JGE_K | BPF_JMP_JSET_K => {
                    let taken = match instruction.code {
                        BPF_JMP_JEQ_K => accumulator == instruction.k,
                        BPF_JMP_JGE_K => accumulator >= instruction.k,
                        _ => accumulator & instruction.k != 0,
                    };
                    pc += usize::from(if taken {
                        instruction.jump_true
                    } else {
                        instruction.jump_false
                    });
                }
                BPF_RET_K => return instruction.k,
                other => unreachable!("unexpected instruction {other}"),
            }
        }
    }

    const ALLOW: u32 = SECCOMP_RET_ALLOW;
    const KILL: u32 = SECCOMP_RET_KILL_PROCESS;

    #[test]
    fn the_allowlist_admits_only_its_entries() {
        for number in ALLOWED {
            assert_eq!(verdict(AUDIT_ARCH_X86_64, *number, 0), ALLOW, "{number}");
        }
        for number in 0..450u32 {
            if !ALLOWED.contains(&number) && !NEVER_EXECUTABLE.contains(&number) {
                assert_eq!(verdict(AUDIT_ARCH_X86_64, number, 0), KILL, "{number}");
            }
        }
    }

    #[test]
    fn the_escalation_and_escape_syscalls_are_all_refused() {
        // open, socket, connect, fork, execve, kill, ptrace, setuid, chroot,
        // openat, tgkill, process_vm_writev.
        for number in [2u32, 41, 42, 57, 59, 62, 101, 105, 161, 257, 234, 311] {
            assert_eq!(verdict(AUDIT_ARCH_X86_64, number, 0), KILL, "{number}");
        }
    }

    #[test]
    fn mappings_may_not_become_executable() {
        for number in NEVER_EXECUTABLE {
            assert_eq!(verdict(AUDIT_ARCH_X86_64, *number, 3), ALLOW);
            assert_eq!(verdict(AUDIT_ARCH_X86_64, *number, PROT_EXEC), KILL);
            assert_eq!(verdict(AUDIT_ARCH_X86_64, *number, 7), KILL);
        }
    }

    #[test]
    fn the_jail_directory_must_be_roots_and_unwritable_by_others() {
        assert!(jail_directory_passes(true, 0, 0o755));
        assert!(jail_directory_passes(true, 0, 0o700));
        assert!(!jail_directory_passes(false, 0, 0o755));
        assert!(!jail_directory_passes(true, 995, 0o755));
        assert!(!jail_directory_passes(true, 0, 0o775));
        assert!(!jail_directory_passes(true, 0, 0o757));
        assert!(!jail_directory_passes(true, 0, 0o1777));
    }

    #[test]
    fn other_architectures_and_the_x32_range_die() {
        assert_eq!(verdict(0x4000_0003, SYS_READ, 0), KILL);
        assert_eq!(verdict(AUDIT_ARCH_X86_64, X32_SYSCALL_BIT, 0), KILL);
        assert_eq!(verdict(AUDIT_ARCH_X86_64, X32_SYSCALL_BIT | 1, 0), KILL);
    }
}

//! Compiled x86-64 application syscall policy.

use crate::sys::{self, SockFilter};
use std::io::{self, Write};

pub(crate) const AUDIT_ARCH_X86_64: u32 = 0xc000_003e;
pub(crate) const SECCOMP_RET_KILL_PROCESS: u32 = 0x8000_0000;
pub(crate) const SECCOMP_RET_ALLOW: u32 = 0x7fff_0000;
const SECCOMP_RET_ERRNO: u32 = 0x0005_0000;
const EPERM: u32 = 1;
const ENOSYS: u32 = 38;
const EAFNOSUPPORT: u32 = 97;

const BPF_LD_W_ABS: u16 = 0x20;
const BPF_JMP_JEQ_K: u16 = 0x15;
const BPF_ALU_AND_K: u16 = 0x54;
const BPF_RET_K: u16 = 0x06;

const OFFSET_NR: u32 = 0;
const OFFSET_ARCH: u32 = 4;
const OFFSET_ARG0_LOW: u32 = 16;
const OFFSET_ARG1_LOW: u32 = 24;

const SYS_IOCTL: u32 = 16;
const SYS_SOCKET: u32 = 41;
const SYS_CLONE: u32 = 56;
const SYS_PERSONALITY: u32 = 135;
const SYS_CLONE3: u32 = 435;
const CLONE_NEWUSER: u32 = 0x1000_0000;
const X32_SYSCALL_MASK: u32 = 0xc000_0000;
const X32_SYSCALL_BIT: u32 = 0x4000_0000;
const TIOCSTI: u32 = 0x5412;
const TIOCLINUX: u32 = 0x541c;

const AF_UNIX: u32 = 1;
const AF_INET: u32 = 2;
const AF_INET6: u32 = 10;
const AF_NETLINK: u32 = 16;

const fn insn(code: u16, jt: u8, jf: u8, k: u32) -> SockFilter {
    SockFilter { code, jt, jf, k }
}

const fn load(offset: u32) -> SockFilter {
    insn(BPF_LD_W_ABS, 0, 0, offset)
}

const fn jump_eq(value: u32, yes: u8, no: u8) -> SockFilter {
    insn(BPF_JMP_JEQ_K, yes, no, value)
}

const fn and(value: u32) -> SockFilter {
    insn(BPF_ALU_AND_K, 0, 0, value)
}

const fn ret(action: u32) -> SockFilter {
    insn(BPF_RET_K, 0, 0, action)
}

const fn errno(error: u32) -> u32 {
    SECCOMP_RET_ERRNO | error
}

macro_rules! count_items {
    ($($item:expr),* $(,)?) => {
        <[()]>::len(&[$(count_items!(@one $item)),*])
    };
    (@one $item:expr) => { () };
}

macro_rules! define_filter {
    ($($item:expr),+ $(,)?) => {
        pub(crate) const STANDARD_FILTER: [SockFilter; count_items!($($item),+)] = [
            $($item),+
        ];
    };
}

macro_rules! define_policy {
    ($($denied:expr),+ $(,)?) => {
        #[cfg(test)]
        const DENIED_SYSCALLS: &[u32] = &[$($denied),+];

        define_filter!(
            load(OFFSET_ARCH),
            jump_eq(AUDIT_ARCH_X86_64, 1, 0),
            ret(SECCOMP_RET_KILL_PROCESS),

            load(OFFSET_NR),
            and(X32_SYSCALL_MASK),
            jump_eq(X32_SYSCALL_BIT, 0, 1),
            ret(SECCOMP_RET_KILL_PROCESS),
            load(OFFSET_NR),

            // A socket family outside the four compiled families fails as if
            // the kernel did not support it.
            jump_eq(SYS_SOCKET, 0, 10),
            load(OFFSET_ARG0_LOW),
            jump_eq(AF_UNIX, 0, 1),
            ret(SECCOMP_RET_ALLOW),
            jump_eq(AF_INET, 0, 1),
            ret(SECCOMP_RET_ALLOW),
            jump_eq(AF_INET6, 0, 1),
            ret(SECCOMP_RET_ALLOW),
            jump_eq(AF_NETLINK, 0, 1),
            ret(SECCOMP_RET_ALLOW),
            ret(errno(EAFNOSUPPORT)),

            // Zero changes nothing and UINT32_MAX is the query operation.
            jump_eq(SYS_PERSONALITY, 0, 6),
            load(OFFSET_ARG0_LOW),
            jump_eq(0, 0, 1),
            ret(SECCOMP_RET_ALLOW),
            jump_eq(u32::MAX, 0, 1),
            ret(SECCOMP_RET_ALLOW),
            ret(errno(EPERM)),

            // ioctl requests are unsigned 32-bit values in the kernel ABI.
            jump_eq(SYS_IOCTL, 0, 6),
            load(OFFSET_ARG1_LOW),
            jump_eq(TIOCSTI, 0, 1),
            ret(errno(EPERM)),
            jump_eq(TIOCLINUX, 0, 1),
            ret(errno(EPERM)),
            load(OFFSET_NR),

            jump_eq(SYS_CLONE, 0, 4),
            load(OFFSET_ARG0_LOW),
            and(CLONE_NEWUSER),
            jump_eq(0, 1, 0),
            ret(errno(EPERM)),
            load(OFFSET_NR),

            // cBPF cannot inspect clone3's pointed-to flags. ENOSYS makes
            // libc retry with clone, whose inline flags are checked above.
            jump_eq(SYS_CLONE3, 0, 1),
            ret(errno(ENOSYS)),

            $(jump_eq($denied, 0, 1), ret(errno(EPERM)),)+
            ret(SECCOMP_RET_ALLOW),
        );
    };
}

// x86-64 syscall numbers are pinned to Linux's syscall_64.tbl. The obsolete
// entries are the named but unimplemented 64-bit table slots through vserver.
define_policy!(
    101, // ptrace
    103, // syslog
    134, // uselib
    136, // ustat
    139, // sysfs
    153, // vhangup
    154, // modify_ldt
    155, // pivot_root
    156, // _sysctl
    161, // chroot
    163, // acct
    165, // mount
    166, // umount2 (the x86-64 umount entry)
    167, // swapon
    168, // swapoff
    169, // reboot
    170, // sethostname
    171, // setdomainname
    174, // create_module
    175, // init_module
    176, // delete_module
    177, // get_kernel_syms
    178, // query_module
    179, // quotactl
    180, // nfsservctl
    181, // getpmsg
    182, // putpmsg
    183, // afs_syscall
    184, // tuxcall
    185, // security
    205, // set_thread_area
    211, // get_thread_area
    214, // epoll_ctl_old
    215, // epoll_wait_old
    236, // vserver
    237, // mbind
    238, // set_mempolicy
    239, // get_mempolicy
    246, // kexec_load
    248, // add_key
    249, // request_key
    250, // keyctl
    256, // migrate_pages
    272, // unshare
    279, // move_pages
    298, // perf_event_open
    304, // open_by_handle_at
    308, // setns
    310, // process_vm_readv
    311, // process_vm_writev
    313, // finit_module
    320, // kexec_file_load
    321, // bpf
    323, // userfaultfd
    425, // io_uring_setup
    426, // io_uring_enter
    427, // io_uring_register
    428, // open_tree
    429, // move_mount
    430, // fsopen
    431, // fsconfig
    432, // fsmount
    433, // fspick
    438, // pidfd_getfd
    442, // mount_setattr
    467, // open_tree_attr
);

pub(crate) struct Program<'a> {
    instructions: &'a [SockFilter],
}

impl Program<'_> {
    pub(crate) fn instructions(&self) -> &[SockFilter] {
        self.instructions
    }
}

pub(crate) fn standard_program() -> io::Result<Program<'static>> {
    validate(&STANDARD_FILTER, STANDARD_FILTER.len())
}

pub(crate) fn write_standard_filter(mut output: impl Write) -> io::Result<()> {
    let program = standard_program()?;
    let len = u16::try_from(program.instructions().len()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "seccomp filter exceeds the export format",
        )
    })?;
    output.write_all(b"TDB1")?;
    output.write_all(&len.to_le_bytes())?;
    output.write_all(&[0, 0])?;
    for instruction in program.instructions() {
        output.write_all(&instruction.code.to_le_bytes())?;
        output.write_all(&[instruction.jt, instruction.jf])?;
        output.write_all(&instruction.k.to_le_bytes())?;
    }
    Ok(())
}

fn validate(instructions: &[SockFilter], declared_len: usize) -> io::Result<Program<'_>> {
    if declared_len != instructions.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "seccomp filter length does not match its instruction array",
        ));
    }
    if instructions.is_empty()
        || instructions.len() > sys::SECCOMP_MAX_FILTER_INSNS
        || u16::try_from(instructions.len()).is_err()
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "seccomp filter instruction count is outside the kernel ABI",
        ));
    }
    for (index, instruction) in instructions.iter().enumerate() {
        match instruction.code {
            BPF_LD_W_ABS => {
                if instruction.jt != 0
                    || instruction.jf != 0
                    || !matches!(
                        instruction.k,
                        OFFSET_NR | OFFSET_ARCH | OFFSET_ARG0_LOW | OFFSET_ARG1_LOW
                    )
                {
                    return Err(invalid_instruction(index));
                }
            }
            BPF_ALU_AND_K => {
                if instruction.jt != 0 || instruction.jf != 0 {
                    return Err(invalid_instruction(index));
                }
            }
            BPF_JMP_JEQ_K => {
                let next = index.saturating_add(1);
                let yes = next.saturating_add(usize::from(instruction.jt));
                let no = next.saturating_add(usize::from(instruction.jf));
                if yes >= instructions.len() || no >= instructions.len() {
                    return Err(invalid_instruction(index));
                }
            }
            BPF_RET_K => {
                if instruction.jt != 0
                    || instruction.jf != 0
                    || !matches!(
                        instruction.k,
                        SECCOMP_RET_KILL_PROCESS
                            | SECCOMP_RET_ALLOW
                            | 0x0005_0001
                            | 0x0005_0026
                            | 0x0005_0061
                    )
                {
                    return Err(invalid_instruction(index));
                }
            }
            _ => return Err(invalid_instruction(index)),
        }
    }
    if instructions
        .last()
        .is_none_or(|instruction| instruction.code != BPF_RET_K)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "seccomp filter does not end in a return instruction",
        ));
    }
    Ok(Program { instructions })
}

fn invalid_instruction(index: usize) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("seccomp filter instruction {index} is invalid"),
    )
}

#[cfg(test)]
mod tests {
    #![allow(clippy::indexing_slicing, clippy::unwrap_used)]

    use super::*;

    #[derive(Clone, Copy)]
    struct Data {
        nr: i32,
        arch: u32,
        args: [u64; 6],
    }

    fn data(nr: i32) -> Data {
        Data {
            nr,
            arch: AUDIT_ARCH_X86_64,
            args: [0; 6],
        }
    }

    fn interpret(program: &[SockFilter], data: Data) -> Result<u32, String> {
        let mut accumulator = 0_u32;
        let mut pc = 0_usize;
        for _ in 0..4096 {
            let instruction = program
                .get(pc)
                .ok_or_else(|| format!("interpreter left the program at {pc}"))?;
            match instruction.code {
                BPF_LD_W_ABS => {
                    accumulator = match instruction.k {
                        OFFSET_NR => data.nr as u32,
                        OFFSET_ARCH => data.arch,
                        OFFSET_ARG0_LOW => data.args[0] as u32,
                        OFFSET_ARG1_LOW => data.args[1] as u32,
                        offset => return Err(format!("unsupported load offset {offset}")),
                    };
                    pc += 1;
                }
                BPF_ALU_AND_K => {
                    accumulator &= instruction.k;
                    pc += 1;
                }
                BPF_JMP_JEQ_K => {
                    let distance = if accumulator == instruction.k {
                        instruction.jt
                    } else {
                        instruction.jf
                    };
                    pc += 1 + usize::from(distance);
                }
                BPF_RET_K => return Ok(instruction.k),
                code => return Err(format!("unsupported BPF code {code:#x}")),
            }
        }
        Err("interpreter exceeded its instruction bound".to_string())
    }

    #[test]
    fn compiled_program_is_structurally_valid_and_length_bound() {
        let program = standard_program().unwrap();
        assert_eq!(program.instructions(), STANDARD_FILTER);
        assert_eq!(std::mem::size_of::<SockFilter>(), 8);
        assert!(validate(&STANDARD_FILTER, STANDARD_FILTER.len() + 1).is_err());

        let mut corrupt = STANDARD_FILTER;
        corrupt[1].jt = u8::MAX;
        assert!(validate(&corrupt, corrupt.len()).is_err());

        let maximum = vec![ret(SECCOMP_RET_ALLOW); sys::SECCOMP_MAX_FILTER_INSNS];
        assert!(validate(&maximum, maximum.len()).is_ok());
        let oversized = vec![ret(SECCOMP_RET_ALLOW); sys::SECCOMP_MAX_FILTER_INSNS + 1];
        assert!(validate(&oversized, oversized.len()).is_err());

        let mut exported = Vec::new();
        write_standard_filter(&mut exported).unwrap();
        assert_eq!(exported.get(..4), Some(b"TDB1".as_slice()));
        let count = u16::try_from(STANDARD_FILTER.len()).unwrap().to_le_bytes();
        assert_eq!(exported.get(4..6), Some(count.as_slice()));
        assert_eq!(exported.len(), 8 + STANDARD_FILTER.len() * 8);
    }

    #[test]
    fn architecture_x32_and_negative_numbers_are_exact() {
        let mut wrong = data(0);
        wrong.arch = 0x4000_0003;
        assert_eq!(
            interpret(&STANDARD_FILTER, wrong).unwrap(),
            SECCOMP_RET_KILL_PROCESS
        );
        assert_eq!(
            interpret(&STANDARD_FILTER, data((X32_SYSCALL_BIT | 1) as i32)).unwrap(),
            SECCOMP_RET_KILL_PROCESS
        );
        assert_eq!(
            interpret(&STANDARD_FILTER, data(-1)).unwrap(),
            SECCOMP_RET_ALLOW
        );
        assert_eq!(
            interpret(&STANDARD_FILTER, data(i32::MIN)).unwrap(),
            SECCOMP_RET_ALLOW
        );
    }

    #[test]
    fn argument_rules_match_the_policy() {
        for family in [AF_UNIX, AF_INET, AF_INET6, AF_NETLINK] {
            let mut call = data(SYS_SOCKET as i32);
            call.args[0] = u64::from(family);
            assert_eq!(
                interpret(&STANDARD_FILTER, call).unwrap(),
                SECCOMP_RET_ALLOW
            );
        }
        for family in [0, 3, 17, u32::MAX] {
            let mut call = data(SYS_SOCKET as i32);
            call.args[0] = u64::from(family);
            assert_eq!(
                interpret(&STANDARD_FILTER, call).unwrap(),
                errno(EAFNOSUPPORT)
            );
        }

        for personality in [0, u32::MAX] {
            let mut call = data(SYS_PERSONALITY as i32);
            call.args[0] = u64::from(personality);
            assert_eq!(
                interpret(&STANDARD_FILTER, call).unwrap(),
                SECCOMP_RET_ALLOW
            );
        }
        let mut personality = data(SYS_PERSONALITY as i32);
        personality.args[0] = 0x0040_0000;
        assert_eq!(
            interpret(&STANDARD_FILTER, personality).unwrap(),
            errno(EPERM)
        );

        for request in [TIOCSTI, TIOCLINUX] {
            for high in [0, 1_u64 << 32, !u64::from(u32::MAX)] {
                let mut call = data(SYS_IOCTL as i32);
                call.args[1] = high | u64::from(request);
                assert_eq!(interpret(&STANDARD_FILTER, call).unwrap(), errno(EPERM));
            }
        }
        let mut allowed_ioctl = data(SYS_IOCTL as i32);
        allowed_ioctl.args[1] = 0x5413;
        assert_eq!(
            interpret(&STANDARD_FILTER, allowed_ioctl).unwrap(),
            SECCOMP_RET_ALLOW
        );

        let mut clone = data(SYS_CLONE as i32);
        assert_eq!(
            interpret(&STANDARD_FILTER, clone).unwrap(),
            SECCOMP_RET_ALLOW
        );
        clone.args[0] = u64::from(CLONE_NEWUSER);
        assert_eq!(interpret(&STANDARD_FILTER, clone).unwrap(), errno(EPERM));
        assert_eq!(
            interpret(&STANDARD_FILTER, data(SYS_CLONE3 as i32)).unwrap(),
            errno(ENOSYS)
        );
    }

    #[test]
    fn every_rostered_number_and_an_allowed_page_have_the_expected_action() {
        for syscall in DENIED_SYSCALLS {
            assert_eq!(
                interpret(&STANDARD_FILTER, data(*syscall as i32)).unwrap(),
                errno(EPERM),
                "syscall {syscall}"
            );
        }
        for syscall in [335, 336] {
            assert_eq!(
                interpret(&STANDARD_FILTER, data(syscall)).unwrap(),
                SECCOMP_RET_ALLOW,
                "kernel-owned probe syscall {syscall}"
            );
        }
        for syscall in 0_u32..=471 {
            if DENIED_SYSCALLS.contains(&syscall)
                || matches!(
                    syscall,
                    SYS_IOCTL | SYS_SOCKET | SYS_CLONE | SYS_PERSONALITY | SYS_CLONE3
                )
            {
                continue;
            }
            assert_eq!(
                interpret(&STANDARD_FILTER, data(syscall as i32)).unwrap(),
                SECCOMP_RET_ALLOW,
                "syscall {syscall}"
            );
        }
    }
}

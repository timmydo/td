//! Compiled x86-64 application syscall policy.

use crate::authority::ApplicationFilter;
use crate::sys::{self, SockFilter};
use std::io::{self, Write};

pub(crate) const AUDIT_ARCH_X86_64: u32 = 0xc000_003e;
const AUDIT_ARCH_I386: u32 = 0x4000_0003;
pub(crate) const SECCOMP_RET_KILL_PROCESS: u32 = 0x8000_0000;
pub(crate) const SECCOMP_RET_ALLOW: u32 = 0x7fff_0000;
const SECCOMP_RET_ERRNO: u32 = 0x0005_0000;
const EPERM: u32 = 1;
const ENOSYS: u32 = 38;
const EAFNOSUPPORT: u32 = 97;
const ENOTTY: u32 = 25;
const ENOPROTOOPT: u32 = 92;
const SOL_SOCKET: u32 = 1;
const SO_RCVTIMEO: u32 = 20;
const SO_SNDTIMEO: u32 = 21;

pub(crate) const FIREFOX_AUDIT_MARKER: &str = "TD-FIREFOX-SECCOMP-OK probes=19";
pub(crate) const FIREFOX_PROBE_PATH: &str = "/opt/td-firefox-seccomp-probe";
const FIREFOX_PROBE_EXE: &str = "\"/opt/td-firefox-seccomp-probe\"";
const FIREFOX_AUDIT_PROBE_EXE: &str = "\"/var/lib/td-test/td-jail-seccomp-probe\"";
// Must match td-util's dmesg producer; the image source test binds them.
const DMESG_INCOMPLETE_MARKER: &str = "TD-DMESG-INCOMPLETE";
const AUDIT_SECCOMP_TYPE: &str = "type=1326";
const MAX_AUDIT_RECORDS: usize = 4096;
const MAX_AUDIT_LINE_BYTES: usize = 4096;

const BPF_LD_W_ABS: u16 = 0x20;
const BPF_JMP_JEQ_K: u16 = 0x15;
const BPF_JMP_JGE_K: u16 = 0x35;
const BPF_ALU_AND_K: u16 = 0x54;
const BPF_RET_K: u16 = 0x06;

const OFFSET_NR: u32 = 0;
const OFFSET_ARCH: u32 = 4;
const OFFSET_ARG0_LOW: u32 = 16;
const OFFSET_ARG1_LOW: u32 = 24;
const OFFSET_ARG2_LOW: u32 = 32;

const SYS_IOCTL: u32 = 16;
const SYS_SOCKET: u32 = 41;
const SYS_SOCKETPAIR: u32 = 53;
const SYS_SETSOCKOPT: u32 = 54;
const SYS_CLONE: u32 = 56;
const SYS_GETPPID: u32 = 110;
const SYS_PERSONALITY: u32 = 135;
const SYS_CLONE3: u32 = 435;
const CLONE_NEWUSER: u32 = 0x1000_0000;
const X32_SYSCALL_MASK: u32 = 0xc000_0000;
const X32_SYSCALL_BIT: u32 = 0x4000_0000;
const TIOCSTI: u32 = 0x5412;
const TIOCLINUX: u32 = 0x541c;
const FIONBIO: u32 = 0x5421;

// The highest number reviewed for this policy, from Linux 7.3's syscall_64.tbl.
// A higher one answers ENOSYS, as a kernel without it does, until a review
// raises this (APPLICATIONS.md, "The seccomp filter"). The recipe catalog holds
// the pinned kernel's series to the one reviewed.
const HIGHEST_REVIEWED_SYSCALL: u32 = 472;
// The hole in that table. An x86-only syscall lands here, as uprobe and
// uretprobe did just below it, so it answers as above the ceiling does.
const FIRST_UNASSIGNED_SYSCALL: u32 = 337;
const FIRST_ASSIGNED_AFTER_HOLE: u32 = 424;
// Sign bit set: no kernel has a syscall there, so its own answer stands.
const NEGATIVE_SYSCALL: u32 = 0x8000_0000;

const FIREFOX_REQUIRED_AUDIT_SYSCALLS: &[(u32, usize)] = &[
    (SYS_PERSONALITY, 1),
    (SYS_SOCKET, 1),
    (101, 1),
    (SYS_IOCTL, 3),
    (SYS_CLONE, 1),
    (SYS_CLONE3, 1),
    (272, 1),
    (165, 1),
    (467, 1),
    (472, 1),
    (HIGHEST_REVIEWED_SYSCALL + 1, 1),
    (323, 1),
    (321, 1),
    (425, 1),
    (438, 1),
    (310, 1),
    (X32_SYSCALL_BIT | 1, 1),
];

const AF_UNIX: u32 = 1;
const AF_INET: u32 = 2;
const AF_INET6: u32 = 10;
const AF_NETLINK: u32 = 16;
const SOCK_STREAM: u32 = 1;
const SOCK_SEQPACKET: u32 = 5;
const SOCK_NONBLOCK: u32 = 0o4000;
const SOCK_CLOEXEC: u32 = 0o2_000_000;

const fn insn(code: u16, jt: u8, jf: u8, k: u32) -> SockFilter {
    SockFilter { code, jt, jf, k }
}

const fn load(offset: u32) -> SockFilter {
    insn(BPF_LD_W_ABS, 0, 0, offset)
}

const fn jump_eq(value: u32, yes: u8, no: u8) -> SockFilter {
    insn(BPF_JMP_JEQ_K, yes, no, value)
}

const fn jump_ge(value: u32, yes: u8, no: u8) -> SockFilter {
    insn(BPF_JMP_JGE_K, yes, no, value)
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

macro_rules! define_workspace_filter {
    ($($item:expr),+ $(,)?) => {
        pub(crate) const WORKSPACE_FILTER: [SockFilter; count_items!($($item),+)] = [
            $($item),+
        ];
    };
}

macro_rules! define_policy {
    ($($denied:expr),+ $(,)?) => {
        pub(crate) const DENIED_SYSCALLS: &[u32] = &[$($denied),+];

        define_filter!(
            load(OFFSET_ARCH),
            jump_eq(AUDIT_ARCH_X86_64, 1, 0),
            ret(SECCOMP_RET_KILL_PROCESS),

            load(OFFSET_NR),
            and(X32_SYSCALL_MASK),
            jump_eq(X32_SYSCALL_BIT, 0, 1),
            ret(SECCOMP_RET_KILL_PROCESS),
            load(OFFSET_NR),

            jump_ge(NEGATIVE_SYSCALL, 0, 1),
            ret(SECCOMP_RET_ALLOW),
            jump_ge(HIGHEST_REVIEWED_SYSCALL + 1, 0, 1),
            ret(errno(ENOSYS)),
            jump_ge(FIRST_ASSIGNED_AFTER_HOLE, 2, 0),
            jump_ge(FIRST_UNASSIGNED_SYSCALL, 0, 1),
            ret(errno(ENOSYS)),

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

        // The `workspace` kind's program (APPLICATIONS.md §C): the standard
        // program but for its socket rules. A tool may make no Unix socket,
        // so it can reach none published in a worktree, and its only Unix
        // sockets are stream pairs: what a build's or test's own processes
        // talk over, and nothing anyone outside the pair can name.
        define_workspace_filter!(
            load(OFFSET_ARCH),
            jump_eq(AUDIT_ARCH_X86_64, 1, 0),
            ret(SECCOMP_RET_KILL_PROCESS),

            load(OFFSET_NR),
            and(X32_SYSCALL_MASK),
            jump_eq(X32_SYSCALL_BIT, 0, 1),
            ret(SECCOMP_RET_KILL_PROCESS),
            load(OFFSET_NR),

            jump_ge(NEGATIVE_SYSCALL, 0, 1),
            ret(SECCOMP_RET_ALLOW),
            jump_ge(HIGHEST_REVIEWED_SYSCALL + 1, 0, 1),
            ret(errno(ENOSYS)),
            jump_ge(FIRST_ASSIGNED_AFTER_HOLE, 2, 0),
            jump_ge(FIRST_UNASSIGNED_SYSCALL, 0, 1),
            ret(errno(ENOSYS)),

            // A Unix socket is refused outright; the other three families
            // as the standard program has them.
            jump_eq(SYS_SOCKET, 0, 10),
            load(OFFSET_ARG0_LOW),
            jump_eq(AF_UNIX, 0, 1),
            ret(errno(EPERM)),
            jump_eq(AF_INET, 0, 1),
            ret(SECCOMP_RET_ALLOW),
            jump_eq(AF_INET6, 0, 1),
            ret(SECCOMP_RET_ALLOW),
            jump_eq(AF_NETLINK, 0, 1),
            ret(SECCOMP_RET_ALLOW),
            ret(errno(EAFNOSUPPORT)),

            // A pair only of connected Unix sockets, stream or
            // sequenced-packet, its type compared with the two flag bits a
            // caller may add masked off. Neither can address another
            // socket: the kernel ignores a sequenced-packet pair's
            // destination. Rust's own `Command::spawn` makes the latter.
            jump_eq(SYS_SOCKETPAIR, 0, 8),
            load(OFFSET_ARG0_LOW),
            jump_eq(AF_UNIX, 0, 5),
            load(OFFSET_ARG1_LOW),
            and(!(SOCK_NONBLOCK | SOCK_CLOEXEC)),
            jump_eq(SOCK_STREAM, 1, 0),
            jump_eq(SOCK_SEQPACKET, 0, 1),
            ret(SECCOMP_RET_ALLOW),
            ret(errno(EPERM)),

            jump_eq(SYS_PERSONALITY, 0, 6),
            load(OFFSET_ARG0_LOW),
            jump_eq(0, 0, 1),
            ret(SECCOMP_RET_ALLOW),
            jump_eq(u32::MAX, 0, 1),
            ret(SECCOMP_RET_ALLOW),
            ret(errno(EPERM)),

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
    116, // setgroups
    134, // uselib
    136, // ustat
    139, // sysfs
    153, // vhangup
    154, // modify_ldt
    155, // pivot_root
    156, // _sysctl
    161, // chroot
    163, // acct
    164, // settimeofday
    165, // mount
    166, // umount2 (the x86-64 umount entry)
    167, // swapon
    168, // swapoff
    169, // reboot
    170, // sethostname
    171, // setdomainname
    172, // iopl
    173, // ioperm
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
    212, // lookup_dcookie
    214, // epoll_ctl_old
    215, // epoll_wait_old
    227, // clock_settime
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
    443, // quotactl_fd
    450, // set_mempolicy_home_node
    467, // open_tree_attr
    472, // fchroot
);

// Development permits ordinary execution and nested isolation. The paired
// numbers are x86-64/i386 from Linux's syscall tables; socketcall is allowed.
macro_rules! development_policy {
    ($(($native:expr, $compat:expr)),+ $(,)?) => {
        #[cfg(test)]
        const DEVELOPMENT_DENIED: &[(u32, u32)] = &[$(($native, $compat)),+];
        const DEVELOPMENT_NATIVE: &[SockFilter] = &[
            load(OFFSET_NR),
            and(X32_SYSCALL_MASK),
            jump_eq(X32_SYSCALL_BIT, 0, 1),
            ret(SECCOMP_RET_KILL_PROCESS),
            load(OFFSET_NR),
            jump_eq(SYS_IOCTL, 0, 6),
            load(OFFSET_ARG1_LOW),
            jump_eq(TIOCSTI, 0, 1), ret(errno(EPERM)),
            jump_eq(TIOCLINUX, 0, 1), ret(errno(EPERM)),
            load(OFFSET_NR),
            $(jump_eq($native, 0, 1), ret(errno(EPERM)),)+
            ret(SECCOMP_RET_ALLOW),
        ];
        const DEVELOPMENT_COMPAT: &[SockFilter] = &[
            load(OFFSET_NR),
            jump_eq(54, 0, 6), // i386 ioctl
            load(OFFSET_ARG1_LOW),
            jump_eq(TIOCSTI, 0, 1), ret(errno(EPERM)),
            jump_eq(TIOCLINUX, 0, 1), ret(errno(EPERM)),
            load(OFFSET_NR),
            $(jump_eq($compat, 0, 1), ret(errno(EPERM)),)+
            ret(SECCOMP_RET_ALLOW),
        ];
    };
}

development_policy!(
    (103, 103),      // syslog
    (153, 111),      // vhangup
    (163, 51),       // acct
    (167, 87),       // swapon
    (168, 115),      // swapoff
    (172, 110),      // iopl
    (173, 101),      // ioperm
    (175, 128),      // init_module
    (176, 129),      // delete_module
    (246, 283),      // kexec_load
    (248, 286),      // add_key
    (249, 287),      // request_key
    (250, 288),      // keyctl
    (304, 342),      // open_by_handle_at
    (313, 350),      // finit_module
    (320, u32::MAX), // kexec_file_load has no i386 entry
    (321, 357),      // bpf
);

pub(crate) fn development_program() -> io::Result<Vec<SockFilter>> {
    let compat = u8::try_from(DEVELOPMENT_NATIVE.len() + 1)
        .map_err(|_| io::Error::other("development ABI dispatch exceeds BPF jump bound"))?;
    let mut program = vec![
        load(OFFSET_ARCH),
        jump_eq(AUDIT_ARCH_X86_64, 2, 0),
        jump_eq(AUDIT_ARCH_I386, compat, 0),
        ret(SECCOMP_RET_KILL_PROCESS),
    ];
    program.extend_from_slice(DEVELOPMENT_NATIVE);
    program.extend_from_slice(DEVELOPMENT_COMPAT);
    validate(&program, program.len())?;
    Ok(program)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct AuditAction {
    code: u32,
    signal: u32,
}

/// A number the reviewed table does not have, which both programs refuse.
const fn unreviewed_number(syscall: u32) -> bool {
    (HIGHEST_REVIEWED_SYSCALL < syscall && syscall < NEGATIVE_SYSCALL)
        || (FIRST_UNASSIGNED_SYSCALL <= syscall && syscall < FIRST_ASSIGNED_AFTER_HOLE)
}

fn expected_audit_action(syscall: u32) -> Option<AuditAction> {
    if syscall & X32_SYSCALL_MASK == X32_SYSCALL_BIT {
        return Some(AuditAction {
            code: SECCOMP_RET_KILL_PROCESS,
            signal: 31,
        });
    }
    if syscall == SYS_SOCKET
        || matches!(
            syscall,
            SYS_PERSONALITY | SYS_IOCTL | SYS_CLONE | SYS_CLONE3
        )
        || DENIED_SYSCALLS.contains(&syscall)
        || unreviewed_number(syscall)
    {
        // Audit records omit SECCOMP_RET_DATA, so the helper proves the exact
        // errno while the log can prove only the ERRNO action class.
        Some(AuditAction {
            code: SECCOMP_RET_ERRNO,
            signal: 0,
        })
    } else {
        None
    }
}

fn audit_field<'a>(line: &'a str, name: &str) -> io::Result<&'a str> {
    let mut values = line.split_ascii_whitespace().filter_map(|field| {
        let (key, value) = field.split_once('=')?;
        (key == name).then_some(value)
    });
    let value = values.next().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("seccomp audit record has no {name}"),
        )
    })?;
    if values.next().is_some() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("seccomp audit record repeats {name}"),
        ));
    }
    Ok(value)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AuditBarrier {
    Begin,
    End,
}

fn audit_barrier(line: &str) -> io::Result<Option<AuditBarrier>> {
    if !line
        .split_ascii_whitespace()
        .any(|field| field == AUDIT_SECCOMP_TYPE)
    {
        return Ok(None);
    }
    if audit_field(line, "exe")? != FIREFOX_AUDIT_PROBE_EXE {
        return Ok(None);
    }
    if parse_decimal_field(line, "uid")? != 0
        || parse_decimal_field(line, "syscall")? != SYS_GETPPID
    {
        return Ok(None);
    }
    let arch = parse_hex_field(line, "arch")?;
    let compat = parse_decimal_field(line, "compat")?;
    let signal = parse_decimal_field(line, "sig")?;
    let code = parse_hex_field(line, "code")?;
    if arch != AUDIT_ARCH_X86_64 || compat != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Firefox seccomp audit barrier has the wrong ABI",
        ));
    }
    match (code, signal) {
        (SECCOMP_RET_KILL_PROCESS, 31) => Ok(Some(AuditBarrier::Begin)),
        (SECCOMP_RET_ERRNO, 0) => Ok(Some(AuditBarrier::End)),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Firefox seccomp audit barrier has the wrong action",
        )),
    }
}

fn parse_decimal_field(line: &str, name: &str) -> io::Result<u32> {
    audit_field(line, name)?.parse().map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("seccomp audit record has invalid {name}"),
        )
    })
}

fn parse_hex_field(line: &str, name: &str) -> io::Result<u32> {
    let value = audit_field(line, name)?;
    let value = value.strip_prefix("0x").unwrap_or(value);
    u32::from_str_radix(value, 16).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("seccomp audit record has invalid {name}"),
        )
    })
}

pub(crate) fn verify_firefox_audit(
    log: &str,
    firefox_pid: u32,
    firefox_uid: u32,
) -> io::Result<usize> {
    if firefox_pid == 0 || firefox_uid == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Firefox seccomp audit expected PID and UID must be nonzero",
        ));
    }
    let mut observed = vec![0_usize; FIREFOX_REQUIRED_AUDIT_SYSCALLS.len()];
    let mut records = 0_usize;
    let mut begun = false;
    let mut ended = false;
    for line in log.lines() {
        if line.contains(DMESG_INCOMPLETE_MARKER) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Firefox seccomp audit has an incomplete kernel-log drain",
            ));
        }
        if line.contains("audit_lost=")
            || line.contains("audit: rate limit exceeded")
            || (line.contains("audit:") && line.contains("callbacks suppressed"))
            || (line.contains("kauditd_printk_skb:") && line.contains("callbacks suppressed"))
            || line.contains("audit: kauditd")
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Firefox seccomp audit reports dropped or suppressed kernel records",
            ));
        }
        match audit_barrier(line)? {
            Some(AuditBarrier::Begin) if !begun && !ended => {
                begun = true;
                continue;
            }
            Some(AuditBarrier::End) if begun && !ended => {
                ended = true;
                continue;
            }
            Some(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Firefox seccomp audit barriers are repeated or out of order",
                ));
            }
            None => {}
        }
        if !begun || ended {
            continue;
        }
        if line.len() > MAX_AUDIT_LINE_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Firefox seccomp audit line exceeds 4096 bytes",
            ));
        }
        if !line
            .split_ascii_whitespace()
            .any(|field| field == AUDIT_SECCOMP_TYPE)
        {
            continue;
        }
        records = records.saturating_add(1);
        if records > MAX_AUDIT_RECORDS {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Firefox seccomp audit exceeds 4096 records",
            ));
        }
        let uid = parse_decimal_field(line, "uid")?;
        let pid = parse_decimal_field(line, "pid")?;
        let arch = parse_hex_field(line, "arch")?;
        let syscall = parse_decimal_field(line, "syscall")?;
        let compat = parse_decimal_field(line, "compat")?;
        let signal = parse_decimal_field(line, "sig")?;
        let code = parse_hex_field(line, "code")?;
        // The independent boot-health probe runs concurrently under the human
        // UID. Its fixed root-owned executable's ABI kills are not Firefox data.
        if uid == 1000
            && pid != 0
            && pid != firefox_pid
            && ((arch == AUDIT_ARCH_X86_64 && compat == 0 && syscall == X32_SYSCALL_BIT | 1)
                || (arch == AUDIT_ARCH_I386 && compat == 1 && syscall == 1))
            && signal == 31
            && code == SECCOMP_RET_KILL_PROCESS
            && audit_field(line, "exe")? == "\"/run/td-jail-seccomp-probe/probe\""
        {
            continue;
        }
        if uid != firefox_uid || pid == 0 || arch != AUDIT_ARCH_X86_64 || compat != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("seccomp audit record is not an x86-64 assigned-UID Firefox-jail denial: uid={uid} expected={firefox_uid} pid={pid} arch={arch:#x} compat={compat}"),
            ));
        }
        let action = expected_audit_action(syscall).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("seccomp audit denied syscall {syscall}, which the filter allows"),
            )
        })?;
        if action.code != code || action.signal != signal {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "seccomp audit syscall {syscall} used code {code:#x}/signal {signal}, expected {:#x}/{}",
                    action.code, action.signal
                ),
            ));
        }
        if audit_field(line, "exe")? == FIREFOX_PROBE_EXE {
            let x32_child = syscall & X32_SYSCALL_MASK == X32_SYSCALL_BIT;
            if (!x32_child && pid != firefox_pid) || (x32_child && pid == firefox_pid) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "Firefox outer-process probe syscall {syscall} used pid {pid}, \
                         expected main pid {firefox_pid}{}",
                        if x32_child { " to differ" } else { "" }
                    ),
                ));
            }
            for (slot, (wanted_syscall, _)) in FIREFOX_REQUIRED_AUDIT_SYSCALLS.iter().enumerate() {
                if syscall == *wanted_syscall {
                    if let Some(count) = observed.get_mut(slot) {
                        *count = count.saturating_add(1);
                    }
                }
            }
        }
    }
    if !begun || !ended {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Firefox seccomp audit has no complete ordered barrier pair",
        ));
    }
    for (slot, (syscall, minimum)) in FIREFOX_REQUIRED_AUDIT_SYSCALLS.iter().enumerate() {
        if observed.get(slot).copied().unwrap_or_default() < *minimum {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "Firefox outer-process probe did not log syscall {syscall} at least {minimum} time(s)"
                ),
            ));
        }
    }
    Ok(records)
}

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

pub(crate) fn workspace_program() -> io::Result<Program<'static>> {
    validate(&WORKSPACE_FILTER, WORKSPACE_FILTER.len())
}

// What a td-owned static application's jail reaches, by number. The derivation
// is in APPLICATIONS.md, "Per-application allowlists": the realized binaries'
// reachable set, without what only an option td never sets can arrive at, plus
// the one call no instruction in them names. socket, socketpair, setsockopt,
// ioctl and clone are not here:
// each has its argument rule in `allowlist_program`.
const APPLICATION_ALLOWED: &[u32] = &[
    0,   // read
    1,   // write
    3,   // close
    5,   // fstat
    7,   // poll
    8,   // lseek
    9,   // mmap
    10,  // mprotect
    11,  // munmap
    12,  // brk
    13,  // rt_sigaction
    14,  // rt_sigprocmask
    15,  // rt_sigreturn
    17,  // pread64
    18,  // pwrite64
    20,  // writev
    24,  // sched_yield
    25,  // mremap
    28,  // madvise
    32,  // dup
    33,  // dup2
    34,  // pause
    39,  // getpid
    42,  // connect
    44,  // sendto
    45,  // recvfrom
    46,  // sendmsg
    47,  // recvmsg
    51,  // getsockname
    52,  // getpeername
    59,  // execve
    60,  // exit
    61,  // wait4
    62,  // kill: stage 2 ends the namespace's survivors
    63,  // uname
    72,  // fcntl
    73,  // flock
    74,  // fsync
    77,  // ftruncate
    79,  // getcwd
    80,  // chdir
    81,  // fchdir
    82,  // rename
    83,  // mkdir
    87,  // unlink
    89,  // readlink
    99,  // sysinfo
    102, // getuid
    104, // getgid
    107, // geteuid
    108, // getegid
    115, // getgroups
    131, // sigaltstack
    143, // sched_getparam
    145, // sched_getscheduler
    157, // prctl
    158, // arch_prctl
    186, // gettid
    202, // futex
    204, // sched_getaffinity
    217, // getdents64
    218, // set_tid_address
    219, // restart_syscall: the kernel's, after a signal, in no binary
    228, // clock_gettime: also the vDSO's fallback
    230, // clock_nanosleep
    231, // exit_group
    234, // tgkill
    247, // waitid: glibc reaps a spawn whose exec failed
    257, // openat
    262, // newfstatat
    267, // readlinkat
    269, // faccessat
    273, // set_robust_list
    293, // pipe2
    302, // prlimit64
    318, // getrandom
    332, // statx
    334, // rseq
    439, // faccessat2
];

// What td-mail and td-secret reach and td-news and td-open do not. Its
// fdatasync is td-kv's sync_data, which news reaches too and td-web does
// not link.
const MAIL_ALLOWED: &[u32] = &[
    55,  // getsockopt: td-secret's peer_uid
    75,  // fdatasync: td-kv
    84,  // rmdir: td-mail
    91,  // fchmod: td-mail and td-secret
    93,  // fchown: td-secret
    263, // unlinkat: td-mail
    319, // memfd_create: td-secret's create_credential
];

// td-kv's sync_data, which td-web does not link.
const NEWS_ALLOWED: &[u32] = &[
    75, // fdatasync: td-kv
];

// td-web reaches nothing beyond what every application does: its threads,
// fetches, decoders and unwinding name no other number.
const WEB_ALLOWED: &[u32] = &[];

pub(crate) fn application_program(filter: ApplicationFilter) -> io::Result<Vec<SockFilter>> {
    match filter {
        ApplicationFilter::Standard => Ok(standard_program()?.instructions().to_vec()),
        ApplicationFilter::Mail => allowlist_program(MAIL_ALLOWED),
        ApplicationFilter::News => allowlist_program(NEWS_ALLOWED),
        ApplicationFilter::Web => allowlist_program(WEB_ALLOWED),
    }
}

// ENOSYS for what the list lacks, as a kernel without the call answers, and
// ENOTTY for an ioctl, as a descriptor without the request does. No such
// program has a terminal grant, so tcgetattr answers as it would anyway.
// Neither is a kill: a program must be able to report what failed.
fn allowlist_program(extra: &[u32]) -> io::Result<Vec<SockFilter>> {
    let mut program = vec![
        load(OFFSET_ARCH),
        jump_eq(AUDIT_ARCH_X86_64, 1, 0),
        ret(SECCOMP_RET_KILL_PROCESS),
        load(OFFSET_NR),
        and(X32_SYSCALL_MASK),
        jump_eq(X32_SYSCALL_BIT, 0, 1),
        ret(SECCOMP_RET_KILL_PROCESS),
        load(OFFSET_NR),
        // A Unix socket and nothing else: these programs reach the network
        // through the fetch service's socket.
        jump_eq(SYS_SOCKET, 0, 4),
        load(OFFSET_ARG0_LOW),
        jump_eq(AF_UNIX, 0, 1),
        ret(SECCOMP_RET_ALLOW),
        ret(errno(EAFNOSUPPORT)),
        // As the workspace program has it: Rust's Command makes the second.
        jump_eq(SYS_SOCKETPAIR, 0, 8),
        load(OFFSET_ARG0_LOW),
        jump_eq(AF_UNIX, 0, 5),
        load(OFFSET_ARG1_LOW),
        and(!(SOCK_NONBLOCK | SOCK_CLOEXEC)),
        jump_eq(SOCK_STREAM, 1, 0),
        jump_eq(SOCK_SEQPACKET, 0, 1),
        ret(SECCOMP_RET_ALLOW),
        ret(errno(EPERM)),
        // A socket's two timeouts, which is all std's Unix stream sets. Among
        // what this leaves out is SO_ATTACH_FILTER, a program of the caller's.
        jump_eq(SYS_SETSOCKOPT, 0, 7),
        load(OFFSET_ARG1_LOW),
        jump_eq(SOL_SOCKET, 0, 4),
        load(OFFSET_ARG2_LOW),
        jump_eq(SO_RCVTIMEO, 1, 0),
        jump_eq(SO_SNDTIMEO, 0, 1),
        ret(SECCOMP_RET_ALLOW),
        ret(errno(ENOPROTOOPT)),
        // ioctl requests are unsigned 32-bit values in the kernel ABI. Why this
        // is the one request: APPLICATIONS.md, "Per-application allowlists".
        jump_eq(SYS_IOCTL, 0, 4),
        load(OFFSET_ARG1_LOW),
        jump_eq(FIONBIO, 0, 1),
        ret(SECCOMP_RET_ALLOW),
        ret(errno(ENOTTY)),
        jump_eq(SYS_CLONE, 0, 5),
        load(OFFSET_ARG0_LOW),
        and(CLONE_NEWUSER),
        jump_eq(0, 1, 0),
        ret(errno(EPERM)),
        ret(SECCOMP_RET_ALLOW),
    ];
    for syscall in APPLICATION_ALLOWED.iter().chain(extra) {
        program.push(jump_eq(*syscall, 0, 1));
        program.push(ret(SECCOMP_RET_ALLOW));
    }
    // clone3 is among what this refuses: libc retries with clone, above.
    program.push(ret(errno(ENOSYS)));
    validate(&program, program.len())?;
    Ok(program)
}

pub(crate) fn write_filter(filter: ApplicationFilter, mut output: impl Write) -> io::Result<()> {
    let program = application_program(filter)?;
    let len = u16::try_from(program.len()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "seccomp filter exceeds the export format",
        )
    })?;
    output.write_all(b"TDB1")?;
    output.write_all(&len.to_le_bytes())?;
    output.write_all(&[0, 0])?;
    for instruction in &program {
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
                        OFFSET_NR
                            | OFFSET_ARCH
                            | OFFSET_ARG0_LOW
                            | OFFSET_ARG1_LOW
                            | OFFSET_ARG2_LOW
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
            BPF_JMP_JEQ_K | BPF_JMP_JGE_K => {
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
                            | 0x0005_0019
                            | 0x0005_0026
                            | 0x0005_005c
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

    const FIREFOX_PID: u32 = 7;
    const X32_CHILD_PID: u32 = 8;

    fn verify_firefox_audit(log: &str) -> io::Result<usize> {
        super::verify_firefox_audit(log, FIREFOX_PID, 65536)
    }

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
                        OFFSET_ARG2_LOW => data.args[2] as u32,
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
                BPF_JMP_JGE_K => {
                    let distance = if accumulator >= instruction.k {
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
    fn development_abis_preserve_denials_and_allow_nested_builds() {
        let program = development_program().unwrap();
        for (native, compat) in DEVELOPMENT_DENIED {
            for (arch, number) in [(AUDIT_ARCH_X86_64, *native), (AUDIT_ARCH_I386, *compat)] {
                let mut call = data(number as i32);
                call.arch = arch;
                assert_eq!(interpret(&program, call).unwrap(), errno(EPERM));
            }
        }
        for (arch, numbers, ioctl) in [
            (
                AUDIT_ARCH_X86_64,
                &[
                    56, 435, 272, 165, 166, 155, 161, 472, 308, 101, 298, 310, 425, 426, 427, 442,
                ][..],
                16,
            ),
            (
                AUDIT_ARCH_I386,
                &[
                    120, 435, 310, 21, 52, 217, 61, 472, 346, 26, 336, 347, 425, 426, 427, 442,
                ][..],
                54,
            ),
        ] {
            for number in numbers {
                let mut call = data(*number);
                call.arch = arch;
                call.args[0] = u64::MAX;
                assert_eq!(interpret(&program, call).unwrap(), SECCOMP_RET_ALLOW);
            }
            for request in [TIOCSTI, TIOCLINUX] {
                let mut call = data(ioctl);
                call.arch = arch;
                call.args[1] = u64::from(request);
                assert_eq!(interpret(&program, call).unwrap(), errno(EPERM));
            }
        }
        let mut call = data(1);
        call.arch = 0;
        assert_eq!(interpret(&program, call).unwrap(), SECCOMP_RET_KILL_PROCESS);
        assert_eq!(
            interpret(&program, data((X32_SYSCALL_BIT | 1) as i32)).unwrap(),
            SECCOMP_RET_KILL_PROCESS
        );
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
        write_filter(ApplicationFilter::Standard, &mut exported).unwrap();
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

    // Linux 7.3's fchroot(FD_FAILFS_ROOT, 0) needs no capability from a caller
    // with no_new_privs, an unshared fs_struct and no chroot, so unlike chroot
    // no dropped capability refuses it. The page loop below would read a
    // dropped entry as an allowed number.
    #[test]
    fn both_seccomp_programs_refuse_fchroot_by_number() {
        for filter in [STANDARD_FILTER.as_slice(), WORKSPACE_FILTER.as_slice()] {
            assert_eq!(interpret(filter, data(472)).unwrap(), errno(EPERM));
        }
    }

    // The other loops read the roster they test, so a line dropped by a bad
    // rebase would leave them all green. This one does not.
    #[test]
    fn the_roster_is_exactly_this() {
        assert_eq!(
            DENIED_SYSCALLS,
            &[
                101, 103, 116, 134, 136, 139, 153, 154, 155, 156, 161, 163, 164, 165, 166, 167,
                168, 169, 170, 171, 172, 173, 174, 175, 176, 177, 178, 179, 180, 181, 182, 183,
                184, 185, 205, 211, 212, 214, 215, 227, 236, 237, 238, 239, 246, 248, 249, 250,
                256, 272, 279, 298, 304, 308, 310, 311, 313, 320, 321, 323, 425, 426, 427, 428,
                429, 430, 431, 432, 433, 438, 442, 443, 450, 467, 472
            ]
        );
    }

    // The default-allow development program must never refuse what these do
    // not: iopl and ioperm once were the example.
    #[test]
    fn the_application_roster_covers_the_development_denials() {
        for (native, _) in DEVELOPMENT_DENIED {
            assert!(DENIED_SYSCALLS.contains(native), "syscall {native}");
        }
    }

    // A kernel bump must not widen either program by itself, whichever end of
    // the table its new number lands at. The page loop and fchroot's test above
    // hold the answers for the numbers the table has.
    #[test]
    fn numbers_the_reviewed_table_lacks_fail_as_on_a_kernel_without_them() {
        for filter in [STANDARD_FILTER.as_slice(), WORKSPACE_FILTER.as_slice()] {
            for syscall in [
                FIRST_UNASSIGNED_SYSCALL,
                386,
                387,
                FIRST_ASSIGNED_AFTER_HOLE - 1,
                HIGHEST_REVIEWED_SYSCALL + 1,
                HIGHEST_REVIEWED_SYSCALL + 2,
                511,
                512,
                X32_SYSCALL_BIT - 1,
            ] {
                assert_eq!(
                    interpret(filter, data(syscall as i32)).unwrap(),
                    errno(ENOSYS),
                    "syscall {syscall}"
                );
            }
            // The boundary, and what the kernel still answers for itself.
            for syscall in [
                FIRST_UNASSIGNED_SYSCALL - 1,
                FIRST_ASSIGNED_AFTER_HOLE,
                HIGHEST_REVIEWED_SYSCALL,
            ] {
                assert_ne!(
                    interpret(filter, data(syscall as i32)).unwrap(),
                    errno(ENOSYS),
                    "syscall {syscall}"
                );
            }
            for syscall in [-1, i32::MIN, 0xc000_0000_u32 as i32] {
                assert_eq!(
                    interpret(filter, data(syscall)).unwrap(),
                    SECCOMP_RET_ALLOW,
                    "syscall {syscall}"
                );
            }
            assert_eq!(
                interpret(filter, data(X32_SYSCALL_BIT as i32)).unwrap(),
                SECCOMP_RET_KILL_PROCESS
            );
        }
    }

    // The page loop skips what the predicate names, so widening the predicate
    // would thin the loop rather than fail it, and the audit parser would then
    // accept denials no filter makes. Both programs hold it to the exact set.
    #[test]
    fn the_refused_number_predicate_is_exactly_what_the_programs_refuse() {
        for filter in [STANDARD_FILTER.as_slice(), WORKSPACE_FILTER.as_slice()] {
            for syscall in 0_u32..=600 {
                let enosys = interpret(filter, data(syscall as i32)).unwrap() == errno(ENOSYS);
                assert_eq!(
                    enosys,
                    unreviewed_number(syscall) || syscall == SYS_CLONE3,
                    "syscall {syscall}"
                );
            }
        }
    }

    // The interpreter shares these constants with the programs, so it cannot
    // tell JGE from JGT: under that, the first refused number reaches the
    // kernel, which answers ENOSYS too, and only the audit oracle would see.
    #[test]
    fn jump_opcodes_are_the_kernel_values() {
        assert_eq!(BPF_JMP_JEQ_K, 0x15);
        assert_eq!(BPF_JMP_JGE_K, 0x35);
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
        for syscall in 0_u32..=HIGHEST_REVIEWED_SYSCALL {
            if DENIED_SYSCALLS.contains(&syscall)
                || unreviewed_number(syscall)
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

    #[test]
    fn the_workspace_program_differs_only_in_its_socket_rules() {
        let program = workspace_program().unwrap();
        assert_eq!(program.instructions(), WORKSPACE_FILTER);
        let mut unix = data(SYS_SOCKET as i32);
        unix.args[0] = u64::from(AF_UNIX);
        assert_eq!(interpret(&WORKSPACE_FILTER, unix).unwrap(), errno(EPERM));
        for family in [AF_INET, AF_INET6, AF_NETLINK] {
            let mut call = data(SYS_SOCKET as i32);
            call.args[0] = u64::from(family);
            assert_eq!(
                interpret(&WORKSPACE_FILTER, call).unwrap(),
                SECCOMP_RET_ALLOW
            );
        }
        for family in [0, 3, 17, u32::MAX] {
            let mut call = data(SYS_SOCKET as i32);
            call.args[0] = u64::from(family);
            assert_eq!(
                interpret(&WORKSPACE_FILTER, call).unwrap(),
                errno(EAFNOSUPPORT)
            );
        }
        let pair = |family: u32, kind: u32| {
            let mut call = data(SYS_SOCKETPAIR as i32);
            call.args[0] = u64::from(family);
            call.args[1] = u64::from(kind);
            interpret(&WORKSPACE_FILTER, call).unwrap()
        };
        for flags in [0, SOCK_NONBLOCK, SOCK_CLOEXEC, SOCK_NONBLOCK | SOCK_CLOEXEC] {
            assert_eq!(pair(AF_UNIX, SOCK_STREAM | flags), SECCOMP_RET_ALLOW);
            assert_eq!(pair(AF_UNIX, SOCK_SEQPACKET | flags), SECCOMP_RET_ALLOW);
        }
        // Datagram, raw and reliable-datagram pairs, another family, and a
        // stream or sequenced-packet type with any other bit, are refused.
        for (family, kind) in [
            (AF_UNIX, 2),
            (AF_UNIX, 3),
            (AF_UNIX, 4),
            (AF_UNIX, 6),
            (AF_UNIX, SOCK_STREAM | 0x100),
            (AF_UNIX, SOCK_SEQPACKET | 0x100),
            (AF_INET, SOCK_STREAM),
            (0, SOCK_STREAM),
        ] {
            assert_eq!(pair(family, kind), errno(EPERM), "{family} {kind:#x}");
        }
        // The standard program leaves socketpair to the kernel.
        let mut datagram = data(SYS_SOCKETPAIR as i32);
        datagram.args[0] = u64::from(AF_UNIX);
        datagram.args[1] = 2;
        assert_eq!(
            interpret(&STANDARD_FILTER, datagram).unwrap(),
            SECCOMP_RET_ALLOW
        );
        // Everything else is the standard program's answer.
        for syscall in 0_u32..=HIGHEST_REVIEWED_SYSCALL {
            if matches!(syscall, SYS_SOCKET | SYS_SOCKETPAIR) {
                continue;
            }
            let mut call = data(syscall as i32);
            for args in [
                [0_u64; 6],
                [u64::from(CLONE_NEWUSER), u64::from(TIOCSTI), 0, 0, 0, 0],
            ] {
                call.args = args;
                assert_eq!(
                    interpret(&WORKSPACE_FILTER, call).unwrap(),
                    interpret(&STANDARD_FILTER, call).unwrap(),
                    "syscall {syscall}"
                );
            }
        }
        let mut wrong = data(0);
        wrong.arch = 0x4000_0003;
        assert_eq!(
            interpret(&WORKSPACE_FILTER, wrong).unwrap(),
            SECCOMP_RET_KILL_PROCESS
        );
        assert_eq!(
            interpret(&WORKSPACE_FILTER, data((X32_SYSCALL_BIT | 1) as i32)).unwrap(),
            SECCOMP_RET_KILL_PROCESS
        );
    }

    fn allowlist_programs() -> [(ApplicationFilter, Vec<SockFilter>); 3] {
        [
            ApplicationFilter::Mail,
            ApplicationFilter::News,
            ApplicationFilter::Web,
        ]
        .map(|filter| {
            let program = application_program(filter).unwrap();
            (filter, program)
        })
    }

    // As for the roster: the other tests read these lists, so a line dropped by
    // a bad rebase would stay green and fail for a person.
    #[test]
    fn the_allowlists_are_exactly_these() {
        assert_eq!(
            APPLICATION_ALLOWED,
            &[
                0, 1, 3, 5, 7, 8, 9, 10, 11, 12, 13, 14, 15, 17, 18, 20, 24, 25, 28, 32, 33, 34,
                39, 42, 44, 45, 46, 47, 51, 52, 59, 60, 61, 62, 63, 72, 73, 74, 77, 79, 80, 81, 82,
                83, 87, 89, 99, 102, 104, 107, 108, 115, 131, 143, 145, 157, 158, 186, 202, 204,
                217, 218, 219, 228, 230, 231, 234, 247, 257, 262, 267, 269, 273, 293, 302, 318,
                332, 334, 439
            ]
        );
        assert_eq!(MAIL_ALLOWED, &[55, 75, 84, 91, 93, 263, 319]);
        assert_eq!(NEWS_ALLOWED, &[75]);
        assert!(WEB_ALLOWED.is_empty());
    }

    #[test]
    fn the_allowlists_are_sorted_distinct_real_and_never_a_denied_number() {
        for list in [APPLICATION_ALLOWED, MAIL_ALLOWED, NEWS_ALLOWED, WEB_ALLOWED] {
            assert!(list.windows(2).all(|pair| pair[0] < pair[1]));
            for syscall in list {
                assert!(!DENIED_SYSCALLS.contains(syscall), "denied {syscall}");
                assert!(!unreviewed_number(*syscall), "not in the table: {syscall}");
                assert!(
                    !matches!(
                        *syscall,
                        SYS_SOCKET
                            | SYS_SOCKETPAIR
                            | SYS_SETSOCKOPT
                            | SYS_IOCTL
                            | SYS_CLONE
                            | SYS_CLONE3
                    ),
                    "has an argument rule: {syscall}"
                );
            }
        }
        for syscall in MAIL_ALLOWED.iter().chain(NEWS_ALLOWED).chain(WEB_ALLOWED) {
            assert!(!APPLICATION_ALLOWED.contains(syscall), "repeated {syscall}");
        }
    }

    // Everything on the list is allowed whatever its arguments, and everything
    // off it is refused, across the whole table and past it.
    #[test]
    fn an_allowlist_answers_exactly_its_list_and_its_argument_rules() {
        for (filter, program) in allowlist_programs() {
            let extra = match filter {
                ApplicationFilter::Mail => MAIL_ALLOWED,
                ApplicationFilter::News => NEWS_ALLOWED,
                ApplicationFilter::Web => WEB_ALLOWED,
                // Not an allowlist; allowlist_programs yields none.
                ApplicationFilter::Standard => continue,
            };
            for syscall in 0_u32..=600 {
                if matches!(
                    syscall,
                    SYS_SOCKET | SYS_SOCKETPAIR | SYS_SETSOCKOPT | SYS_IOCTL | SYS_CLONE
                ) {
                    continue;
                }
                let listed = APPLICATION_ALLOWED.contains(&syscall) || extra.contains(&syscall);
                let mut call = data(syscall as i32);
                for args in [[0_u64; 6], [u64::MAX; 6]] {
                    call.args = args;
                    assert_eq!(
                        interpret(&program, call).unwrap(),
                        if listed {
                            SECCOMP_RET_ALLOW
                        } else {
                            errno(ENOSYS)
                        },
                        "{filter:?} syscall {syscall}"
                    );
                }
            }
            for syscall in [X32_SYSCALL_BIT - 1, 0x8000_0000, u32::MAX] {
                assert_eq!(
                    interpret(&program, data(syscall as i32)).unwrap(),
                    errno(ENOSYS),
                    "{filter:?} syscall {syscall:#x}"
                );
            }
            let mut wrong = data(0);
            wrong.arch = 0x4000_0003;
            assert_eq!(
                interpret(&program, wrong).unwrap(),
                SECCOMP_RET_KILL_PROCESS
            );
            assert_eq!(
                interpret(&program, data((X32_SYSCALL_BIT | 1) as i32)).unwrap(),
                SECCOMP_RET_KILL_PROCESS
            );
        }
    }

    #[test]
    fn an_allowlist_narrows_sockets_ioctls_and_clone() {
        for (_, program) in allowlist_programs() {
            let mut socket = data(SYS_SOCKET as i32);
            socket.args[0] = u64::from(AF_UNIX);
            assert_eq!(interpret(&program, socket).unwrap(), SECCOMP_RET_ALLOW);
            for family in [0, AF_INET, AF_INET6, AF_NETLINK, 17, u32::MAX] {
                socket.args[0] = u64::from(family);
                assert_eq!(
                    interpret(&program, socket).unwrap(),
                    errno(EAFNOSUPPORT),
                    "family {family}"
                );
            }

            let option = |level: u32, name: u32| {
                let mut call = data(SYS_SETSOCKOPT as i32);
                call.args[1] = (1_u64 << 32) | u64::from(level);
                call.args[2] = (1_u64 << 32) | u64::from(name);
                interpret(&program, call).unwrap()
            };
            for name in [SO_RCVTIMEO, SO_SNDTIMEO] {
                assert_eq!(option(SOL_SOCKET, name), SECCOMP_RET_ALLOW);
                assert_eq!(option(6, name), errno(ENOPROTOOPT));
            }
            // SO_ATTACH_FILTER, SO_ATTACH_REUSEPORT_CBPF, SO_REUSEADDR, SO_PASSCRED.
            for name in [26, 51, 2, 16, 0, u32::MAX] {
                assert_eq!(option(SOL_SOCKET, name), errno(ENOPROTOOPT), "{name}");
            }

            let mut ioctl = data(SYS_IOCTL as i32);
            for high in [0, 1_u64 << 32] {
                ioctl.args[1] = high | u64::from(FIONBIO);
                assert_eq!(interpret(&program, ioctl).unwrap(), SECCOMP_RET_ALLOW);
            }
            for request in [
                TIOCSTI,
                TIOCLINUX,
                0x5401,
                0x5404,
                0x5410,
                0x5413,
                0x541b,
                0x5450,
                0x5451,
                0x8912,
                0,
                u32::MAX,
            ] {
                for high in [0, 1_u64 << 32] {
                    ioctl.args[1] = high | u64::from(request);
                    assert_eq!(
                        interpret(&program, ioctl).unwrap(),
                        errno(ENOTTY),
                        "request {request:#x}"
                    );
                }
            }

            let pair = |family: u32, kind: u32| {
                let mut call = data(SYS_SOCKETPAIR as i32);
                call.args[0] = u64::from(family);
                call.args[1] = u64::from(kind);
                interpret(&program, call).unwrap()
            };
            for flags in [0, SOCK_NONBLOCK, SOCK_CLOEXEC, SOCK_NONBLOCK | SOCK_CLOEXEC] {
                assert_eq!(pair(AF_UNIX, SOCK_STREAM | flags), SECCOMP_RET_ALLOW);
                assert_eq!(pair(AF_UNIX, SOCK_SEQPACKET | flags), SECCOMP_RET_ALLOW);
            }
            for (family, kind) in [(AF_UNIX, 2), (AF_UNIX, 3), (AF_INET, SOCK_STREAM)] {
                assert_eq!(pair(family, kind), errno(EPERM), "{family} {kind}");
            }

            let mut clone = data(SYS_CLONE as i32);
            assert_eq!(interpret(&program, clone).unwrap(), SECCOMP_RET_ALLOW);
            clone.args[0] = u64::from(CLONE_NEWUSER);
            assert_eq!(interpret(&program, clone).unwrap(), errno(EPERM));
            assert_eq!(
                interpret(&program, data(SYS_CLONE3 as i32)).unwrap(),
                errno(ENOSYS)
            );
        }
    }

    // A signal's return, the kernel's restart of what it interrupted, the vDSO's
    // clock fallback, and the reap of a spawn whose exec failed. An omission is
    // a program that dies on a signal or an unusual clocksource, or leaks a
    // zombie against its pid limit, which no ordinary test reaches.
    #[test]
    fn every_allowlist_keeps_what_an_ordinary_test_never_reaches() {
        for (_, program) in allowlist_programs() {
            for syscall in [15, 219, 228, 247] {
                assert_eq!(
                    interpret(&program, data(syscall)).unwrap(),
                    SECCOMP_RET_ALLOW,
                    "syscall {syscall}"
                );
            }
        }
    }

    #[test]
    fn the_filter_names_round_trip_and_refuse_anything_else() {
        for filter in [
            ApplicationFilter::Standard,
            ApplicationFilter::Mail,
            ApplicationFilter::News,
            ApplicationFilter::Web,
        ] {
            assert_eq!(ApplicationFilter::from_wire(filter.wire()), Some(filter));
        }
        assert_eq!(ApplicationFilter::from_wire(""), None);
        assert_eq!(ApplicationFilter::from_wire("Mail"), None);
        assert_eq!(
            application_program(ApplicationFilter::Standard).unwrap(),
            STANDARD_FILTER
        );
    }

    fn audit_line(uid: u32, syscall: u32, code: u32, signal: u32, executable: &str) -> String {
        let pid = if syscall & X32_SYSCALL_MASK == X32_SYSCALL_BIT {
            X32_CHILD_PID
        } else {
            FIREFOX_PID
        };
        audit_line_for_pid(uid, pid, syscall, code, signal, executable)
    }

    fn audit_line_for_pid(
        uid: u32,
        pid: u32,
        syscall: u32,
        code: u32,
        signal: u32,
        executable: &str,
    ) -> String {
        format!(
            "[  12.000] audit: type=1326 audit(1.2:3): auid=4294967295 uid={uid} gid={uid} ses=4294967295 pid={pid} comm=\"probe\" exe=\"{executable}\" sig={signal} arch=c000003e syscall={syscall} compat=0 ip=0x1 code=0x{code:x}\n"
        )
    }

    fn barrier(action: AuditBarrier) -> String {
        let (code, signal) = match action {
            AuditBarrier::Begin => (SECCOMP_RET_KILL_PROCESS, 31),
            AuditBarrier::End => (SECCOMP_RET_ERRNO, 0),
        };
        audit_line(
            0,
            SYS_GETPPID,
            code,
            signal,
            "/var/lib/td-test/td-jail-seccomp-probe",
        )
    }

    fn before_end(mut log: String, text: &str) -> String {
        let end = barrier(AuditBarrier::End);
        let offset = log.rfind(&end).unwrap();
        log.insert_str(offset, text);
        log
    }

    fn complete_audit() -> String {
        let mut log = barrier(AuditBarrier::Begin);
        for (syscall, code, signal) in [
            (SYS_PERSONALITY, SECCOMP_RET_ERRNO, 0),
            (SYS_SOCKET, SECCOMP_RET_ERRNO, 0),
            (101, SECCOMP_RET_ERRNO, 0),
            (SYS_IOCTL, SECCOMP_RET_ERRNO, 0),
            (SYS_IOCTL, SECCOMP_RET_ERRNO, 0),
            (SYS_IOCTL, SECCOMP_RET_ERRNO, 0),
            (SYS_CLONE, SECCOMP_RET_ERRNO, 0),
            (SYS_CLONE3, SECCOMP_RET_ERRNO, 0),
            (272, SECCOMP_RET_ERRNO, 0),
            (165, SECCOMP_RET_ERRNO, 0),
            (467, SECCOMP_RET_ERRNO, 0),
            (472, SECCOMP_RET_ERRNO, 0),
            (HIGHEST_REVIEWED_SYSCALL + 1, SECCOMP_RET_ERRNO, 0),
            (323, SECCOMP_RET_ERRNO, 0),
            (321, SECCOMP_RET_ERRNO, 0),
            (425, SECCOMP_RET_ERRNO, 0),
            (438, SECCOMP_RET_ERRNO, 0),
            (310, SECCOMP_RET_ERRNO, 0),
            (X32_SYSCALL_BIT | 1, SECCOMP_RET_KILL_PROCESS, 31),
        ] {
            log.push_str(&audit_line(
                65536,
                syscall,
                code,
                signal,
                FIREFOX_PROBE_PATH,
            ));
        }
        log.push_str(&barrier(AuditBarrier::End));
        log
    }

    #[test]
    fn firefox_audit_requires_every_outer_probe_and_accepts_rostered_runtime_denials() {
        assert_eq!(
            FIREFOX_REQUIRED_AUDIT_SYSCALLS
                .iter()
                .map(|(_, count)| count)
                .sum::<usize>(),
            19
        );
        assert_eq!(
            FIREFOX_AUDIT_MARKER,
            format!(
                "TD-FIREFOX-SECCOMP-OK probes={}",
                FIREFOX_REQUIRED_AUDIT_SYSCALLS
                    .iter()
                    .map(|(_, count)| count)
                    .sum::<usize>()
            )
        );
        let log = before_end(
            complete_audit(),
            &audit_line(
                65536,
                SYS_CLONE3,
                SECCOMP_RET_ERRNO,
                0,
                "/app/lib/firefox/firefox",
            ),
        );
        assert_eq!(verify_firefox_audit(&log).unwrap(), 20);

        for probe in [310, HIGHEST_REVIEWED_SYSCALL + 1] {
            let missing = complete_audit().replacen(
                &audit_line(65536, probe, SECCOMP_RET_ERRNO, 0, FIREFOX_PROBE_PATH),
                "",
                1,
            );
            assert!(verify_firefox_audit(&missing).is_err(), "probe {probe}");
        }
    }

    #[test]
    fn firefox_audit_does_not_count_the_parallel_standalone_probe() {
        let standalone = audit_line_for_pid(
            1000,
            FIREFOX_PID + 200,
            X32_SYSCALL_BIT | 1,
            SECCOMP_RET_KILL_PROCESS,
            31,
            "/run/td-jail-seccomp-probe/probe",
        );
        assert_eq!(
            verify_firefox_audit(&before_end(complete_audit(), &standalone)).unwrap(),
            20
        );
        let missing = complete_audit().replace(
            &audit_line(
                65536,
                X32_SYSCALL_BIT | 1,
                SECCOMP_RET_KILL_PROCESS,
                31,
                FIREFOX_PROBE_PATH,
            ),
            "",
        );
        assert!(verify_firefox_audit(&before_end(missing, &standalone)).is_err());
        for bad in [
            standalone.replace("uid=1000", "uid=65537"),
            standalone.replace("uid=1000", "uid=0"),
            standalone.replace("/run/td-jail-seccomp-probe/probe", "/run/untrusted/probe"),
            standalone.replace("sig=31", "sig=0"),
            standalone.replace("1073741825", "1073741826"),
            standalone.replace("0x80000000", "0x50000"),
        ] {
            assert!(verify_firefox_audit(&before_end(complete_audit(), &bad)).is_err());
        }
    }

    #[test]
    fn firefox_audit_excludes_only_the_exact_standalone_i386_exit_kill() {
        let standalone = audit_line_for_pid(
            1000,
            FIREFOX_PID + 200,
            1,
            SECCOMP_RET_KILL_PROCESS,
            31,
            "/run/td-jail-seccomp-probe/probe",
        )
        .replace("arch=c000003e", "arch=40000003")
        .replace("compat=0", "compat=1");
        assert_eq!(
            verify_firefox_audit(&before_end(complete_audit(), &standalone)).unwrap(),
            20
        );
        let missing = complete_audit().replace(
            &audit_line(
                65536,
                X32_SYSCALL_BIT | 1,
                SECCOMP_RET_KILL_PROCESS,
                31,
                FIREFOX_PROBE_PATH,
            ),
            "",
        );
        assert!(verify_firefox_audit(&before_end(missing, &standalone)).is_err());
        for bad in [
            standalone.replace("uid=1000", "uid=65536"),
            standalone.replace("uid=1000", "uid=0"),
            standalone.replace(&format!("pid={}", FIREFOX_PID + 200), "pid=0"),
            standalone.replace(
                &format!("pid={}", FIREFOX_PID + 200),
                &format!("pid={FIREFOX_PID}"),
            ),
            standalone.replace("/run/td-jail-seccomp-probe/probe", FIREFOX_PROBE_PATH),
            standalone.replace("sig=31", "sig=0"),
            standalone.replace("syscall=1 ", "syscall=2 "),
            standalone.replace("compat=1", "compat=0"),
            standalone.replace("arch=40000003", "arch=c000003e"),
            standalone.replace("0x80000000", "0x50000"),
        ] {
            assert!(
                verify_firefox_audit(&before_end(complete_audit(), &bad)).is_err(),
                "{bad}"
            );
        }
    }

    #[test]
    fn firefox_audit_refuses_unrostered_malformed_or_lost_records() {
        let good = complete_audit();
        assert!(
            verify_firefox_audit(&good.replacen(&barrier(AuditBarrier::Begin), "", 1,)).is_err()
        );
        assert!(verify_firefox_audit(&good.replacen(&barrier(AuditBarrier::End), "", 1,)).is_err());
        assert!(
            verify_firefox_audit(&before_end(good.clone(), "[  13.0] audit: audit_lost=1\n",))
                .is_err()
        );
        for loss in [
            "audit: audit_lost=1",
            "audit: rate limit exceeded",
            "audit: callbacks suppressed",
            "kauditd_printk_skb: 17 callbacks suppressed",
            "audit: kauditd hold queue overflow",
        ] {
            assert!(
                verify_firefox_audit(&format!("{loss}\n{good}")).is_err(),
                "pre-barrier loss indicator passed: {loss}"
            );
        }
        assert!(
            verify_firefox_audit(&format!("net_ratelimit: callbacks suppressed\n{good}")).is_ok()
        );
        assert!(verify_firefox_audit(&good.replace("comm=\"probe\"", "comm=\"kauditd\"")).is_ok());
        assert!(verify_firefox_audit(&before_end(
            good.clone(),
            &audit_line(65536, 39, SECCOMP_RET_ERRNO, 0, "/app/lib/firefox/firefox",),
        ))
        .is_err());
        // The filter refuses every number the table lacks, not only the probe's,
        // and a negative one it leaves to the kernel, so no record names one.
        for syscall in [
            FIRST_UNASSIGNED_SYSCALL,
            FIRST_ASSIGNED_AFTER_HOLE - 1,
            HIGHEST_REVIEWED_SYSCALL + 2,
            X32_SYSCALL_BIT - 1,
        ] {
            assert!(verify_firefox_audit(&before_end(
                good.clone(),
                &audit_line(
                    65536,
                    syscall,
                    SECCOMP_RET_ERRNO,
                    0,
                    "/app/lib/firefox/firefox",
                ),
            ))
            .is_ok());
        }
        assert!(verify_firefox_audit(&before_end(
            good.clone(),
            &audit_line(
                65536,
                NEGATIVE_SYSCALL,
                SECCOMP_RET_ERRNO,
                0,
                "/app/lib/firefox/firefox",
            ),
        ))
        .is_err());
        for uid in [0, 1000, 65537, 65538, 65539] {
            assert!(
                verify_firefox_audit(&good.replacen("uid=65536", &format!("uid={uid}"), 1))
                    .is_err()
            );
        }
        assert!(super::verify_firefox_audit(&good, FIREFOX_PID, 0).is_err());
        assert!(verify_firefox_audit(&good.replacen(
            &audit_line(
                65536,
                SYS_PERSONALITY,
                SECCOMP_RET_ERRNO,
                0,
                FIREFOX_PROBE_PATH,
            ),
            &audit_line_for_pid(
                65536,
                9,
                SYS_PERSONALITY,
                SECCOMP_RET_ERRNO,
                0,
                FIREFOX_PROBE_PATH,
            ),
            1,
        ))
        .is_err());
        assert!(verify_firefox_audit(&good.replacen(
            &audit_line(
                65536,
                X32_SYSCALL_BIT | 1,
                SECCOMP_RET_KILL_PROCESS,
                31,
                FIREFOX_PROBE_PATH,
            ),
            &audit_line_for_pid(
                65536,
                FIREFOX_PID,
                X32_SYSCALL_BIT | 1,
                SECCOMP_RET_KILL_PROCESS,
                31,
                FIREFOX_PROBE_PATH,
            ),
            1,
        ))
        .is_err());
        assert!(verify_firefox_audit(&good.replacen("compat=0", "compat=1", 1)).is_err());
        assert!(verify_firefox_audit(&good.replacen("code=0x50000", "code=0x30000", 1)).is_err());
        assert!(verify_firefox_audit(&good.replacen(" sig=0 ", " sig=0 sig=0 ", 1)).is_err());
        assert!(verify_firefox_audit(&format!("{good}{}", barrier(AuditBarrier::Begin))).is_err());
        assert!(verify_firefox_audit(&format!("{DMESG_INCOMPLETE_MARKER}\n{good}")).is_err());
    }

    #[test]
    fn firefox_audit_line_and_record_ceilings_are_exact() {
        let good = complete_audit();
        let padding = "x".repeat(MAX_AUDIT_LINE_BYTES);
        assert!(verify_firefox_audit(&before_end(good.clone(), &format!("{padding}\n"))).is_ok());
        assert!(verify_firefox_audit(&before_end(good.clone(), &format!("{padding}x\n"))).is_err());

        let runtime = audit_line(
            65536,
            SYS_CLONE3,
            SECCOMP_RET_ERRNO,
            0,
            "/app/lib/firefox/firefox",
        );
        let accepted = before_end(good, &runtime.repeat(MAX_AUDIT_RECORDS.saturating_sub(19)));
        assert_eq!(verify_firefox_audit(&accepted).unwrap(), MAX_AUDIT_RECORDS);
        assert!(verify_firefox_audit(&before_end(accepted, &runtime)).is_err());
    }
}

//! td-jail — td's application confinement boundary.
//!
//! Applications are selected only by an installed argv[0] name, resolved
//! through the immutable image index, then launched below the same transition
//! exercised by the target-kernel probe.
#![deny(unsafe_code)]

mod authority;
mod seccomp;
mod sys;
mod transition;

use std::io::Write;
use std::process::ExitCode;

const RESERVED_LAUNCHER_NAMES: &[&str] = &["td-jail", "td-jail-reaper-probe"];

fn run() -> std::io::Result<()> {
    let mut arguments = std::env::args_os();
    let argv0 = arguments.next().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "argv[0] is missing")
    })?;
    let name = authority::application_name(&argv0)?;
    if !RESERVED_LAUNCHER_NAMES.contains(&name) {
        return transition::launch_application(authority::resolve(name, arguments)?);
    }
    match transition::parse_mode(arguments)? {
        transition::Mode::Probe => transition::probe_transition(),
        transition::Mode::WriteFilter => transition::write_standard_filter(),
        transition::Mode::Stage2 {
            token,
            identity,
            action,
        } => transition::run_stage2(token, identity, action),
        transition::Mode::ReaperChild => transition::run_reaper_child(),
        transition::Mode::ReaperOrphan => transition::run_reaper_orphan(),
    }
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            let _ = writeln!(std::io::stderr(), "td-jail: {error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod confinement {
    #![allow(clippy::unwrap_used)]

    const AUTHORITY: &str = include_str!("authority.rs");
    const MAIN: &str = include_str!("main.rs");
    const SECCOMP: &str = include_str!("seccomp.rs");
    const SYS: &str = include_str!("sys.rs");
    const TRANSITION: &str = include_str!("transition.rs");

    #[test]
    fn unsafe_is_confined_to_one_syscall_instruction() {
        let shipped_main = MAIN.split_once("#[cfg(test)]").unwrap().0;
        assert!(shipped_main.contains("#![deny(unsafe_code)]"));
        assert_eq!(SYS.matches("#[allow(unsafe_code)]").count(), 1);
        assert_eq!(SYS.matches("unsafe {").count(), 1);
        assert_eq!(shipped_main.matches("#[allow(unsafe_code)]").count(), 0);
        assert_eq!(AUTHORITY.matches("#[allow(unsafe_code)]").count(), 0);
        assert_eq!(AUTHORITY.matches("unsafe {").count(), 0);
        assert_eq!(SECCOMP.matches("#[allow(unsafe_code)]").count(), 0);
        assert_eq!(SECCOMP.matches("unsafe {").count(), 0);
        assert_eq!(TRANSITION.matches("#[allow(unsafe_code)]").count(), 0);
        assert_eq!(TRANSITION.matches("unsafe {").count(), 0);
    }

    #[test]
    fn syscall_and_argument_rosters_are_pinned() {
        let shipped_sys = SYS.split_once("#[cfg(test)]").unwrap().0;
        for syscall in [
            "const SYS_CLOSE: usize = 3;",
            "const SYS_IOCTL: usize = 16;",
            "const SYS_WAIT4: usize = 61;",
            "const SYS_CAPGET: usize = 125;",
            "const SYS_CAPSET: usize = 126;",
            "const SYS_PIVOT_ROOT: usize = 155;",
            "const SYS_PRCTL: usize = 157;",
            "const SYS_MOUNT: usize = 165;",
            "const SYS_UMOUNT2: usize = 166;",
            "const SYS_UNSHARE: usize = 272;",
            "const SYS_SECCOMP: usize = 317;",
        ] {
            assert!(SYS.contains(syscall), "missing syscall pin: {syscall}");
        }
        assert_eq!(SYS.matches("const SYS_").count(), 11);
        assert!(SYS.contains("const BASE_NAMESPACE_FLAGS: usize ="));
        assert!(SYS.contains("const ISOLATED_NETWORK_FLAGS: usize ="));
        for flag in [
            "pub const MS_RDONLY: usize = 0x1;",
            "pub const MS_NOSUID: usize = 0x2;",
            "pub const MS_NODEV: usize = 0x4;",
            "pub const MS_NOEXEC: usize = 0x8;",
            "pub const MS_REMOUNT: usize = 0x20;",
            "pub const MS_BIND: usize = 0x1000;",
            "pub const MS_REC: usize = 0x4000;",
            "pub const MS_PRIVATE: usize = 0x4_0000;",
            "pub const MNT_DETACH: usize = 0x2;",
        ] {
            assert!(SYS.contains(flag), "missing mount flag pin: {flag}");
        }
        assert_eq!(SYS.matches("pub const MS_").count(), 8);
        assert!(SYS.contains("const LINUX_CAPABILITY_VERSION_3: u32 = 0x2008_0522;"));
        assert!(SYS.contains("const PR_SET_PDEATHSIG: usize = 1;"));
        assert!(SYS.contains("const PR_GET_DUMPABLE: usize = 3;"));
        assert!(SYS.contains("const PR_SET_DUMPABLE: usize = 4;"));
        assert!(SYS.contains("const PR_CAPBSET_READ: usize = 23;"));
        assert!(SYS.contains("const PR_CAPBSET_DROP: usize = 24;"));
        assert!(SYS.contains("const PR_SET_NO_NEW_PRIVS: usize = 38;"));
        assert!(SYS.contains("const PR_GET_NO_NEW_PRIVS: usize = 39;"));
        assert!(SYS.contains("const PR_CAP_AMBIENT: usize = 47;"));
        assert!(SYS.contains("const PR_CAP_AMBIENT_IS_SET: usize = 1;"));
        assert!(SYS.contains("const PR_CAP_AMBIENT_RAISE: usize = 2;"));
        assert!(SYS.contains("const PR_CAP_AMBIENT_CLEAR_ALL: usize = 4;"));
        assert_eq!(SYS.matches("const PR_").count(), 11);
        assert_eq!(SYS.matches("const PR_CAP_AMBIENT_").count(), 3);
        assert!(SYS.contains("pub const CAP_SETPCAP: u32 = 8;"));
        assert!(SYS.contains("pub const CAP_SYS_ADMIN: u32 = 21;"));
        assert!(SYS.contains("std::ptr::from_mut(&mut data) as usize"));
        assert!(SYS.contains("std::ptr::from_ref(&data) as usize"));
        assert!(SYS.contains("check(syscall5(SYS_UNSHARE, flags, 0, 0, 0, 0))"));
        assert!(SYS.contains("const SECCOMP_SET_MODE_FILTER: usize = 1;"));
        assert!(SYS.contains("const SIOCGIFFLAGS: usize = 0x8913;"));
        assert!(SYS.contains("const SIOCSIFFLAGS: usize = 0x8914;"));
        assert!(SYS.contains("const IFF_UP: i16 = 0x1;"));
        assert_eq!(shipped_sys.matches("SYS_IOCTL,").count(), 2);
        assert_eq!(shipped_sys.matches("SIOCGIFFLAGS,").count(), 1);
        assert_eq!(shipped_sys.matches("SIOCSIFFLAGS,").count(), 1);
        assert_eq!(shipped_sys.matches("read_interface_flags(fd,").count(), 2);
        assert_eq!(shipped_sys.matches("write_interface_flags(fd,").count(), 1);
        assert!(!shipped_sys.contains("request: usize"));
        assert!(SYS.contains("std::mem::size_of::<IfreqFlags>(), 40"));
        assert!(SYS.contains("SYS_SECCOMP,\n        SECCOMP_SET_MODE_FILTER,\n        0,"));
        assert!(TRANSITION.contains("sys::MS_REC | sys::MS_PRIVATE"));
        let reaper_mount = TRANSITION
            .split_once("fn mount_reaper_probe")
            .unwrap()
            .1
            .split_once("fn last_capability")
            .unwrap()
            .0;
        assert!(
            reaper_mount.contains("sys::MS_BIND | sys::MS_NOSUID | sys::MS_NODEV,")
        );
        assert!(TRANSITION.contains(
            "REAPER_PROBE_PATH,\n            None,\n            &[\"ro\", \"nosuid\", \"nodev\"],\n            &[\"rw\", \"noexec\"],"
        ));
        assert!(!reaper_mount.contains("fs::copy"));
    }

    #[test]
    fn transition_is_the_only_syscall_caller() {
        let shipped_main = MAIN.split_once("#[cfg(test)]").unwrap().0;
        assert_eq!(
            TRANSITION.matches("sys::unshare_namespaces(true)?;").count(),
            2
        );
        assert_eq!(TRANSITION.matches("sys::unshare_namespaces(").count(), 2);
        for call in [
            "sys::close(",
            "sys::bring_up_loopback(",
            "sys::wait_any(",
            "sys::mount(",
            "sys::pivot_root(",
            "sys::umount_detach(",
            "sys::capabilities(",
            "sys::set_capabilities(",
            "sys::clear_ambient_capabilities(",
            "sys::set_parent_death_signal(",
            "sys::raise_ambient_sys_admin(",
            "sys::ambient_capability(",
            "sys::drop_bounding_capability(",
            "sys::bounding_capability(",
            "sys::set_no_new_privileges(",
            "sys::no_new_privileges(",
            "sys::set_dumpable(",
            "sys::dumpable(",
            "sys::install_seccomp_filter(",
        ] {
            assert!(TRANSITION.contains(call), "missing syscall caller: {call}");
        }
        assert!(!shipped_main.contains("sys::"));
        assert!(!AUTHORITY.contains("sys::"));
        assert!(!TRANSITION.contains("pre_exec"));
        assert!(!TRANSITION.contains("CommandExt"));
        assert!(!TRANSITION.contains("fork("));
        assert!(TRANSITION.contains("const TEST_LEAK_ENV: &str = \"TD_JAIL_TEST_LEAK_FD\";"));
        assert!(TRANSITION.contains(".into_raw_fd()"));
        assert!(TRANSITION.contains("require_descriptor_closed(descriptor)?;"));
        assert!(TRANSITION.contains("clear_and_require_empty_capabilities()?;"));
        assert!(TRANSITION.contains("install_standard_seccomp_filter()?;"));
        assert!(TRANSITION.contains("probe_pid1_reaper()?;"));
        assert_eq!(TRANSITION.matches(".env_clear()").count(), 2);
        assert_eq!(TRANSITION.matches(".envs(").count(), 1);
        assert!(SECCOMP.contains("pub(crate) const STANDARD_FILTER:"));
        assert!(SECCOMP.contains("const OFFSET_NR: u32 = 0;"));
        assert!(SECCOMP.contains("const OFFSET_ARCH: u32 = 4;"));
        assert!(SECCOMP.contains("const OFFSET_ARG0_LOW: u32 = 16;"));
        assert!(SECCOMP.contains("const OFFSET_ARG1_LOW: u32 = 24;"));
    }

    #[test]
    fn syscall_abi_has_all_five_arguments_and_memory_is_not_hidden() {
        for register in [
            "inlateout(\"rax\") n as isize => ret",
            "in(\"rdi\") a1",
            "in(\"rsi\") a2",
            "in(\"rdx\") a3",
            "in(\"r10\") a4",
            "in(\"r8\") a5",
            "out(\"rcx\") _",
            "out(\"r11\") _",
        ] {
            assert!(
                SYS.contains(register),
                "missing syscall ABI pin: {register}"
            );
        }
        assert!(SYS.contains("options(nostack)"));
        assert!(!SYS.contains("options(nostack, nomem)"));
    }
}

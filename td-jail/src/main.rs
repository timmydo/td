//! td-jail — td's application confinement boundary.
//!
//! This first increment proves only the single-threaded namespace transition
//! into a stage-2 process that is PID 1. Application launch remains disabled
//! until the mount, capability, reaper, and seccomp increments have landed.
#![deny(unsafe_code)]

mod sys;
mod transition;

use std::io::Write;
use std::process::ExitCode;

fn run() -> std::io::Result<()> {
    match transition::parse_mode(std::env::args_os().skip(1))? {
        transition::Mode::Probe => transition::probe_transition(),
        transition::Mode::Stage2 { token, identity } => transition::run_stage2(token, identity),
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

    const MAIN: &str = include_str!("main.rs");
    const SYS: &str = include_str!("sys.rs");
    const TRANSITION: &str = include_str!("transition.rs");

    #[test]
    fn unsafe_is_confined_to_one_syscall_instruction() {
        let shipped_main = MAIN.split_once("#[cfg(test)]").unwrap().0;
        assert!(shipped_main.contains("#![deny(unsafe_code)]"));
        assert_eq!(SYS.matches("#[allow(unsafe_code)]").count(), 1);
        assert_eq!(SYS.matches("unsafe {").count(), 1);
        assert_eq!(shipped_main.matches("#[allow(unsafe_code)]").count(), 0);
        assert_eq!(TRANSITION.matches("#[allow(unsafe_code)]").count(), 0);
        assert_eq!(TRANSITION.matches("unsafe {").count(), 0);
    }

    #[test]
    fn syscall_and_argument_rosters_are_pinned() {
        assert!(SYS.contains("const SYS_UNSHARE: usize = 272;"));
        assert!(SYS.contains("const BASE_NAMESPACE_FLAGS: usize ="));
        assert!(SYS.contains("const ISOLATED_NETWORK_FLAGS: usize ="));
        assert_eq!(SYS.matches("check(syscall5(").count(), 1);
        assert!(SYS.contains("check(syscall5(SYS_UNSHARE, flags, 0, 0, 0, 0))"));
    }

    #[test]
    fn transition_is_the_only_syscall_caller() {
        let shipped_main = MAIN.split_once("#[cfg(test)]").unwrap().0;
        assert_eq!(TRANSITION.matches("sys::unshare_namespaces(").count(), 1);
        assert!(!shipped_main.contains("sys::"));
        assert!(!TRANSITION.contains("pre_exec"));
        assert!(!TRANSITION.contains("CommandExt"));
        assert!(!TRANSITION.contains("fork("));
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

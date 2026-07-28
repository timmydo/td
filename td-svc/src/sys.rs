//! The confined raw-syscall layer — the whole `unsafe` surface of this crate.
//!
//! The crate root `#![deny(unsafe_code)]`s and exactly one item here carries a
//! scoped `#[allow]`: `syscall2`, the `syscall`-instruction body copied from
//! `td-login/src/sys.rs` (itself copied from `td-init`, itself from
//! `builder/src/sys.rs`). Everything else in the crate — and every other
//! function in this module — is ordinary safe Rust. This is the FIFTH
//! target-side unsafe exception AGENTS.md records, after td-kexec (two
//! syscalls), td-netd (one ioctl), td-init (nine), and td-login (three).
//!
//! The amended surface is exactly ONE syscall: `kill(2)`.
//!
//! td-svc used to `exec` the uutils `/bin/kill` instead, precisely to avoid
//! this exception. That traded an `unsafe` block for something worse: the
//! supervisor's ability to stop ANYTHING became a runtime dependency on a
//! third-party multicall binary existing at an absolute path and parsing
//! `-<pgid>` as a process group rather than as a flag. Nothing tied that
//! binary to td-svc, so dropping `kill` from the image's applet list would
//! have left every `stop`, every `restart` and the whole ordered teardown
//! silently unable to signal, with no build-time complaint. It also cost a
//! `fork`+`exec` per signal on the shutdown path, and made seven of this
//! crate's own stop-path tests skip on any host without `/bin/kill` — so the
//! code that most needed covering was the code least often exercised.
//!
//! Deliberately NOT in the surface: `killpg(2)` (it is `kill(2)` with a
//! negated argument, and one way to address a group is enough), the
//! `rt_sig*` family (td-svc installs no handlers — DESIGN.md §5 turns on
//! there being none), `getpid`/`getpgid`/`getsid` (`/proc` answers those and
//! I3 requires reading it anyway), and `waitpid` (`Child::wait`/`try_wait`
//! cover it). A SECOND syscall is a reviewed amendment, not an edit;
//! `main.rs`'s confinement tests assert the roster.

use std::io;

#[cfg(not(all(target_arch = "x86_64", target_os = "linux")))]
compile_error!("td-svc is x86_64-linux only (raw syscall ABI)");

// The amended one (x86_64 syscall number).
const SYS_KILL: usize = 62;

// The signals td-svc sends. Numbers, not names, because there is no `kill`
// binary left to parse a name — and these are the x86_64 Linux values, which
// differ per architecture for exactly the signals nobody uses.

/// What the kernel sends `cad_pid` on a press. Not a signal td-svc originates:
/// it is here because the CAD sentinel dying OF it is what distinguishes
/// "someone asked to shut down" from "the sentinel broke".
pub const SIGINT: i32 = 2;
pub const SIGKILL: i32 = 9;
pub const SIGTERM: i32 = 15;

/// The single raw-syscall entry point (x86_64 SysV syscall ABI). Its body is
/// the ONLY `unsafe` in the crate. The scoped `#[allow]` under the crate
/// `#![deny(unsafe_code)]` covers where `unsafe` may appear, not what may be
/// passed here — this fn is safe to CALL, so its confinement is module privacy
/// plus the one typed wrapper below being its only caller. `kill(2)` takes two
/// arguments; there is no unused register to zero.
#[inline]
#[allow(unsafe_code)]
fn syscall2(n: usize, a1: usize, a2: usize) -> isize {
    let ret: isize;
    // SAFETY: the `syscall` instruction clobbers rcx/r11 and returns in rax.
    // Both arguments are plain integers — no pointer is passed, so unlike
    // td-init's wait4 or td-login's setgroups the kernel neither reads nor
    // writes through anything here, and there is no lifetime to keep live.
    unsafe {
        core::arch::asm!(
            "syscall",
            inlateout("rax") n as isize => ret,
            in("rdi") a1,
            in("rsi") a2,
            out("rcx") _,
            out("r11") _,
            options(nostack),
        );
    }
    ret
}

/// Turn a raw syscall return into a `Result`, mirroring `td-login/src/sys.rs`.
fn check(ret: isize) -> io::Result<()> {
    if ret < 0 {
        Err(io::Error::from_raw_os_error(-ret as i32))
    } else {
        Ok(())
    }
}

/// Send `signal` to `target`.
///
/// A NEGATIVE `target` addresses a process GROUP — `kill(-pgid, sig)` — which
/// is how a service's containment is reached when it leads one. That is the
/// kernel's own convention, not a `kill(1)` argument-parsing quirk, which is
/// the point of issuing it here: there is no longer an argv for anything to
/// misread.
///
/// The `i32`s are widened with a sign-extending cast because that is what the
/// value MEANS, not because the upper bits matter: the kernel takes `pid_t`
/// and `int`, reads the low 32 bits of each register, and ignores the rest —
/// so a zero-extending cast would reach the same group. Written the honest way
/// so the register holds the number the signature says it does.
pub fn kill(target: i32, signal: i32) -> io::Result<()> {
    check(syscall2(
        SYS_KILL,
        target as isize as usize,
        signal as isize as usize,
    ))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    /// The syscall reaches the kernel, with the arguments the signature says.
    ///
    /// Everything above is asserted about this module's TEXT — that there is
    /// one syscall, that its registers are the pinned ones. None of that runs
    /// it. A wrapper that returned `Ok(())` without issuing anything satisfies
    /// every confinement test in the crate and every stop td-svc performs.
    ///
    /// Signal 0 is the probe the kernel provides for exactly this: it delivers
    /// nothing and runs the existence and permission checks, so the answer
    /// distinguishes "the call arrived" from "the call was never made" without
    /// this test having to survive being signalled itself.
    #[test]
    fn the_wrapper_actually_issues_the_syscall() {
        let me = i32::try_from(std::process::id()).unwrap();
        assert!(
            kill(me, 0).is_ok(),
            "signal 0 to our own pid must succeed; the syscall is not arriving"
        );
        // A pid above the kernel's ceiling names nothing and cannot come to
        // name something mid-test. Neither 0 nor a negative stands in for it:
        // those are our own process group and an arbitrary one.
        assert_eq!(
            kill(i32::MAX, 0).map_err(|e| e.raw_os_error()),
            Err(Some(3)),
            "a pid that cannot exist must come back ESRCH, not success"
        );
    }

    /// An error is reported as one, rather than folded into success.
    ///
    /// `check` turns the negative-errno return into a `Result`; getting that
    /// backwards — or dropping the sign test — makes every refused signal look
    /// delivered, which is the failure the caller's own ESRCH policy is built
    /// on top of and cannot detect.
    #[test]
    fn a_refused_signal_comes_back_as_an_error() {
        let me = i32::try_from(std::process::id()).unwrap();
        // Above the highest real-time signal, so the kernel rejects the NUMBER
        // rather than delivering anything — the one refusal that does not
        // depend on who is running the suite.
        assert_eq!(
            kill(me, 65).map_err(|e| e.raw_os_error()),
            Err(Some(22)),
            "an invalid signal number must be reported as EINVAL"
        );
    }
}

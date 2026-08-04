//! The confined raw-syscall layer — the whole `unsafe` surface of this crate.
//!
//! The crate root `#![deny(unsafe_code)]`s and exactly one item here carries a
//! scoped `#[allow]`: `syscall1`, the `syscall`-instruction body copied from
//! `td-util/src/sys.rs`. Everything else in the crate — and every other function
//! in this module — is ordinary safe Rust. This is the EIGHTH target-side unsafe
//! exception UNSAFE.md records.
//!
//! The surface is ONE syscall, `umask(2)`, reached from TWO modules and no
//! others: `builtin.rs`, for the builtin itself, and `process.rs`, for the
//! guard that hands a subshell back the mask a real fork would have kept for
//! it. It is not reachable through safe `std` at all: `std` exposes
//! no umask API, which is why the shipped `/init` still shells out to
//! `busybox sh -c 'umask 077; …'` for the one line that needs it.
//!
//! `umask(2)` is unusual and the wrappers below turn both quirks into
//! properties. It CANNOT FAIL — there is no error return, so there is no
//! `check()` here — and it RETURNS THE PREVIOUS MASK, which is the only way to
//! observe the current one. That makes reading it a set-and-restore, and it also
//! makes the readback free: calling `umask(new)` a second time is idempotent and
//! returns whatever the first call left, so `set` can prove its own effect
//! without a second syscall number and without depending on `/proc` being
//! mounted. The proof matters because nothing observable distinguishes a mask
//! that did not take — the wrong bits show up later, as a file created with
//! permissions nobody asked for.
//!
//! Deliberately NOT here: reading the mask out of `/proc/self/status`'s `Umask:`
//! field. It is real and it is safe, but it answers only half the builtin (there
//! is no way to SET through it), so it would buy a `/proc` dependency without
//! removing the syscall.

#[cfg(not(all(target_arch = "x86_64", target_os = "linux")))]
compile_error!("td-sh's umask layer is x86_64-linux only (raw syscall ABI)");

const SYS_UMASK: usize = 95;

/// Every bit `umask(2)` can hold: the nine rwx permission bits and NOT ONE MORE.
/// Linux's `sys_umask` ands the argument with `S_IRWXUGO`, so setuid, setgid and
/// sticky are simply dropped -- a file-creation mask has no say over them. The
/// clamp is here rather than left to the kernel so a caller's arithmetic slip
/// cannot be silently reinterpreted.
pub const MODE_BITS: u32 = 0o777;

/// The single raw-syscall entry point (x86_64 SysV syscall ABI), copied from
/// `td-util/src/sys.rs`. Its body is the ONLY `unsafe` in the crate. The scoped
/// `#[allow]` covers where `unsafe` may appear, not what may be passed here — this
/// fn is safe to CALL, so its confinement is module privacy plus the two typed
/// wrappers below being its only callers.
#[inline]
#[allow(unsafe_code)]
fn syscall1(n: usize, a1: usize) -> isize {
    let ret: isize;
    // SAFETY: the `syscall` instruction clobbers rcx/r11 and returns in rax. The
    // one argument is a plain integer — this surface passes NO pointers, which is
    // why the kernel cannot write through anything here.
    unsafe {
        core::arch::asm!(
            "syscall",
            inlateout("rax") n as isize => ret,
            in("rdi") a1,
            out("rcx") _,
            out("r11") _,
            options(nostack),
        );
    }
    ret
}

/// `umask(mask)` — install `mask`, returning the PREVIOUS one.
///
/// Private on purpose: every caller outside this module goes through `get` or
/// `set`, so the set-and-restore and the readback cannot be forgotten.
fn umask(mask: u32) -> u32 {
    (syscall1(SYS_UMASK, (mask & MODE_BITS) as usize) as u32) & MODE_BITS
}

/// The current mask. There is no "read" syscall, so this SETS zero to learn the
/// old value and immediately puts it back — the dance every shell does. Safe
/// here because the window is two instructions inside a shell that runs its
/// pipeline stages sequentially and installs no signal handlers; a threaded
/// stage or a handler that creates files would make it observable.
pub fn get() -> u32 {
    let old = umask(0);
    let _ = umask(old);
    old
}

/// Install `mask`, and REFUSE unless the kernel agrees it took.
///
/// The second call is the readback, not a second write: it asks for the same
/// mask again, so it changes nothing, and its return value is what the first
/// call actually left in place.
pub fn set(mask: u32) -> Result<(), String> {
    let mask = mask & MODE_BITS;
    let _prev = umask(mask);
    let took = umask(mask);
    if took != mask {
        return Err(format!("umask: kernel kept {took:04o}, not {mask:04o}"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    /// The mask is PROCESS-global and cargo runs these on parallel threads, so
    /// without this they would read each other's writes. Not a property of the
    /// code under test -- a property of what is being tested.
    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn serial() -> std::sync::MutexGuard<'static, ()> {
        SERIAL.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The syscall is really ISSUED and the kernel really answers.
    ///
    /// Every other assertion about this module is about source TEXT; a wrapper
    /// that returned a plausible number without issuing anything would satisfy
    /// all of them. `/proc/self/status` is a SECOND, independent view of the same
    /// kernel state, so agreement between it and `get()` cannot come from this
    /// module talking to itself.
    #[test]
    fn the_syscall_is_issued_and_proc_agrees() {
        let _serial = serial();
        let restore = get();
        // NO value here masks 0o400. The mask is process-global and cargo runs
        // these beside tests that create a file and read it back; a mask that
        // took away owner-read would fail THOSE, in a way that would read as
        // their flakiness rather than as this test's reach.
        for want in [0o022u32, 0o077, 0o000, 0o377, 0o027] {
            set(want).unwrap();
            assert_eq!(get(), want, "get() disagrees with what set() installed");
            if let Ok(status) = std::fs::read_to_string("/proc/self/status") {
                let line = status
                    .lines()
                    .filter(|l| l.starts_with("Umask:"))
                    .map(|l| l.trim_start_matches("Umask:").trim().to_string())
                    .next();
                if let Some(text) = line {
                    let seen = u32::from_str_radix(&text, 8).unwrap();
                    assert_eq!(seen, want, "/proc/self/status says {text}, not {want:04o}");
                }
            }
        }
        set(restore).unwrap();
    }

    /// `get` must LEAVE the mask alone — it is a read, spelled as two writes.
    #[test]
    fn reading_the_mask_does_not_change_it() {
        let _serial = serial();
        let restore = get();
        set(0o027).unwrap();
        assert_eq!(get(), 0o027);
        assert_eq!(get(), 0o027, "a second read saw a different mask");
        set(restore).unwrap();
    }

    /// Bits the kernel would drop are clamped here instead, so `set` never asks
    /// for something it cannot get. Linux keeps only the nine rwx bits, which is
    /// why 0o7777 and 0o10000 both come back as 0o777.
    #[test]
    fn the_mask_is_clamped_to_the_permission_bits() {
        let _serial = serial();
        let restore = get();
        set(0o7377).unwrap();
        assert_eq!(get(), 0o377, "setuid/setgid/sticky are not part of a umask");
        set(0o10000 | 0o022).unwrap();
        assert_eq!(get(), 0o022);
        set(restore).unwrap();
    }
}

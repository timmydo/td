//! The confined raw-syscall layer — the whole `unsafe` surface of this crate.
//!
//! The crate root `#![deny(unsafe_code)]`s and exactly one item here carries a
//! scoped `#[allow]`: `syscall4`, the `syscall`-instruction body copied from
//! `td-util/src/sys.rs`. Everything else in the crate — and every other function
//! in this module — is ordinary safe Rust. This is the EIGHTH target-side unsafe
//! exception UNSAFE.md records.
//!
//! The surface is FOUR syscalls, reached from THREE modules and no others:
//! `builtin.rs`, for the `umask` and `trap` builtins; `process.rs`, for the
//! guards that hand a subshell back the process state a real fork would have
//! kept for it, for the one that stops the shell listening to the terminal
//! while a foreground child runs, AND for the descriptor question `read -t`
//! asks; and `term.rs`, for the terminal mode and width the line editor needs.
//! None is reachable through safe `std`, which exposes no umask API, no
//! signal-disposition API, no terminal-mode API and no readiness API at all —
//! `IsTerminal` answers whether a descriptor is a terminal, never how wide, in
//! what mode, or whether reading it would wait.
//!
//! `umask(2)` is unusual and the wrappers below turn both quirks into
//! properties. It CANNOT FAIL — there is no error return — and it RETURNS THE
//! PREVIOUS MASK, which is the only way to observe the current one. That makes
//! reading it a set-and-restore, and it also makes the readback free: calling
//! `umask(new)` a second time is idempotent and returns whatever the first call
//! left, so `umask_set` can prove its own effect without a second syscall number
//! and without depending on `/proc` being mounted.
//!
//! `rt_sigaction(2)` is DISPOSITION-ONLY here: the only two handler words this
//! module will write are `SIG_DFL` and `SIG_IGN`, and `Disposition` cannot spell
//! a third. That is why this surface could be taken at all — catching a signal
//! on x86-64 needs an `SA_RESTORER` trampoline for the handler to return
//! through, and neither `SIG_DFL` nor `SIG_IGN` ever runs a handler, so neither
//! needs one. What it buys is `trap '' SIG`, which POSIX defines as a real
//! kernel disposition rather than shell bookkeeping: an ignore survives
//! `execve`, so it is the half of `trap` that has to reach the CHILDREN a shell
//! starts. Catching (`trap 'action' SIG`) still needs a handler and remains a
//! separate reviewed amendment.
//!
//! Both readbacks below make the same argument, and it is `losetup`'s: nothing
//! observable distinguishes a mask or a disposition that did not take. The wrong
//! mask shows up later as a file created with permissions nobody asked for, and
//! a signal the kernel still defaults on kills the shell at the moment the
//! script was written to survive.
//!
//! Deliberately NOT here: reading the mask out of `/proc/self/status`'s `Umask:`
//! field. It is real and it is safe, but it answers only half the builtin (there
//! is no way to SET through it), so it would buy a `/proc` dependency without
//! removing the syscall. Nor `kill(2)`, `rt_sigprocmask(2)` or `sigaltstack(2)`:
//! this shell sends no signals, blocks none, and runs on no alternate stack. Nor
//! `isatty`, which `std::io::IsTerminal` already answers safely.
//!
//! `ioctl(2)` is the one with a REQUEST roster rather than a single meaning, so
//! the three it may issue are pinned by value and `ioctl` refuses anything else
//! before the syscall — the roster is code, not just a test.
//!
//! `poll(2)` is the narrowest of the four: it exists for ONE builtin, `read -t`,
//! whose whole question is the one poll answers — would a read return without
//! waiting. Nothing in `std` asks it. Its absence is the one the shell had to
//! ANNOUNCE: readiness was answered from the descriptor table's own shape, which
//! is right for a cursor over a here-document and a guess about a pipe, a FIFO,
//! a socket or an idle terminal, so `-t` refused those rather than guess.

use std::os::fd::RawFd;

#[cfg(not(all(target_arch = "x86_64", target_os = "linux")))]
compile_error!("td-sh's syscall layer is x86_64-linux only (raw syscall ABI)");

use std::sync::Mutex;

const SYS_POLL: usize = 7;
const SYS_RT_SIGACTION: usize = 13;
const SYS_IOCTL: usize = 16;
const SYS_UMASK: usize = 95;

/// Every bit `umask(2)` can hold: the nine rwx permission bits and NOT ONE MORE.
/// Linux's `sys_umask` ands the argument with `S_IRWXUGO`, so setuid, setgid and
/// sticky are simply dropped -- a file-creation mask has no say over them. The
/// clamp is here rather than left to the kernel so a caller's arithmetic slip
/// cannot be silently reinterpreted.
pub const MODE_BITS: u32 = 0o777;

/// The two handler words this module will write, and the only two that run no
/// code: `SIG_DFL` hands the signal back to the kernel, `SIG_IGN` discards it.
/// Both are magic small integers rather than addresses, which is why neither
/// needs the `SA_RESTORER` a real handler would return through.
const SIG_DFL: usize = 0;
const SIG_IGN: usize = 1;

/// x86-64 `struct kernel_sigaction`, as FOUR words in THIS order: the handler,
/// `sa_flags`, `sa_restorer`, then `sa_mask` (one 64-bit `sigset_t`, last for
/// extensibility). A plain array rather than a `#[repr(C)]` struct, so the order
/// is a tested function of how `install` and `decode` spell it -- a handler
/// written at the wrong offset is a well-formed `sa_flags` and a disposition
/// left alone. The COUNT is pinned because `sys_rt_sigaction` copies `sizeof(
/// struct sigaction)` through the pointer with no length negotiation, so a
/// shorter buffer is an out-of-bounds kernel write from code the compiler reads
/// as safe.
const SIGACTION_WORDS: usize = 4;

/// The buffer is exactly `sizeof(struct sigaction)`, checked where it MATTERS:
/// the shipped binary is compiled by `recipes/src/recipes/td-sh.rs` calling
/// rustc directly and never runs a test, so a `#[test]` would pin this only in
/// the gate. A short buffer is an out-of-bounds kernel write, which is not a
/// thing to learn about from a test that did not run.
const _: () = assert!(SIGACTION_WORDS * core::mem::size_of::<usize>() == 32);

/// `sizeof(sigset_t)`. `sys_rt_sigaction` compares its fourth argument against
/// this exactly and answers `EINVAL` on any other value, which is what makes a
/// mis-sized struct a refusal rather than a partial read.
const SIGSETSIZE: usize = 8;

/// Linux's `_NSIG`: the highest number `valid_signal` accepts.
pub const SIG_MAX: u8 = 64;

/// The two `sig_kernel_only` signals. `do_sigaction` answers `EINVAL` for both
/// whenever an action is supplied, so nothing here asks -- which keeps every
/// refusal below a case of the kernel disagreeing about something it could have
/// done, rather than an expected failure callers learn to skip past.
const UNCHANGEABLE: [u8; 2] = [9, 19];

/// The single raw-syscall entry point (x86_64 SysV syscall ABI), copied from
/// `td-util/src/sys.rs`. Its body is the ONLY `unsafe` in the crate. The scoped
/// `#[allow]` covers where `unsafe` may appear, not what may be passed here — this
/// fn is safe to CALL, so its confinement is module privacy plus the two typed
/// wrappers below being its only callers.
#[inline]
#[allow(unsafe_code)]
fn syscall4(n: usize, a1: usize, a2: usize, a3: usize, a4: usize) -> isize {
    let ret: isize;
    // SAFETY: the `syscall` instruction clobbers rcx/r11 and returns in rax; the
    // args are plain integers or a pointer-as-usize whose pointee the caller keeps
    // live and correctly sized across the call. `options(nomem)` is deliberately
    // ABSENT and load-bearing by its absence: `rt_sigaction` has the kernel WRITE
    // the previous action through one pointer and READ the new one through
    // another. Those addresses reach here only as integers, and this asm is what
    // makes both accesses visible to the compiler -- an alloca whose address is
    // cast to an integer and passed to an asm that may touch memory is escaped,
    // so its stores cannot be eliminated and its slot cannot be reused across the
    // call. `nomem` would withdraw exactly that.
    unsafe {
        core::arch::asm!(
            "syscall",
            inlateout("rax") n as isize => ret,
            in("rdi") a1,
            in("rsi") a2,
            in("rdx") a3,
            in("r10") a4,
            out("rcx") _,
            out("r11") _,
            options(nostack),
        );
    }
    ret
}

/// A kernel refusal, as the negative errno the raw ABI returns in place of
/// libc's `-1` plus `errno`.
fn check(what: &str, ret: isize) -> Result<(), String> {
    if ret < 0 {
        return Err(format!("{what}: errno {}", -ret));
    }
    Ok(())
}

/// `umask(mask)` — install `mask`, returning the PREVIOUS one.
///
/// Private on purpose: every caller outside this module goes through `umask_get`
/// or `umask_set`, so the set-and-restore and the readback cannot be forgotten.
fn umask(mask: u32) -> u32 {
    (syscall4(SYS_UMASK, (mask & MODE_BITS) as usize, 0, 0, 0) as u32) & MODE_BITS
}

/// The shell's own view of the mask, so reading one costs no syscall and opens
/// no window.
///
/// There is no "read" syscall: learning the mask means SETTING zero and putting
/// it back, and for the two instructions in between the process has NO mask at
/// all. That was safe while pipeline stages ran one at a time — the shell was
/// the only thing running — and it stopped being safe the moment they ran
/// concurrently, because a sibling stage creating a file inside that window gets
/// it with permissions nobody asked for. Silent, and exactly the failure the
/// readback in `umask_set` exists to prevent at the other end.
///
/// So the dance happens ONCE, in `umask_prime`, before any stage can exist, and
/// after that the shell answers from what it remembers. That is sound because
/// nothing else can change this process's mask: `umask(2)` is reachable only
/// through this module, `main.rs`'s confinement tests pin `builtin.rs` and
/// `process.rs` as the only callers, and a child's own `umask` cannot reach back
/// into its parent.
static CURRENT: Mutex<u32> = Mutex::new(0o022);

/// Read the mask the one way that needs the window, and remember it. Called from
/// `main` before the shell runs anything, which is what keeps the window
/// single-threaded; calling it twice is harmless but pointless.
pub fn umask_prime() {
    let mut cur = lock_current();
    let old = umask(0);
    let _ = umask(old);
    *cur = old;
}

/// The current mask, from the shell's own record.
pub fn umask_get() -> u32 {
    *lock_current()
}

/// The record, whether or not a panicking thread poisoned it. Poisoning cannot
/// actually happen — this crate is `panic = "abort"`, so nothing unwinds out of
/// a held lock — but reading THROUGH it rather than around it means the two
/// accessors cannot disagree about what a poisoned lock means, which is the kind
/// of difference that only shows up once it matters.
fn lock_current() -> std::sync::MutexGuard<'static, u32> {
    CURRENT.lock().unwrap_or_else(|e| e.into_inner())
}

/// Install `mask`, and REFUSE unless the kernel agrees it took.
///
/// The second call is the readback, not a second write: it asks for the same
/// mask again, so it changes nothing, and its return value is what the first
/// call actually left in place.
///
/// The record is held for the WHOLE of that, which is what makes the readback
/// mean anything now that pipeline stages run at once. Two stages each setting a
/// mask would otherwise interleave their two calls, and each would read back the
/// OTHER's mask and report `kernel kept …` about a kernel that had done exactly
/// what it was told — measured at 6 spurious diagnostics in 100 runs of
/// `umask 077 | umask 022` before this lock. Holding it also keeps the record and
/// the kernel in step, since the update lands before any other stage can look.
pub fn umask_set(mask: u32) -> Result<(), String> {
    let mask = mask & MODE_BITS;
    let mut cur = lock_current();
    let _prev = umask(mask);
    let took = umask(mask);
    // What the kernel KEPT, recorded before the disagreement is reported: the
    // mask is installed either way, so a record left holding the old value would
    // have the shell reporting one mask while creating files under another --
    // compounding exactly the disagreement this readback exists to catch.
    *cur = took;
    if took != mask {
        return Err(format!("umask: kernel kept {took:04o}, not {mask:04o}"));
    }
    Ok(())
}

/// `struct termios` as the x86-64 kernel lays it out: four `u32` flag words, a
/// `c_line` byte, then `NCCS` = 19 control characters. OPAQUE here — this module
/// never decides what a field means, so the layout knowledge lives in exactly
/// one place (`term.rs`), next to the readback that checks it.
pub const TERMIOS_LEN: usize = 36;

/// `struct winsize`: four `u16` — rows, columns, and two pixel fields td ignores.
pub const WINSIZE_LEN: usize = 8;

// Pinned in the SHIPPED build, not only in a test: `TCGETS` and `TIOCGWINSZ`
// copy `sizeof(struct …)` through the pointer with no length negotiation, so a
// short buffer is an out-of-bounds kernel write into a stack array — from code
// the compiler reads as `deny(unsafe_code)` clean. The binary the image runs is
// compiled by `recipes/src/recipes/td-sh.rs` calling rustc directly and never
// runs a test, so a `#[test]` would pin these only in the gate. Same argument as
// `SIGACTION_WORDS` above.
const _: () = assert!(TERMIOS_LEN == 4 * 4 + 1 + 19);
const _: () = assert!(WINSIZE_LEN == 4 * 2);

/// `ioctl(2)` is ONE syscall onto an unbounded space of operations, so the number
/// in `rax` is not the surface — the request in `rsi` is. All three are pinned by
/// VALUE, and `ioctl` below refuses anything outside this list BEFORE issuing,
/// so the roster is enforced in code rather than only in a test.
///
/// `TCGETS`/`TCSETS` read and set the line discipline, which is what lets the
/// shell take a keystroke without waiting for Enter. `TIOCGWINSZ` asks how many
/// columns there are, so a line longer than the terminal can be scrolled inside
/// one row rather than wrapping into a redraw nothing can undo.
const TCGETS: usize = 0x5401;
const TCSETS: usize = 0x5402;
const TIOCGWINSZ: usize = 0x5413;
const IOCTL_REQUESTS: [usize; 3] = [TCGETS, TCSETS, TIOCGWINSZ];

/// The ONE `ioctl` call site, and the gate on its request.
///
/// Deliberately NOT in that roster: `TIOCSWINSZ` (the setter; nothing td ships
/// has a reason to resize an operator's terminal), `TCSETSW`/`TCSETSF` (they
/// drain or discard pending terminal I/O another process may own), and `TIOCSTI`
/// (it injects input into a terminal, the classic escape from a restricted
/// session). A fourth request is an amendment to UNSAFE.md.
fn ioctl(fd: RawFd, request: usize, arg: usize) -> Result<(), String> {
    if !IOCTL_REQUESTS.contains(&request) {
        return Err(format!("ioctl: request {request:#x} is not td-sh's to issue"));
    }
    check("ioctl", syscall4(SYS_IOCTL, fd as usize, request, arg, 0))
}

/// `ioctl(fd, TCGETS, &mut termios)` — read the current line discipline.
pub fn termios_get(fd: RawFd, out: &mut [u8; TERMIOS_LEN]) -> Result<(), String> {
    ioctl(fd, TCGETS, out.as_mut_ptr() as usize)
}

/// `ioctl(fd, TCSETS, &termios)` — set it, effective immediately.
pub fn termios_set(fd: RawFd, termios: &[u8; TERMIOS_LEN]) -> Result<(), String> {
    ioctl(fd, TCSETS, termios.as_ptr() as usize)
}

/// `ioctl(fd, TIOCGWINSZ, &mut winsize)` — the terminal's size.
pub fn window_size(fd: RawFd, out: &mut [u8; WINSIZE_LEN]) -> Result<(), String> {
    ioctl(fd, TIOCGWINSZ, out.as_mut_ptr() as usize)
}

/// `struct pollfd`: `int fd; short events; short revents;` — two words on
/// x86-64, laid out as a plain `[u32; 2]` rather than a `#[repr(C)]` type so its
/// field ORDER is a tested function, as td-compositor's winsize is. The two
/// `short`s share the second word, little-endian: `events` low, `revents` high.
/// A swapped pair is a well-formed request for a DIFFERENT event, which the
/// kernel accepts and answers.
const POLLFD_WORDS: usize = 2;

/// Exactly ONE descriptor. Named rather than written as a bare `1` at the call
/// site so the count the kernel is TOLD and the buffer it may write through are
/// pinned together: `nfds = 2` over an eight-byte buffer is precisely the
/// out-of-bounds kernel write `POLLFD_WORDS` exists to prevent, and a literal
/// there is a number no test looks at.
const POLLFD_COUNT: usize = 1;

/// Pinned in the SHIPPED build, not only in a test, for the reason the termios
/// and winsize lengths are: `poll(2)` reads `nfds * sizeof(struct pollfd)`
/// through the pointer and writes `revents` back through it, with no length
/// negotiation, so a short buffer is an out-of-bounds kernel write from code the
/// compiler reads as `deny(unsafe_code)` clean.
const _: () = assert!(
    POLLFD_COUNT * POLLFD_WORDS * core::mem::size_of::<u32>() == 8
);

/// The event bits this shell asks about and the ones it accepts as an answer.
///
/// `POLLIN` is the only one REQUESTED. The other three are output-only — the
/// kernel reports them whether or not they were asked for — and each means a
/// `read` would return at once rather than block: end of file on a pipe whose
/// writer is gone, an error condition, or a descriptor that is not open. That is
/// the question `read -t` asks, so all four count as ready. It is also what
/// `read -t 0 </dev/null` has always answered, and why ash answers 0 for a
/// closed descriptor: not "there are bytes" but "a read would not wait".
const POLLIN: u32 = 0x001;
const POLLERR: u32 = 0x008;
const POLLHUP: u32 = 0x010;
const POLLNVAL: u32 = 0x020;
const POLL_READY: u32 = POLLIN | POLLERR | POLLHUP | POLLNVAL;

/// Whether a read on `fd` would return without waiting, blocking up to
/// `timeout_ms` for that to become true (0 asks and returns at once; a negative
/// value would wait forever and no caller passes one).
///
/// The ONE `poll` call site. Unlike `ioctl` there is no request roster to gate —
/// `poll` has a single meaning — so what is pinned instead is the ARGUMENT: one
/// descriptor, `POLLIN` and nothing else requested, and a buffer whose length
/// the kernel is told matches what it may write.
/// The request as the kernel reads it: `fd` in the first word, `events` in the
/// LOW half of the second. Little-endian x86-64, and a function rather than an
/// inline expression so the field order is something a test can state.
fn pollfd(fd: u32, events: u32) -> [u32; POLLFD_WORDS] {
    [fd, events]
}

/// `revents`, which the kernel writes into the HIGH half of that second word.
///
/// Reading the wrong half is not otherwise detectable: `events` is `POLLIN`,
/// which is also a member of `POLL_READY`, so a wrapper that read the request
/// back as though it were the answer would say "ready" whenever poll returned
/// at all and agree with every other observation.
fn revents(words: &[u32; POLLFD_WORDS]) -> u32 {
    words.get(1).map_or(0, |w| w >> 16)
}

pub fn poll_readable(fd: RawFd, timeout_ms: i32) -> Result<bool, String> {
    // Both guards refuse an ANSWER rather than a failure, which is why they are
    // here and not left to the kernel. `poll` IGNORES a negative descriptor —
    // revents 0, not counted — so `read -t 5` on one would wait the whole five
    // seconds and report a timeout that never happened; and a negative timeout
    // is poll's spelling of "wait forever", which is the one thing `read -t`
    // exists to avoid. Neither is reachable from today's callers.
    let Ok(fd_word) = u32::try_from(fd) else {
        return Err(format!("poll: bad descriptor {fd}"));
    };
    if timeout_ms < 0 {
        return Err(format!("poll: negative timeout {timeout_ms}"));
    }
    // `revents` is what the kernel writes back, so the second word starts as the
    // request alone and is read for the answer afterwards.
    let mut request = pollfd(fd_word, POLLIN);
    let ret = syscall4(
        SYS_POLL,
        request.as_mut_ptr() as usize,
        POLLFD_COUNT,
        timeout_ms as usize,
        0,
    );
    // EINTR is not retried because it cannot arrive: td-sh installs no signal
    // handler, an IGNORED signal does not interrupt a syscall, and Linux
    // restarts `poll` itself across a stop/continue. Serving `trap 'action'`
    // for real would need a handler -- a separate amendment to UNSAFE.md --
    // and this is one of the places that amendment has to revisit.
    check("poll", ret)?;
    // 0 means the timeout expired with nothing ready. Otherwise the answer is in
    // the HIGH half of the second word, and a read is ready if any of the four
    // bits above came back — an empty `revents` with a positive return would be
    // the kernel contradicting itself, and is reported rather than guessed at.
    if ret == 0 {
        return Ok(false);
    }
    let answer = revents(&request);
    if answer & POLL_READY == 0 {
        return Err(format!("poll: returned {ret} with revents {answer:#x}"));
    }
    Ok(true)
}

/// The two dispositions td-sh installs, and a type that cannot spell a third.
///
/// A HANDLER is deliberately not representable. Running one on x86-64 needs a
/// hand-laid `SA_RESTORER` trampoline for the handler to return through, so this
/// shell cannot CATCH a signal at all — only ignore it, or hand it back to the
/// kernel's default. Reads therefore answer with one of these or with `None`,
/// and `None` is a disposition td-sh must leave alone rather than one it could
/// put back.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Disposition {
    Default,
    Ignore,
}

/// Whether `signo` is one `rt_sigaction` will accept an action for at all.
/// Callers check this BEFORE asking, so a refusal from the two functions below
/// always means the kernel disagreed about something it could have done.
pub fn changeable(signo: u8) -> bool {
    (1..=SIG_MAX).contains(&signo) && !UNCHANGEABLE.contains(&signo)
}

/// `rt_sigaction(signo, act, old, sigsetsize)`, with both structs passed as
/// addresses and 0 standing for the NULL that makes each one optional. The ONE
/// call site for this syscall, so the argument order is written down once.
fn rt_sigaction(signo: u8, act: usize, old: usize) -> Result<(), String> {
    check(
        "rt_sigaction",
        syscall4(SYS_RT_SIGACTION, signo as usize, act, old, SIGSETSIZE),
    )
}

/// Read `signo`'s action without touching it: `act` is NULL, so the kernel only
/// writes the previous one back.
fn query(signo: u8) -> Result<[usize; SIGACTION_WORDS], String> {
    let mut old = [0usize; SIGACTION_WORDS];
    rt_sigaction(signo, 0, old.as_mut_ptr() as usize)?;
    Ok(old)
}

/// Install `handler` for `signo` and return the action it replaced.
///
/// EVERY OTHER WORD IS ZERO, which is the disposition-only contract in one line:
/// `sa_flags` 0 means no `SA_RESTORER` (there is no handler to return through),
/// no `SA_SIGINFO` and no `SA_RESTART` to argue about; `sa_restorer` 0 is the
/// address such a return would have used; and an empty `sa_mask` is the set
/// blocked while a handler runs, of which there is none.
fn install(signo: u8, handler: usize) -> Result<[usize; SIGACTION_WORDS], String> {
    // Written out rather than built by index: the handler is visibly FIRST, and
    // the annotation is what makes a change to `SIGACTION_WORDS` a compile error
    // here instead of a short buffer the kernel writes past.
    let act: [usize; SIGACTION_WORDS] = [handler, 0, 0, 0];
    let mut old = [0usize; SIGACTION_WORDS];
    rt_sigaction(signo, act.as_ptr() as usize, old.as_mut_ptr() as usize)?;
    Ok(old)
}

/// A handler word as one of the two dispositions td-sh knows, or `None` for an
/// address — which after `execve` can only be one this process installed itself,
/// since exec resets every caught signal to default.
fn decode(action: &[usize; SIGACTION_WORDS]) -> Option<Disposition> {
    // Destructured rather than indexed: the pattern is total for a fixed-size
    // array, and it names the same word `install` writes.
    let [handler, ..] = action;
    match *handler {
        SIG_DFL => Some(Disposition::Default),
        SIG_IGN => Some(Disposition::Ignore),
        _ => None,
    }
}

/// `signo`'s current disposition, or `None` if a handler is installed.
pub fn signal_get(signo: u8) -> Result<Option<Disposition>, String> {
    if !changeable(signo) {
        return Err(format!("rt_sigaction: signal {signo} is not td-sh's to set"));
    }
    Ok(decode(&query(signo)?))
}

/// The handler word each disposition asks for, and the ONE place either is
/// written. Keeping the mapping in a single function is what makes "these two
/// words and no others" a property of the module rather than of each call site.
fn handler_word(want: Disposition) -> usize {
    match want {
        Disposition::Default => SIG_DFL,
        Disposition::Ignore => SIG_IGN,
    }
}

/// Install `want` for `signo`, and REFUSE unless the kernel agrees it took.
/// Returns the disposition it replaced, which is what a subshell has to put back
/// when it ends.
///
/// The readback is a query, not a second write: `act` is NULL, so it changes
/// nothing and its answer is what the first call actually left in place.
///
/// ATOMIC in the sense the caller needs: a refusal leaves the disposition where
/// it was found, so a caller that gives up is never giving up on a change it
/// made. Without that the one path most needing an undo — the kernel taking
/// something nobody asked for — would be the one path that recorded none.
pub fn signal_set(signo: u8, want: Disposition) -> Result<Option<Disposition>, String> {
    if !changeable(signo) {
        return Err(format!("rt_sigaction: signal {signo} is not td-sh's to set"));
    }
    let prev = decode(&install(signo, handler_word(want))?);
    let took = decode(&query(signo)?);
    if took != Some(want) {
        // Best effort, and honestly so: the readback has already established
        // this kernel is not doing what it is told, so nothing here can promise
        // the way back. Leaving the surprise standing would be worse.
        if let Some(prev) = prev {
            let _ = install(signo, handler_word(prev));
        }
        return Err(format!(
            "trap: kernel kept {took:?} for signal {signo}, not {want:?}"
        ));
    }
    Ok(prev)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    /// The mask and the dispositions are both PROCESS-global and cargo runs
    /// these on parallel threads, so without this they would read each other's
    /// writes. Not a property of the code under test -- a property of what is
    /// being tested.
    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn serial() -> std::sync::MutexGuard<'static, ()> {
        SERIAL.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// `poll_readable` ISSUES the syscall and gets the kernel's answer.
    ///
    /// Every other assertion about this surface is over source TEXT, and a
    /// wrapper that returned a plausible `bool` without issuing anything would
    /// satisfy all of them. So: a pipe with nothing in it is NOT ready, the same
    /// pipe is ready once a byte is in it, and it stays ready at EOF after the
    /// writer is dropped — which is the distinction `read -t` turns on and the
    /// one a "bytes remain" answer would get wrong.
    #[test]
    fn poll_answers_about_a_real_descriptor() {
        use std::io::Write as _;
        use std::os::fd::AsRawFd as _;

        let (reader, mut writer) = std::io::pipe().unwrap();
        let fd = reader.as_raw_fd();
        // Nothing written yet: a read would wait. A zero timeout is the whole
        // question, so this also pins that the wrapper does not block on it.
        assert_eq!(poll_readable(fd, 0), Ok(false));
        writer.write_all(b"x").unwrap();
        assert_eq!(poll_readable(fd, 0), Ok(true));
        // ... and with a NONZERO timeout on an already-ready descriptor, which
        // no other assertion here reaches: a wrapper that answered `false` for
        // every positive timeout would satisfy all of them, and that is the
        // shape `read -t 5` spends its whole deadline in.
        let start = std::time::Instant::now();
        assert_eq!(poll_readable(fd, 5_000), Ok(true));
        assert!(
            start.elapsed() < std::time::Duration::from_secs(1),
            "poll waited {:?} for a descriptor that was already ready",
            start.elapsed()
        );
        // EOF is READY, not "no data": the writer is gone, so a read returns 0
        // at once. This is the arm that makes `read -t` terminate at all.
        drop(writer);
        assert_eq!(poll_readable(fd, 0), Ok(true));

        // A timeout is honoured rather than ignored: an empty pipe whose writer
        // is still alive must take at least the time asked for.
        let (empty, _keep) = std::io::pipe().unwrap();
        let start = std::time::Instant::now();
        assert_eq!(poll_readable(empty.as_raw_fd(), 60), Ok(false));
        assert!(
            start.elapsed() >= std::time::Duration::from_millis(50),
            "poll returned early: {:?}",
            start.elapsed()
        );

        // The two arguments the wrapper refuses rather than passes on. Both
        // would be ANSWERED by poll — a negative descriptor is ignored and
        // reported not-ready, a negative timeout waits forever — so neither
        // failure would look like one at the call site.
        assert!(poll_readable(-1, 0).is_err());
        assert!(poll_readable(fd, -1).is_err());
    }

    /// `struct pollfd`'s field order, which no observation of the syscall can
    /// check: `events` is `POLLIN`, itself a member of `POLL_READY`, so a
    /// wrapper reading the request back as the answer says "ready" whenever
    /// poll returns at all and agrees with every other test in this file.
    #[test]
    fn the_pollfd_words_put_each_field_where_the_kernel_reads_it() {
        // fd in the first word; events in the LOW half of the second.
        assert_eq!(pollfd(7, POLLIN), [7, 0x0001]);
        assert_eq!(pollfd(0, 0), [0, 0]);
        // revents comes out of the HIGH half, and nothing of `events` leaks in.
        assert_eq!(revents(&[0, 0x0010_0001]), 0x0010);
        assert_eq!(revents(&[0xffff_ffff, 0x0000_ffff]), 0);
        // The four bits that count as ready, each restated by value here so a
        // wrong one is caught independently of the declaration, and one that
        // does not: 0x004 is POLLOUT, never requested and never an answer about
        // reading.
        assert_eq!((POLLIN, POLLERR, POLLHUP, POLLNVAL), (0x001, 0x008, 0x010, 0x020));
        assert_eq!(POLL_READY, 0x001 | 0x008 | 0x010 | 0x020);
        assert_eq!(POLL_READY & 0x004, 0);
        // One descriptor, and the buffer the kernel is told about is the one it
        // may write through.
        assert_eq!(POLLFD_COUNT, 1);
        assert_eq!(POLLFD_COUNT * POLLFD_WORDS * core::mem::size_of::<u32>(), 8);
    }

    /// Two threads setting a umask at once do not read back each other's.
    ///
    /// `umask_set` installs and then asks what took, because there is no reading
    /// syscall — two calls, and with pipeline stages running at once two of those
    /// pairs interleave. Each thread then reads the OTHER's mask and reports
    /// `kernel kept …` about a kernel that did exactly what it was told.
    ///
    /// In-process rather than over the built binary: the same race measured
    /// through `td-sh -c 'umask 077 | umask 022'` fires about 4 times in 1000, so
    /// a process-level test needs thousands of spawns to be reliable and still
    /// caught its own mutant only about half the time. Here the window is all
    /// there is between the two calls, and the mutant reds in well under a
    /// second.
    #[test]
    fn concurrent_setters_do_not_read_back_each_other_s_mask() {
        let _serial = serial();
        let restore = umask_get();
        let failures = std::sync::Mutex::new(Vec::<String>::new());
        std::thread::scope(|scope| {
            for mask in [0o077u32, 0o022] {
                let failures = &failures;
                scope.spawn(move || {
                    for _ in 0..4000 {
                        if let Err(e) = umask_set(mask) {
                            if let Ok(mut f) = failures.lock() {
                                f.push(e);
                            }
                            return;
                        }
                    }
                });
            }
        });
        let seen = failures.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let _ = umask_set(restore);
        assert!(
            seen.is_empty(),
            "a setter read back the mask another thread had installed: {seen:?}"
        );
    }

    /// The value of a `/proc/self/status` field, when that file is readable.
    fn proc_status_field(key: &str) -> Option<String> {
        let status = std::fs::read_to_string("/proc/self/status").ok()?;
        status
            .lines()
            .filter(|l| l.starts_with(key))
            .map(|l| l.trim_start_matches(key).trim().to_string())
            .next()
    }

    /// The umask syscall is really ISSUED and the kernel really answers.
    ///
    /// Every other assertion about this module is about source TEXT; a wrapper
    /// that returned a plausible number without issuing anything would satisfy
    /// all of them. `/proc/self/status` is a SECOND, independent view of the same
    /// kernel state, so agreement between it and `umask_get()` cannot come from
    /// this module talking to itself.
    #[test]
    fn the_umask_syscall_is_issued_and_proc_agrees() {
        let _serial = serial();
        let restore = umask_get();
        // NO value here masks 0o400. The mask is process-global and cargo runs
        // these beside tests that create a file and read it back; a mask that
        // took away owner-read would fail THOSE, in a way that would read as
        // their flakiness rather than as this test's reach.
        for want in [0o022u32, 0o077, 0o000, 0o377, 0o027] {
            umask_set(want).unwrap();
            assert_eq!(umask_get(), want, "umask_get disagrees with what was installed");
            if let Some(text) = proc_status_field("Umask:") {
                let seen = u32::from_str_radix(&text, 8).unwrap();
                assert_eq!(seen, want, "/proc/self/status says {text}, not {want:04o}");
            }
        }
        umask_set(restore).unwrap();
    }

    /// `umask_get` must LEAVE the mask alone — it is a read, spelled as two writes.
    #[test]
    fn reading_the_mask_does_not_change_it() {
        let _serial = serial();
        let restore = umask_get();
        umask_set(0o027).unwrap();
        assert_eq!(umask_get(), 0o027);
        assert_eq!(umask_get(), 0o027, "a second read saw a different mask");
        umask_set(restore).unwrap();
    }

    /// Bits the kernel would drop are clamped here instead, so `umask_set` never
    /// asks for something it cannot get. Linux keeps only the nine rwx bits,
    /// which is why 0o7777 and 0o10000 both come back as 0o777.
    #[test]
    fn the_mask_is_clamped_to_the_permission_bits() {
        let _serial = serial();
        let restore = umask_get();
        umask_set(0o7377).unwrap();
        assert_eq!(umask_get(), 0o377, "setuid/setgid/sticky are not part of a umask");
        umask_set(0o10000 | 0o022).unwrap();
        assert_eq!(umask_get(), 0o022);
        umask_set(restore).unwrap();
    }

    /// `/proc/self/status`'s `SigIgn:` is a 64-bit mask, one bit per signal,
    /// numbered from 1. This is the SECOND, independent view that keeps the
    /// assertions below from being this module talking to itself -- and it is
    /// the only one available, since observing an ignore by DELIVERING the
    /// signal would need `kill(2)`, which is not this surface.
    fn proc_ignored(signo: u8) -> Option<bool> {
        let text = proc_status_field("SigIgn:")?;
        let bits = u64::from_str_radix(&text, 16).ok()?;
        Some(bits & (1u64 << signo.checked_sub(1)?) != 0)
    }

    /// A disposition is really ISSUED, and `/proc` sees it move both ways.
    ///
    /// 63 and 64 deliberately, and they are RESERVED for this test. Dispositions
    /// are process-global, and the shell-level tests beside this one run real
    /// `trap` commands on parallel threads, so a signal both could name is a
    /// race no mutex in this module can settle. These two have no name in
    /// `builtin.rs`'s table at all, so reaching them from a script means writing
    /// the number -- and 64 is `SIG_MAX`, so the top of the range is exercised
    /// with it.
    #[test]
    fn a_disposition_is_issued_and_proc_agrees() {
        let _serial = serial();
        for signo in [63u8, 64] {
            assert_eq!(signal_get(signo).unwrap(), Some(Disposition::Default));
            if let Some(seen) = proc_ignored(signo) {
                assert!(!seen, "signal {signo} is ignored before anything asked");
            }

            let prev = signal_set(signo, Disposition::Ignore).unwrap();
            assert_eq!(prev, Some(Disposition::Default), "the replaced action came back");
            assert_eq!(signal_get(signo).unwrap(), Some(Disposition::Ignore));
            if let Some(seen) = proc_ignored(signo) {
                assert!(seen, "/proc/self/status does not list {signo} as ignored");
            }

            let prev = signal_set(signo, Disposition::Default).unwrap();
            assert_eq!(prev, Some(Disposition::Ignore), "the ignore came back");
            assert_eq!(signal_get(signo).unwrap(), Some(Disposition::Default));
            if let Some(seen) = proc_ignored(signo) {
                assert!(!seen, "signal {signo} is still ignored after a restore");
            }
        }
    }

    /// The handler is word ZERO. A layout slip that wrote it as `sa_flags` would
    /// leave the disposition untouched -- which is what `signal_set`'s readback
    /// is there to catch, and what makes the field order a tested function
    /// rather than a comment.
    #[test]
    fn the_handler_is_the_first_word_of_the_struct() {
        // The 32-byte size is pinned by a `const` assertion above rather than
        // here, so it holds in the SHIPPED build, which runs no test.
        assert_eq!(decode(&[SIG_IGN, 0, 0, 0]), Some(Disposition::Ignore));
        assert_eq!(decode(&[SIG_DFL, 0, 0, 0]), Some(Disposition::Default));
        // A handler ADDRESS decodes to neither, which is what lets a caller act
        // on "not td-sh's to change" rather than guess at it.
        assert_eq!(decode(&[0x7fff_0000_1000, 0, 0, 0]), None);
        // ... and the same word anywhere else is not the handler.
        assert_eq!(
            decode(&[SIG_DFL, SIG_IGN, SIG_IGN, SIG_IGN]),
            Some(Disposition::Default)
        );
    }

    /// The kernel-only signals are refused HERE, so no call is made that the
    /// kernel was always going to answer `EINVAL` to.
    #[test]
    fn kill_and_stop_are_refused_before_the_syscall() {
        for signo in [9u8, 19] {
            assert!(!changeable(signo), "signal {signo} must not be changeable");
            assert!(signal_set(signo, Disposition::Ignore).is_err());
            assert!(signal_get(signo).is_err());
        }
        // 0 is POSIX's EXIT condition, not a signal, and 65 is past `_NSIG`.
        assert!(!changeable(0));
        assert!(!changeable(SIG_MAX + 1));
        assert!(changeable(1) && changeable(SIG_MAX));
    }

    /// Rust's own runtime ignores SIGPIPE before `main`, so this process starts
    /// with a disposition td-sh did not install -- the case the caller's
    /// first-touch rule turns into "not mine to change". Asserted here because
    /// it is a fact about the RUNTIME: a std that stopped doing it would quietly
    /// turn `trap - PIPE` into a shell that dies on `yes | head`.
    #[test]
    fn the_runtime_ignores_sigpipe_before_main() {
        let _serial = serial();
        assert_eq!(signal_get(13).unwrap(), Some(Disposition::Ignore));
    }
}

//! The confined raw-syscall layer for a full-screen terminal application — the
//! whole `unsafe` surface of this crate.
//!
//! The crate root `#![deny(unsafe_code)]`s and exactly one item here carries a
//! scoped `#[allow]`: `syscall4`, the syscall-instruction body copied from
//! `td-sh/src/sys.rs` (which took it from `td-util/src/sys.rs`, which took it
//! from `td-init/src/sys.rs`). Everything else in the crate — and every other
//! function in this module — is ordinary safe Rust over byte arrays.
//!
//! The surface is td-sh's, minus two syscalls and plus nothing. td-sh carries
//! `umask(2)`, a disposition-only `rt_sigaction(2)`, `ioctl(2)` with three
//! value-pinned requests, and `poll(2)`. A terminal application needs neither of
//! the first two: it creates no files, and it installs no signal handler — which
//! is a deliberate exclusion rather than an omission, because the obvious way to
//! learn about a resize is a SIGWINCH handler and that is exactly the
//! `rt_sigaction(2)` this module refuses to own. The window size is asked for
//! instead, once per input tick, which costs one `TIOCGWINSZ` on a timer that
//! already exists and needs no handler, no `SA_RESTORER` trampoline, and no
//! async-signal-safe store.
//!
//! So: `ioctl(2)` with the same THREE value-pinned requests td-sh admits —
//! `TCGETS`/`TCSETS` to read and set the line discipline, `TIOCGWINSZ` to ask
//! how many rows and columns — and one readiness call. `std` exposes an API for
//! none of them: `IsTerminal` answers whether a descriptor IS a terminal, never
//! how big it is, what mode it is in, or whether reading it would wait.
//!
//! Because `ioctl(2)` is one syscall onto an unbounded space of operations, the
//! number in the syscall register is not the surface — the request is. All three
//! are pinned by VALUE and `ioctl` below refuses anything outside the roster
//! BEFORE issuing, so the roster is code and not merely a test. Deliberately NOT
//! in it, each a digit away from something admitted: `TCSETSW`/`TCSETSF` (they
//! drain or discard pending terminal I/O another process may own — a screen
//! application has no business throwing away what someone else wrote, and
//! `TCSETS` mistyped as 0x5404 IS `TCSETSF`), `TIOCSWINSZ` (the setter; nothing
//! here has a reason to resize an operator's terminal), `TIOCSTI` (it injects
//! input into a terminal, the classic escape from a restricted session), and
//! `TIOCSCTTY` (that one is td-init's, for cttyhack and getty).
//!
//! Readiness is `poll(2)`, td-sh's exact number and argument shape, asking about
//! one descriptor.
//!
//! x86_64-linux and no other target, which is what makes the surface td-sh's
//! rather than merely like it: the syscall numbers, the instruction and the
//! register mapping are all properties of one ABI, and every td crate on the
//! roster states the same restriction the same way. It is not a portability gap
//! to be filled in quietly — asm-generic has no `poll(2)` at all, so a second
//! architecture would arrive carrying a syscall this file does not admit, and
//! that is an amendment to UNSAFE.md rather than a `#[cfg]`.
//!
//! The termios and winsize buffers are OPAQUE to the syscall wrappers: they hand
//! the kernel a correctly sized array and give one back. The layout knowledge
//! lives further down this file, next to the readback that checks it, and TWO
//! things keep that honest — both copied from `td-sh/src/term.rs` and
//! `td-util/src/term.rs` before it. First, a termios is never CONSTRUCTED: the
//! kernel's own bytes are read, known offsets are patched, and the untouched
//! original is what a restore writes back. Second, raw mode is read back and
//! REFUSED unless the kernel agrees, because a `TCSETS` can succeed having
//! applied only part of what was asked and a terminal still in canonical mode is
//! indistinguishable from one whose reader has not typed yet. The readback
//! compares the WHOLE 36 bytes against exactly what was computed, so a byte the
//! patch never named moving is a failure too — which is what makes "never
//! constructs a termios" a property rather than a claim, since a zeroed buffer
//! has ICANON and ECHO clear and would pass a check that only looked at the bits
//! raw mode names, while `c_cflag = 0` is B0, a hang-up on a serial console.
//!
//! Descriptors follow td-sh's split. The private wrappers take a `RawFd`, as
//! `sys.rs` does; every public entry point takes a `BorrowedFd`, as `term.rs`
//! does. That is load-bearing for exactly one of them: `Raw` issues a syscall
//! from `Drop` on a descriptor it does not own, so the borrow checker is what
//! stops the terminal being closed — or closed and RECYCLED — before the restore
//! reaches it.
//!
//! Deliberately NOT here: `umask(2)` and `rt_sigaction(2)` (above);
//! `TIOCSPTLCK`/`TIOCGPTPEER` (a terminal application is the child of a pty, not
//! its allocator); `isatty`, which `std::io::IsTerminal` already answers safely;
//! `read`/`write` on the terminal, which are `std::io`; and `tcdrain`/`tcflush`
//! in any spelling, which is the `TCSETSW`/`TCSETSF` argument by another name.

use std::num::NonZeroU8;
use std::os::fd::{AsRawFd, BorrowedFd, RawFd};
use std::sync::Mutex;

#[cfg(not(all(target_arch = "x86_64", target_os = "linux")))]
compile_error!("tdstd's terminal layer is x86_64-linux only (raw syscall ABI)");

// The syscall numbers. TWO and no more: `ioctl` for the three requests below,
// and `poll` for the one readiness question.
const SYS_POLL: usize = 7;
const SYS_IOCTL: usize = 16;

/// The single raw-syscall entry point, copied from `td-sh/src/sys.rs`. Its body
/// is the ONLY `unsafe` in the crate. The scoped `#[allow]` covers where
/// `unsafe` may appear, not what may be passed here — this fn is safe to CALL,
/// so its confinement is module privacy plus the two typed wrappers below being
/// its only callers.
#[inline]
#[allow(unsafe_code)]
fn syscall4(n: usize, a1: usize, a2: usize, a3: usize, a4: usize) -> isize {
    let ret: isize;
    // SAFETY: the `syscall` instruction clobbers rcx/r11 and returns in rax; the
    // args are plain integers or a pointer-as-usize whose pointee the caller
    // keeps live and correctly sized across the call. `options(nomem)` is
    // deliberately ABSENT and load-bearing by its absence: TCGETS, TIOCGWINSZ
    // and poll all have the kernel WRITE through a pointer that reaches here
    // only as an integer, and this asm is what makes those accesses visible to
    // the compiler -- a stack array whose address is cast to an integer and
    // passed to an asm that may touch memory is escaped, so its stores cannot be
    // eliminated and its slot cannot be reused across the call. `nomem` would
    // withdraw exactly that.
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

/// `struct termios` as the kernel lays it out on both of this module's targets:
/// four `u32` flag words, a `c_line` byte, then `NCCS` = 19 control characters.
/// OPAQUE to the wrappers below — they never decide what a field means.
pub const TERMIOS_LEN: usize = 36;

/// `struct winsize`: four `u16` — rows, columns, and two pixel fields this
/// module reports but nothing here interprets.
pub const WINSIZE_LEN: usize = 8;

// Pinned in the build, not only in a test: TCGETS and TIOCGWINSZ copy
// `sizeof(struct ...)` through the pointer with no length negotiation, so a
// short buffer is an out-of-bounds kernel write into a stack array -- from code
// the compiler reads as `deny(unsafe_code)` clean.
const _: () = assert!(TERMIOS_LEN == 4 * 4 + 1 + 19);
const _: () = assert!(WINSIZE_LEN == 4 * 2);

/// `ioctl(2)` is ONE syscall onto an unbounded space of operations, so the
/// number in the syscall register is not the surface — the request is. All three
/// are pinned by VALUE, and `ioctl` below refuses anything outside this list
/// BEFORE issuing, so the roster is enforced in code rather than only in a test.
const TCGETS: usize = 0x5401;
const TCSETS: usize = 0x5402;
const TIOCGWINSZ: usize = 0x5413;
const IOCTL_REQUESTS: [usize; 3] = [TCGETS, TCSETS, TIOCGWINSZ];

/// The ONE `ioctl` call site, and the gate on its request.
///
/// A fourth request is an amendment to UNSAFE.md, not an edit somebody has to
/// notice at a new call site.
fn ioctl(fd: RawFd, request: usize, arg: usize) -> Result<(), String> {
    if !IOCTL_REQUESTS.contains(&request) {
        return Err(format!(
            "ioctl: request {request:#x} is not tdstd's to issue"
        ));
    }
    check("ioctl", syscall4(SYS_IOCTL, fd as usize, request, arg, 0))
}

/// `ioctl(fd, TCGETS, &mut termios)` — read the current line discipline.
fn termios_get(fd: RawFd, out: &mut [u8; TERMIOS_LEN]) -> Result<(), String> {
    ioctl(fd, TCGETS, out.as_mut_ptr() as usize)
}

/// `ioctl(fd, TCSETS, &termios)` — set it, effective immediately.
///
/// Private, and the only way to reach it from outside this module is through the
/// guard below: every set is paired with a saved original and a readback, so
/// neither can be forgotten.
fn termios_set(fd: RawFd, termios: &[u8; TERMIOS_LEN]) -> Result<(), String> {
    ioctl(fd, TCSETS, termios.as_ptr() as usize)
}

/// `ioctl(fd, TIOCGWINSZ, &mut winsize)` — the terminal's size.
fn winsize(fd: RawFd, out: &mut [u8; WINSIZE_LEN]) -> Result<(), String> {
    ioctl(fd, TIOCGWINSZ, out.as_mut_ptr() as usize)
}

/// `struct pollfd`: `int fd; short events; short revents;` — two words, laid out
/// as a plain `[u32; 2]` rather than a `#[repr(C)]` type so its field ORDER is a
/// tested function. The two `short`s share the second word, little-endian:
/// `events` low, `revents` high. A swapped pair is a well-formed request for a
/// DIFFERENT event, which the kernel accepts and answers.
const POLLFD_WORDS: usize = 2;

/// Exactly ONE descriptor. Named rather than written as a bare `1` at the call
/// site so the count the kernel is TOLD and the buffer it may write through are
/// pinned together: `nfds = 2` over an eight-byte buffer is precisely the
/// out-of-bounds kernel write `POLLFD_WORDS` exists to prevent.
const POLLFD_COUNT: usize = 1;

// Pinned in the build for the reason the termios and winsize lengths are: poll
// reads `nfds * sizeof(struct pollfd)` through the pointer and writes `revents`
// back through it, with no length negotiation.
const _: () = assert!(POLLFD_COUNT * POLLFD_WORDS * core::mem::size_of::<u32>() == 8);

/// The event bits this module asks about and the ones it accepts as an answer.
///
/// `POLLIN` is the only one REQUESTED. The other three are output-only — the
/// kernel reports them whether or not they were asked for — and each means a
/// `read` would return at once rather than block: end of file on a pipe whose
/// writer is gone, an error condition, or a descriptor that is not open. The
/// question is "would a read wait", not "are there bytes", so all four count.
const POLLIN: u32 = 0x001;
const POLLERR: u32 = 0x008;
const POLLHUP: u32 = 0x010;
const POLLNVAL: u32 = 0x020;
const POLL_READY: u32 = POLLIN | POLLERR | POLLHUP | POLLNVAL;

/// The request as the kernel reads it: `fd` in the first word, `events` in the
/// LOW half of the second. A function rather than an inline expression so the
/// field order is something a test can state.
fn pollfd(fd: u32, events: u32) -> [u32; POLLFD_WORDS] {
    [fd, events]
}

/// `revents`, which the kernel writes into the HIGH half of that second word.
///
/// Reading the wrong half is not otherwise detectable: `events` is `POLLIN`,
/// which is also a member of `POLL_READY`, so a wrapper that read the request
/// back as though it were the answer would say "ready" whenever poll returned at
/// all and agree with every other observation.
fn revents(words: &[u32; POLLFD_WORDS]) -> u32 {
    words.get(1).map_or(0, |w| w >> 16)
}

/// Whether a read on `tty` would return without waiting, blocking up to
/// `timeout_ms` for that to become true (0 asks and returns at once).
///
/// This is what replaces a SIGWINCH handler: a screen application asks for
/// readiness with a bounded timeout, draws when a key arrives, and re-reads the
/// window size when the timeout expires. Nothing in `std` asks this question —
/// a blocking `read` answers it only by not returning.
///
/// The ONE `poll` call site. Unlike `ioctl` there is no request roster to gate —
/// poll has a single meaning — so what is pinned instead is the ARGUMENT: one
/// descriptor, `POLLIN` and nothing else requested, and a buffer whose length
/// the kernel is told matches what it may write.
pub fn poll_readable(tty: BorrowedFd<'_>, timeout_ms: i32) -> Result<bool, String> {
    // Both guards refuse an ANSWER rather than a failure, which is why they are
    // here and not left to the kernel. poll IGNORES a negative descriptor --
    // revents 0, not counted -- so a five-second wait on one would spend the
    // whole five seconds and report a timeout that never happened; and a
    // negative timeout is poll's spelling of "wait forever", which is the one
    // thing a draw loop exists to avoid.
    let fd = tty.as_raw_fd();
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
    // EINTR is not retried because it cannot arrive: this crate installs no
    // signal handler, an IGNORED signal does not interrupt a syscall, and Linux
    // restarts poll itself across a stop/continue. A handler-bearing
    // `rt_sigaction` would be a separate amendment to UNSAFE.md, and this is one
    // of the places that amendment has to revisit.
    check("poll", ret)?;
    // 0 means the timeout expired with nothing ready. Otherwise the answer is in
    // the HIGH half of the second word, and a read is ready if any of the four
    // bits came back -- an empty `revents` with a positive return would be the
    // kernel contradicting itself, and is reported rather than guessed at.
    if ret == 0 {
        return Ok(false);
    }
    let answer = revents(&request);
    if answer & POLL_READY == 0 {
        return Err(format!("poll: returned {ret} with revents {answer:#x}"));
    }
    Ok(true)
}

// ---------------------------------------------------------------------------
// The layout half: the only code in the crate that knows what a termios byte
// means, kept next to the readback that checks it.
// ---------------------------------------------------------------------------

/// The four leading `u32` flag words, in order.
const IFLAG_AT: usize = 0;
const OFLAG_AT: usize = 4;
const CFLAG_AT: usize = 8;
const LFLAG_AT: usize = 12;
/// `c_line`, the one byte between the flags and the control characters.
const LINE_AT: usize = 16;
/// `c_cc` follows it, and holds `NCCS` slots.
const CC_AT: usize = 17;
const NCCS: usize = 19;

const _: () = assert!(TERMIOS_LEN == CC_AT + NCCS);

/// Indices into `c_cc`. `VTIME` and `VMIN` are 5 and 6 on this module's target —
/// the asm-generic order, which is NOT the order the POSIX-canonical
/// architectures use, so they are pinned by value in a test rather than trusted.
const VINTR: usize = 0;
const VEOF: usize = 4;
const VTIME: usize = 5;
const VMIN: usize = 6;

// `c_iflag` bits. ICRNL turns a carriage return into a newline, which a screen
// application must see as the key that was pressed; IXON steals Ctrl-S/Ctrl-Q
// for flow control; BRKINT raises SIGINT on a break; INPCK checks parity;
// ISTRIP clears the eighth bit of every byte, which would shred UTF-8.
const BRKINT: u32 = 0x0000_0002;
const INPCK: u32 = 0x0000_0010;
const ISTRIP: u32 = 0x0000_0020;
const ICRNL: u32 = 0x0000_0100;
const IXON: u32 = 0x0000_0400;

// `c_oflag`. OPOST is all output post-processing, ONLCR included: with it set,
// every `\n` a full-screen renderer writes becomes a carriage return too and the
// frame staircases.
const OPOST: u32 = 0x0000_0001;

// `c_cflag`. CS8 is the character size field with both bits set, and CSIZE is
// that same pair — so OR-ing CS8 in sets the size to eight bits whatever it was.
const CS8: u32 = 0x0000_0030;

// `c_lflag`. ISIG turns three keystrokes into signals, ICANON assembles lines,
// ECHO prints what was typed, IEXTEN gives Ctrl-V its literal-next meaning.
const ISIG: u32 = 0x0000_0001;
const ICANON: u32 = 0x0000_0002;
const ECHO: u32 = 0x0000_0008;
const IEXTEN: u32 = 0x0000_8000;

/// Which published flag set raw mode applies. Two arms and no third, and no way
/// to name a bit from outside this module: a caller free to compose a flag word
/// could compose one that hangs up a serial console, and nothing at the call
/// site would show it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FlagSet {
    /// The minimum a full-screen reader needs: `ISIG`/`ICANON`/`ECHO` off so
    /// keystrokes arrive as bytes and the application draws every one itself,
    /// `ICRNL`/`IXON` off so Enter and Ctrl-S arrive as themselves, and `OPOST`
    /// off so a drawn frame is not post-processed.
    Keys,
    /// That, plus what `cfmakeraw` adds: `BRKINT`/`INPCK`/`ISTRIP` off, `IEXTEN`
    /// off, and `CS8` on. Nothing here is optional for a byte-oriented reader —
    /// `ISTRIP` alone would clear the eighth bit of every UTF-8 continuation —
    /// but it is a second arm rather than the only one because widening the set
    /// a caller already ships with is a behaviour change, not a tidy-up.
    Bytes,
}

impl FlagSet {
    /// The `c_iflag` bits this set clears.
    fn iflag_clear(self) -> u32 {
        match self {
            FlagSet::Keys => ICRNL | IXON,
            FlagSet::Bytes => BRKINT | ICRNL | INPCK | ISTRIP | IXON,
        }
    }

    /// The `c_oflag` bits it clears. Both sets clear the same one.
    fn oflag_clear(self) -> u32 {
        OPOST
    }

    /// The `c_cflag` bits it SETS — the one place this module adds a bit rather
    /// than removing one.
    fn cflag_set(self) -> u32 {
        match self {
            FlagSet::Keys => 0,
            FlagSet::Bytes => CS8,
        }
    }

    /// The `c_lflag` bits it clears.
    fn lflag_clear(self) -> u32 {
        match self {
            FlagSet::Keys => ISIG | ICANON | ECHO,
            FlagSet::Bytes => ECHO | ICANON | IEXTEN | ISIG,
        }
    }
}

/// What satisfies a read while raw mode is in force — `VMIN` and `VTIME`, which
/// are the two bytes a partial `TCSETS` most often leaves behind.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ReadMode {
    /// `VMIN = 1`, `VTIME = 0`: a read waits for one byte, with no inter-byte
    /// timer. The loop blocks until the operator presses something.
    Blocking,
    /// `VMIN = 0`, `VTIME = tenths`: a read returns as soon as a byte arrives,
    /// or after `tenths` tenths of a second with nothing — which is how a draw
    /// loop gets a tick to re-ask the window size on without a signal handler.
    ///
    /// `NonZeroU8` because `VMIN = 0` with `VTIME = 0` is not a timer at all: it
    /// is a non-blocking read, and a loop over one is a busy wait that pins a
    /// core. A caller that wants to ask without waiting has `poll_readable`.
    Timed { tenths: NonZeroU8 },
}

impl ReadMode {
    fn vmin(self) -> u8 {
        match self {
            ReadMode::Blocking => 1,
            ReadMode::Timed { .. } => 0,
        }
    }

    fn vtime(self) -> u8 {
        match self {
            ReadMode::Blocking => 0,
            ReadMode::Timed { tenths } => tenths.get(),
        }
    }
}

/// Everything raw mode is allowed to vary.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct RawOptions {
    pub flags: FlagSet,
    pub read: ReadMode,
}

impl RawOptions {
    /// Keystrokes as bytes, and a read that waits for one.
    pub const KEYS_BLOCKING: RawOptions = RawOptions {
        flags: FlagSet::Keys,
        read: ReadMode::Blocking,
    };

    /// The `cfmakeraw` flags, and a read that gives up after a tenth of a second
    /// so the loop around it has somewhere to check the window size.
    pub const BYTES_TENTH: RawOptions = RawOptions {
        flags: FlagSet::Bytes,
        read: ReadMode::Timed {
            tenths: NonZeroU8::MIN,
        },
    };
}

/// The termios raw mode asks for: the terminal's own bytes with four flag words
/// patched and two control bytes set, and NOTHING else touched.
///
/// Split out and pure because it is the part a test without a terminal can
/// reach, and because `raw`'s readback compares the kernel against this same
/// computation — so the two stay consistent whichever bits are named above.
fn raw_termios(saved: &[u8; TERMIOS_LEN], options: RawOptions) -> [u8; TERMIOS_LEN] {
    let mut want = *saved;
    let flags = options.flags;
    write_u32(
        &mut want,
        IFLAG_AT,
        read_u32(saved, IFLAG_AT) & !flags.iflag_clear(),
    );
    write_u32(
        &mut want,
        OFLAG_AT,
        read_u32(saved, OFLAG_AT) & !flags.oflag_clear(),
    );
    write_u32(
        &mut want,
        CFLAG_AT,
        read_u32(saved, CFLAG_AT) | flags.cflag_set(),
    );
    write_u32(
        &mut want,
        LFLAG_AT,
        read_u32(saved, LFLAG_AT) & !flags.lflag_clear(),
    );
    set_cc(&mut want, VMIN, options.read.vmin());
    set_cc(&mut want, VTIME, options.read.vtime());
    want
}

/// Where `got` differs from `want`, named, or `None` when the kernel agreed
/// about every one of the 36 bytes.
///
/// The whole struct, not the bits raw mode named. Checking only the named bits
/// would pass a ZEROED buffer — a zeroed `c_lflag` has ICANON and ECHO clear —
/// while `c_cflag = 0` is B0, a hang-up on a serial console, and `c_oflag = 0`
/// drops ONLCR for whatever runs next. Comparing everything is what makes "a
/// termios is never constructed here" a property rather than a claim, and it can
/// only ever fire on bytes this module handed back verbatim.
fn disagreement(got: &[u8; TERMIOS_LEN], want: &[u8; TERMIOS_LEN]) -> Option<String> {
    for (name, at) in [
        ("c_iflag", IFLAG_AT),
        ("c_oflag", OFLAG_AT),
        ("c_cflag", CFLAG_AT),
        ("c_lflag", LFLAG_AT),
    ] {
        let (mine, theirs) = (read_u32(want, at), read_u32(got, at));
        if mine != theirs {
            return Some(format!("{name}: kernel kept {theirs:#x}, not {mine:#x}"));
        }
    }
    if got.get(LINE_AT) != want.get(LINE_AT) {
        return Some("c_line: the kernel moved a byte nothing asked about".to_string());
    }
    for idx in 0..NCCS {
        let (mine, theirs) = (want.get(CC_AT + idx), got.get(CC_AT + idx));
        if mine != theirs {
            let name = match idx {
                VMIN => "c_cc[VMIN]",
                VTIME => "c_cc[VTIME]",
                _ => "c_cc",
            };
            return Some(format!(
                "{name}: kernel kept {theirs:?} at slot {idx}, not {mine:?}"
            ));
        }
    }
    None
}

/// A terminal switched to raw mode, and the bytes to put back.
///
/// Restoring in `Drop` is not tidiness: every exit path — a key, an error, a `?`
/// three frames down, a panic in a caller that unwinds — otherwise leaves the
/// terminal in whatever mode it was last set to, and a shell prompt on a
/// terminal with no echo and no line assembly is a machine the operator has to
/// guess how to recover.
///
/// The descriptor is BORROWED, not a bare `RawFd`, because this type issues a
/// syscall from a `Drop` that runs whenever the guard goes out of scope: if the
/// terminal could be closed first the restore would go to a closed or, worse, a
/// RECYCLED descriptor, and `BorrowedFd` makes the borrow checker refuse that
/// whole family at compile time.
pub struct Raw<'a> {
    fd: BorrowedFd<'a>,
    saved: [u8; TERMIOS_LEN],
    restored: bool,
}

impl Raw<'_> {
    /// The byte the DRIVER would have turned into SIGINT, and the one it would
    /// have turned into end-of-input. With `ISIG`/`ICANON` cleared the kernel
    /// does neither, so the application has to recognise them itself — and it
    /// must use the terminal's own settings rather than 0x03/0x04, or
    /// `stty intr '^X'` would leave the operator with no way out.
    ///
    /// `None` when the terminal has the character DISABLED, which POSIX spells
    /// `_POSIX_VDISABLE` and Linux gives as a zero byte. Handing that back as an
    /// ordinary binding would make a NUL keystroke — which `stty intr ''` was
    /// asking NOT to be one — quit the application.
    pub fn intr(&self) -> Option<u8> {
        enabled(self.saved.get(CC_AT + VINTR).copied().unwrap_or(0x03))
    }

    pub fn eof(&self) -> Option<u8> {
        enabled(self.saved.get(CC_AT + VEOF).copied().unwrap_or(0x04))
    }

    /// Put the terminal back and SAY whether it took.
    ///
    /// `Drop` does this too, and has nowhere to report from — a destructor that
    /// panicked on a failed restore would replace a terminal in the wrong mode
    /// with an abort during unwinding, which is strictly worse. So a caller that
    /// wants the answer asks for it here, and one that does not gets the record
    /// through `take_restore_failure`.
    pub fn restore(mut self) -> Result<(), String> {
        let outcome = self.put_back();
        self.restored = true;
        outcome
    }

    /// Write the saved bytes back, and REFUSE unless the kernel agrees. A
    /// restore can be partial exactly as an entry can, and a terminal left half
    /// in raw mode is the failure this whole module exists to make visible.
    fn put_back(&mut self) -> Result<(), String> {
        let fd = self.fd.as_raw_fd();
        termios_set(fd, &self.saved)?;
        let mut got = [0u8; TERMIOS_LEN];
        termios_get(fd, &mut got)?;
        match disagreement(&got, &self.saved) {
            Some(why) => Err(format!("terminal did not leave raw mode: {why}")),
            None => Ok(()),
        }
    }
}

impl Drop for Raw<'_> {
    fn drop(&mut self) {
        if self.restored {
            return;
        }
        if let Err(why) = self.put_back() {
            record_restore_failure(why);
        }
    }
}

/// The last restore that did not take, and nothing else. Process-global because
/// `Drop` has no caller to hand a `Result` to; one slot rather than a list
/// because a program with two terminals in the wrong mode has the same one
/// problem.
static RESTORE_FAILURE: Mutex<Option<String>> = Mutex::new(None);

fn record_restore_failure(why: String) {
    let mut slot = RESTORE_FAILURE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    *slot = Some(why);
}

/// Take the recorded restore failure, if a dropped guard left one.
///
/// An application should ask once on its way out and print what it finds: the
/// terminal is in the wrong mode and the operator is the only one who can see
/// it. Taking rather than peeking so a second look does not report the same
/// failure twice.
pub fn take_restore_failure() -> Option<String> {
    RESTORE_FAILURE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .take()
}

/// `_POSIX_VDISABLE`: a control character set to zero is turned OFF, not bound
/// to NUL.
fn enabled(cc: u8) -> Option<u8> {
    (cc != 0).then_some(cc)
}

/// Put `tty` in raw mode, returning the guard that restores it.
///
/// `Err` means the caller should carry on WITHOUT raw mode rather than fail:
/// not every console is a terminal that can do this. Every path out of here
/// after the first `TCSETS` puts the terminal back itself — the guard that would
/// normally do it does not exist yet, so a bare `?` would hand back a half-raw
/// terminal that nothing owns.
pub fn raw(tty: BorrowedFd<'_>, options: RawOptions) -> Result<Raw<'_>, String> {
    let fd = tty.as_raw_fd();
    let mut saved = [0u8; TERMIOS_LEN];
    termios_get(fd, &mut saved)?;
    let want = raw_termios(&saved, options);
    // Restoring on THIS failure too, not just the ones below: a `TCSETS` can
    // succeed having applied only part of what was asked, and a partial apply
    // that then REPORTS failure would leave a half-raw terminal behind on the
    // one exit that did not put it back.
    if let Err(why) = termios_set(fd, &want) {
        let _ = termios_set(fd, &saved);
        return Err(why);
    }
    let mut got = [0u8; TERMIOS_LEN];
    if let Err(why) = termios_get(fd, &mut got) {
        let _ = termios_set(fd, &saved);
        return Err(why);
    }
    if let Some(why) = disagreement(&got, &want) {
        let _ = termios_set(fd, &saved);
        return Err(format!("terminal did not enter raw mode: {why}"));
    }
    Ok(Raw {
        fd: tty,
        saved,
        restored: false,
    })
}

/// The terminal's size as `(columns, rows)`, or `None` when it will not say.
///
/// COLUMNS FIRST, which is the opposite of the struct: `struct winsize` puts
/// `ws_row` first and `ws_col` second. The pair is two `u16` side by side, so
/// reading or returning them the wrong way round is a well-formed answer about
/// the wrong axis — a frame laid out for 24 columns and 80 rows, with every test
/// green — which is why the order is pinned in a test rather than trusted.
pub fn window_size(tty: BorrowedFd<'_>) -> Option<(u16, u16)> {
    let mut buf = [0u8; WINSIZE_LEN];
    winsize(tty.as_raw_fd(), &mut buf).ok()?;
    columns_and_rows(&buf)
}

/// The columns and rows of a `winsize`, or `None` when either is zero.
///
/// Split from `window_size` because it is the part a test without a terminal can
/// reach. A terminal that reports zero is one that does not know; treating that
/// as a size would lay every frame out for an empty screen.
fn columns_and_rows(buf: &[u8; WINSIZE_LEN]) -> Option<(u16, u16)> {
    let rows = read_u16(buf, 0);
    let cols = read_u16(buf, 2);
    (rows != 0 && cols != 0).then_some((cols, rows))
}

/// A little-endian `u32` at `at`, or 0 if the buffer is too short to hold one.
/// The length is a constant, so the fallback is unreachable; it is here because
/// the panicking indexing that would replace it is not allowed in this crate.
fn read_u32(buf: &[u8; TERMIOS_LEN], at: usize) -> u32 {
    let mut value = 0u32;
    for i in (0..4).rev() {
        value = (value << 8) | u32::from(buf.get(at + i).copied().unwrap_or(0));
    }
    value
}

fn write_u32(buf: &mut [u8; TERMIOS_LEN], at: usize, value: u32) {
    for i in 0..4 {
        if let Some(slot) = buf.get_mut(at + i) {
            *slot = ((value >> (8 * i)) & 0xff) as u8;
        }
    }
}

fn set_cc(buf: &mut [u8; TERMIOS_LEN], idx: usize, value: u8) {
    if let Some(slot) = buf.get_mut(CC_AT + idx) {
        *slot = value;
    }
}

fn read_u16(buf: &[u8; WINSIZE_LEN], at: usize) -> u16 {
    let lo = u16::from(buf.get(at).copied().unwrap_or(0));
    let hi = u16::from(buf.get(at + 1).copied().unwrap_or(0));
    (hi << 8) | lo
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use std::io::IsTerminal;
    use std::os::fd::AsFd;

    /// The restore record and a real terminal's mode are both PROCESS-global and
    /// cargo runs these on parallel threads. Not a property of the code under
    /// test — a property of what is being tested.
    static SERIAL: Mutex<()> = Mutex::new(());

    fn serial() -> std::sync::MutexGuard<'static, ()> {
        SERIAL
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// This module's own text. Every scan below is over source TEXT, so
    /// needles that could match the scanning test itself are built with
    /// `concat!` and the scans stop where the test module begins.
    const TERM_SYS: &str = include_str!("term_sys.rs");

    /// The crate root's file name and text: `lib.rs` for a library, else
    /// `main.rs` for a binary. Read from disk rather than included, since
    /// this module is copied whole into crates of either shape.
    fn crate_root() -> (String, String) {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        for name in ["lib.rs", "main.rs"] {
            if let Ok(text) = std::fs::read_to_string(dir.join(name)) {
                return (name.to_string(), text);
            }
        }
        panic!("no src/lib.rs or src/main.rs beside {}", dir.display());
    }

    /// The part of this file the compiler ships — everything before the test
    /// module, whose own text names refused requests and pinned bodies freely.
    fn shipped() -> String {
        code_only(
            TERM_SYS
                .split(concat!("#[cfg(", "test)]"))
                .next()
                .unwrap_or_default(),
        )
    }

    fn squeeze(text: &str) -> String {
        text.chars().filter(|c| !c.is_whitespace()).collect()
    }

    /// Comments out, string literals through untouched. A comment carrying a
    /// declaration's text would otherwise PAY for a match that deleting the
    /// declaration lost — a green tree over a broken rule — and the absence
    /// scans would red on a `// TIOCSTI` in prose. Line comments only, which
    /// `the_module_has_no_block_comments` is what makes complete.
    fn code_only(text: &str) -> String {
        let src: Vec<char> = text.chars().collect();
        let at = |k: usize| src.get(k).copied();
        let mut out = String::with_capacity(text.len());
        let mut i = 0;
        while let Some(c) = at(i) {
            match c {
                '/' if at(i + 1) == Some('/') => {
                    while at(i).is_some_and(|n| n != '\n') {
                        i += 1;
                    }
                }
                '"' => {
                    out.push(c);
                    i += 1;
                    while let Some(ch) = at(i) {
                        out.push(ch);
                        i += 1;
                        if ch == '\\' {
                            if let Some(esc) = at(i) {
                                out.push(esc);
                                i += 1;
                            }
                        } else if ch == '"' {
                            break;
                        }
                    }
                }
                _ => {
                    out.push(c);
                    i += 1;
                }
            }
        }
        out
    }

    /// What `code_only` must and must not remove.
    #[test]
    fn comments_go_and_literals_stay() {
        assert_eq!(code_only("keep // drop\n"), "keep \n");
        let literal = "let s = \"a // b\";";
        assert_eq!(code_only(literal), literal);
        let escaped = "let s = \"a\\\" // b\";";
        assert_eq!(code_only(escaped), escaped);
        assert_eq!(code_only("a\n// gone\nb"), "a\n\nb");
    }

    /// The comment strip is line-based, which is complete only while this file
    /// holds no block comment: one opened and closed between two tokens changes
    /// nothing the compiler sees and would hide a declaration from every scan
    /// here — so the rule is that this file never spells one, rather than that
    /// the strip is clever enough to survive one.
    #[test]
    fn the_module_has_no_block_comments() {
        assert!(
            !TERM_SYS.contains(concat!("/", "*")),
            "term_sys.rs must use line comments only"
        );
    }

    /// The offsets are the whole point of the layout half, so they are pinned as
    /// VALUES rather than left to the functions that use them: a `c_lflag` read
    /// from the wrong word is a well-formed number, and raw mode would then be
    /// refused — or, worse, granted — for reasons nothing at the call site shows.
    #[test]
    fn the_termios_layout_is_pinned() {
        // Four u32 flag words: c_iflag, c_oflag, c_cflag, c_lflag.
        assert_eq!((IFLAG_AT, OFLAG_AT, CFLAG_AT, LFLAG_AT), (0, 4, 8, 12));
        // ... then the one-byte c_line, then NCCS = 19 control characters.
        assert_eq!(LINE_AT, 16);
        assert_eq!(CC_AT, 17);
        assert_eq!(NCCS, 19);
        assert_eq!(TERMIOS_LEN, CC_AT + NCCS);
        assert_eq!(TERMIOS_LEN, 36);
        assert_eq!(WINSIZE_LEN, 8);
    }

    /// `VMIN` and `VTIME` are 6 and 5 — the asm-generic `c_cc` order, which
    /// x86-64 uses and which the POSIX-canonical architectures (alpha, mips,
    /// powerpc, sparc) do NOT: there `VMIN`/`VTIME` are 4 and 5, overlapping
    /// `VEOF`/`VEOL`. Written out here against the kernel's own header values,
    /// because swapping the two is a terminal that waits for six keystrokes or
    /// reads end-of-file at once, and neither says why.
    #[test]
    fn the_control_slot_indices_are_the_asm_generic_ones() {
        assert_eq!(VINTR, 0);
        assert_eq!(VEOF, 4);
        assert_eq!(VTIME, 5);
        assert_eq!(VMIN, 6);
        // They are ADJACENT, which is why the pair is worth pinning at all.
        assert_eq!(VMIN, VTIME + 1);
        // And this module is compiled for the one architecture whose numbers,
        // instruction and slot order are all written down above, which the
        // `compile_error!` at the top is what enforces.
        const { assert!(cfg!(all(target_arch = "x86_64", target_os = "linux"))) };
    }

    /// Every flag bit, by value. A name alone would let the number change under
    /// it, and each of these is one bit away from a neighbour that means
    /// something else entirely — `ICRNL` (0x100) sits beside `IGNCR` (0x80),
    /// `CS8` (0x30) beside `CS6` (0x10).
    #[test]
    fn the_flag_bits_are_pinned_by_value() {
        assert_eq!(
            (BRKINT, INPCK, ISTRIP, ICRNL, IXON),
            (0x2, 0x10, 0x20, 0x100, 0x400)
        );
        assert_eq!(OPOST, 0x1);
        assert_eq!(CS8, 0x30);
        assert_eq!((ISIG, ICANON, ECHO, IEXTEN), (0x1, 0x2, 0x8, 0x8000));
    }

    /// The `ioctl` roster is THREE REQUESTS — not syscalls — pinned by VALUE,
    /// since the in-code roster cannot tell an admitted request from a mistyped
    /// one: `TCSETS` written as 0x5404 is `TCSETSF`, which DISCARDS terminal
    /// input another process may own, and nothing observable at the call site
    /// tells those two apart.
    #[test]
    fn the_ioctl_requests_are_exactly_three_and_value_pinned() {
        const REQUESTS: &[(&str, &str)] = &[
            ("TCGETS", "0x5401"),
            ("TCSETS", "0x5402"),
            ("TIOCGWINSZ", "0x5413"),
        ];
        // Neighbours deliberately outside the surface, each a digit away from
        // something admitted: the two `TCSETS` variants that drain or discard
        // pending I/O, the winsize SETTER, the input injector, and the
        // controlling-terminal call that is td-init's.
        const REFUSED: &[&str] = &["TCSETSW", "TCSETSF", "TIOCSWINSZ", "TIOCSTI", "TIOCSCTTY"];
        let sys = squeeze(&shipped());
        for (name, value) in REQUESTS {
            assert_eq!(
                sys.matches(&format!("const{name}:usize={value};")).count(),
                1,
                "{name} must be declared exactly once as {value}"
            );
            // Three mentions and no more: the declaration, the roster the gate
            // checks against, and the ONE wrapper that issues it. A fourth is a
            // place the pinned value could be shadowed or recomputed.
            assert_eq!(
                sys.matches(name).count(),
                3,
                "{name} must be named exactly three times"
            );
        }
        for refused in REFUSED {
            assert_eq!(
                sys.matches(refused).count(),
                0,
                "{refused} is deliberately outside this crate's ioctl surface"
            );
        }
        // The gate itself, so the roster is code and not only this test.
        assert_eq!(
            sys.matches("if!IOCTL_REQUESTS.contains(&request){").count(),
            1,
            "the one ioctl entry point must refuse an unrostered request"
        );
    }

    /// The roster is enforced at RUNTIME, not only in the text above. Issued
    /// against descriptor -1 so that a gate which had stopped working would
    /// answer EBADF rather than actually resizing somebody's terminal.
    #[test]
    fn an_unrostered_request_is_refused_before_the_syscall() {
        // TIOCSWINSZ (0x5414), TCSETSF (0x5404), TIOCSTI (0x5412): the setter,
        // the flusher, and the input injector.
        for request in [0x5414usize, 0x5404, 0x5412] {
            let refused = ioctl(-1, request, 0).unwrap_err();
            assert!(
                refused.contains("is not tdstd's to issue"),
                "request {request:#x} reached the kernel: {refused}"
            );
        }
        // ... and an admitted one gets past the gate to the kernel, which is
        // what makes the refusals above about the roster rather than about
        // every call failing.
        let admitted = ioctl(-1, TCGETS, 0).unwrap_err();
        assert!(
            admitted.contains("errno 9"),
            "expected EBADF, got {admitted}"
        );
    }

    /// The syscall roster is exactly TWO, pinned by value — td-sh's four minus
    /// `umask(2)` and `rt_sigaction(2)`. The number reaches the kernel as an
    /// argument, so pinning the declarations is only half of it;
    /// `the_raw_syscall_has_one_call_site_per_syscall` pins that each call
    /// passes the named constant rather than a bare literal.
    #[test]
    fn the_syscall_roster_is_exactly_two_and_value_pinned() {
        let decl = concat!("const", "SYS", "_");
        let sys = squeeze(&shipped());
        let mut seen: Vec<String> = Vec::new();
        for (offset, _) in sys.match_indices(decl) {
            let rest = sys.get(offset + decl.len()..).unwrap_or_default();
            let text = rest.split(';').next().unwrap_or_default();
            seen.push(format!("{}{text}", concat!("SYS", "_")));
        }
        assert_eq!(
            seen,
            vec![
                concat!("SYS", "_POLL:usize=7").to_string(),
                concat!("SYS", "_IOCTL:usize=16").to_string(),
            ],
            "x86-64 poll and ioctl, and nothing else"
        );
        // The restriction that makes those two numbers mean anything: a syscall
        // number is a property of one ABI, and a second target would arrive
        // carrying a call this file does not admit -- asm-generic has no
        // `poll(2)` at all -- so the guard is pinned here rather than left to
        // whoever adds the target.
        assert_eq!(
            squeeze(&shipped())
                .matches(concat!(
                    "#[cfg(not(all(target_arch=\"x86_64\",target_os=\"linux\")))]",
                    "compile_",
                    "error!"
                ))
                .count(),
            1,
            "the x86_64-linux guard must stay, immediately before the message"
        );
    }

    /// The assembly body is pinned WHOLE, including which register each argument
    /// lands in and that `options(nomem)` stays absent, which is load-bearing by
    /// its absence: three of the four operations have the kernel write through a
    /// pointer that reaches the asm only as an integer.
    #[test]
    fn the_confined_block_is_pinned_whole() {
        let sys = squeeze(TERM_SYS);
        let body = squeeze(concat!(
            "core::arch::",
            "asm",
            "!(\n",
            "    \"syscall\",\n",
            "    inlateout(\"rax\") n as isize => ret,\n",
            "    in(\"rdi\") a1,\n",
            "    in(\"rsi\") a2,\n",
            "    in(\"rdx\") a3,\n",
            "    in(\"r10\") a4,\n",
            "    out(\"rcx\") _,\n",
            "    out(\"r11\") _,\n",
            "    options(nostack),\n",
            ");"
        ));
        assert!(sys.contains(&body), "the confined assembly body changed");
        // ONE instruction sequence, ONE `unsafe` block and ONE entry point.
        assert_eq!(
            code_only(TERM_SYS)
                .matches(concat!("arch::", "asm", "!"))
                .count(),
            1,
            "one asm site only"
        );
        assert_eq!(
            shipped().matches(concat!("unsafe", " {")).count(),
            1,
            "more than one unsafe block"
        );
        assert_eq!(
            shipped().matches(concat!("fn ", "syscall", "4(")).count(),
            1,
            "more than one raw entry point"
        );
    }

    /// The raw entry point has ONE call site PER SYSCALL. Module privacy stops
    /// another module reaching `syscall4`, but not another wrapper inside this
    /// one, and a second wrapper is a second syscall however safe its signature
    /// looks. Two calls: the one `ioctl` entry point, and `poll_readable`.
    #[test]
    fn the_raw_syscall_has_one_call_site_per_syscall() {
        let code = shipped();
        let name = concat!("syscall", "4");
        assert_eq!(
            code.matches(name).count(),
            3,
            "`{name}` should appear three times: its definition and its two calls"
        );
        // The number reaches the kernel as an ARGUMENT, so pinning the `SYS_*`
        // declarations is not enough on its own: `syscall4(16, ...)` names no
        // constant and would satisfy the roster test above.
        let squeezed = squeeze(&code);
        for number in [concat!("SYS", "_IOCTL,"), concat!("SYS", "_POLL,")] {
            assert_eq!(
                squeezed.matches(number).count(),
                1,
                "`{number}` must be passed by name at exactly one call site"
            );
        }
    }

    /// The lint is named twice in the whole crate: denied at the root, allowed
    /// once here. A third mention is a second surface — and the scan covers
    /// every module beside this one, since a scoped `allow` in any of them would
    /// compile perfectly well under the root's `deny`.
    #[test]
    fn the_unsafe_allowance_is_the_only_one_in_the_crate() {
        let lint = concat!("unsafe", "_code");
        let (root_name, root) = crate_root();
        assert_eq!(
            code_only(&root)
                .matches(&format!("#![deny({lint})]"))
                .count(),
            1,
            "the crate root {root_name} must deny the lint"
        );
        assert_eq!(code_only(&root).matches(lint).count(), 1);
        assert_eq!(
            shipped().matches(&format!("#[allow({lint})]")).count(),
            1,
            "the one allowance must sit on the one entry point"
        );
        assert_eq!(code_only(TERM_SYS).matches(lint).count(), 1);
        // Every other module in the crate, read from disk rather than included:
        // they are owned elsewhere, and a scan that only knew the files this
        // module names could not see one that arrived later.
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut checked = 0usize;
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("")
                .to_string();
            if name == "term_sys.rs" || name == root_name || !name.ends_with(".rs") {
                continue;
            }
            let code = code_only(&std::fs::read_to_string(&path).unwrap());
            // The forms that INTRODUCE unsafe, rather than the bare word: a
            // sibling module is free to hold `unsafe` inside a string literal
            // -- test data, an error message -- and reding on that would be
            // this test lying about somebody else's file.
            for form in [
                lint,
                concat!("unsafe", " {"),
                concat!("unsafe", " fn"),
                concat!("unsafe", " impl"),
                concat!("unsafe", " trait"),
                concat!("unsafe", " extern"),
            ] {
                assert!(
                    !code.contains(form),
                    "{name} carries `{form}`; the crate's one unsafe surface is term_sys.rs"
                );
            }
            checked += 1;
        }
        assert!(checked > 0, "the sibling scan found no modules to check");
    }

    /// `struct pollfd`'s field order, which no observation of the syscall can
    /// check: `events` is `POLLIN`, itself a member of `POLL_READY`, so a
    /// wrapper reading the request back as the answer says "ready" whenever poll
    /// returns at all and agrees with every other test in this file.
    #[test]
    fn the_pollfd_words_put_each_field_where_the_kernel_reads_it() {
        // fd in the first word; events in the LOW half of the second.
        assert_eq!(pollfd(7, POLLIN), [7, 0x0001]);
        assert_eq!(pollfd(0, 0), [0, 0]);
        // revents comes out of the HIGH half, and nothing of `events` leaks in.
        assert_eq!(revents(&[0, 0x0010_0001]), 0x0010);
        assert_eq!(revents(&[0xffff_ffff, 0x0000_ffff]), 0);
        // The four bits that count as ready, each restated by value so a wrong
        // one is caught independently of the declaration, and one that does not:
        // 0x004 is POLLOUT, never requested and never an answer about reading.
        assert_eq!(
            (POLLIN, POLLERR, POLLHUP, POLLNVAL),
            (0x001, 0x008, 0x010, 0x020)
        );
        assert_eq!(POLL_READY, 0x001 | 0x008 | 0x010 | 0x020);
        assert_eq!(POLL_READY & 0x004, 0);
        // One descriptor, and the buffer the kernel is told about is the one it
        // may write through.
        assert_eq!(POLLFD_COUNT, 1);
        assert_eq!(POLLFD_COUNT * POLLFD_WORDS * core::mem::size_of::<u32>(), 8);
    }

    /// `poll_readable` ISSUES the syscall and gets the kernel's answer.
    ///
    /// Every other assertion about the readiness call is over source TEXT, and a
    /// wrapper that returned a plausible `bool` without issuing anything would
    /// satisfy all of them. So: an empty pipe is NOT ready, the same pipe is
    /// ready once a byte is in it, and it stays ready at EOF — which is the
    /// distinction a draw loop turns on and the one a "bytes remain" answer
    /// would get wrong.
    #[test]
    fn poll_answers_about_a_real_descriptor() {
        use std::io::Write as _;

        let (reader, mut writer) = std::io::pipe().unwrap();
        // Nothing written yet: a read would wait. A zero timeout is the whole
        // question, so this also pins that the wrapper does not block on it.
        assert_eq!(poll_readable(reader.as_fd(), 0), Ok(false));
        writer.write_all(b"x").unwrap();
        assert_eq!(poll_readable(reader.as_fd(), 0), Ok(true));
        // ... and with a NONZERO timeout on an already-ready descriptor, which
        // no other assertion here reaches: a wrapper that answered `false` for
        // every positive timeout would satisfy all of them, and that is the
        // shape a hundred-millisecond input tick spends its whole life in.
        let start = std::time::Instant::now();
        assert_eq!(poll_readable(reader.as_fd(), 5_000), Ok(true));
        assert!(
            start.elapsed() < std::time::Duration::from_secs(1),
            "poll waited {:?} for a descriptor that was already ready",
            start.elapsed()
        );
        // EOF is READY, not "no data": the writer is gone, so a read returns 0
        // at once. This is the arm that makes an input loop terminate at all.
        drop(writer);
        assert_eq!(poll_readable(reader.as_fd(), 0), Ok(true));

        // A timeout is honoured rather than ignored: an empty pipe whose writer
        // is still alive must take at least the time asked for. This is the
        // whole of the SIGWINCH replacement — the tick that expires is where the
        // window size gets re-read.
        let (empty, _keep) = std::io::pipe().unwrap();
        let start = std::time::Instant::now();
        assert_eq!(poll_readable(empty.as_fd(), 60), Ok(false));
        assert!(
            start.elapsed() >= std::time::Duration::from_millis(50),
            "poll returned early: {:?}",
            start.elapsed()
        );

        // The argument the wrapper refuses rather than passes on: a negative
        // timeout waits forever, so the failure would not look like one at the
        // call site.
        assert!(poll_readable(empty.as_fd(), -1).is_err());
    }

    /// The word accessors are inverses, and each reads the word it wrote rather
    /// than a neighbouring one — the failure a wrong offset actually produces.
    #[test]
    fn the_flag_word_is_read_and_written_where_it_was_put() {
        let mut buf = [0u8; TERMIOS_LEN];
        write_u32(&mut buf, LFLAG_AT, 0xdead_beef);
        assert_eq!(read_u32(&buf, LFLAG_AT), 0xdead_beef);
        // Little-endian, and confined to its own four bytes.
        assert_eq!(buf.get(LFLAG_AT), Some(&0xef));
        assert_eq!(buf.get(LFLAG_AT + 3), Some(&0xde));
        assert_eq!(read_u32(&buf, CFLAG_AT), 0, "a neighbouring word moved");
        assert_eq!(buf.get(LFLAG_AT + 4), Some(&0), "the write ran over");
    }

    /// Every byte of a synthetic termios, so the arithmetic is checked without a
    /// terminal. `KEYS_BLOCKING` is what tn asks for: ECHO|ICANON|ISIG off,
    /// IXON|ICRNL off, OPOST off, VMIN 1, VTIME 0, and `c_cflag` untouched.
    #[test]
    fn the_keys_blocking_set_clears_what_tn_clears_and_nothing_else() {
        // Bits the patch must NOT touch, chosen next door to ones it does:
        // IGNCR (0x80) beside ICRNL, IXOFF (0x1000) beside IXON, ONLCR (0x4)
        // beside OPOST, ECHOE (0x10) and IEXTEN (0x8000) beside ECHO.
        let mut saved = [0u8; TERMIOS_LEN];
        write_u32(&mut saved, IFLAG_AT, ICRNL | IXON | 0x80 | 0x1000);
        write_u32(&mut saved, OFLAG_AT, OPOST | 0x4);
        write_u32(&mut saved, CFLAG_AT, 0x1234);
        write_u32(&mut saved, LFLAG_AT, ISIG | ICANON | ECHO | 0x10 | IEXTEN);
        set_cc(&mut saved, VINTR, 0x03);
        set_cc(&mut saved, VMIN, 99);
        set_cc(&mut saved, VTIME, 99);

        let want = raw_termios(&saved, RawOptions::KEYS_BLOCKING);
        assert_eq!(read_u32(&want, IFLAG_AT), 0x80 | 0x1000, "IXON|ICRNL only");
        assert_eq!(read_u32(&want, OFLAG_AT), 0x4, "OPOST only");
        assert_eq!(
            read_u32(&want, CFLAG_AT),
            0x1234,
            "c_cflag is not tn's to set"
        );
        assert_eq!(
            read_u32(&want, LFLAG_AT),
            0x10 | IEXTEN,
            "ECHO|ICANON|ISIG only"
        );
        assert_eq!(want.get(CC_AT + VMIN), Some(&1));
        assert_eq!(want.get(CC_AT + VTIME), Some(&0));
        // And not one byte outside the patch: c_line and the other 17 control
        // slots come back exactly as the kernel gave them.
        assert_eq!(want.get(LINE_AT), saved.get(LINE_AT));
        assert_eq!(want.get(CC_AT + VINTR), Some(&0x03));
    }

    /// The same for `BYTES_TENTH`, which is what tmc asks for:
    /// BRKINT|ICRNL|INPCK|ISTRIP|IXON off, OPOST off, ECHO|ICANON|IEXTEN|ISIG
    /// off, CS8 on, VMIN 0, VTIME 1.
    #[test]
    fn the_bytes_tenth_set_is_what_tmc_asks_for() {
        let mut saved = [0u8; TERMIOS_LEN];
        write_u32(
            &mut saved,
            IFLAG_AT,
            BRKINT | ICRNL | INPCK | ISTRIP | IXON | 0x1000,
        );
        write_u32(&mut saved, OFLAG_AT, OPOST | 0x4);
        // CS5 is a zero character size, so this is the case where OR-ing CS8 in
        // has to CHANGE something -- and the baud bits beside it must not move.
        write_u32(&mut saved, CFLAG_AT, 0x000d);
        write_u32(&mut saved, LFLAG_AT, ECHO | ICANON | IEXTEN | ISIG | 0x100);

        let want = raw_termios(&saved, RawOptions::BYTES_TENTH);
        assert_eq!(
            read_u32(&want, IFLAG_AT),
            0x1000,
            "IXOFF is not tmc's to clear"
        );
        assert_eq!(read_u32(&want, OFLAG_AT), 0x4);
        assert_eq!(
            read_u32(&want, CFLAG_AT),
            0x000d | CS8,
            "CS8 set, baud kept"
        );
        assert_eq!(
            read_u32(&want, LFLAG_AT),
            0x100,
            "TOSTOP is not tmc's to clear"
        );
        assert_eq!(want.get(CC_AT + VMIN), Some(&0));
        assert_eq!(want.get(CC_AT + VTIME), Some(&1), "a tenth of a second");
        // The two option sets differ in exactly the bits their documentation
        // claims, which is the only thing that makes offering both worth it.
        let keys = raw_termios(&saved, RawOptions::KEYS_BLOCKING);
        assert_eq!(
            read_u32(&keys, IFLAG_AT) & !read_u32(&want, IFLAG_AT),
            BRKINT | INPCK | ISTRIP
        );
        assert_eq!(
            read_u32(&keys, LFLAG_AT) & !read_u32(&want, LFLAG_AT),
            IEXTEN
        );
        assert_eq!(read_u32(&keys, CFLAG_AT) & CS8, 0);
    }

    /// `disagreement` is what stops a CONSTRUCTED termios passing for the one
    /// the patch computed, so it has to notice a byte the patch never named —
    /// and must not complain about the patch itself.
    #[test]
    fn a_byte_outside_the_patch_is_noticed() {
        let saved = [7u8; TERMIOS_LEN];
        let want = raw_termios(&saved, RawOptions::KEYS_BLOCKING);
        assert_eq!(
            disagreement(&want, &want),
            None,
            "the patch itself is allowed"
        );

        // c_cflag = 0 is B0, a hang-up on a serial console, and c_oflag = 0
        // drops ONLCR for whatever runs next -- the exact case a zeroed buffer
        // would smuggle past a check that only looked at the bits raw mode
        // names, since a zeroed c_lflag has ICANON and ECHO clear.
        let mut zeroed = [0u8; TERMIOS_LEN];
        set_cc(&mut zeroed, VMIN, 1);
        let why = disagreement(&zeroed, &want).unwrap();
        assert!(
            why.contains("c_iflag"),
            "the first disagreement is named: {why}"
        );

        // One stray control byte, in a slot the patch does not own.
        let mut stray = want;
        set_cc(&mut stray, VEOF, 99);
        let why = disagreement(&stray, &want).unwrap();
        assert!(why.contains("slot 4"), "a stray c_cc byte is named: {why}");

        // And c_line, which nothing patches and nothing else would notice.
        let mut line = want;
        if let Some(slot) = line.get_mut(LINE_AT) {
            *slot = 3;
        }
        assert!(disagreement(&line, &want).unwrap().contains("c_line"));

        // The two bytes a partial TCSETS most often leaves behind are named as
        // themselves, because "the terminal kept its own VMIN" is the failure
        // that presents as an application exiting by itself.
        let mut vmin = want;
        set_cc(&mut vmin, VMIN, 0);
        assert!(disagreement(&vmin, &want).unwrap().contains("VMIN"));
    }

    /// The reported pair is COLUMNS then ROWS, out of a struct that stores rows
    /// then columns. Swapping them is a well-formed answer about the wrong axis:
    /// a frame laid out 24 wide and 80 tall, with every other test green.
    #[test]
    fn the_window_size_is_columns_then_rows() {
        let mut buf = [0u8; WINSIZE_LEN];
        // ws_row = 24, ws_col = 80, then the two pixel fields nothing reads.
        if let Some(slot) = buf.get_mut(..2) {
            slot.copy_from_slice(&24u16.to_le_bytes());
        }
        if let Some(slot) = buf.get_mut(2..4) {
            slot.copy_from_slice(&80u16.to_le_bytes());
        }
        if let Some(slot) = buf.get_mut(4..6) {
            slot.copy_from_slice(&640u16.to_le_bytes());
        }
        if let Some(slot) = buf.get_mut(6..8) {
            slot.copy_from_slice(&480u16.to_le_bytes());
        }
        assert_eq!(read_u16(&buf, 0), 24);
        assert_eq!(read_u16(&buf, 2), 80);
        assert_eq!(columns_and_rows(&buf), Some((80, 24)));
        // ... and the high byte is really the high byte.
        let wide = [0u8, 1, 0x2c, 0x01, 0, 0, 0, 0];
        assert_eq!(columns_and_rows(&wide), Some((300, 256)));
        // A terminal that does not know says zero, which is not a size.
        let mut unknown = buf;
        if let Some(slot) = unknown.get_mut(2..4) {
            slot.copy_from_slice(&0u16.to_le_bytes());
        }
        assert_eq!(columns_and_rows(&unknown), None);
    }

    /// The two bytes the DRIVER owns come from the terminal's own `c_cc`, and
    /// they are read from their own slots — reading the wrong one inverts Ctrl-C
    /// and Ctrl-D, which is the pair every full-screen reader is built around.
    #[test]
    fn the_driver_bytes_are_read_from_their_own_slots() {
        let f = std::fs::File::open("/dev/null").unwrap();
        let mut saved = [0u8; TERMIOS_LEN];
        set_cc(&mut saved, VINTR, 0x18); // ^X, as `stty intr '^X'` leaves it
        set_cc(&mut saved, VEOF, 0x02); // ^B, a value neither default could be
        let guard = Raw {
            fd: f.as_fd(),
            saved,
            restored: true,
        };
        assert_eq!(guard.intr(), Some(0x18));
        assert_eq!(guard.eof(), Some(0x02));
        // `_POSIX_VDISABLE` is a zero byte: the character is OFF, not bound to
        // NUL, or `stty intr ''` would make a NUL keystroke quit.
        let guard = Raw {
            fd: f.as_fd(),
            saved: [0u8; TERMIOS_LEN],
            restored: true,
        };
        assert_eq!(guard.intr(), None);
        assert_eq!(guard.eof(), None);
    }

    /// The ioctl is really ISSUED, and really refused for a non-terminal.
    ///
    /// Every other assertion about the layout half is about arithmetic; a
    /// wrapper that returned `Ok(())` without issuing anything would satisfy all
    /// of them. A regular file is not a terminal, so the kernel answers ENOTTY
    /// (errno 25) — which proves the request reached it.
    #[test]
    fn the_ioctl_is_issued_and_the_kernel_answers() {
        let path = std::env::temp_dir().join(format!("tdstd-term-{}", std::process::id()));
        let f = std::fs::File::create(&path).unwrap();
        let why = match raw(f.as_fd(), RawOptions::KEYS_BLOCKING) {
            Ok(_) => panic!("a regular file entered raw mode"),
            Err(why) => why,
        };
        assert!(
            why.contains("25"),
            "expected ENOTTY from a regular file, got {why}"
        );
        assert_eq!(window_size(f.as_fd()), None, "a regular file has no size");
        let _ = std::fs::remove_file(&path);
    }

    /// A failed restore is RECORDED rather than panicked on, and `Drop` is what
    /// issues it. Built over a regular file, whose `TCSETS` answers ENOTTY: the
    /// only way to observe the failing path without a terminal that can be put
    /// in a bad state.
    #[test]
    fn a_failed_restore_is_recorded_and_not_a_panic() {
        let _serial = serial();
        let _ = take_restore_failure();
        let path = std::env::temp_dir().join(format!("tdstd-drop-{}", std::process::id()));
        let f = std::fs::File::create(&path).unwrap();
        {
            let _guard = Raw {
                fd: f.as_fd(),
                saved: [0u8; TERMIOS_LEN],
                restored: false,
            };
        }
        let why = take_restore_failure().unwrap();
        assert!(
            why.contains("25"),
            "the ENOTTY from the restore is recorded: {why}"
        );
        // Taken, not merely read: a second look does not report it again.
        assert_eq!(take_restore_failure(), None);
        // ... and an explicit restore hands the same failure back directly,
        // which is the path a caller that wants to know uses.
        let guard = Raw {
            fd: f.as_fd(),
            saved: [0u8; TERMIOS_LEN],
            restored: false,
        };
        assert!(guard.restore().unwrap_err().contains("25"));
        assert_eq!(
            take_restore_failure(),
            None,
            "an explicit restore records nothing"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// A REAL terminal enters raw mode, reports a size, and is put back exactly
    /// as it was found.
    ///
    /// Skipped when stdin is not a terminal, which is what it is under `cargo
    /// test` in a pipeline and in the build sandbox: this module's other tests
    /// are arithmetic and ENOTTY, so the one thing none of them can reach is a
    /// `TCSETS` the kernel ACCEPTS. Run it from a terminal to exercise that.
    #[test]
    fn a_real_terminal_enters_and_leaves_raw_mode() {
        let _serial = serial();
        let stdin = std::io::stdin();
        if !stdin.is_terminal() {
            eprintln!("skipped: stdin is not a terminal");
            return;
        }
        let fd = stdin.as_fd();
        let mut before = [0u8; TERMIOS_LEN];
        termios_get(fd.as_raw_fd(), &mut before).unwrap();

        for options in [RawOptions::KEYS_BLOCKING, RawOptions::BYTES_TENTH] {
            let guard = raw(fd, options).unwrap();
            // The kernel took it: read back independently of `raw`'s own check.
            let mut during = [0u8; TERMIOS_LEN];
            termios_get(fd.as_raw_fd(), &mut during).unwrap();
            assert_eq!(read_u32(&during, LFLAG_AT) & ECHO, 0, "ECHO survived");
            assert_eq!(read_u32(&during, LFLAG_AT) & ICANON, 0, "ICANON survived");
            assert_eq!(read_u32(&during, OFLAG_AT) & OPOST, 0, "OPOST survived");
            assert_eq!(during.get(CC_AT + VMIN), Some(&options.read.vmin()));
            assert_eq!(during.get(CC_AT + VTIME), Some(&options.read.vtime()));
            // A terminal ANSWERS TIOCGWINSZ, which is the part that is always
            // true. What it answers may still be zeroes -- a pty nobody has
            // sized yet reports 0x0, and `window_size` calls that "does not
            // know" rather than a size -- so the call and the answer are
            // checked separately. When there IS a size, this is the one place
            // the columns-before-rows contract meets a real kernel: the pair
            // must be the winsize fields the other way round.
            let mut buf = [0u8; WINSIZE_LEN];
            winsize(fd.as_raw_fd(), &mut buf).unwrap();
            match window_size(fd) {
                Some((cols, rows)) => {
                    assert!(cols > 0 && rows > 0);
                    assert_eq!(
                        (cols, rows),
                        (read_u16(&buf, 2), read_u16(&buf, 0)),
                        "window_size reported the winsize fields the wrong way round"
                    );
                }
                None => assert!(
                    read_u16(&buf, 0) == 0 || read_u16(&buf, 2) == 0,
                    "a terminal that reported a size was called sizeless"
                ),
            }
            // ... and the explicit restore agrees the terminal came back.
            guard.restore().unwrap();
            let mut after = [0u8; TERMIOS_LEN];
            termios_get(fd.as_raw_fd(), &mut after).unwrap();
            assert_eq!(after, before, "the terminal was not put back as found");
        }
        // Dropping the guard restores just as well as calling `restore`.
        {
            let _guard = raw(fd, RawOptions::KEYS_BLOCKING).unwrap();
        }
        let mut after = [0u8; TERMIOS_LEN];
        termios_get(fd.as_raw_fd(), &mut after).unwrap();
        assert_eq!(after, before, "Drop did not put the terminal back");
        assert_eq!(take_restore_failure(), None);
    }
}

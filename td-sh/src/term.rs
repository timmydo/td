//! Terminal mode and width — the only module that knows what a `termios` byte
//! means.
//!
//! `sys.rs` hands the kernel opaque buffers; the field offsets live HERE, next to
//! the readback that checks them, for the reason UNSAFE.md gives for preferring
//! `LOOP_SET_FD` over `LOOP_CONFIGURE`: a field at the wrong offset is a kernel
//! operation invisible at the call site. Copied from `td-util/src/term.rs`, which
//! took this surface first for its pager; the two are the same three requests and
//! the same argument for each.
//!
//! Two things keep it honest. First, a termios is never CONSTRUCTED here — the
//! kernel's own bytes are read, two known offsets are patched, and the untouched
//! original is what gets written back on restore, so a mistake cannot invent a
//! line discipline out of nothing. Second, raw mode is read back and REFUSED
//! unless the kernel agrees the bits actually cleared; nothing else would notice,
//! because a terminal still in canonical mode looks exactly like one that is
//! waiting for a slow typist.

use std::os::fd::{AsRawFd, BorrowedFd};

use crate::sys;

/// `c_lflag`, the fourth of the four leading `u32` flag words.
const LFLAG_AT: usize = 12;
/// `c_cc` follows the flags and the one-byte `c_line`.
const CC_AT: usize = 17;
/// Indices into `c_cc`.
const VINTR: usize = 0;
const VEOF: usize = 4;
const VTIME: usize = 5;
const VMIN: usize = 6;

/// The three bits raw mode clears: line-at-a-time assembly, echoing what the
/// reader typed, and signal generation. The editor draws every character itself,
/// so leaving ECHO on would print each one twice.
///
/// `ISIG` is where this differs from `td-util`'s pager, which keeps it so Ctrl-C
/// can kill a pager stuck on a huge file. A SHELL wants the opposite, and for a
/// reason particular to this one: td-sh installs no signal handler, so `SIG_DFL`
/// for SIGINT would END THE SHELL at its own prompt. With signal generation off,
/// Ctrl-C arrives as byte 0x03 and the editor abandons the line instead. The
/// trade is real and bounded: Ctrl-C, Ctrl-\ and Ctrl-Z send nothing WHILE A
/// LINE IS BEING EDITED, which is the only time this mode is in force — the
/// guard is dropped before any command runs, so a child still gets all three.
///
/// Nothing else is touched. In particular the OUTPUT flags are left alone, so
/// ONLCR still turns the editor's `\n` into a carriage return and line feed.
const ISIG: u32 = 0x0000_0001;
const ICANON: u32 = 0x0000_0002;
const ECHO: u32 = 0x0000_0008;

/// A terminal switched to raw mode, and the bytes to put back.
///
/// Restoring in `Drop` is not tidiness: every exit path from the editor — Enter,
/// EOF, a write error, a `?` three frames down — leaves the terminal in whatever
/// mode it was last set to, and a shell prompt on a terminal with no echo and no
/// line assembly is a machine the operator has to guess how to recover.
///
/// The descriptor is BORROWED, not a bare `RawFd`. This type issues a syscall on
/// a descriptor it does not own, from a `Drop` that runs whenever the guard goes
/// out of scope — so if the terminal could be closed first, the restore would go
/// to a closed or, worse, a RECYCLED descriptor. `BorrowedFd` makes the borrow
/// checker refuse that whole family at compile time.
pub struct Raw<'a> {
    fd: BorrowedFd<'a>,
    saved: [u8; sys::TERMIOS_LEN],
}

impl Raw<'_> {
    /// The byte the DRIVER would have turned into SIGINT, and the one it would
    /// have turned into end-of-input. With `ISIG`/`ICANON` cleared the kernel
    /// does neither, so the editor has to recognise them itself — and it must
    /// use the terminal's own settings rather than 0x03/0x04, or `stty intr '^X'`
    /// would leave the operator with no way to abandon a line.
    ///
    /// `None` when the terminal has the character DISABLED, which POSIX spells
    /// `_POSIX_VDISABLE` and Linux gives as a zero byte. Handing that back as an
    /// ordinary binding would make a NUL keystroke — which `stty intr ''` was
    /// asking NOT to be one — abandon the line or end the session.
    pub fn intr(&self) -> Option<u8> {
        enabled(self.saved.get(CC_AT + VINTR).copied().unwrap_or(0x03))
    }

    pub fn eof(&self) -> Option<u8> {
        enabled(self.saved.get(CC_AT + VEOF).copied().unwrap_or(0x04))
    }
}

/// `_POSIX_VDISABLE`: a control character set to zero is turned OFF, not bound
/// to NUL.
fn enabled(cc: u8) -> Option<u8> {
    (cc != 0).then_some(cc)
}

impl Drop for Raw<'_> {
    fn drop(&mut self) {
        let _ = sys::termios_set(self.fd.as_raw_fd(), &self.saved);
    }
}

/// Put `fd` in raw mode, returning the guard that restores it.
///
/// `Err` means the caller should carry on WITHOUT raw mode rather than fail: not
/// every console is a terminal that can do this, and a shell that refuses to read
/// is worse than one with no line editing.
///
/// Only `c_lflag` is touched. `IXON` in particular stays set, so Ctrl-S still
/// stops output and Ctrl-Q resumes it, and the editor cannot notice either —
/// the same trade td-util's pager makes, and the reason a frozen prompt is a
/// terminal question rather than a shell bug. Clearing it would be a second
/// flag word patched for a key nothing here binds.
pub fn raw(tty: BorrowedFd<'_>) -> Result<Raw<'_>, String> {
    let fd = tty.as_raw_fd();
    let mut saved = [0u8; sys::TERMIOS_LEN];
    sys::termios_get(fd, &mut saved)?;
    let mut want = saved;
    let lflag = raw_lflag(read_u32(&want, LFLAG_AT));
    write_u32(&mut want, LFLAG_AT, lflag);
    // One byte satisfies a read, and no inter-byte timer: the editor blocks until
    // the operator presses something, which is exactly the wait it wants.
    set_cc(&mut want, VMIN, 1);
    set_cc(&mut want, VTIME, 0);
    // Restoring on THIS failure too, not just the ones below: the module's own
    // argument is that a `TCSETS` can succeed having applied only part of what
    // was asked, and a partial apply that then REPORTS failure would leave a
    // half-raw terminal behind on the one exit that did not put it back.
    if let Err(e) = sys::termios_set(fd, &want) {
        let _ = sys::termios_set(fd, &saved);
        return Err(e);
    }
    // ...and REFUSE unless the kernel agrees. A `TCSETS` can succeed having
    // applied only part of what was asked, and a terminal still in canonical mode
    // is indistinguishable from one whose reader has not typed yet.
    //
    // Every path out of here from this point on must PUT THE TERMINAL BACK: the
    // guard that would normally do it does not exist yet, and the caller treats
    // an `Err` as "carry on without raw mode" — so a bare `?` would hand back a
    // half-raw terminal that nothing owns.
    let mut got = [0u8; sys::TERMIOS_LEN];
    if let Err(e) = sys::termios_get(fd, &mut got) {
        let _ = sys::termios_set(fd, &saved);
        return Err(e);
    }
    // The WHOLE word, not just the three bits: `only_the_patch_changed` below
    // exempts these four bytes, so checking only that the three cleared would
    // let every OTHER local flag move unnoticed -- and a ZEROED c_lflag has the
    // three clear, so it would pass while IEXTEN, ECHOE and TOSTOP silently
    // went with it.
    if read_u32(&got, LFLAG_AT) != lflag {
        let _ = sys::termios_set(fd, &saved);
        return Err("terminal did not enter raw mode".to_string());
    }
    // The control bytes too, not just the flags. `TCSETS` applying ICANON/ECHO
    // but not these leaves a terminal that passes the check above while a read
    // waits for several keystrokes, or times out and reads EOF — which the editor
    // treats as end of input, so the failure looks like a shell that exits by
    // itself.
    if got.get(CC_AT + VMIN) != Some(&1) || got.get(CC_AT + VTIME) != Some(&0) {
        let _ = sys::termios_set(fd, &saved);
        return Err("terminal kept its own VMIN/VTIME".to_string());
    }
    // ...and NOTHING else moved. This is what makes "a termios is never
    // constructed here" a property rather than a claim: hand the kernel a zeroed
    // buffer instead of the patched original and every check above still passes —
    // a zeroed c_lflag has ICANON and ECHO clear — while c_cflag = 0 is B0, which
    // on a serial console is a hang-up, and c_oflag = 0 drops ONLCR so every line
    // staircases. Comparing the untouched bytes catches it, and can only ever
    // fire on bytes this function handed back verbatim.
    if !only_the_patch_changed(&got, &saved) {
        let _ = sys::termios_set(fd, &saved);
        return Err("terminal changed more than raw mode asked for".to_string());
    }
    Ok(Raw { fd: tty, saved })
}

/// The terminal's width in columns, or `None` when it will not say.
///
/// Only the columns: the editor keeps its line inside ONE row and scrolls it
/// horizontally, so it never needs to know how many rows there are. That is also
/// why a wrong answer here is survivable — the line scrolls at the wrong point
/// rather than the redraw losing track of where the cursor is.
pub fn width(tty: BorrowedFd<'_>) -> Option<u16> {
    let mut buf = [0u8; sys::WINSIZE_LEN];
    sys::window_size(tty.as_raw_fd(), &mut buf).ok()?;
    columns_of(&buf)
}

/// The columns field of a `winsize`, or `None` when the terminal will not say.
///
/// Split from `width` because it is the part a test without a terminal can
/// reach, and the part where being wrong is silent: rows and columns are two
/// `u16` side by side, so reading the wrong one is a well-formed answer about
/// the wrong axis.
fn columns_of(buf: &[u8; sys::WINSIZE_LEN]) -> Option<u16> {
    let cols = read_u16(buf, 2);
    // A terminal that reports zero is one that does not know; treating it as a
    // width would make every line scroll immediately.
    (cols != 0).then_some(cols)
}

/// The local-mode word raw mode asks for: signal generation, canonical input and
/// echo all OFF, every other bit as the terminal had it.
///
/// Split from `raw` because clearing `ISIG` is the whole point of the editor —
/// with it set, Ctrl-C at td-sh's own prompt ends the session — and `raw`'s
/// readback compares the kernel against this same computation, so it stays
/// self-consistent whichever bits are named here.
fn raw_lflag(current: u32) -> u32 {
    current & !(ISIG | ICANON | ECHO)
}

/// Whether `got` differs from `saved` ONLY where the patch was applied.
fn only_the_patch_changed(
    got: &[u8; sys::TERMIOS_LEN],
    saved: &[u8; sys::TERMIOS_LEN],
) -> bool {
    for i in 0..sys::TERMIOS_LEN {
        let patched =
            (LFLAG_AT..LFLAG_AT + 4).contains(&i) || i == CC_AT + VMIN || i == CC_AT + VTIME;
        if !patched && got.get(i) != saved.get(i) {
            return false;
        }
    }
    true
}

/// A little-endian `u32` at `at`, or 0 if the buffer is too short to hold one.
/// The length is a constant, so the fallback is unreachable; it is here because
/// AGENTS.md forbids the indexing that would make it a panic instead.
fn read_u32(buf: &[u8; sys::TERMIOS_LEN], at: usize) -> u32 {
    let mut v = 0u32;
    for i in (0..4).rev() {
        v = (v << 8) | u32::from(buf.get(at + i).copied().unwrap_or(0));
    }
    v
}

fn write_u32(buf: &mut [u8; sys::TERMIOS_LEN], at: usize, v: u32) {
    for i in 0..4 {
        if let Some(slot) = buf.get_mut(at + i) {
            *slot = ((v >> (8 * i)) & 0xff) as u8;
        }
    }
}

fn set_cc(buf: &mut [u8; sys::TERMIOS_LEN], idx: usize, v: u8) {
    if let Some(slot) = buf.get_mut(CC_AT + idx) {
        *slot = v;
    }
}

fn read_u16(buf: &[u8; sys::WINSIZE_LEN], at: usize) -> u16 {
    let lo = u16::from(buf.get(at).copied().unwrap_or(0));
    let hi = u16::from(buf.get(at + 1).copied().unwrap_or(0));
    (hi << 8) | lo
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    /// The offsets are the whole point of this module, so they are pinned as
    /// VALUES rather than left to the two functions that use them: a `c_lflag`
    /// read from the wrong word is a well-formed number, and raw mode would then
    /// be refused (or, worse, granted) for reasons nothing at the call site shows.
    #[test]
    fn the_termios_layout_is_pinned() {
        // Four u32 flag words: c_iflag, c_oflag, c_cflag, c_lflag.
        assert_eq!(LFLAG_AT, 12);
        // ... then the one-byte c_line, then NCCS = 19 control characters.
        assert_eq!(CC_AT, 17);
        assert_eq!(sys::TERMIOS_LEN, CC_AT + 19);
        assert_eq!(sys::WINSIZE_LEN, 8);
        // The three bits raw mode clears, and no others.
        assert_eq!(ISIG, 0x1);
        assert_eq!(ICANON, 0x2);
        assert_eq!(ECHO, 0x8);
    }

    /// The word accessors are inverses, and each reads the word it wrote rather
    /// than a neighbouring one — the failure a wrong offset actually produces.
    #[test]
    fn the_flag_word_is_read_and_written_where_it_was_put() {
        let mut buf = [0u8; sys::TERMIOS_LEN];
        write_u32(&mut buf, LFLAG_AT, 0xdead_beef);
        assert_eq!(read_u32(&buf, LFLAG_AT), 0xdead_beef);
        // Little-endian, and confined to its own four bytes.
        assert_eq!(buf.get(LFLAG_AT), Some(&0xef));
        assert_eq!(buf.get(LFLAG_AT + 3), Some(&0xde));
        assert_eq!(read_u32(&buf, 0), 0, "a neighbouring word moved");
        assert_eq!(buf.get(LFLAG_AT + 4), Some(&0), "the write ran over");
    }

    /// `only_the_patch_changed` is what stops a CONSTRUCTED termios passing the
    /// flag and control-byte checks, so it has to notice a byte outside the patch
    /// — and must not complain about one inside it.
    #[test]
    fn a_byte_outside_the_patch_is_noticed() {
        let saved = [7u8; sys::TERMIOS_LEN];
        assert!(only_the_patch_changed(&saved, &saved));

        let mut patched = saved;
        write_u32(&mut patched, LFLAG_AT, 0);
        set_cc(&mut patched, VMIN, 1);
        set_cc(&mut patched, VTIME, 0);
        assert!(only_the_patch_changed(&patched, &saved), "the patch itself is allowed");

        // c_cflag = 0 is B0, a hang-up on a serial console -- the exact case a
        // zeroed buffer would smuggle past the flag check.
        let mut zeroed = [0u8; sys::TERMIOS_LEN];
        set_cc(&mut zeroed, VMIN, 1);
        assert!(!only_the_patch_changed(&zeroed, &saved));

        // ... and one stray byte anywhere else in c_cc.
        let mut stray = patched;
        if let Some(slot) = stray.get_mut(CC_AT + VMIN + 1) {
            *slot = 99;
        }
        assert!(!only_the_patch_changed(&stray, &saved));
    }

    /// The control bytes the DRIVER owns are read from the saved termios, not
    /// assumed: `stty intr '^X'` moves the one that abandons a line.
    #[test]
    fn the_driver_control_bytes_come_from_the_terminal() {
        assert_eq!(VINTR, 0);
        assert_eq!(VEOF, 4);
        let mut saved = [0u8; sys::TERMIOS_LEN];
        set_cc(&mut saved, VINTR, 0x18);
        set_cc(&mut saved, VEOF, 0x04);
        // `Raw` cannot be built without a terminal, so the accessors are checked
        // through the same offsets they read.
        assert_eq!(saved.get(CC_AT + VINTR), Some(&0x18));
        assert_eq!(saved.get(CC_AT + VEOF), Some(&0x04));
    }

    /// The window-size reader takes COLUMNS, the second `u16`, not the rows.
    /// Swapping the pair is a well-formed number and a line that scrolls at the
    /// wrong place, so the offset is asserted rather than trusted.
    #[test]
    fn the_width_is_the_second_field_of_winsize() {
        // rows = 24, columns = 80, both little-endian.
        let buf = [24u8, 0, 80, 0, 0, 0, 0, 0];
        assert_eq!(read_u16(&buf, 0), 24);
        assert_eq!(read_u16(&buf, 2), 80);
        // ... and the high byte is really the high byte.
        let wide = [0u8, 0, 0x2c, 0x01, 0, 0, 0, 0];
        assert_eq!(read_u16(&wide, 2), 300);
    }

    /// The syscall is really ISSUED, and really refused for a non-terminal.
    ///
    /// Every other assertion here is about arithmetic; a wrapper that returned
    /// `Ok(())` without issuing anything would satisfy all of them. A regular
    /// file is not a terminal, so the kernel answers ENOTTY (errno 25) — which
    /// proves the request reached it.
    #[test]
    fn the_ioctl_is_issued_and_the_kernel_answers() {
        use std::os::fd::AsFd;
        let path =
            std::env::temp_dir().join(format!("td-sh-term-{}", std::process::id()));
        let f = std::fs::File::create(&path).unwrap();
        let err = match raw(f.as_fd()) {
            Ok(_) => panic!("a regular file entered raw mode"),
            Err(e) => e,
        };
        assert!(err.contains("25"), "expected ENOTTY from a regular file, got {err}");
        assert_eq!(width(f.as_fd()), None, "a regular file has no width");
        let _ = std::fs::remove_file(&path);
    }

    /// Raw mode clears signal generation as well as canonical input and echo.
    ///
    /// `ISIG` is the one the editor exists for -- with it set, Ctrl-C at td-sh's
    /// own prompt ends the session -- and `raw`'s readback compares the kernel
    /// against its own computation, so it stays green whichever bits are named
    /// there. This is what pins WHICH.
    #[test]
    fn raw_mode_turns_off_signals_canonical_input_and_echo() {
        // Bits raw mode must NOT touch, as literals: naming them as constants
        // would put three more `c_lflag` bits in the module that only a test
        // reads. ECHOE, TOSTOP, IEXTEN.
        let others = 0x10 | 0x100 | 0x8000;
        let got = raw_lflag(ISIG | ICANON | ECHO | others);
        assert_eq!(got & ISIG, 0, "ISIG survived: Ctrl-C would kill the shell");
        assert_eq!(got & ICANON, 0, "ICANON survived");
        assert_eq!(got & ECHO, 0, "ECHO survived");
        assert_eq!(got, others, "raw mode moved a bit it does not own");
        // Nothing is SET, only cleared: the terminal's own flags come back out.
        assert_eq!(raw_lflag(0), 0);
    }

    /// The width is the SECOND `u16` of a `winsize`. Rows and columns sit side by
    /// side, so reading the wrong one is a well-formed answer about the wrong
    /// axis -- and a shell that lays its line out for the row count looks broken
    /// with every test green.
    #[test]
    fn the_width_comes_from_the_columns_field() {
        let mut buf = [0u8; sys::WINSIZE_LEN];
        // rows = 24, columns = 80, then the two pixel fields td ignores.
        buf.get_mut(..2).map(|s| s.copy_from_slice(&24u16.to_le_bytes()));
        buf.get_mut(2..4).map(|s| s.copy_from_slice(&80u16.to_le_bytes()));
        buf.get_mut(4..6).map(|s| s.copy_from_slice(&640u16.to_le_bytes()));
        buf.get_mut(6..8).map(|s| s.copy_from_slice(&480u16.to_le_bytes()));
        assert_eq!(columns_of(&buf), Some(80));
        // A terminal that does not know its width says zero, which is not one.
        let mut unknown = buf;
        unknown.get_mut(2..4).map(|s| s.copy_from_slice(&0u16.to_le_bytes()));
        assert_eq!(columns_of(&unknown), None);
    }

    /// The two bytes the DRIVER owns come from the terminal's own `c_cc`, and
    /// they are two ADJACENT slots -- so reading the wrong one inverts Ctrl-C and
    /// Ctrl-D, which is the pair the editor is built around.
    #[test]
    fn the_driver_bytes_are_read_from_their_own_slots() {
        let mut saved = [0u8; sys::TERMIOS_LEN];
        set_cc(&mut saved, VINTR, 0x18); // ^X, as `stty intr '^X'` leaves it
        set_cc(&mut saved, VEOF, 0x02); // ^B, a value neither default could be
        use std::os::fd::AsFd;
        let f = std::fs::File::open("/dev/null").unwrap();
        let raw = Raw { fd: f.as_fd(), saved };
        assert_eq!(raw.intr(), Some(0x18));
        assert_eq!(raw.eof(), Some(0x02));
        // `_POSIX_VDISABLE` is a zero byte: the character is OFF, not bound to
        // NUL, or `stty intr ''` would make a NUL keystroke abandon the line.
        let mut off = [0u8; sys::TERMIOS_LEN];
        set_cc(&mut off, VINTR, 0);
        set_cc(&mut off, VEOF, 0);
        let raw = Raw { fd: f.as_fd(), saved: off };
        assert_eq!(raw.intr(), None);
        assert_eq!(raw.eof(), None);
    }
}

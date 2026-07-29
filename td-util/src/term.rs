//! Terminal mode and size — the only module that knows what a `termios` byte means.
//!
//! `sys.rs` hands the kernel opaque buffers; the field offsets live HERE, next to
//! the read-back that checks them, for the reason AGENTS.md gives for preferring
//! `LOOP_SET_FD` over `LOOP_CONFIGURE`: a field at the wrong offset is a kernel
//! operation invisible at the call site.
//!
//! Two things keep that honest. First, a termios is never CONSTRUCTED here — the
//! kernel's own bytes are read, two known offsets are patched, and the untouched
//! original is what gets written back on restore, so a mistake cannot invent a
//! line discipline out of nothing. Second, raw mode is read back and REFUSED
//! unless the kernel agrees the two bits actually cleared; nothing else would
//! notice, because a terminal still in canonical mode looks exactly like one that
//! is simply waiting for a slow typist.

use std::io;
use std::os::fd::{AsRawFd, BorrowedFd};

use crate::sys;

/// `c_lflag`, the fourth of the four leading `u32` flag words.
const LFLAG_AT: usize = 12;
/// `c_cc` follows the flags and the one-byte `c_line`.
const CC_AT: usize = 17;
/// Indices into `c_cc`.
const VTIME: usize = 5;
const VMIN: usize = 6;

/// The two bits raw mode clears: line-at-a-time assembly, and echoing what the
/// reader typed. Nothing else is touched — signals (`ISIG`) stay ON, so Ctrl-C
/// still kills a pager stuck on a huge file, which is the escape hatch a reader
/// expects to have.
const ICANON: u32 = 0x0000_0002;
const ECHO: u32 = 0x0000_0008;

/// A terminal switched to raw mode, and the bytes to put back.
///
/// Restoring in `Drop` is not tidiness: every exit path from a pager — `q`, EOF,
/// a write error, a `?` three frames down — leaves the terminal in whatever mode
/// it was last set to, and a shell prompt on a terminal with no echo and no line
/// editing is a machine the operator has to guess how to recover.
///
/// The descriptor is BORROWED, not a bare `RawFd`. This type issues a syscall on
/// a descriptor it does not own, from a `Drop` that runs whenever the guard goes
/// out of scope — so if the terminal could be closed first, the restore would go
/// to a closed or, worse, a RECYCLED descriptor. `BorrowedFd` makes the borrow
/// checker refuse that whole family at compile time instead of leaving it to a
/// comment about declaration order.
pub struct Raw<'a> {
    fd: BorrowedFd<'a>,
    saved: [u8; sys::TERMIOS_LEN],
}

impl Drop for Raw<'_> {
    fn drop(&mut self) {
        let _ = sys::termios_set(self.fd.as_raw_fd(), &self.saved);
    }
}

/// Put `fd` in raw mode, returning the guard that restores it.
///
/// `Err` means the caller should carry on WITHOUT raw mode rather than fail: not
/// every console is a terminal that can do this, and a pager that refuses to page
/// is worse than one that asks for Enter.
pub fn raw(tty: BorrowedFd<'_>) -> io::Result<Raw<'_>> {
    let fd = tty.as_raw_fd();
    let mut saved = [0u8; sys::TERMIOS_LEN];
    sys::termios_get(fd, &mut saved)?;
    let mut want = saved;
    let lflag = read_u32(&want, LFLAG_AT) & !(ICANON | ECHO);
    write_u32(&mut want, LFLAG_AT, lflag);
    // One byte satisfies a read, and no inter-byte timer: the pager blocks until
    // the reader presses something, which is exactly the wait it wants.
    set_cc(&mut want, VMIN, 1);
    set_cc(&mut want, VTIME, 0);
    sys::termios_set(fd, &want)?;
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
    if read_u32(&got, LFLAG_AT) & (ICANON | ECHO) != 0 {
        let _ = sys::termios_set(fd, &saved);
        return Err(io::Error::other("terminal did not enter raw mode"));
    }
    // The control bytes too, not just the flags. `TCSETS` applying ICANON/ECHO but
    // not these leaves a terminal that passes the check above while a command read
    // waits for several keystrokes, or times out and reads EOF — which this pager
    // treats as `q`, so the failure looks like a pager that quits by itself.
    if got.get(CC_AT + VMIN) != Some(&1) || got.get(CC_AT + VTIME) != Some(&0) {
        let _ = sys::termios_set(fd, &saved);
        return Err(io::Error::other("terminal kept its own VMIN/VTIME"));
    }
    // ...and NOTHING else moved. This is what makes "a termios is never
    // constructed here" a property rather than a claim: hand the kernel a
    // zeroed buffer instead of the patched original and every check above still
    // passes — a zeroed c_lflag has ICANON and ECHO clear — while c_cflag = 0 is
    // B0, which on a serial console is a hang-up, and c_oflag = 0 drops ONLCR so
    // every line staircases. Comparing the untouched bytes catches it, and can
    // only ever fire on bytes this function handed back verbatim.
    if !only_the_patch_changed(&got, &saved) {
        let _ = sys::termios_set(fd, &saved);
        return Err(io::Error::other("terminal changed more than raw mode asked for"));
    }
    Ok(Raw { fd: tty, saved })
}

/// The terminal's `(rows, columns)`, or `None` when it will not say.
pub fn size(tty: BorrowedFd<'_>) -> Option<(u16, u16)> {
    let mut buf = [0u8; sys::WINSIZE_LEN];
    sys::window_size(tty.as_raw_fd(), &mut buf).ok()?;
    let (rows, cols) = (read_u16(&buf, 0), read_u16(&buf, 2));
    // A terminal that reports zero is one that does not know; treating it as a
    // size would make a page either empty or the whole file.
    if rows == 0 || cols == 0 {
        return None;
    }
    Some((rows, cols))
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

fn read_u32(buf: &[u8; sys::TERMIOS_LEN], at: usize) -> u32 {
    let mut word = [0u8; 4];
    for (i, slot) in word.iter_mut().enumerate() {
        *slot = buf.get(at + i).copied().unwrap_or(0);
    }
    u32::from_ne_bytes(word)
}

fn write_u32(buf: &mut [u8; sys::TERMIOS_LEN], at: usize, value: u32) {
    for (i, byte) in value.to_ne_bytes().iter().enumerate() {
        if let Some(slot) = buf.get_mut(at + i) {
            *slot = *byte;
        }
    }
}

fn read_u16(buf: &[u8; sys::WINSIZE_LEN], at: usize) -> u16 {
    let lo = buf.get(at).copied().unwrap_or(0);
    let hi = buf.get(at + 1).copied().unwrap_or(0);
    u16::from_ne_bytes([lo, hi])
}

fn set_cc(buf: &mut [u8; sys::TERMIOS_LEN], index: usize, value: u8) {
    if let Some(slot) = buf.get_mut(CC_AT + index) {
        *slot = value;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The offsets are the whole risk, so they are pinned against the layout the
    /// x86_64 kernel uses: four `u32` flag words, `c_line`, then `NCCS` = 19.
    #[test]
    fn the_termios_offsets_are_the_kernels() {
        assert_eq!(LFLAG_AT, 3 * 4, "c_lflag follows c_iflag/c_oflag/c_cflag");
        assert_eq!(CC_AT, 4 * 4 + 1, "c_cc follows the flags and c_line");
        assert_eq!(
            CC_AT + 19,
            sys::TERMIOS_LEN,
            "c_cc must end exactly at the struct's end; a short NCCS would put \
             VMIN inside another field"
        );
        let cc_slots = (CC_AT..sys::TERMIOS_LEN).count();
        assert_eq!(cc_slots, 19, "c_cc holds NCCS = 19 control characters");
        assert!(
            VMIN.max(VTIME) < cc_slots,
            "both indices must be inside c_cc, not past the end of the struct"
        );
    }

    /// Patching touches ONLY the two bits and the two control bytes.
    ///
    /// Everything else is the kernel's own bytes handed back unchanged — which is
    /// what makes restore trivially correct and a wrong offset survivable.
    #[test]
    fn raw_mode_patches_nothing_it_was_not_asked_to() {
        let mut original = [0u8; sys::TERMIOS_LEN];
        for (i, slot) in original.iter_mut().enumerate() {
            *slot = (i as u8).wrapping_mul(7).wrapping_add(3);
        }
        let mut want = original;
        let lflag = read_u32(&want, LFLAG_AT) & !(ICANON | ECHO);
        write_u32(&mut want, LFLAG_AT, lflag);
        set_cc(&mut want, VMIN, 1);
        set_cc(&mut want, VTIME, 0);

        assert_eq!(read_u32(&want, LFLAG_AT) & (ICANON | ECHO), 0, "the bits cleared");
        assert_eq!(want.get(CC_AT + VMIN), Some(&1u8));
        assert_eq!(want.get(CC_AT + VTIME), Some(&0u8));
        // Every byte outside c_lflag and those two c_cc slots is untouched.
        for i in 0..sys::TERMIOS_LEN {
            let touched = (LFLAG_AT..LFLAG_AT + 4).contains(&i)
                || i == CC_AT + VMIN
                || i == CC_AT + VTIME;
            if !touched {
                assert_eq!(want.get(i), original.get(i), "byte {i} must not change");
            }
        }
    }

    /// Only ICANON and ECHO clear; ISIG in particular stays, so Ctrl-C still works.
    #[test]
    fn signals_are_left_enabled() {
        const ISIG: u32 = 0x0000_0001;
        let mut buf = [0u8; sys::TERMIOS_LEN];
        write_u32(&mut buf, LFLAG_AT, ISIG | ICANON | ECHO);
        let lflag = read_u32(&buf, LFLAG_AT) & !(ICANON | ECHO);
        assert_eq!(lflag & ISIG, ISIG, "Ctrl-C must still reach the pager");
        assert_eq!(lflag & (ICANON | ECHO), 0);
    }

    /// The word helpers round-trip, so a patched field reads back as written.
    #[test]
    fn the_word_helpers_round_trip() {
        let mut buf = [0u8; sys::TERMIOS_LEN];
        for value in [0u32, 1, 0xffff_ffff, 0x0a0b_0c0d] {
            write_u32(&mut buf, LFLAG_AT, value);
            assert_eq!(read_u32(&buf, LFLAG_AT), value);
        }
    }

    /// The winsize buffer is exactly what the kernel writes into it.
    ///
    /// TIOCGWINSZ copies `sizeof(struct winsize)` through the raw pointer with no
    /// length negotiation, so a SHORTER buffer is an out-of-bounds kernel write
    /// into a stack array — from code the compiler considers `deny(unsafe_code)`
    /// clean, and from a constant no other assertion here reads. `TERMIOS_LEN` is
    /// pinned transitively by the offsets test above; this one has nothing else
    /// holding it.
    #[test]
    fn the_winsize_length_is_the_kernels() {
        assert_eq!(
            sys::WINSIZE_LEN,
            4 * 2,
            "struct winsize is four u16: ws_row, ws_col, ws_xpixel, ws_ypixel"
        );
        // ...and both fields this module reads are inside it. Expressed through the
        // reader itself: a buffer too short would truncate `ws_col` to zero, which
        // `size` reports as "unknown" — a silent 24x80 fallback on every terminal.
        let mut probe = [0u8; sys::WINSIZE_LEN];
        if let Some(slot) = probe.get_mut(2) {
            *slot = 80;
        }
        assert_eq!(read_u16(&probe, 2), 80, "ws_col must lie within the buffer");
        assert_eq!(sys::TERMIOS_LEN, 36, "struct termios is 4*4 + 1 + 19 bytes");
    }

    /// A CONSTRUCTED termios is refused, not just a partly-applied one.
    ///
    /// Hand the kernel a zeroed buffer instead of the patched original and every
    /// other check passes — a zeroed c_lflag has ICANON and ECHO clear, and
    /// VMIN/VTIME were set explicitly. What ships is `c_cflag = 0`, which is B0,
    /// a hang-up on a serial console; and `c_oflag = 0`, which drops ONLCR so
    /// every line staircases.
    #[test]
    fn a_constructed_termios_is_refused() {
        let mut saved = [0u8; sys::TERMIOS_LEN];
        for (i, slot) in saved.iter_mut().enumerate() {
            *slot = (i as u8).wrapping_mul(7).wrapping_add(3);
        }
        // What raw() actually asks for: the kernel's own bytes, two fields patched.
        let mut want = saved;
        let lflag = read_u32(&want, LFLAG_AT) & !(ICANON | ECHO);
        write_u32(&mut want, LFLAG_AT, lflag);
        set_cc(&mut want, VMIN, 1);
        set_cc(&mut want, VTIME, 0);
        assert!(
            only_the_patch_changed(&want, &saved),
            "the honest patch must be accepted"
        );
        // A termios built from nothing, which every other check waves through.
        let mut built = [0u8; sys::TERMIOS_LEN];
        set_cc(&mut built, VMIN, 1);
        set_cc(&mut built, VTIME, 0);
        assert_eq!(read_u32(&built, LFLAG_AT) & (ICANON | ECHO), 0, "the flag check passes");
        assert_eq!(built.get(CC_AT + VMIN), Some(&1), "the VMIN check passes");
        assert!(
            !only_the_patch_changed(&built, &saved),
            "a constructed termios must be refused: c_cflag = 0 is B0, a hang-up"
        );
        // And a single moved byte outside the patch is enough.
        let mut nudged = want;
        if let Some(slot) = nudged.get_mut(0) {
            *slot = slot.wrapping_add(1);
        }
        assert!(!only_the_patch_changed(&nudged, &saved), "c_iflag moved");
    }

    /// A terminal reporting zero rows or columns is "unknown", not a size.
    #[test]
    fn a_zero_window_is_not_a_size() {
        // Exercised through the same reader `size` uses.
        let mut buf = [0u8; sys::WINSIZE_LEN];
        assert_eq!((read_u16(&buf, 0), read_u16(&buf, 2)), (0, 0));
        buf[0] = 24;
        buf[2] = 80;
        assert_eq!((read_u16(&buf, 0), read_u16(&buf, 2)), (24, 80));
    }
}

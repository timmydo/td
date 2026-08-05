//! The login line's settings — the half of `getty` that is not a session.
//!
//! A getty's job on a line it is about to hand to `login` is to make that line
//! USABLE, whatever the last session left behind. The failure this exists for is
//! concrete: a shell that took raw mode and died before its restore ran leaves
//! ICANON and ECHO clear, and the next session on that terminal echoes nothing
//! and submits no line. Nothing in the boot reports it, because a terminal in
//! raw mode is indistinguishable from one whose operator has not typed yet.
//!
//! This module is the only one that knows what a `termios` BYTE means; `sys.rs`
//! carries the buffer as opaque bytes. It never CONSTRUCTS a termios — the
//! kernel's own bytes are read, named bits are patched, and the result is read
//! back and refused unless the kernel agrees. That last part is the discipline
//! `td-util`'s pager and `td-sh`'s editor already carry, and it is here for the
//! same reason: `TCSETS` can succeed having applied only part of what was asked.
//! A zeroed buffer is the case that makes it a property rather than a claim —
//! zeroed passes any "the bits I wanted are set" check while `c_cflag = 0` is
//! B0, a hang-up on a serial console.
//!
//! The LIMIT of that readback is worth stating, because it is not obvious and it
//! decides what has to be named below: it catches bits the KERNEL changed, never
//! bits this patch never named. An unnamed bit is copied out of `before` into the
//! expected buffer, so it agrees with itself and reports success. That is why the
//! set here is not only "turn the login bits on" but also "turn off what makes a
//! line unusable" — CRTSCTS, EXTPROC, and the input/output mangling flags. Bits
//! outside both sets are deliberately ACCEPTED as the line already had them.

use crate::sys;
use std::os::fd::RawFd;

/// The four leading `u32` flag words of the kernel `struct termios`, in order.
const IFLAG_AT: usize = 0;
const OFLAG_AT: usize = 4;
const CFLAG_AT: usize = 8;
const LFLAG_AT: usize = 12;
/// `c_cc` follows the flags and the one-byte `c_line`.
const CC_AT: usize = 17;

/// Indices into `c_cc`. Only the slots a canonical login line needs to be
/// usable: interrupt, erase, kill, end-of-file, and the two read bounds.
const VINTR: usize = 0;
const VERASE: usize = 2;
const VKILL: usize = 3;
const VEOF: usize = 4;
const VTIME: usize = 5;
const VMIN: usize = 6;

/// `c_iflag`. `ICRNL` set and `IGNCR` clear are one requirement stated twice: a
/// terminal sends CR for Enter, so with `IGNCR` set the byte is DISCARDED and
/// with `ICRNL` clear it never terminates the canonical line. Either way the
/// operator types and nothing happens.
const ICRNL: u32 = 0x0000_0100;
const IGNCR: u32 = 0x0000_0080;
/// Input mangling a dead session can leave behind: strip the eighth bit, turn
/// newlines into carriage returns, lowercase everything typed.
const ISTRIP: u32 = 0x0000_0020;
const INLCR: u32 = 0x0000_0040;
const IUCLC: u32 = 0x0000_0200;

/// `c_oflag`. Without `ONLCR` (which needs `OPOST`) every line staircases down
/// the screen — the same failure UNSAFE.md records for a zeroed `c_oflag`.
const OPOST: u32 = 0x0000_0001;
const ONLCR: u32 = 0x0000_0004;
/// Output mangling, cleared beside the two set above: map CR to NL, treat NL as
/// having done the carriage return, uppercase everything printed.
const OCRNL: u32 = 0x0000_0008;
const ONLRET: u32 = 0x0000_0020;
const OLCUC: u32 = 0x0000_0002;

/// `c_cflag`. `CBAUD` is the line speed's bit field; `CS8` with `PARENB` and
/// `CSTOPB` clear is 8N1, what every serial console on this image speaks;
/// `CREAD` is what lets the line RECEIVE at all, and a console that cannot be
/// typed into is a console that looks hung; `CLOCAL` is `-L`, which stops the
/// driver waiting on a carrier signal no virtual or emulated line ever raises.
const CBAUD: u32 = 0x0000_100f;
/// The INPUT speed, a second field in the same word. Zero is not "no speed": it
/// is the kernel's spelling of "same as the output speed", which is why clearing
/// it is how a line gets one speed rather than two. A line some earlier session
/// left with this set keeps it — verified against a real terminal — and since
/// the patch copies the kernel's own bytes forward, that stale input rate would
/// be in the expected buffer too and the readback would report success for a
/// console that garbles everything typed into it.
const CIBAUD: u32 = 0x100f_0000;
const CSIZE: u32 = 0x0000_0030;
const CS8: u32 = 0x0000_0030;
const CSTOPB: u32 = 0x0000_0040;
const CREAD: u32 = 0x0000_0080;
const PARENB: u32 = 0x0000_0100;
const CLOCAL: u32 = 0x0000_0800;
/// Hardware flow control. Cleared rather than inherited, and it is the sharpest
/// case for the rule in this module's header: a line an earlier session left
/// with RTS/CTS on, over a cable with no CTS wired, blocks every WRITE —
/// `login` prints its prompt and hangs, and the terminal simply looks dead.
const CRTSCTS: u32 = 0x8000_0000;

/// `c_lflag` — the four bits whose absence makes a line unusable for a login:
/// assemble input a line at a time, show the operator what they typed, let
/// Ctrl-C reach the session, and make an erase actually rub the character out
/// rather than echo `^?`.
const ISIG: u32 = 0x0000_0001;
const ICANON: u32 = 0x0000_0002;
const ECHO: u32 = 0x0000_0008;
const ECHOE: u32 = 0x0000_0010;
/// `EXTPROC` hands canonical processing to a pty master, so with it set ICANON
/// reads back as on and does nothing. Cleared for CRTSCTS's reason.
const EXTPROC: u32 = 0x0001_0000;

/// The control bytes a canonical line needs, at their `c_cc` slots. `VMIN`/
/// `VTIME` are set because a leftover raw configuration can carry `VMIN = 0`,
/// under which a read returns 0 bytes — which `login` reads as end-of-file.
const CC_PATCH: &[(usize, u8)] = &[
    (VMIN, 1),
    (VTIME, 0),
    (VINTR, 0x03),  // ^C
    (VERASE, 0x7f), // DEL
    (VKILL, 0x15),  // ^U
    (VEOF, 0x04),   // ^D
];

/// The line speeds `getty`'s BAUD operand may name, paired with the `CBAUD` bit
/// pattern each one is. A table rather than arithmetic because the encoding is
/// not one: the values run 1..15 and then jump to `CBAUDEX`, so 115200 is
/// 0x1002 and nothing about "115200" says so.
const SPEEDS: &[(&str, u32)] = &[
    ("50", 0x1),
    ("75", 0x2),
    ("110", 0x3),
    ("134", 0x4),
    ("150", 0x5),
    ("200", 0x6),
    ("300", 0x7),
    ("600", 0x8),
    ("1200", 0x9),
    ("1800", 0xa),
    ("2400", 0xb),
    ("4800", 0xc),
    ("9600", 0xd),
    ("19200", 0xe),
    ("38400", 0xf),
    ("57600", 0x1001),
    ("115200", 0x1002),
    ("230400", 0x1003),
    ("460800", 0x1004),
    ("500000", 0x1005),
    ("576000", 0x1006),
    ("921600", 0x1007),
];

/// The `CBAUD` pattern for a speed named on the command line.
pub fn speed_named(name: &str) -> Option<u32> {
    for (spelling, bits) in SPEEDS {
        if *spelling == name {
            return Some(*bits);
        }
    }
    None
}

/// What the readback said about the SPEED specifically.
///
/// Every other bit is required to take, but line speed is the one setting a
/// driver may legitimately not implement: a virtual console has no baud rate and
/// ignores the field, while a serial line programs its divisor from it. Refusing
/// on a VT would make this applet unusable on `tty1` — where a graphical image
/// wants it — and accepting silently would hide a serial console left at the
/// wrong speed, which is a console that prints garbage. So the two are told
/// apart by what the kernel reports back.
///
/// There is no third variant. A speed that is neither what was asked for nor
/// what the line already had is an ERROR from `verify`, which names the field it
/// disagreed about — nothing good explains that outcome, so it does not get a
/// value a caller could go on to ignore.
#[derive(Debug, PartialEq, Eq)]
pub enum Speed {
    /// The kernel reports the requested speed.
    Took,
    /// The kernel left the speed exactly as it found it — a driver with no line
    /// speed to set. Worth one line on stderr, not a refusal.
    Ignored,
}

/// Read the line, patch the named bits, write it, and read it back.
///
/// Fails unless the kernel agrees with everything but the speed, on which it
/// reports. The comparison is against the WHOLE buffer this function computed,
/// so a byte the patch never touched moving is a failure too — that is what
/// makes "never constructs a termios" checkable rather than merely intended.
pub fn configure(fd: RawFd, speed: u32, local: bool) -> Result<Speed, String> {
    let mut before = [0u8; sys::TERMIOS_LEN];
    sys::termios_get(fd, &mut before).map_err(|e| format!("TCGETS: {e}"))?;
    let want = patched(&before, speed, local);
    sys::termios_set(fd, &want).map_err(|e| format!("TCSETS: {e}"))?;
    let mut after = [0u8; sys::TERMIOS_LEN];
    sys::termios_get(fd, &mut after).map_err(|e| format!("TCGETS (readback): {e}"))?;
    verify(&before, &want, &after)
}

/// `before` with exactly the named bits and control bytes applied.
fn patched(
    before: &[u8; sys::TERMIOS_LEN],
    speed: u32,
    local: bool,
) -> [u8; sys::TERMIOS_LEN] {
    let mut out = *before;
    // CLOCAL is CLEARED before `-L` may set it, so every bit this patch names
    // ends up deterministic. Leaving it to be inherited would make the one flag
    // the caller can ask for the one flag a dead session decides: a stale
    // CLOCAL means an omitted `-L` silently ignores carrier detect anyway.
    let mut cflag = (read_u32(before, CFLAG_AT)
        & !(CBAUD | CIBAUD | CSIZE | CSTOPB | PARENB | CLOCAL | CRTSCTS))
        | speed
        | CS8
        | CREAD;
    if local {
        cflag |= CLOCAL;
    }
    write_u32(&mut out, CFLAG_AT, cflag);
    write_u32(
        &mut out,
        LFLAG_AT,
        (read_u32(before, LFLAG_AT) | ISIG | ICANON | ECHO | ECHOE) & !EXTPROC,
    );
    write_u32(
        &mut out,
        OFLAG_AT,
        (read_u32(before, OFLAG_AT) | OPOST | ONLCR) & !(OCRNL | ONLRET | OLCUC),
    );
    write_u32(
        &mut out,
        IFLAG_AT,
        (read_u32(before, IFLAG_AT) | ICRNL) & !(IGNCR | ISTRIP | INLCR | IUCLC),
    );
    for (slot, value) in CC_PATCH {
        set_cc(&mut out, *slot, *value);
    }
    out
}

/// Compare the kernel's answer with what was asked for, treating the speed as
/// the one field a driver may decline. `before` is needed to tell "declined"
/// from "changed to something else".
fn verify(
    before: &[u8; sys::TERMIOS_LEN],
    want: &[u8; sys::TERMIOS_LEN],
    after: &[u8; sys::TERMIOS_LEN],
) -> Result<Speed, String> {
    if after == want {
        return Ok(Speed::Took);
    }
    // The same request with the speed left as the line already had it. If THAT
    // is what came back, the driver has no line speed and everything else took.
    // BOTH speed fields, not just CBAUD: a driver with no line speed hands back
    // the CIBAUD it already had as well, and reconstructing only half of that
    // would turn "this console has no baud rate" into a refusal — which on the
    // boot path is the `&&` short-circuiting and the greeter respawning forever.
    const SPEED: u32 = CBAUD | CIBAUD;
    let mut declined = *want;
    let kept = (read_u32(want, CFLAG_AT) & !SPEED) | (read_u32(before, CFLAG_AT) & SPEED);
    write_u32(&mut declined, CFLAG_AT, kept);
    if after == &declined {
        return Ok(Speed::Ignored);
    }
    Err(disagreement(want, after))
}

/// Name the first field the kernel disagreed about. A byte offset alone would
/// send whoever reads the console back to a header to learn what moved.
fn disagreement(want: &[u8; sys::TERMIOS_LEN], after: &[u8; sys::TERMIOS_LEN]) -> String {
    for (at, field) in [
        (IFLAG_AT, "c_iflag"),
        (OFLAG_AT, "c_oflag"),
        (CFLAG_AT, "c_cflag"),
        (LFLAG_AT, "c_lflag"),
    ] {
        let (asked, got) = (read_u32(want, at), read_u32(after, at));
        if asked != got {
            return format!("the terminal did not take {field}: asked {asked:#x}, got {got:#x}");
        }
    }
    for i in 0..sys::TERMIOS_LEN {
        if want.get(i) != after.get(i) {
            return format!("the terminal changed byte {i} of its settings");
        }
    }
    // Unreachable: the caller compared the buffers and found them different.
    "the terminal did not take the line settings".to_string()
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

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]
    use super::*;

    /// The offsets are the whole point of this module, so they are pinned as
    /// VALUES rather than left to the functions that use them: a `c_cflag` read
    /// from the wrong word is a well-formed number, and the line would be
    /// configured for a speed and a character size nobody asked for.
    #[test]
    fn the_termios_layout_is_pinned() {
        assert_eq!((IFLAG_AT, OFLAG_AT, CFLAG_AT, LFLAG_AT), (0, 4, 8, 12));
        assert_eq!(CC_AT, 17);
        assert_eq!((VINTR, VERASE, VKILL, VEOF, VTIME, VMIN), (0, 2, 3, 4, 5, 6));
        assert_eq!(sys::TERMIOS_LEN, 36);
    }

    /// 115200 is `CBAUDEX | 2`, not 115200 and not a small ordinal. The table is
    /// the only thing that knows, so it is pinned at both ends of the jump.
    #[test]
    fn the_speed_table_carries_the_cbaudex_encoding() {
        assert_eq!(speed_named("115200"), Some(0x1002));
        assert_eq!(speed_named("38400"), Some(0xf));
        assert_eq!(speed_named("57600"), Some(0x1001));
        assert_eq!(speed_named("9600"), Some(0xd));
        // Every listed speed fits the field it is written into.
        for (name, bits) in SPEEDS {
            assert_eq!(bits & !CBAUD, 0, "{name} does not fit CBAUD");
        }
    }

    /// An unknown speed is refused rather than defaulted. Defaulting would hand
    /// a serial console a speed the operator did not ask for, which prints
    /// garbage and says nothing about why.
    #[test]
    fn an_unknown_speed_is_refused() {
        assert_eq!(speed_named("115201"), None);
        assert_eq!(speed_named(""), None);
        assert_eq!(speed_named("0"), None);
    }

    /// A line left in raw mode by a dead session is the failure this module
    /// exists for: the patch must turn canonical input, echo and signals back
    /// on rather than preserving what it found.
    #[test]
    fn a_line_left_in_raw_mode_is_made_canonical_again() {
        let mut raw = [0u8; sys::TERMIOS_LEN];
        write_u32(&mut raw, LFLAG_AT, 0);
        write_u32(&mut raw, OFLAG_AT, 0);
        write_u32(&mut raw, IFLAG_AT, IGNCR);
        raw[CC_AT + VMIN] = 0;
        let out = patched(&raw, 0x1002, true);
        let lflag = read_u32(&out, LFLAG_AT);
        assert_eq!(lflag & (ISIG | ICANON | ECHO | ECHOE), ISIG | ICANON | ECHO | ECHOE);
        assert_eq!(read_u32(&out, OFLAG_AT) & (OPOST | ONLCR), OPOST | ONLCR);
        assert_eq!(read_u32(&out, IFLAG_AT) & ICRNL, ICRNL);
        assert_eq!(read_u32(&out, IFLAG_AT) & IGNCR, 0, "IGNCR would discard Enter");
        assert_eq!(out[CC_AT + VMIN], 1);
    }

    /// The speed replaces whatever the line carried, and the character size is
    /// set rather than or-ed: a line left at CS5 would otherwise stay there,
    /// since CS8 is two bits and one of them is already set in CS5's neighbour.
    #[test]
    fn the_speed_and_character_size_replace_what_was_there() {
        let mut before = [0u8; sys::TERMIOS_LEN];
        // 9600 baud, 5-bit characters, even parity, two stop bits.
        write_u32(&mut before, CFLAG_AT, 0xd | PARENB | CSTOPB);
        let out = patched(&before, 0x1002, false);
        let cflag = read_u32(&out, CFLAG_AT);
        assert_eq!(cflag & CBAUD, 0x1002, "the speed did not replace the old one");
        assert_eq!(cflag & CSIZE, CS8);
        assert_eq!(cflag & (PARENB | CSTOPB), 0, "8N1 means parity and stop bits off");
        assert_eq!(cflag & CREAD, CREAD);
        assert_eq!(cflag & CLOCAL, 0, "CLOCAL is -L's, and -L was not given");
    }

    /// The bits that make a line unusable are CLEARED, not inherited. Each is a
    /// state a dead session can leave and the readback structurally cannot see,
    /// since an unnamed bit is copied into the expected buffer and agrees with
    /// itself. CRTSCTS is the one with teeth: hardware flow control on a cable
    /// with no CTS blocks every write, so `login` prints its prompt and hangs.
    #[test]
    fn the_bits_that_make_a_line_unusable_are_cleared() {
        let mut before = [0u8; sys::TERMIOS_LEN];
        write_u32(&mut before, CFLAG_AT, CRTSCTS);
        write_u32(&mut before, LFLAG_AT, EXTPROC);
        write_u32(&mut before, IFLAG_AT, ISTRIP | INLCR | IUCLC);
        write_u32(&mut before, OFLAG_AT, OCRNL | ONLRET | OLCUC);
        let out = patched(&before, 0x1002, true);
        assert_eq!(read_u32(&out, CFLAG_AT) & CRTSCTS, 0, "flow control would hang login");
        assert_eq!(read_u32(&out, LFLAG_AT) & EXTPROC, 0, "ICANON would read on and do nothing");
        assert_eq!(read_u32(&out, IFLAG_AT) & (ISTRIP | INLCR | IUCLC), 0);
        assert_eq!(read_u32(&out, OFLAG_AT) & (OCRNL | ONLRET | OLCUC), 0);
        // ...and the bits it does set are still set alongside.
        assert_eq!(read_u32(&out, LFLAG_AT) & ICANON, ICANON);
        assert_eq!(read_u32(&out, OFLAG_AT) & ONLCR, ONLCR);
    }

    /// A line carries TWO speeds, and only one of them is `CBAUD`. `CIBAUD` is
    /// cleared so input follows output — the kernel's meaning for zero — because
    /// a stale input rate survives into the expected buffer and would be read
    /// back as agreement. The failure it hides is one-directional: output reads
    /// fine and everything typed comes back garbage.
    #[test]
    fn a_stale_input_speed_does_not_survive() {
        let mut before = [0u8; sys::TERMIOS_LEN];
        write_u32(&mut before, CFLAG_AT, 0xd | (0xd << 16));
        let out = patched(&before, 0x1002, true);
        assert_eq!(read_u32(&out, CFLAG_AT) & CIBAUD, 0, "input speed must follow output");
        assert_eq!(read_u32(&out, CFLAG_AT) & CBAUD, 0x1002);
    }

    /// Without `-L`, a CLOCAL the line already carried is CLEARED rather than
    /// inherited. Otherwise the one flag the caller can ask for would be the one
    /// flag a previous session decides, and `getty 9600 ttyS0` would ignore
    /// carrier detect on a line some earlier `-L` had touched.
    #[test]
    fn a_stale_clocal_does_not_survive_an_omitted_dash_l() {
        let mut before = [0u8; sys::TERMIOS_LEN];
        write_u32(&mut before, CFLAG_AT, CLOCAL);
        assert_eq!(read_u32(&patched(&before, 0xd, false), CFLAG_AT) & CLOCAL, 0);
        assert_eq!(read_u32(&patched(&before, 0xd, true), CFLAG_AT) & CLOCAL, CLOCAL);
    }

    /// `-L` is the only thing that sets CLOCAL, and it sets nothing else.
    #[test]
    fn the_local_flag_sets_clocal_and_nothing_else() {
        let before = [0u8; sys::TERMIOS_LEN];
        let with = patched(&before, 0xd, true);
        let without = patched(&before, 0xd, false);
        assert_eq!(read_u32(&with, CFLAG_AT) ^ read_u32(&without, CFLAG_AT), CLOCAL);
        for at in [IFLAG_AT, OFLAG_AT, LFLAG_AT] {
            assert_eq!(read_u32(&with, at), read_u32(&without, at));
        }
    }

    /// Bytes the patch does not name are left exactly as the kernel had them.
    /// This is the "never constructs a termios" property: `c_line` and the
    /// control slots nothing here claims must survive untouched.
    #[test]
    fn every_byte_outside_the_patch_survives() {
        let mut before = [0u8; sys::TERMIOS_LEN];
        for (i, slot) in before.iter_mut().enumerate() {
            *slot = i as u8 + 1;
        }
        let out = patched(&before, 0x1002, true);
        let claimed: Vec<usize> = CC_PATCH.iter().map(|(slot, _)| CC_AT + slot).collect();
        for i in 16..sys::TERMIOS_LEN {
            if !claimed.contains(&i) {
                assert_eq!(out[i], before[i], "byte {i} was not the patch's to change");
            }
        }
    }

    /// The readback is the check, so its three outcomes are pinned. An exact
    /// match is success; a speed the driver left alone is reported but allowed;
    /// anything else fails, including a flag word that came back different.
    #[test]
    fn the_readback_tells_a_declined_speed_from_a_changed_one() {
        let mut before = [0u8; sys::TERMIOS_LEN];
        write_u32(&mut before, CFLAG_AT, 0xf); // the line was at 38400
        let want = patched(&before, 0x1002, true);
        assert_eq!(verify(&before, &want, &want), Ok(Speed::Took));

        // A virtual console: everything took but the speed, still 38400.
        let mut ignored = want;
        let kept = (read_u32(&want, CFLAG_AT) & !CBAUD) | 0xf;
        write_u32(&mut ignored, CFLAG_AT, kept);
        assert_eq!(verify(&before, &want, &ignored), Ok(Speed::Ignored));

        // ...and a driver that declines the speed FIELD hands back the input
        // half unchanged too. Reconstructing only CBAUD would call that an
        // error, which on the boot path is a greeter that respawns forever.
        let mut both = [0u8; sys::TERMIOS_LEN];
        write_u32(&mut both, CFLAG_AT, 0xf | (0xd << 16));
        let want_both = patched(&both, 0x1002, true);
        let mut declined_both = want_both;
        let kept_both = (read_u32(&want_both, CFLAG_AT) & !(CBAUD | CIBAUD)) | 0xf | (0xd << 16);
        write_u32(&mut declined_both, CFLAG_AT, kept_both);
        assert_eq!(verify(&both, &want_both, &declined_both), Ok(Speed::Ignored));

        // A THIRD speed is nobody's request and is refused.
        let mut altered = want;
        write_u32(&mut altered, CFLAG_AT, (read_u32(&want, CFLAG_AT) & !CBAUD) | 0xd);
        assert!(verify(&before, &want, &altered).is_err());
    }

    /// A zeroed readback is the case the whole-buffer comparison exists for: it
    /// carries none of the requested bits, so it must be refused rather than
    /// read as "canonical is off, therefore raw was wanted".
    #[test]
    fn a_zeroed_readback_is_refused_and_names_the_field() {
        let before = [0u8; sys::TERMIOS_LEN];
        let want = patched(&before, 0x1002, true);
        let error = verify(&before, &want, &[0u8; sys::TERMIOS_LEN]).unwrap_err();
        assert!(error.contains("c_iflag"), "{error}");
    }

    /// A control byte that did not take is refused too — the flag words alone
    /// would pass, and a `VMIN` of 0 makes `login` read end-of-file at once.
    #[test]
    fn a_control_byte_that_did_not_take_is_refused() {
        let before = [0u8; sys::TERMIOS_LEN];
        let want = patched(&before, 0x1002, true);
        let mut after = want;
        after[CC_AT + VMIN] = 0;
        let error = verify(&before, &want, &after).unwrap_err();
        assert!(error.contains("byte"), "{error}");
    }

    /// The accessors read and write where the offsets say, round-trip, and
    /// never index past the buffer.
    #[test]
    fn the_accessors_agree_with_the_offsets() {
        let mut buf = [0u8; sys::TERMIOS_LEN];
        write_u32(&mut buf, CFLAG_AT, 0x1234_5678);
        assert_eq!(read_u32(&buf, CFLAG_AT), 0x1234_5678);
        assert_eq!(buf[CFLAG_AT], 0x78, "little-endian, low byte first");
        assert_eq!(buf[CFLAG_AT + 3], 0x12);
        set_cc(&mut buf, VEOF, 4);
        assert_eq!(buf[CC_AT + VEOF], 4);
        // Past the end: refused rather than panicking.
        write_u32(&mut buf, sys::TERMIOS_LEN, 0xffff_ffff);
        set_cc(&mut buf, sys::TERMIOS_LEN, 0xff);
        assert_eq!(read_u32(&buf, sys::TERMIOS_LEN), 0);
    }
}

//! td-term's keyboard adapter: evdev key codes and XKB modifier masks in,
//! bounded terminal byte sequences out.
//!
//! Nothing here reads a device, socket, clock, or environment. The compositor
//! suppresses evdev autorepeat and publishes a rate instead, so the repetition
//! below is td-term's own and runs off an injected clock.

use crate::keyboard::{MOD_ALT, MOD_CAPS, MOD_CONTROL, MOD_NUM, MOD_SHIFT};
use std::collections::VecDeque;

/// The modifiers this profile translates.
const HANDLED: u32 = MOD_SHIFT | MOD_CAPS | MOD_CONTROL | MOD_ALT;

/// Num Lock is reported by the compositor but selects nothing here: this
/// profile's keypad is digits-only, so the bit is present and inert rather
/// than unhandled.
const IGNORED: u32 = MOD_NUM;

/// Room for the longest sequence this profile emits — an Alt prefix before
/// `CSI 24 ~`, six bytes — with slack. `with_alt` returns `None` on overflow,
/// which is indistinguishable from a deliberately silent chord, so the table
/// test below asserts every translatable key survives an Alt prefix.
pub const MAX_SEQUENCE: usize = 8;

/// 25 keys per second after 600 ms, the rate the compositor advertises.
pub const REPEAT_DELAY_MS: u64 = 600;
pub const REPEAT_INTERVAL_MS: u64 = 40;

/// §10's keyboard-input ceiling. A sequence is admitted whole or not at all.
pub const MAX_INPUT_BYTES: usize = 64 * 1024;

const ESC: u8 = 0x1b;
const DEL: u8 = 0x7f;

/// The terminal modes that select between two spellings of the same key.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Modes {
    pub application_cursor: bool,
}

/// A translated key press: a bounded byte string with no allocation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Sequence {
    bytes: [u8; MAX_SEQUENCE],
    length: usize,
}

impl Sequence {
    fn new(source: &[u8]) -> Option<Sequence> {
        let mut bytes = [0u8; MAX_SEQUENCE];
        let room = bytes.get_mut(..source.len())?;
        room.copy_from_slice(source);
        if source.is_empty() {
            return None;
        }
        Some(Sequence {
            bytes,
            length: source.len(),
        })
    }

    pub fn as_slice(&self) -> &[u8] {
        self.bytes.get(..self.length).unwrap_or(&[])
    }

    /// Alt is Meta: the whole resulting sequence is prefixed with ESC rather
    /// than folded into a CSI parameter, which this profile does not encode.
    fn with_alt(self) -> Option<Sequence> {
        let mut prefixed = [0u8; MAX_SEQUENCE];
        *prefixed.first_mut()? = ESC;
        prefixed
            .get_mut(1..self.length.checked_add(1)?)?
            .copy_from_slice(self.as_slice());
        Some(Sequence {
            bytes: prefixed,
            length: self.length.checked_add(1)?,
        })
    }
}

/// A move of td-term's own scrollback viewport. These never reach the child:
/// the terminal is looking at what it already received.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Scroll {
    Back,
    Forward,
    /// Produced for the End key while the viewport is scrolled back. The
    /// client drops every scroll until there is a viewport to move.
    Bottom,
}

/// What one key press does. Exactly one of three things, which is why this
/// is an enum rather than an `Option<Sequence>` plus a flag: a key that
/// scrolls must not also be able to send bytes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Action {
    Bytes(Sequence),
    Scroll(Scroll),
    Silent,
}

/// How one key spells itself. The variants exist because the modifier rules
/// differ per class, not because the byte strings do.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Kind {
    /// An alphabetic key: Caps Lock selects its second level as Shift does.
    Letter { lower: u8, upper: u8 },
    /// A printable key with a Shift level Caps Lock does not reach.
    Text { base: u8, shifted: u8 },
    /// One fixed byte string at both levels, so Shift passes through it: real
    /// terminals send CR for `Shift+Enter` and DEL for `Shift+Backspace`. Alt
    /// still prefixes it; Ctrl does not reach it.
    Fixed(&'static [u8]),
    /// A key whose Shift level is a different sequence, not a different level.
    ShiftFixed {
        plain: &'static [u8],
        shifted: &'static [u8],
    },
    /// A cursor key: `CSI` normally, `SS3` under DECCKM.
    Cursor {
        normal: &'static [u8],
        application: &'static [u8],
    },
    /// A `CSI n ~` editing or navigation key.
    Tilde(&'static [u8]),
    /// A function key with one fixed sequence.
    Function(&'static [u8]),
    /// Shift selects td-term's scrollback viewport, so the shifted form
    /// scrolls the terminal's own view rather than reaching the child. The
    /// direction sits in the roster the keymap tests already cover, so the
    /// two keys cannot be told apart by their bytes alone.
    Paging {
        bytes: &'static [u8],
        shifted: Scroll,
    },
    /// A modifier or lock. It is never text and never repeats — this set is
    /// exactly the keymap's `repeat=no` exclusions.
    Modifier,
    /// In the pinned keymap but deliberately untranslated by this profile.
    Silent,
}

/// The pinned key roster: corpus name, evdev code, and spelling. Every entry
/// corresponds to a keycode in `keyboard::XKB_KEYMAP`, and the tests below
/// assert that correspondence in both directions.
const KEYS: &[(&str, u16, Kind)] = &[
    ("escape", 1, Kind::Fixed(b"\x1b")),
    ("1", 2, Kind::Text { base: b'1', shifted: b'!' }),
    ("2", 3, Kind::Text { base: b'2', shifted: b'@' }),
    ("3", 4, Kind::Text { base: b'3', shifted: b'#' }),
    ("4", 5, Kind::Text { base: b'4', shifted: b'$' }),
    ("5", 6, Kind::Text { base: b'5', shifted: b'%' }),
    ("6", 7, Kind::Text { base: b'6', shifted: b'^' }),
    ("7", 8, Kind::Text { base: b'7', shifted: b'&' }),
    ("8", 9, Kind::Text { base: b'8', shifted: b'*' }),
    ("9", 10, Kind::Text { base: b'9', shifted: b'(' }),
    ("0", 11, Kind::Text { base: b'0', shifted: b')' }),
    ("minus", 12, Kind::Text { base: b'-', shifted: b'_' }),
    ("equal", 13, Kind::Text { base: b'=', shifted: b'+' }),
    ("backspace", 14, Kind::Fixed(b"\x7f")),
    ("tab", 15, Kind::ShiftFixed { plain: b"\t", shifted: b"\x1b[Z" }),
    ("q", 16, Kind::Letter { lower: b'q', upper: b'Q' }),
    ("w", 17, Kind::Letter { lower: b'w', upper: b'W' }),
    ("e", 18, Kind::Letter { lower: b'e', upper: b'E' }),
    ("r", 19, Kind::Letter { lower: b'r', upper: b'R' }),
    ("t", 20, Kind::Letter { lower: b't', upper: b'T' }),
    ("y", 21, Kind::Letter { lower: b'y', upper: b'Y' }),
    ("u", 22, Kind::Letter { lower: b'u', upper: b'U' }),
    ("i", 23, Kind::Letter { lower: b'i', upper: b'I' }),
    ("o", 24, Kind::Letter { lower: b'o', upper: b'O' }),
    ("p", 25, Kind::Letter { lower: b'p', upper: b'P' }),
    ("leftbracket", 26, Kind::Text { base: b'[', shifted: b'{' }),
    ("rightbracket", 27, Kind::Text { base: b']', shifted: b'}' }),
    ("enter", 28, Kind::Fixed(b"\r")),
    ("leftcontrol", 29, Kind::Modifier),
    ("a", 30, Kind::Letter { lower: b'a', upper: b'A' }),
    ("s", 31, Kind::Letter { lower: b's', upper: b'S' }),
    ("d", 32, Kind::Letter { lower: b'd', upper: b'D' }),
    ("f", 33, Kind::Letter { lower: b'f', upper: b'F' }),
    ("g", 34, Kind::Letter { lower: b'g', upper: b'G' }),
    ("h", 35, Kind::Letter { lower: b'h', upper: b'H' }),
    ("j", 36, Kind::Letter { lower: b'j', upper: b'J' }),
    ("k", 37, Kind::Letter { lower: b'k', upper: b'K' }),
    ("l", 38, Kind::Letter { lower: b'l', upper: b'L' }),
    ("semicolon", 39, Kind::Text { base: b';', shifted: b':' }),
    ("apostrophe", 40, Kind::Text { base: b'\'', shifted: b'"' }),
    ("grave", 41, Kind::Text { base: b'`', shifted: b'~' }),
    ("leftshift", 42, Kind::Modifier),
    ("backslash", 43, Kind::Text { base: b'\\', shifted: b'|' }),
    ("z", 44, Kind::Letter { lower: b'z', upper: b'Z' }),
    ("x", 45, Kind::Letter { lower: b'x', upper: b'X' }),
    ("c", 46, Kind::Letter { lower: b'c', upper: b'C' }),
    ("v", 47, Kind::Letter { lower: b'v', upper: b'V' }),
    ("b", 48, Kind::Letter { lower: b'b', upper: b'B' }),
    ("n", 49, Kind::Letter { lower: b'n', upper: b'N' }),
    ("m", 50, Kind::Letter { lower: b'm', upper: b'M' }),
    ("comma", 51, Kind::Text { base: b',', shifted: b'<' }),
    ("period", 52, Kind::Text { base: b'.', shifted: b'>' }),
    ("slash", 53, Kind::Text { base: b'/', shifted: b'?' }),
    ("rightshift", 54, Kind::Modifier),
    ("kpasterisk", 55, Kind::Text { base: b'*', shifted: b'*' }),
    ("leftalt", 56, Kind::Modifier),
    ("space", 57, Kind::Text { base: b' ', shifted: b' ' }),
    ("capslock", 58, Kind::Modifier),
    ("f1", 59, Kind::Function(b"\x1bOP")),
    ("f2", 60, Kind::Function(b"\x1bOQ")),
    ("f3", 61, Kind::Function(b"\x1bOR")),
    ("f4", 62, Kind::Function(b"\x1bOS")),
    ("f5", 63, Kind::Function(b"\x1b[15~")),
    ("f6", 64, Kind::Function(b"\x1b[17~")),
    ("f7", 65, Kind::Function(b"\x1b[18~")),
    ("f8", 66, Kind::Function(b"\x1b[19~")),
    ("f9", 67, Kind::Function(b"\x1b[20~")),
    ("f10", 68, Kind::Function(b"\x1b[21~")),
    ("numlock", 69, Kind::Modifier),
    ("scrolllock", 70, Kind::Modifier),
    ("kp7", 71, Kind::Text { base: b'7', shifted: b'7' }),
    ("kp8", 72, Kind::Text { base: b'8', shifted: b'8' }),
    ("kp9", 73, Kind::Text { base: b'9', shifted: b'9' }),
    ("kpminus", 74, Kind::Text { base: b'-', shifted: b'-' }),
    ("kp4", 75, Kind::Text { base: b'4', shifted: b'4' }),
    ("kp5", 76, Kind::Text { base: b'5', shifted: b'5' }),
    ("kp6", 77, Kind::Text { base: b'6', shifted: b'6' }),
    ("kpplus", 78, Kind::Text { base: b'+', shifted: b'+' }),
    ("kp1", 79, Kind::Text { base: b'1', shifted: b'1' }),
    ("kp2", 80, Kind::Text { base: b'2', shifted: b'2' }),
    ("kp3", 81, Kind::Text { base: b'3', shifted: b'3' }),
    ("kp0", 82, Kind::Text { base: b'0', shifted: b'0' }),
    ("kpperiod", 83, Kind::Text { base: b'.', shifted: b'.' }),
    ("less", 86, Kind::Text { base: b'<', shifted: b'>' }),
    ("f11", 87, Kind::Function(b"\x1b[23~")),
    ("f12", 88, Kind::Function(b"\x1b[24~")),
    ("kpenter", 96, Kind::Fixed(b"\r")),
    ("rightcontrol", 97, Kind::Modifier),
    ("kpslash", 98, Kind::Text { base: b'/', shifted: b'/' }),
    ("print", 99, Kind::Silent),
    ("rightalt", 100, Kind::Modifier),
    ("home", 102, Kind::Cursor { normal: b"\x1b[H", application: b"\x1bOH" }),
    ("up", 103, Kind::Cursor { normal: b"\x1b[A", application: b"\x1bOA" }),
    ("pageup", 104, Kind::Paging { bytes: b"\x1b[5~", shifted: Scroll::Back }),
    ("left", 105, Kind::Cursor { normal: b"\x1b[D", application: b"\x1bOD" }),
    ("right", 106, Kind::Cursor { normal: b"\x1b[C", application: b"\x1bOC" }),
    ("end", 107, Kind::Cursor { normal: b"\x1b[F", application: b"\x1bOF" }),
    ("down", 108, Kind::Cursor { normal: b"\x1b[B", application: b"\x1bOB" }),
    ("pagedown", 109, Kind::Paging { bytes: b"\x1b[6~", shifted: Scroll::Forward }),
    ("insert", 110, Kind::Tilde(b"\x1b[2~")),
    ("delete", 111, Kind::Tilde(b"\x1b[3~")),
    ("mute", 113, Kind::Silent),
    ("volumedown", 114, Kind::Silent),
    ("volumeup", 115, Kind::Silent),
    ("power", 116, Kind::Silent),
    ("kpequal", 117, Kind::Text { base: b'=', shifted: b'=' }),
    ("pause", 119, Kind::Silent),
    ("leftmeta", 125, Kind::Modifier),
    ("rightmeta", 126, Kind::Modifier),
    ("menu", 127, Kind::Silent),
];

/// Whether a physical key changes modifier state without being ordinary
/// terminal input. Selection survives these presses so the modifiers needed
/// for a copy chord cannot erase the range before its final key arrives.
pub fn is_modifier(code: u16) -> bool {
    matches!(kind(code), Some(Kind::Modifier))
}

fn kind(code: u16) -> Option<Kind> {
    for (_, candidate, kind) in KEYS {
        if *candidate == code {
            return Some(*kind);
        }
    }
    None
}

/// The C0 byte Ctrl produces for a resolved character. The character is the one
/// Shift already selected, so `Ctrl+Shift+6` arrives here as `^` and needs no
/// second rule.
fn control(character: u8) -> Option<u8> {
    match character {
        b'@' | b' ' => Some(0x00),
        b'a'..=b'z' => character.checked_sub(b'a').and_then(|base| base.checked_add(1)),
        b'A'..=b'Z' => character.checked_sub(b'A').and_then(|base| base.checked_add(1)),
        b'[' => Some(0x1b),
        b'\\' => Some(0x1c),
        b']' => Some(0x1d),
        b'^' => Some(0x1e),
        b'_' => Some(0x1f),
        b'?' => Some(DEL),
        _ => None,
    }
}

/// The bytes a key press sends to the child, or `None` when this profile
/// deliberately sends nothing.
///
/// `modifiers` is the effective mask, so a caller composing it from a
/// `wl_keyboard.modifiers` snapshot ORs the depressed, latched, and locked
/// fields: the compositor reports Caps Lock and Num Lock as locked, and a
/// caller reading only the depressed field would lose Caps entirely.
///
/// Ctrl reaches printable keys only; Shift selects a defined second spelling;
/// Alt prefixes whatever the other two produced. Any other combination — Ctrl
/// on an arrow, Shift on a function key — is unlisted and silent rather than
/// guessed, because a modified-key encoding this profile does not claim would
/// be indistinguishable from one it does.
pub fn sequence(code: u16, modifiers: u32, modes: Modes) -> Option<Sequence> {
    // A modifier this profile does not translate makes the whole chord
    // unlisted, not a bare keypress. The compositor forwards Super chords it
    // has no binding for, so without this `Super+q` would type `q`. The rule
    // is the profile's, not that compositor's: it holds under any of them,
    // whatever each keeps for itself.
    if modifiers & !(HANDLED | IGNORED) != 0 {
        return None;
    }
    let shift = modifiers & MOD_SHIFT != 0;
    let caps = modifiers & MOD_CAPS != 0;
    let control_held = modifiers & MOD_CONTROL != 0;
    let alt = modifiers & MOD_ALT != 0;
    let plain = match kind(code)? {
        Kind::Letter { lower, upper } => {
            let character = if shift != caps { upper } else { lower };
            let byte = if control_held {
                control(character)?
            } else {
                character
            };
            Sequence::new(&[byte])?
        }
        Kind::Text { base, shifted } => {
            let character = if shift { shifted } else { base };
            let byte = if control_held {
                control(character)?
            } else {
                character
            };
            Sequence::new(&[byte])?
        }
        _ if control_held => return None,
        Kind::Fixed(bytes) => Sequence::new(bytes)?,
        Kind::ShiftFixed { plain, shifted } => {
            Sequence::new(if shift { shifted } else { plain })?
        }
        Kind::Cursor {
            normal,
            application,
        } => {
            if shift {
                return None;
            }
            Sequence::new(if modes.application_cursor {
                application
            } else {
                normal
            })?
        }
        Kind::Tilde(bytes) | Kind::Function(bytes) => {
            if shift {
                return None;
            }
            Sequence::new(bytes)?
        }
        // Shift+PageUp and Shift+PageDown belong to the scrollback viewport,
        // so they generate nothing here; `action` is what routes them.
        Kind::Paging { bytes, .. } => {
            if shift {
                return None;
            }
            Sequence::new(bytes)?
        }
        Kind::Modifier | Kind::Silent => return None,
    };
    if alt {
        plain.with_alt()
    } else {
        Some(plain)
    }
}

/// The one key whose meaning depends on where the viewport is. Pinned to the
/// roster by the test below rather than read out of it per press.
const END: u16 = 107;

/// Route one key press: to the viewport, to the child, or nowhere.
///
/// `viewing` is the viewport's EFFECTIVE position rather than whether it was
/// ever opened, because §10 gives End two meanings and the one it has must
/// follow what is on screen: a viewport whose history evicted underneath it
/// is at the live bottom, and End there belongs to the child.
pub fn action(code: u16, modifiers: u32, modes: Modes, viewing: bool) -> Action {
    if modifiers & !(HANDLED | IGNORED) != 0 {
        return Action::Silent;
    }
    let shift = modifiers & MOD_SHIFT != 0;
    // Caps Lock is not consulted: it selects a letter's level, and neither of
    // these keys has one. Ctrl or Alt makes the chord unlisted, as elsewhere.
    let compound = modifiers & (MOD_CONTROL | MOD_ALT) != 0;
    if let Some(Kind::Paging { shifted, .. }) = kind(code) {
        if shift && !compound {
            return Action::Scroll(shifted);
        }
    }
    if code == END && viewing && !shift && !compound {
        return Action::Scroll(Scroll::Bottom);
    }
    match sequence(code, modifiers, modes) {
        Some(sequence) => Action::Bytes(sequence),
        None => Action::Silent,
    }
}

/// How far one `Shift+PageUp` moves: a screen less one row, so the line the
/// reader was looking at is still there to read on from. Never zero, since a
/// one-row grid would otherwise have no way to scroll at all.
fn page_lines(rows: usize) -> usize {
    rows.saturating_sub(1).max(1)
}

/// What the model's primary history looks like right now. The three travel
/// together because an offset means nothing without all of them: which
/// numbering the lines are counted in, how many have been counted, and how
/// many are still held.
#[derive(Default, Clone, Copy, Debug, Eq, PartialEq)]
pub struct Scrollback {
    pub epoch: u64,
    pub pushed: u64,
    pub lines: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Anchor {
    epoch: u64,
    line: u64,
}

/// td-term's scrollback viewport.
///
/// It stores the line it is looking at, not a distance from the live bottom,
/// because the bottom moves: with a stored distance a child writing
/// underneath an open viewport would drag the view along with it, one line
/// per line of output.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Viewport {
    anchor: Option<Anchor>,
}

impl Viewport {
    pub fn new() -> Viewport {
        Viewport { anchor: None }
    }

    /// How many lines back from the live bottom the view sits, given what
    /// history holds now. Clamped on every ask rather than stored, since
    /// eviction and resize both shorten the history an anchor lives in: an
    /// anchor whose line has been evicted rides the top of what remains,
    /// which is where the reader was heading, rather than being thrown back
    /// to the live bottom on the next line of output.
    pub fn offset(&self, history: Scrollback) -> usize {
        let Some(anchor) = self.anchor else {
            return 0;
        };
        // A clear renumbers from zero. Without this an old anchor would come
        // back into range as new lines arrived and reopen a view the clear
        // had closed, on lines that have nothing to do with it.
        if anchor.epoch != history.epoch {
            return 0;
        }
        let back = history.pushed.saturating_sub(anchor.line);
        usize::try_from(back).unwrap_or(usize::MAX).min(history.lines)
    }

    /// Whether anything but the live screen is showing. This is the question
    /// End asks, so it is the EFFECTIVE position rather than whether the
    /// viewport was ever opened: a view with nothing left to show is at the
    /// live bottom however it got there.
    pub fn viewing(&self, history: Scrollback) -> bool {
        self.offset(history) != 0
    }

    /// Apply what `action` decided. Bytes return to the live bottom: §10's
    /// rule that ordinary input does, and the child cannot answer a key
    /// whose result the reader would not be looking at.
    pub fn apply(&mut self, action: &Action, rows: usize, history: Scrollback) {
        let current = self.offset(history);
        let next = match action {
            // Leave the anchor alone rather than re-anchoring at `current`:
            // a clamped anchor still names the line it was put on, and a
            // silent key must not quietly move it to where it landed.
            Action::Silent => return,
            Action::Bytes(_) | Action::Scroll(Scroll::Bottom) => 0,
            // Clamped to what history HOLDS, unlike the read side. `offset`
            // clamping is what lets an anchor already inside history ride the
            // top as eviction shortens it; writing one BEYOND history is a
            // different thing, and on an empty history it is a delayed jump —
            // the chord looks inert, then the first line of output brings the
            // anchor into range and pins the view at the oldest line.
            Action::Scroll(Scroll::Back) => current
                .saturating_add(page_lines(rows))
                .min(history.lines),
            Action::Scroll(Scroll::Forward) => current.saturating_sub(page_lines(rows)),
        };
        self.anchor_at(next, history);
    }

    /// Move by a count of LINES rather than by what a key does. A wheel is
    /// the caller: a notch is not a key press, so it does not go through
    /// `Action` — that enum is "what one key press does", and a notch
    /// arriving as one would be a third thing a key could mean.
    ///
    /// `back` is toward older lines, which is the direction a wheel turned
    /// away from the operator asks for. Signed rather than two methods
    /// because a wheel reports a signed count and splitting it here would
    /// put the sign test in every caller.
    pub fn by_lines(&mut self, back: i32, history: Scrollback) {
        let current = self.offset(history);
        let next = if back >= 0 {
            current
                .saturating_add(usize::try_from(back).unwrap_or(usize::MAX))
                .min(history.lines)
        } else {
            // `unsigned_abs` rather than `-back`: `i32::MIN` has no positive
            // counterpart, so negating it overflows — a panic in a debug
            // build, which this crate does not permit anywhere. Nothing a
            // wheel reports comes near it; the spelling is what makes that
            // irrelevant rather than an argument about the input.
            current.saturating_sub(usize::try_from(back.unsigned_abs()).unwrap_or(usize::MAX))
        };
        self.anchor_at(next, history);
    }

    /// The half both movers share: an OFFSET becomes the anchor that names
    /// it. Zero is the live bottom and is `None` rather than a line number,
    /// so the view follows new output instead of pinning to where it was.
    fn anchor_at(&mut self, next: usize, history: Scrollback) {
        self.anchor = if next == 0 {
            None
        } else {
            Some(Anchor {
                epoch: history.epoch,
                line: history
                    .pushed
                    .saturating_sub(u64::try_from(next).unwrap_or(u64::MAX)),
            })
        };
    }
}

/// Whether a key repeats while held. This set is the keymap's `repeat=no`
/// exclusions inverted, so a client using the published keymap and td-term
/// agree about which keys autorepeat.
pub fn repeats(code: u16) -> bool {
    !matches!(kind(code), Some(Kind::Modifier) | None)
}

/// td-term's own autorepeat. The compositor suppresses evdev repeat records and
/// publishes a rate instead, so this is where a held key becomes a stream.
pub struct Repeat {
    delay: u64,
    interval: u64,
    active: Option<Active>,
    /// Whether the compositor publishes repeat at all. A rate of zero is the
    /// protocol's "no repeat", which is not the same as a long delay.
    enabled: bool,
}

/// The key itself, not its translation: the child can change DECCKM while a
/// cursor key is held, and a stored `Sequence` would keep sending the spelling
/// that was correct when the key went down.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Active {
    code: u16,
    modifiers: u32,
    due: u64,
}

impl Repeat {
    pub fn new() -> Repeat {
        Repeat::with_timing(REPEAT_DELAY_MS, REPEAT_INTERVAL_MS)
    }

    /// A machine that never arms, for a compositor that publishes a rate of
    /// zero — the protocol's way of saying keys do not repeat. Distinct from
    /// a very long delay: nothing here should ever come due.
    pub fn disabled() -> Repeat {
        Repeat {
            delay: 0,
            interval: 1,
            active: None,
            enabled: false,
        }
    }

    /// Adopt another machine's TIMINGS without disturbing the held key.
    /// Wayland permits `repeat_info` at any time, and a rate change is not a
    /// reason to stop repeating the key someone is holding — but a rate of
    /// zero is, since it says this seat does not repeat at all.
    pub fn retime(&mut self, source: &Repeat) {
        self.delay = source.delay;
        self.interval = source.interval;
        self.enabled = source.enabled;
        if !self.enabled {
            self.active = None;
        }
    }

    pub fn with_timing(delay: u64, interval: u64) -> Repeat {
        Repeat {
            delay,
            interval: interval.max(1),
            active: None,
            enabled: true,
        }
    }

    /// Arm the repeat for a press. A key that repeats but does nothing arms
    /// nothing; the newest qualifying press replaces any earlier one. A
    /// scroll qualifies: holding `Shift+PageUp` is how a reader walks back
    /// through scrollback, and the compositor sends no repeat events of its
    /// own for this to fall back on.
    pub fn press(&mut self, code: u16, modifiers: u32, modes: Modes, viewing: bool, now: u64) {
        if self.enabled
            && repeats(code)
            && !matches!(action(code, modifiers, modes, viewing), Action::Silent)
        {
            self.active = Some(Active {
                code,
                modifiers,
                due: now.saturating_add(self.delay),
            });
        } else {
            self.active = None;
        }
    }

    /// Release the named key. Releasing some other key leaves the repeat alone,
    /// which is what makes a held key survive a modifier tap.
    pub fn release(&mut self, code: u16) {
        if self.active.is_some_and(|active| active.code == code) {
            self.active = None;
        }
    }

    /// Focus loss or any modifier change cancels: the sequence armed under the
    /// old modifiers is no longer the one the key would send.
    pub fn cancel(&mut self) {
        self.active = None;
    }

    /// The next repetition, if one is due, routed against the modes and the
    /// viewport the terminal is in NOW. Ticks missed while the main loop was
    /// busy are dropped rather than delivered as a burst.
    ///
    /// Re-routing per repetition is what makes a held End coherent: the
    /// first repetition closes the viewport, and the ones after it are the
    /// child's, because by then the view is at the live bottom.
    pub fn due(&mut self, now: u64, modes: Modes, viewing: bool) -> Option<Action> {
        let active = self.active.as_mut().filter(|active| now >= active.due)?;
        active.due = now.saturating_add(self.interval);
        match action(active.code, active.modifiers, modes, viewing) {
            Action::Silent => None,
            found => Some(found),
        }
    }

    /// When the caller should next ask, so a main loop can wait rather than poll.
    pub fn deadline(&self) -> Option<u64> {
        self.active.map(|active| active.due)
    }

    pub fn armed(&self) -> bool {
        self.active.is_some()
    }
}

/// The bounded keyboard-input queue between the adapter and the PTY writer.
///
/// A sequence is admitted whole or dropped whole: half a `CSI` arriving at the
/// child would be worse than the key never having been pressed, so an
/// overflowing queue rings the visual bell instead of truncating.
pub struct InputQueue {
    bytes: VecDeque<u8>,
    capacity: usize,
    dropped: bool,
}

impl InputQueue {
    pub fn new() -> InputQueue {
        InputQueue::with_capacity(MAX_INPUT_BYTES)
    }

    pub fn with_capacity(capacity: usize) -> InputQueue {
        InputQueue {
            bytes: VecDeque::with_capacity(capacity.min(4096)),
            capacity,
            dropped: false,
        }
    }

    /// `false` when the whole sequence was dropped, which also marks the bell.
    pub fn push(&mut self, sequence: &[u8]) -> bool {
        if sequence.is_empty() {
            return true;
        }
        let admitted = self
            .bytes
            .len()
            .checked_add(sequence.len())
            .is_some_and(|total| total <= self.capacity);
        if !admitted {
            self.dropped = true;
            return false;
        }
        self.bytes.extend(sequence.iter().copied());
        true
    }

    /// The next bytes to write, still owned by the queue. The writer consumes
    /// what it actually wrote: draining first would lose the remainder when a
    /// write fails partway, and those bytes are keystrokes with nowhere to
    /// come back from.
    pub fn front(&mut self, limit: usize) -> &[u8] {
        self.bytes.make_contiguous();
        let count = limit.min(self.bytes.len());
        self.bytes.as_slices().0.get(..count).unwrap_or(&[])
    }

    pub fn consume(&mut self, count: usize) {
        self.bytes.drain(..count.min(self.bytes.len()));
    }

    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    /// Whether a sequence has been dropped since this was last asked.
    pub fn take_dropped(&mut self) -> bool {
        std::mem::take(&mut self.dropped)
    }
}

/// The packaged binary's own check of the keyboard adapter: one translation of
/// each shape, one full repeat cycle, and one atomic queue rejection. It touches
/// no device, so it runs wherever the artifact does.
pub fn selftest() -> Result<(), String> {
    let escape = Modes::default();
    let application = Modes {
        application_cursor: true,
    };
    let text = sequence(30, 0, escape).ok_or_else(|| "keys selftest lost a text key".to_string())?;
    let control =
        sequence(46, MOD_CONTROL, escape).ok_or_else(|| "keys selftest lost Ctrl+C".to_string())?;
    let meta =
        sequence(30, MOD_ALT, escape).ok_or_else(|| "keys selftest lost Alt+a".to_string())?;
    let normal_up =
        sequence(103, 0, escape).ok_or_else(|| "keys selftest lost Up".to_string())?;
    let applied =
        sequence(103, 0, application).ok_or_else(|| "keys selftest lost SS3 Up".to_string())?;
    if text.as_slice() != b"a"
        || control.as_slice() != [0x03]
        || meta.as_slice() != [ESC, b'a']
        || normal_up.as_slice() != b"\x1b[A"
        || applied.as_slice() != b"\x1bOA"
        || sequence(42, 0, escape).is_some()
        || repeats(42)
        || !repeats(30)
    {
        return Err("keyboard adapter selftest translated a key wrongly".into());
    }

    let mut repeat = Repeat::with_timing(REPEAT_DELAY_MS, REPEAT_INTERVAL_MS);
    repeat.press(103, 0, escape, false, 0);
    if repeat.deadline() != Some(REPEAT_DELAY_MS)
        || repeat.due(REPEAT_DELAY_MS - 1, escape, false).is_some()
    {
        return Err("keyboard repeat selftest did not wait out its delay".into());
    }
    // Routed at emission, so a mode the child changed mid-hold is honoured.
    let repetition = match repeat.due(REPEAT_DELAY_MS, application, false) {
        Some(Action::Bytes(found)) => found.as_slice().to_vec(),
        _ => Vec::new(),
    };
    if repetition != b"\x1bOA".to_vec() || !repeat.armed() {
        return Err("keyboard repeat selftest lost its first repetition".into());
    }
    // A held scroll chord repeats too, and as a scroll rather than as bytes.
    let mut scrolling = Repeat::with_timing(REPEAT_DELAY_MS, REPEAT_INTERVAL_MS);
    scrolling.press(104, MOD_SHIFT, escape, true, 0);
    if scrolling.due(REPEAT_DELAY_MS, escape, true) != Some(Action::Scroll(Scroll::Back)) {
        return Err("keyboard repeat selftest did not repeat a held scroll".into());
    }
    repeat.release(103);
    repeat.cancel();
    if repeat.armed() || Repeat::new().due(u64::MAX, escape, false).is_some() {
        return Err("keyboard repeat selftest kept a released key armed".into());
    }

    let mut queue = InputQueue::new();
    if !queue.push(text.as_slice()) || queue.is_empty() || queue.len() != 1 {
        return Err("keyboard queue selftest lost an admitted sequence".into());
    }
    let mut narrow = InputQueue::with_capacity(1);
    if narrow.push(applied.as_slice()) || !narrow.take_dropped() || !narrow.is_empty() {
        return Err("keyboard queue selftest split an oversized sequence".into());
    }
    if queue.front(MAX_INPUT_BYTES) != b"a" {
        return Err("keyboard queue selftest lost its bytes".into());
    }
    queue.consume(MAX_INPUT_BYTES);
    if !queue.is_empty() {
        return Err("keyboard queue selftest kept written bytes".into());
    }
    Ok(())
}

#[cfg(test)]
pub fn key_code(name: &str) -> Option<u16> {
    for (candidate, code, _) in KEYS {
        if *candidate == name {
            return Some(*code);
        }
    }
    None
}

/// `ctrl+alt+a` and friends, for the native corpus's `key` operation.
#[cfg(test)]
pub fn parse_chord(text: &str) -> Result<(u16, u32), String> {
    let mut modifiers = 0u32;
    let mut name = None;
    for part in text.split('+') {
        if part.is_empty() {
            return Err(format!("empty component in key chord '{text}'"));
        }
        let modifier = match part {
            "ctrl" => Some(MOD_CONTROL),
            "alt" => Some(MOD_ALT),
            "shift" => Some(MOD_SHIFT),
            "super" => Some(crate::keyboard::MOD_LOGO),
            "caps" => Some(MOD_CAPS),
            _ => None,
        };
        match (modifier, name) {
            (Some(_), Some(_)) => {
                return Err(format!("key chord '{text}' has a modifier after its key"));
            }
            (Some(bit), None) if modifiers & bit != 0 => {
                return Err(format!("key chord '{text}' repeats a modifier"));
            }
            (Some(bit), None) => modifiers |= bit,
            (None, None) => name = Some(part),
            (None, Some(_)) => return Err(format!("key chord '{text}' names two keys")),
        }
    }
    let name = name.ok_or_else(|| format!("key chord '{text}' names no key"))?;
    let code = key_code(name).ok_or_else(|| format!("unknown key '{name}'"))?;
    Ok((code, modifiers))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keyboard::{MOD_LOGO, XKB_KEYMAP};
    use std::collections::BTreeSet;

    fn bytes(chord: &str, modes: Modes) -> Option<Vec<u8>> {
        let (code, modifiers) = parse_chord(chord).unwrap();
        sequence(code, modifiers, modes).map(|found| found.as_slice().to_vec())
    }

    fn normal(chord: &str) -> Option<Vec<u8>> {
        bytes(chord, Modes::default())
    }

    /// The keycode block of the pinned keymap, as name/evdev-code pairs. XKB
    /// keycodes are evdev codes plus eight.
    fn keymap_codes() -> Vec<(String, u16)> {
        let block = XKB_KEYMAP
            .split_once("xkb_keycodes")
            .unwrap()
            .1
            .split_once("xkb_types")
            .unwrap()
            .0;
        let mut found = Vec::new();
        for entry in block.split(';') {
            let Some((name, value)) = entry.split_once('=') else {
                continue;
            };
            let name = name.trim();
            let Some(name) = name.strip_prefix('<').and_then(|n| n.strip_suffix('>')) else {
                continue;
            };
            let code: u16 = value.trim().parse().unwrap();
            found.push((name.to_string(), code - 8));
        }
        found
    }

    /// One roster, two consumers: every key the compositor publishes has a
    /// td-term spelling, and td-term spells no key the keymap does not have.
    #[test]
    fn the_key_table_matches_the_published_keymap_exactly() {
        let mut published: Vec<u16> = keymap_codes().into_iter().map(|(_, code)| code).collect();
        let mut table: Vec<u16> = KEYS.iter().map(|(_, code, _)| *code).collect();
        published.sort_unstable();
        table.sort_unstable();
        assert_eq!(table, published);
    }

    /// The repeat exclusions are the keymap's, not a second opinion about them.
    #[test]
    fn repeat_exclusions_mirror_the_keymaps_repeat_no_keys() {
        let codes = keymap_codes();
        for (name, code) in &codes {
            let declaration = format!("key <{name}> {{ repeat=no,");
            let excluded = XKB_KEYMAP.contains(&declaration);
            assert_eq!(
                repeats(*code),
                !excluded,
                "{name} ({code}) disagrees with the keymap"
            );
        }
        // A guard against the scan silently matching nothing.
        assert_eq!(codes.iter().filter(|(_, code)| !repeats(*code)).count(), 11);
    }

    /// The XKB keysym names this profile's printable keys use, as ASCII. A
    /// single-character name is that character.
    fn keysym(name: &str) -> Option<u8> {
        let named = [
            ("exclam", b'!'),
            ("at", b'@'),
            ("numbersign", b'#'),
            ("dollar", b'$'),
            ("percent", b'%'),
            ("asciicircum", b'^'),
            ("ampersand", b'&'),
            ("asterisk", b'*'),
            ("parenleft", b'('),
            ("parenright", b')'),
            ("minus", b'-'),
            ("underscore", b'_'),
            ("equal", b'='),
            ("plus", b'+'),
            ("bracketleft", b'['),
            ("braceleft", b'{'),
            ("bracketright", b']'),
            ("braceright", b'}'),
            ("semicolon", b';'),
            ("colon", b':'),
            ("apostrophe", b'\''),
            ("quotedbl", b'"'),
            ("grave", b'`'),
            ("asciitilde", b'~'),
            ("backslash", b'\\'),
            ("bar", b'|'),
            ("comma", b','),
            ("less", b'<'),
            ("period", b'.'),
            ("greater", b'>'),
            ("slash", b'/'),
            ("question", b'?'),
            ("space", b' '),
        ];
        for (candidate, byte) in named {
            if candidate == name {
                return Some(byte);
            }
        }
        let mut bytes = name.bytes();
        match (bytes.next(), bytes.next()) {
            (Some(byte), None) if byte.is_ascii_graphic() => Some(byte),
            _ => None,
        }
    }

    /// The published keymap's two levels for a printable key, by keycode name.
    fn keymap_levels(name: &str) -> Option<(u8, u8)> {
        let (base, shifted) = keymap_keysyms(name)?;
        let base = keysym(base)?;
        let shifted = shifted.map_or(Some(base), keysym)?;
        Some((base, shifted))
    }

    /// Codes alone are not the contract: a client computes CHARACTERS from the
    /// published keymap, so a layout edit that moved `Shift+2` from `@` to `"`
    /// would leave td-term sending `@` for the same physical key with every
    /// other test green.
    #[test]
    fn printable_keys_carry_the_characters_the_keymap_publishes() {
        let mut checked = 0usize;
        for (name, code) in keymap_codes() {
            let Some((base, shifted)) = keymap_levels(&name) else {
                continue;
            };
            let Some(kind) = kind(code) else {
                continue;
            };
            let ours = match kind {
                Kind::Letter { lower, upper } => (lower, upper),
                Kind::Text { base, shifted } => (base, shifted),
                _ => continue,
            };
            assert_eq!(
                ours,
                (base, shifted),
                "{name} ({code}) disagrees with the published keymap"
            );
            checked += 1;
        }
        // A guard against the scan silently matching nothing: 26 letters, 10
        // digits, 12 punctuation keys, and space. The keypad is excluded on
        // purpose — its `KP_7`-style symbols are not ASCII names, and its
        // digits-only spelling is this profile's choice, not the keymap's.
        assert_eq!(checked, 49);
    }

    /// Caps Lock behaviour is the keymap's `type=`, not a guess from the shape
    /// of the symbols: retyping a key to `TWO_LEVEL` changes what Caps does for
    /// every xkbcommon client while its two symbols stay exactly as they were.
    #[test]
    fn letter_keys_are_the_ones_the_keymap_types_alphabetic() {
        let mut checked = 0usize;
        for (name, code) in keymap_codes() {
            let Some(kind) = kind(code) else {
                continue;
            };
            let Some(body) = keymap_body(&name) else {
                continue;
            };
            assert_eq!(
                matches!(kind, Kind::Letter { .. }),
                body.contains("type=\"ALPHABETIC\""),
                "{name} ({code}) disagrees with the keymap's declared type"
            );
            checked += 1;
        }
        assert_eq!(checked, KEYS.len());
    }

    /// A key's declaration body, up to its closing brace. The keymap pads some
    /// names to align its columns, so match the name and not the spacing.
    fn keymap_body(name: &str) -> Option<&'static str> {
        let (_, tail) = XKB_KEYMAP.split_once(&format!("key <{name}>"))?;
        tail.split_once('}').map(|(body, _)| body)
    }

    /// The keysyms a key declares, in level order.
    fn keymap_keysyms(name: &str) -> Option<(&'static str, Option<&'static str>)> {
        let (_, levels) = keymap_body(name)?.split_once('[')?;
        let (levels, _) = levels.split_once(']')?;
        let mut parts = levels.split(',').map(str::trim);
        Some((parts.next()?, parts.next()))
    }

    /// The first keysym the keymap publishes for an XKB name.
    fn keymap_symbol(name: &str) -> Option<&'static str> {
        keymap_keysyms(name).map(|(first, _)| first)
    }

    /// Every roster name, and the keysym the keymap publishes for that key.
    /// The roster is matched to the keymap by CODE elsewhere and printable keys
    /// by their CHARACTERS, but neither reads a roster NAME: a code set is a
    /// set, so swapping the codes of `up` and `down` leaves it identical, and
    /// the character scan walks the keymap into `Kind` without ever looking at
    /// the tuple's name, so swapping the names of `2` and `3` leaves it green
    /// while `key 2` in the corpus resolves to the physical 3. This table is
    /// the name pin, and it covers the roster exactly.
    const KEYSYMS: &[(&str, &str)] = &[
        ("escape", "Escape"),
        ("1", "1"),
        ("2", "2"),
        ("3", "3"),
        ("4", "4"),
        ("5", "5"),
        ("6", "6"),
        ("7", "7"),
        ("8", "8"),
        ("9", "9"),
        ("0", "0"),
        ("minus", "minus"),
        ("equal", "equal"),
        ("backspace", "BackSpace"),
        ("tab", "Tab"),
        ("q", "q"),
        ("w", "w"),
        ("e", "e"),
        ("r", "r"),
        ("t", "t"),
        ("y", "y"),
        ("u", "u"),
        ("i", "i"),
        ("o", "o"),
        ("p", "p"),
        ("leftbracket", "bracketleft"),
        ("rightbracket", "bracketright"),
        ("enter", "Return"),
        ("leftcontrol", "Control_L"),
        ("a", "a"),
        ("s", "s"),
        ("d", "d"),
        ("f", "f"),
        ("g", "g"),
        ("h", "h"),
        ("j", "j"),
        ("k", "k"),
        ("l", "l"),
        ("semicolon", "semicolon"),
        ("apostrophe", "apostrophe"),
        ("grave", "grave"),
        ("leftshift", "Shift_L"),
        ("backslash", "backslash"),
        ("z", "z"),
        ("x", "x"),
        ("c", "c"),
        ("v", "v"),
        ("b", "b"),
        ("n", "n"),
        ("m", "m"),
        ("comma", "comma"),
        ("period", "period"),
        ("slash", "slash"),
        ("rightshift", "Shift_R"),
        ("kpasterisk", "KP_Multiply"),
        ("leftalt", "Alt_L"),
        ("space", "space"),
        ("capslock", "Caps_Lock"),
        ("f1", "F1"),
        ("f2", "F2"),
        ("f3", "F3"),
        ("f4", "F4"),
        ("f5", "F5"),
        ("f6", "F6"),
        ("f7", "F7"),
        ("f8", "F8"),
        ("f9", "F9"),
        ("f10", "F10"),
        ("numlock", "Num_Lock"),
        ("scrolllock", "Scroll_Lock"),
        ("kp7", "KP_7"),
        ("kp8", "KP_8"),
        ("kp9", "KP_9"),
        ("kpminus", "KP_Subtract"),
        ("kp4", "KP_4"),
        ("kp5", "KP_5"),
        ("kp6", "KP_6"),
        ("kpplus", "KP_Add"),
        ("kp1", "KP_1"),
        ("kp2", "KP_2"),
        ("kp3", "KP_3"),
        ("kp0", "KP_0"),
        ("kpperiod", "KP_Decimal"),
        ("less", "less"),
        ("f11", "F11"),
        ("f12", "F12"),
        ("kpenter", "KP_Enter"),
        ("rightcontrol", "Control_R"),
        ("kpslash", "KP_Divide"),
        ("print", "Print"),
        ("rightalt", "Alt_R"),
        ("home", "Home"),
        ("up", "Up"),
        ("pageup", "Prior"),
        ("left", "Left"),
        ("right", "Right"),
        ("end", "End"),
        ("down", "Down"),
        ("pagedown", "Next"),
        ("insert", "Insert"),
        ("delete", "Delete"),
        ("mute", "XF86AudioMute"),
        ("volumedown", "XF86AudioLowerVolume"),
        ("volumeup", "XF86AudioRaiseVolume"),
        ("power", "XF86PowerOff"),
        ("kpequal", "KP_Equal"),
        ("pause", "Pause"),
        ("leftmeta", "Super_L"),
        ("rightmeta", "Super_R"),
        ("menu", "Menu"),
    ];

    #[test]
    fn every_name_is_pinned_to_the_key_the_keymap_publishes() {
        let codes = keymap_codes();
        for (ours, keysym) in KEYSYMS {
            let published = codes
                .iter()
                .filter(|(name, _)| keymap_symbol(name) == Some(*keysym))
                .map(|(_, code)| *code)
                .collect::<Vec<_>>();
            assert_eq!(published.len(), 1, "{keysym} is not one keymap key");
            let table = KEYS
                .iter()
                .filter(|(name, _, _)| name == ours)
                .map(|(_, code, _)| *code)
                .collect::<Vec<_>>();
            assert_eq!(table, published, "{ours} is not the keymap's {keysym}");
        }

        // The pin covers the roster exactly: every key has one, and it names no
        // key the roster does not have. A key added later must appear here.
        let pinned: BTreeSet<&str> = KEYSYMS.iter().map(|(ours, _)| *ours).collect();
        let roster: BTreeSet<&str> = KEYS.iter().map(|(name, _, _)| *name).collect();
        assert_eq!(pinned, roster, "the pin and the roster name different keys");
        assert_eq!(KEYSYMS.len(), pinned.len(), "a name is pinned twice");
        assert_eq!(KEYS.len(), roster.len(), "a roster name is used twice");
    }

    /// `with_alt` returns `None` on overflow, which reads exactly like a
    /// deliberately silent chord. Every key that translates at all must still
    /// translate with the prefix, or adding a longer sequence later makes
    /// `Alt+<that key>` quietly dead.
    #[test]
    fn an_alt_prefix_fits_every_sequence_this_profile_emits() {
        for modes in [
            Modes::default(),
            Modes {
                application_cursor: true,
            },
        ] {
            for (name, code, _) in KEYS {
                for modifiers in [0, MOD_SHIFT, MOD_CONTROL, MOD_SHIFT | MOD_CONTROL] {
                    let Some(plain) = sequence(*code, modifiers, modes) else {
                        continue;
                    };
                    let prefixed = sequence(*code, modifiers | MOD_ALT, modes)
                        .unwrap_or_else(|| panic!("Alt+{name} overflowed the sequence buffer"));
                    assert_eq!(prefixed.as_slice().first(), Some(&ESC), "{name}");
                    assert_eq!(prefixed.as_slice().get(1..), Some(plain.as_slice()), "{name}");
                }
            }
        }
    }

    /// Shift passes through the fixed keys — real terminals send CR for
    /// `Shift+Enter` — while it silences the navigation and function keys,
    /// which have no defined shifted spelling in this profile.
    #[test]
    fn shift_passes_through_fixed_keys_and_silences_navigation() {
        assert_eq!(normal("shift+enter"), Some(b"\r".to_vec()));
        assert_eq!(normal("shift+kpenter"), Some(b"\r".to_vec()));
        assert_eq!(normal("shift+backspace"), Some(vec![DEL]));
        assert_eq!(normal("shift+escape"), Some(vec![ESC]));
        // Tab is the one fixed key with a defined second spelling.
        assert_eq!(normal("shift+tab"), Some(b"\x1b[Z".to_vec()));
        for chord in ["shift+up", "shift+home", "shift+f1", "shift+delete"] {
            assert_eq!(normal(chord), None, "{chord}");
        }
    }

    /// §10's bounded-input ceiling is a contract number, not an implementation
    /// detail; `64 * 1000` would satisfy every other test here.
    #[test]
    fn the_declared_ceilings_are_the_specified_ones() {
        assert_eq!(MAX_INPUT_BYTES, 64 * 1024);
        assert_eq!(REPEAT_DELAY_MS, 600);
        assert_eq!(REPEAT_INTERVAL_MS, 1000 / 25);
    }

    #[test]
    fn names_and_codes_are_unique() {
        let mut names: Vec<&str> = KEYS.iter().map(|(name, _, _)| *name).collect();
        names.sort_unstable();
        let count = names.len();
        names.dedup();
        assert_eq!(names.len(), count);
    }

    #[test]
    fn text_keys_carry_their_us_levels() {
        assert_eq!(normal("a"), Some(b"a".to_vec()));
        assert_eq!(normal("shift+a"), Some(b"A".to_vec()));
        assert_eq!(normal("caps+a"), Some(b"A".to_vec()));
        // Shift and Caps together return to the base level, as ALPHABETIC does.
        assert_eq!(normal("caps+shift+a"), Some(b"a".to_vec()));
        assert_eq!(normal("1"), Some(b"1".to_vec()));
        assert_eq!(normal("shift+1"), Some(b"!".to_vec()));
        // Caps Lock reaches alphabetic keys only.
        assert_eq!(normal("caps+1"), Some(b"1".to_vec()));
        assert_eq!(normal("space"), Some(b" ".to_vec()));
    }

    #[test]
    fn control_produces_the_specified_c0_bytes() {
        assert_eq!(normal("ctrl+c"), Some(vec![0x03]));
        assert_eq!(normal("ctrl+shift+c"), Some(vec![0x03]));
        assert_eq!(normal("ctrl+space"), Some(vec![0x00]));
        assert_eq!(normal("ctrl+leftbracket"), Some(vec![0x1b]));
        assert_eq!(normal("ctrl+backslash"), Some(vec![0x1c]));
        assert_eq!(normal("ctrl+rightbracket"), Some(vec![0x1d]));
        // `^` and `_` are Shift levels, so their Ctrl forms need no second rule.
        assert_eq!(normal("ctrl+shift+6"), Some(vec![0x1e]));
        assert_eq!(normal("ctrl+shift+minus"), Some(vec![0x1f]));
        assert_eq!(normal("ctrl+shift+slash"), Some(vec![0x7f]));
        // A printable key with no C0 spelling sends nothing rather than itself.
        assert_eq!(normal("ctrl+1"), None);
    }

    #[test]
    fn alt_prefixes_the_resulting_sequence_with_escape() {
        assert_eq!(normal("alt+a"), Some(vec![ESC, b'a']));
        assert_eq!(normal("alt+ctrl+c"), Some(vec![ESC, 0x03]));
        assert_eq!(normal("alt+backspace"), Some(vec![ESC, DEL]));
        assert_eq!(normal("alt+enter"), Some(vec![ESC, b'\r']));
        assert_eq!(normal("alt+up"), Some(b"\x1b\x1b[A".to_vec()));
        // The longest sequence this profile emits still fits the fixed buffer.
        assert_eq!(normal("alt+f12"), Some(b"\x1b\x1b[24~".to_vec()));
    }

    #[test]
    fn named_keys_use_the_bytes_the_slave_expects() {
        // DEL, matching the slave's Linux-default canonical VERASE.
        assert_eq!(normal("backspace"), Some(vec![DEL]));
        assert_eq!(normal("enter"), Some(b"\r".to_vec()));
        assert_eq!(normal("kpenter"), Some(b"\r".to_vec()));
        assert_eq!(normal("tab"), Some(b"\t".to_vec()));
        assert_eq!(normal("shift+tab"), Some(b"\x1b[Z".to_vec()));
        assert_eq!(normal("escape"), Some(vec![ESC]));
        assert_eq!(normal("insert"), Some(b"\x1b[2~".to_vec()));
        assert_eq!(normal("delete"), Some(b"\x1b[3~".to_vec()));
        assert_eq!(normal("f1"), Some(b"\x1bOP".to_vec()));
        assert_eq!(normal("f5"), Some(b"\x1b[15~".to_vec()));
        assert_eq!(normal("f11"), Some(b"\x1b[23~".to_vec()));
    }

    #[test]
    fn cursor_keys_follow_the_terminals_declared_mode() {
        let application = Modes {
            application_cursor: true,
        };
        for (chord, csi, ss3) in [
            ("up", "\x1b[A", "\x1bOA"),
            ("down", "\x1b[B", "\x1bOB"),
            ("right", "\x1b[C", "\x1bOC"),
            ("left", "\x1b[D", "\x1bOD"),
            ("home", "\x1b[H", "\x1bOH"),
            ("end", "\x1b[F", "\x1bOF"),
        ] {
            assert_eq!(normal(chord), Some(csi.as_bytes().to_vec()), "{chord}");
            assert_eq!(
                bytes(chord, application),
                Some(ss3.as_bytes().to_vec()),
                "{chord}"
            );
        }
        // Paging is not a cursor key and does not change spelling.
        assert_eq!(normal("pageup"), Some(b"\x1b[5~".to_vec()));
        assert_eq!(bytes("pageup", application), Some(b"\x1b[5~".to_vec()));
    }

    #[test]
    fn unlisted_combinations_and_untranslated_keys_send_nothing() {
        for chord in [
            "ctrl+up",
            "ctrl+f1",
            "ctrl+insert",
            "ctrl+enter",
            "shift+up",
            "shift+f1",
            "shift+insert",
            "leftshift",
            "leftcontrol",
            "capslock",
            "leftmeta",
            "print",
            "pause",
            "menu",
            "mute",
        ] {
            assert_eq!(normal(chord), None, "{chord}");
        }
        // Shift+PageUp/PageDown are the scrollback viewport's, not the child's.
        assert_eq!(normal("shift+pageup"), None);
        assert_eq!(normal("shift+pagedown"), None);
        // A code outside the published keymap is not invented.
        assert_eq!(sequence(240, 0, Modes::default()), None);
        // A modifier this profile does not translate makes the whole chord
        // unlisted. td's compositor keeps `enter` and `up` for itself, so
        // those two are about a DIFFERENT compositor reaching this profile;
        // the rest it forwards, where a bare `q` would type into the shell.
        for chord in ["a", "enter", "up", "ctrl+c", "space"] {
            let (code, modifiers) = parse_chord(chord).unwrap();
            assert!(
                sequence(code, modifiers | MOD_LOGO, Modes::default()).is_none(),
                "Super+{chord}"
            );
        }
        // An undefined mask bit is refused for the same reason.
        assert_eq!(sequence(30, 1 << 20, Modes::default()), None);
        // Num Lock is reported but inert: it is handled, not unhandled.
        assert_eq!(
            sequence(30, MOD_NUM, Modes::default()).map(|s| s.as_slice().to_vec()),
            Some(b"a".to_vec())
        );
        assert_eq!(
            sequence(71, MOD_NUM, Modes::default()).map(|s| s.as_slice().to_vec()),
            Some(b"7".to_vec())
        );
    }

    #[test]
    fn chord_parsing_rejects_malformed_vocabulary() {
        assert!(parse_chord("ctrl+").is_err());
        assert!(parse_chord("ctrl+a+b").is_err());
        assert!(parse_chord("ctrl").is_err());
        assert!(parse_chord("hyper+a").is_err());
        // The vocabulary is strict in both directions: modifiers lead, and
        // each appears once, so one chord has exactly one spelling.
        assert!(parse_chord("a+ctrl").is_err());
        assert!(parse_chord("ctrl+ctrl+a").is_err());
        assert_eq!(parse_chord("a").unwrap(), (30, 0));
        assert_eq!(parse_chord("ctrl+alt+a").unwrap(), (30, MOD_CONTROL | MOD_ALT));
    }

    #[test]
    fn repeat_waits_a_delay_then_runs_at_the_published_rate() {
        let mut repeat = Repeat::with_timing(600, 40);
        let code = key_code("a").unwrap();
        let modes = Modes::default();
        repeat.press(code, 0, modes, false, 1_000);
        assert_eq!(repeat.deadline(), Some(1_600));
        assert_eq!(repetition(&mut repeat, 1_599, modes), None);
        assert_eq!(repetition(&mut repeat, 1_600, modes), Some(b"a".to_vec()));
        assert_eq!(repeat.deadline(), Some(1_640));
        assert_eq!(repetition(&mut repeat, 1_639, modes), None);
        assert!(repetition(&mut repeat, 1_640, modes).is_some());
    }

    /// The child can change DECCKM while a cursor key is held, so a repetition
    /// is translated when it is emitted, not when the key went down.
    #[test]
    fn a_repetition_is_spelled_for_the_mode_the_terminal_is_in_now() {
        let mut repeat = Repeat::with_timing(600, 40);
        let normal = Modes::default();
        let application = Modes {
            application_cursor: true,
        };
        let up = key_code("up").unwrap();
        repeat.press(up, 0, normal, false, 0);
        assert_eq!(repetition(&mut repeat, 600, normal), Some(b"\x1b[A".to_vec()));
        assert_eq!(
            repetition(&mut repeat, 640, application),
            Some(b"\x1bOA".to_vec())
        );
    }

    #[test]
    fn a_stalled_loop_drops_missed_repetitions_instead_of_bursting() {
        let mut repeat = Repeat::with_timing(600, 40);
        let code = key_code("a").unwrap();
        let modes = Modes::default();
        repeat.press(code, 0, modes, false, 0);
        assert!(repetition(&mut repeat, 10_000, modes).is_some());
        assert_eq!(repeat.deadline(), Some(10_040));
        assert_eq!(repetition(&mut repeat, 10_000, modes), None);
    }

    #[test]
    fn release_focus_loss_and_modifier_changes_cancel_the_repeat() {
        let code = key_code("a").unwrap();
        let other = key_code("b").unwrap();
        let armed = |now: u64| {
            let mut repeat = Repeat::with_timing(600, 40);
            repeat.press(code, 0, Modes::default(), false, now);
            repeat
        };
        let mut repeat = armed(0);
        repeat.release(other);
        assert!(repeat.armed(), "another key's release is not this key's");
        repeat.release(code);
        assert!(!repeat.armed());
        let mut repeat = armed(0);
        repeat.cancel();
        assert!(!repeat.armed());
        assert_eq!(repeat.due(u64::MAX, Modes::default(), false), None);
    }

    #[test]
    fn keys_that_do_nothing_never_arm_a_repeat() {
        let mut repeat = Repeat::with_timing(600, 40);
        let modes = Modes::default();
        let shift = key_code("leftshift").unwrap();
        repeat.press(shift, 0, modes, false, 0);
        assert!(!repeat.armed());
        // Nor does a key whose chord this profile does not translate.
        repeat.press(key_code("a").unwrap(), MOD_LOGO, modes, false, 0);
        assert!(!repeat.armed());
        // A repeating key press replaces an armed one.
        let a = key_code("a").unwrap();
        let b = key_code("b").unwrap();
        repeat.press(a, 0, modes, false, 0);
        repeat.press(b, 0, modes, false, 100);
        assert_eq!(repeat.deadline(), Some(700));
        repeat.release(a);
        assert!(repeat.armed());
    }

    #[test]
    fn the_input_queue_admits_or_drops_a_sequence_whole() {
        let mut queue = InputQueue::with_capacity(4);
        assert!(queue.push(b"ab"));
        assert!(queue.push(b"cd"));
        assert!(!queue.push(b"e"));
        assert_eq!(queue.len(), 4);
        assert!(queue.take_dropped());
        assert!(!queue.take_dropped());
        assert_eq!(queue.front(3), b"abc");
        // Looking is not taking: a writer that fails partway leaves the rest.
        assert_eq!(queue.len(), 4);
        queue.consume(3);
        assert_eq!(queue.len(), 1);
        // Space freed by the writer admits the next sequence.
        assert!(queue.push(b"xyz"));
        assert_eq!(queue.front(usize::MAX), b"dxyz");
        queue.consume(usize::MAX);
        assert!(queue.is_empty());
        assert_eq!(queue.front(4), b"");
        assert!(queue.push(b""));
    }

    #[test]
    fn a_sequence_larger_than_the_queue_is_refused_not_split() {
        let mut queue = InputQueue::with_capacity(2);
        assert!(!queue.push(b"\x1b[24~"));
        assert!(queue.is_empty());
        assert!(queue.take_dropped());
    }

    /// The bytes of a due repetition, so the repeat tests stay about timing
    /// rather than about routing.
    fn repetition(repeat: &mut Repeat, now: u64, modes: Modes) -> Option<Vec<u8>> {
        match repeat.due(now, modes, false) {
            Some(Action::Bytes(found)) => Some(found.as_slice().to_vec()),
            _ => None,
        }
    }

    fn act(chord: &str, viewing: bool) -> Action {
        let (code, modifiers) = parse_chord(chord).unwrap();
        action(code, modifiers, Modes::default(), viewing)
    }

    /// `action` reads one key by number rather than by name. Nothing else
    /// ties that number to the roster entry whose meaning it is taking.
    #[test]
    fn the_end_constant_is_the_key_the_roster_spells() {
        assert_eq!(key_code("end"), Some(END));
    }

    #[test]
    fn shift_paging_scrolls_the_viewport_and_sends_nothing() {
        assert_eq!(act("shift+pageup", false), Action::Scroll(Scroll::Back));
        assert_eq!(
            act("shift+pagedown", false),
            Action::Scroll(Scroll::Forward)
        );
        // Unshifted they are the child's, whatever the viewport is doing.
        for viewing in [false, true] {
            assert!(matches!(act("pageup", viewing), Action::Bytes(_)));
            assert!(matches!(act("pagedown", viewing), Action::Bytes(_)));
        }
        // A further modifier makes the chord unlisted rather than a scroll.
        for chord in ["ctrl+shift+pageup", "alt+shift+pagedown", "super+shift+pageup"] {
            assert_eq!(act(chord, true), Action::Silent, "{chord}");
        }
    }

    #[test]
    fn end_is_the_childs_at_the_bottom_and_the_viewports_above_it() {
        assert!(matches!(act("end", false), Action::Bytes(_)));
        assert_eq!(act("end", true), Action::Scroll(Scroll::Bottom));
        // Caps Lock has no level to select on End, so it does not make the
        // chord a different one; a real modifier does.
        assert_eq!(act("caps+end", true), Action::Scroll(Scroll::Bottom));
        assert_eq!(act("shift+end", true), Action::Silent);
        assert_eq!(act("ctrl+end", true), Action::Silent);
        assert!(matches!(act("alt+end", true), Action::Bytes(_)));
    }

    /// One row of overlap, so the line last read survives the page.
    #[test]
    fn a_page_is_a_screen_less_one_row_and_never_zero() {
        assert_eq!(page_lines(24), 23);
        assert_eq!(page_lines(2), 1);
        assert_eq!(page_lines(1), 1);
        assert_eq!(page_lines(0), 1);
    }

    /// One epoch's worth of history, as the model would report it.
    fn history(pushed: u64, lines: usize) -> Scrollback {
        Scrollback {
            epoch: 0,
            pushed,
            lines,
        }
    }

    #[test]
    fn a_line_count_moves_the_viewport_and_stops_where_a_page_does() {
        // A wheel does not go through `Action`, so this is the second way to
        // move the view and it has to clamp at both ends exactly as the keys
        // do — a wheel that ran past the oldest line would show blank rows,
        // and one that ran past the newest would stop following output.
        let past = history(500, 100);
        let mut viewport = Viewport::new();
        viewport.by_lines(3, past);
        assert_eq!(viewport.offset(past), 3);
        viewport.by_lines(3, past);
        assert_eq!(viewport.offset(past), 6);
        viewport.by_lines(-4, past);
        assert_eq!(viewport.offset(past), 2);

        // Back to the live bottom is `None`, not line zero: the difference is
        // whether the view FOLLOWS new output, and a wheel returning to the
        // bottom must leave it following.
        viewport.by_lines(-2, past);
        assert_eq!(viewport.offset(past), 0);
        assert!(!viewport.viewing(past));
        viewport.by_lines(-9, past);
        assert_eq!(viewport.offset(past), 0);
        assert!(!viewport.viewing(past));

        // And the far end clamps to what history HOLDS rather than to what it
        // has ever pushed.
        viewport.by_lines(i32::MAX, past);
        assert_eq!(viewport.offset(past), past.lines);
        viewport.by_lines(1, past);
        assert_eq!(viewport.offset(past), past.lines);

        // The far-end clamp is a WRITE-side one, and `offset` clamps on the
        // read side too — so it shows only once history GROWS. Without it a
        // flick past the oldest line writes an anchor beyond history, which
        // later output brings back into range: the view jumps to a line
        // nobody scrolled to, seconds after the flick that caused it.
        let small = history(10, 5);
        let mut later = Viewport::new();
        later.by_lines(100, small);
        assert_eq!(later.offset(small), 5, "clamped to what history holds");
        let grown = history(50, 50);
        assert_eq!(
            later.offset(grown),
            45,
            "the anchor was written past history and jumped when it grew"
        );

        // `i32::MIN` is the value a magnitude cannot be taken of by negating
        // — `-i32::MIN` overflows. Asserted for the PANIC rather than for the
        // answer, which saturating either way would also reach.
        viewport.by_lines(i32::MIN, past);
        assert_eq!(viewport.offset(past), 0);
    }

    #[test]
    fn the_viewport_stops_at_both_ends_of_what_history_holds() {
        let mut viewport = Viewport::new();
        let back = Action::Scroll(Scroll::Back);
        let forward = Action::Scroll(Scroll::Forward);
        // Ten lines of history, four rows: three per page.
        for _ in 0..4 {
            viewport.apply(&back, 4, history(10, 10));
        }
        assert_eq!(viewport.offset(history(10, 10)), 10);
        assert!(viewport.viewing(history(10, 10)));
        for _ in 0..4 {
            viewport.apply(&forward, 4, history(10, 10));
        }
        assert_eq!(viewport.offset(history(10, 10)), 0);
        assert!(!viewport.viewing(history(10, 10)));
    }

    /// The anchor names a line, so output underneath an open viewport moves
    /// the live bottom away from it rather than moving the view.
    #[test]
    fn output_under_an_open_viewport_does_not_move_it() {
        let mut viewport = Viewport::new();
        viewport.apply(&Action::Scroll(Scroll::Back), 2, history(10, 10));
        assert_eq!(viewport.offset(history(10, 10)), 1);
        assert_eq!(viewport.offset(history(11, 11)), 2);
        assert_eq!(viewport.offset(history(40, 40)), 31);
    }

    /// Eviction is `pushed` growing while `lines` stays at the ceiling. The
    /// anchored line eventually falls out of the retained window, and the
    /// view then rides the top of what remains rather than being thrown to
    /// the live bottom -- which is where the reader was heading, and is the
    /// only choice that does not move on every further line of output.
    #[test]
    fn an_evicted_anchor_rides_the_top_of_what_history_still_holds() {
        let mut viewport = Viewport::new();
        viewport.apply(&Action::Scroll(Scroll::Back), 4, history(10, 10));
        assert_eq!(viewport.offset(history(10, 10)), 3);
        // Seven more lines: the window is full, so the anchored line is gone.
        assert_eq!(viewport.offset(history(17, 10)), 10);
        assert_eq!(viewport.offset(history(1_000, 10)), 10);
        assert!(viewport.viewing(history(1_000, 10)));
    }

    /// A clear renumbers from zero, so an old anchor's line number becomes a
    /// number some future line will also have. Without the epoch the view
    /// would reopen as output pushed `pushed` back past it.
    #[test]
    fn a_cleared_history_does_not_let_an_old_anchor_reopen() {
        let mut viewport = Viewport::new();
        viewport.apply(&Action::Scroll(Scroll::Back), 4, history(10, 10));
        assert_eq!(viewport.offset(history(10, 10)), 3);
        let after = |pushed: u64, lines: usize| Scrollback {
            epoch: 1,
            pushed,
            lines,
        };
        assert_eq!(viewport.offset(after(0, 0)), 0);
        // The numbers the old anchor named come back around; the view does not.
        for pushed in 1..20u64 {
            let lines = usize::try_from(pushed).unwrap();
            assert_eq!(viewport.offset(after(pushed, lines)), 0, "{pushed}");
            assert!(!viewport.viewing(after(pushed, lines)), "{pushed}");
        }
        // Scrolling again anchors in the new numbering and works normally.
        viewport.apply(&Action::Scroll(Scroll::Back), 4, after(19, 19));
        assert_eq!(viewport.offset(after(19, 19)), 3);
    }

    /// A silent key is neither input nor a scroll, so it must leave the
    /// anchor exactly as it found it -- including its epoch.
    #[test]
    fn a_silent_key_does_not_move_the_anchor() {
        let mut viewport = Viewport::new();
        viewport.apply(&Action::Scroll(Scroll::Back), 9, history(10, 10));
        let anchored = viewport;
        viewport.apply(&Action::Silent, 9, history(10, 10));
        assert_eq!(viewport, anchored);
        // Not even when a clamp is what the offset would have re-anchored at.
        viewport.apply(&Action::Silent, 9, history(10, 3));
        assert_eq!(viewport, anchored);
    }

    /// The compositor suppresses evdev repeat, so a held scroll chord that
    /// did not repeat here would move exactly one page however long it was
    /// held -- and walking back through scrollback is what holding it is for.
    #[test]
    fn a_held_scroll_chord_repeats_as_a_scroll() {
        let modes = Modes::default();
        let mut repeat = Repeat::with_timing(600, 40);
        let (code, modifiers) = parse_chord("shift+pageup").unwrap();
        repeat.press(code, modifiers, modes, true, 0);
        assert!(repeat.armed());
        assert_eq!(repeat.due(600, modes, true), Some(Action::Scroll(Scroll::Back)));
        assert_eq!(repeat.due(640, modes, true), Some(Action::Scroll(Scroll::Back)));
    }

    /// A repetition is routed when it is emitted, so a held End closes the
    /// view once and then types: the state it asks about changed under it.
    #[test]
    fn a_held_end_closes_the_view_and_then_becomes_the_childs() {
        let modes = Modes::default();
        let mut repeat = Repeat::with_timing(600, 40);
        repeat.press(key_code("end").unwrap(), 0, modes, true, 0);
        assert_eq!(
            repeat.due(600, modes, true),
            Some(Action::Scroll(Scroll::Bottom))
        );
        assert!(matches!(
            repeat.due(640, modes, false),
            Some(Action::Bytes(_))
        ));
    }

    #[test]
    fn bytes_and_the_bottom_key_both_close_the_view() {
        for closing in [
            Action::Bytes(Sequence::new(b"a").unwrap()),
            Action::Scroll(Scroll::Bottom),
        ] {
            let mut viewport = Viewport::new();
            viewport.apply(&Action::Scroll(Scroll::Back), 4, history(10, 10));
            assert!(viewport.viewing(history(10, 10)));
            viewport.apply(&closing, 4, history(10, 10));
            assert_eq!(viewport.offset(history(10, 10)), 0);
            assert!(!viewport.viewing(history(10, 10)));
        }
    }
}

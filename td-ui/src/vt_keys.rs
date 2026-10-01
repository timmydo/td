//! The terminal's keyboard encoder: a chord as td-ui's keymap spells it
//! (`keyboard::Stroke`) in; a bounded terminal byte sequence, a move of the
//! scrollback viewport, or nothing out. Beside it, the pointer's report
//! encoder, the viewport and the bounded queue between the encoders and a
//! PTY writer.
//!
//! Chords are keymap-independent: the compositor's own keymap, td's or any
//! other the toolkit compiles, has already chosen the character or named
//! key and which modifier roles are left over. So this module holds the
//! terminal's profile and nothing about keycodes. Nothing here reads a
//! device, socket, clock or environment.

use std::collections::VecDeque;

use crate::vt::{MouseMode, MouseTracking};

/// Room for the longest sequence this profile emits — an Alt prefix before
/// `CSI 24 ~`, six bytes — with slack.
pub const MAX_SEQUENCE: usize = 8;

/// The keyboard-input ceiling. A sequence is admitted whole or not at all.
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

/// A move of the terminal's own scrollback viewport. These never reach the
/// child: the terminal is looking at what it already received.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Scroll {
    Back,
    Forward,
    /// Produced for the End key while the viewport is scrolled back.
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

/// A chord taken apart: the modifier roles left over after the keymap chose
/// the key's level, and the base the keymap named.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Chord<'a> {
    control: bool,
    alt: bool,
    shift: bool,
    base: &'a str,
}

/// `C-`, `M-` and `S-` in that order, each at most once, then a base. A
/// prefix is only a prefix when something follows it, so `C--` is Control
/// with the minus key and `S` alone is the letter.
fn chord(text: &str) -> Option<Chord<'_>> {
    let mut chord = Chord {
        control: false,
        alt: false,
        shift: false,
        base: text,
    };
    if let Some(rest) = chord.base.strip_prefix("C-").filter(|r| !r.is_empty()) {
        chord.control = true;
        chord.base = rest;
    }
    if let Some(rest) = chord.base.strip_prefix("M-").filter(|r| !r.is_empty()) {
        chord.alt = true;
        chord.base = rest;
    }
    if let Some(rest) = chord.base.strip_prefix("S-").filter(|r| !r.is_empty()) {
        chord.shift = true;
        chord.base = rest;
    }
    (!chord.base.is_empty()).then_some(chord)
}

/// How a named key spells itself. The classes exist because the modifier
/// rules differ per class, not because the byte strings do.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Kind {
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
    /// A `CSI n ~` editing key, or a function key with one fixed sequence.
    Tilde(&'static [u8]),
    /// Shift selects the scrollback viewport, so the shifted form scrolls the
    /// terminal's own view rather than reaching the child.
    Paging {
        bytes: &'static [u8],
        shifted: Scroll,
    },
}

/// The named keys, as td-ui's keymap spells them, and their spellings. A
/// name missing here is silent.
const NAMED: &[(&str, Kind)] = &[
    ("Escape", Kind::Fixed(b"\x1b")),
    ("Backspace", Kind::Fixed(b"\x7f")),
    ("Return", Kind::Fixed(b"\r")),
    (
        "Tab",
        Kind::ShiftFixed {
            plain: b"\t",
            shifted: b"\x1b[Z",
        },
    ),
    (
        "Home",
        Kind::Cursor {
            normal: b"\x1b[H",
            application: b"\x1bOH",
        },
    ),
    (
        "End",
        Kind::Cursor {
            normal: b"\x1b[F",
            application: b"\x1bOF",
        },
    ),
    (
        "Up",
        Kind::Cursor {
            normal: b"\x1b[A",
            application: b"\x1bOA",
        },
    ),
    (
        "Down",
        Kind::Cursor {
            normal: b"\x1b[B",
            application: b"\x1bOB",
        },
    ),
    (
        "Right",
        Kind::Cursor {
            normal: b"\x1b[C",
            application: b"\x1bOC",
        },
    ),
    (
        "Left",
        Kind::Cursor {
            normal: b"\x1b[D",
            application: b"\x1bOD",
        },
    ),
    (
        "PageUp",
        Kind::Paging {
            bytes: b"\x1b[5~",
            shifted: Scroll::Back,
        },
    ),
    (
        "PageDown",
        Kind::Paging {
            bytes: b"\x1b[6~",
            shifted: Scroll::Forward,
        },
    ),
    ("Insert", Kind::Tilde(b"\x1b[2~")),
    ("Delete", Kind::Tilde(b"\x1b[3~")),
    ("F1", Kind::Tilde(b"\x1bOP")),
    ("F2", Kind::Tilde(b"\x1bOQ")),
    ("F3", Kind::Tilde(b"\x1bOR")),
    ("F4", Kind::Tilde(b"\x1bOS")),
    ("F5", Kind::Tilde(b"\x1b[15~")),
    ("F6", Kind::Tilde(b"\x1b[17~")),
    ("F7", Kind::Tilde(b"\x1b[18~")),
    ("F8", Kind::Tilde(b"\x1b[19~")),
    ("F9", Kind::Tilde(b"\x1b[20~")),
    ("F10", Kind::Tilde(b"\x1b[21~")),
    ("F11", Kind::Tilde(b"\x1b[23~")),
    ("F12", Kind::Tilde(b"\x1b[24~")),
];

fn named(base: &str) -> Option<Kind> {
    NAMED
        .iter()
        .find(|(name, _)| *name == base)
        .map(|(_, kind)| *kind)
}

/// The one character a text base names: a single printable ASCII
/// character, or `Space`, which the keymap spells by name under Control or
/// Alt.
fn text(base: &str) -> Option<u8> {
    if base == "Space" {
        return Some(b' ');
    }
    match base.as_bytes() {
        [byte] if (0x20..0x7f).contains(byte) => Some(*byte),
        _ => None,
    }
}

/// The C0 byte Ctrl produces for a resolved character. The character is the one
/// Shift already selected, so `Ctrl+Shift+6` arrives here as `^` and needs no
/// second rule.
fn control(character: u8) -> Option<u8> {
    match character {
        b'@' | b' ' => Some(0x00),
        b'a'..=b'z' => character
            .checked_sub(b'a')
            .and_then(|base| base.checked_add(1)),
        b'A'..=b'Z' => character
            .checked_sub(b'A')
            .and_then(|base| base.checked_add(1)),
        b'[' => Some(0x1b),
        b'\\' => Some(0x1c),
        b']' => Some(0x1d),
        b'^' => Some(0x1e),
        b'_' => Some(0x1f),
        b'?' => Some(DEL),
        _ => None,
    }
}

/// The bytes a chord sends to the child, or `None` when this profile
/// deliberately sends nothing.
///
/// The keymap has already applied Shift and Caps Lock to a character, and
/// reports Shift beside a letter only under Control or Alt, where it
/// lowercases the letter. Ctrl reaches printable characters only; Shift
/// selects a defined second spelling of a named key; Alt prefixes whatever
/// the other two produced. Any other combination — Ctrl on an arrow, Shift
/// on a function key — is unlisted and silent rather than guessed, because a
/// modified-key encoding this profile does not claim would be
/// indistinguishable from one it does. A modifier the keymap gives no role
/// (Super, AltGr) never reaches here: the keymap refuses the press.
pub fn sequence(text_chord: &str, modes: Modes) -> Option<Sequence> {
    typed(text_chord, None, modes)
}

/// `sequence`, given the character the keymap resolved the key to
/// (`Stroke::text`). A chord alone cannot say Caps Lock was on under Alt,
/// where it spells the letter lowercase, so a resolved letter that is the
/// chord's own, in either case, is the case sent; without one, Shift decides.
fn typed(text_chord: &str, resolved: Option<char>, modes: Modes) -> Option<Sequence> {
    let chord = chord(text_chord)?;
    let plain = if let Some(character) = text(chord.base) {
        let resolved = resolved
            .and_then(|letter| u8::try_from(letter).ok())
            .filter(|letter| {
                letter.is_ascii_alphabetic() && letter.eq_ignore_ascii_case(&character)
            });
        let character = match resolved {
            Some(letter) if !chord.control => letter,
            _ if chord.shift && !chord.control => character.to_ascii_uppercase(),
            _ => character,
        };
        let byte = if chord.control {
            control(character)?
        } else {
            character
        };
        Sequence::new(&[byte])?
    } else {
        let kind = named(chord.base)?;
        if chord.control {
            return None;
        }
        match kind {
            Kind::Fixed(bytes) => Sequence::new(bytes)?,
            Kind::ShiftFixed { plain, shifted } => {
                Sequence::new(if chord.shift { shifted } else { plain })?
            }
            _ if chord.shift => return None,
            Kind::Cursor {
                normal,
                application,
            } => Sequence::new(if modes.application_cursor {
                application
            } else {
                normal
            })?,
            Kind::Tilde(bytes) | Kind::Paging { bytes, .. } => Sequence::new(bytes)?,
        }
    };
    if chord.alt {
        plain.with_alt()
    } else {
        Some(plain)
    }
}

/// Route one chord: to the viewport, to the child, or nowhere.
///
/// `viewing` is the viewport's EFFECTIVE position rather than whether it was
/// ever opened, because End has two meanings and the one it has must follow
/// what is on screen: a viewport whose history evicted underneath it is at
/// the live bottom, and End there belongs to the child.
pub fn action(text_chord: &str, resolved: Option<char>, modes: Modes, viewing: bool) -> Action {
    let Some(chord) = chord(text_chord) else {
        return Action::Silent;
    };
    let compound = chord.control || chord.alt;
    if let Some(Kind::Paging { shifted, .. }) = named(chord.base) {
        if chord.shift && !compound {
            return Action::Scroll(shifted);
        }
    }
    if chord.base == "End" && viewing && !chord.shift && !compound {
        return Action::Scroll(Scroll::Bottom);
    }
    match typed(text_chord, resolved, modes) {
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

/// Room for the longest pointer report on a `vt::MAX_DIMENSION` grid, SGR's
/// `CSI < 93 ; 16384 ; 16384 M` (18 bytes), with slack.
pub const MAX_REPORT: usize = 24;

/// A pointer button as a terminal reports it; the wheel's two directions
/// are buttons that are pressed and never released.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Button {
    Left,
    Middle,
    Right,
    WheelUp,
    WheelDown,
}

/// What the pointer did over a cell: a press, a release, or motion with
/// the button held, if one is.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Pointer {
    Press(Button),
    Release(Button),
    Motion(Option<Button>),
}

/// The modifier roles held as the pointer acted.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PointerModifiers {
    pub shift: bool,
    pub alt: bool,
    pub control: bool,
}

/// One pointer report: a bounded byte string with no allocation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Report {
    bytes: [u8; MAX_REPORT],
    length: usize,
}

impl Report {
    pub fn as_slice(&self) -> &[u8] {
        self.bytes.get(..self.length).unwrap_or(&[])
    }

    fn push(&mut self, bytes: &[u8]) -> Option<()> {
        let end = self.length.checked_add(bytes.len())?;
        self.bytes.get_mut(self.length..end)?.copy_from_slice(bytes);
        self.length = end;
        Some(())
    }

    fn push_decimal(&mut self, value: usize) -> Option<()> {
        let mut digits = [0u8; 20];
        let mut start = digits.len();
        let mut rest = value;
        loop {
            start = start.checked_sub(1)?;
            *digits.get_mut(start)? = b'0' + u8::try_from(rest % 10).ok()?;
            rest /= 10;
            if rest == 0 {
                break;
            }
        }
        self.push(digits.get(start..)?)
    }
}

/// The report `mode` asks for of `event` at the zero-based cell `at`, or
/// none: when the mode does not report that kind of event (mode 9 reports
/// no wheel, as in xterm), for a wheel's release or motion, and in X10's
/// encoding for a cell past the 223rd row or column, which its one byte
/// cannot carry.
///
/// The button is 0, 1 or 2 for left, middle and right, 64 and 65 for the
/// wheel up and down, and 3 for motion with no button and for any release
/// in X10's encoding, which does not say which button; motion adds 32,
/// and Shift, Alt and Control add 4, 8 and 16, except under mode 9. X10's
/// encoding is `CSI M` then the button, column and row each plus 32 in one
/// byte, the cell one-based; SGR's is `CSI <` then the three in decimal,
/// separated by `;`, ending `M`, or `m` for a release.
pub fn report(
    event: Pointer,
    at: (usize, usize),
    modifiers: PointerModifiers,
    mode: MouseMode,
) -> Option<Report> {
    let (button, motion, release) = match event {
        Pointer::Press(button) => (Some(button), false, false),
        Pointer::Release(button) => (Some(button), false, true),
        Pointer::Motion(button) => (button, true, false),
    };
    let reported = match mode.tracking {
        MouseTracking::Off => false,
        MouseTracking::Press => !motion && !release,
        MouseTracking::Click => !motion,
        MouseTracking::Drag => !motion || button.is_some(),
        MouseTracking::Motion => true,
    };
    let wheel = matches!(button, Some(Button::WheelUp | Button::WheelDown));
    if !reported || (wheel && (release || motion || mode.tracking == MouseTracking::Press)) {
        return None;
    }
    let mut code: u32 = match button {
        Some(Button::Left) => 0,
        Some(Button::Middle) => 1,
        Some(Button::Right) => 2,
        Some(Button::WheelUp) => 64,
        Some(Button::WheelDown) => 65,
        None => 3,
    };
    if release && !mode.sgr {
        code = 3;
    }
    if motion {
        code += 32;
    }
    if mode.tracking != MouseTracking::Press {
        code += u32::from(modifiers.shift) * 4
            + u32::from(modifiers.alt) * 8
            + u32::from(modifiers.control) * 16;
    }
    let (row, column) = (at.0.checked_add(1)?, at.1.checked_add(1)?);
    let mut report = Report {
        bytes: [0; MAX_REPORT],
        length: 0,
    };
    if mode.sgr {
        report.push(b"\x1b[<")?;
        report.push_decimal(usize::try_from(code).ok()?)?;
        report.push(b";")?;
        report.push_decimal(column)?;
        report.push(b";")?;
        report.push_decimal(row)?;
        report.push(if release { b"m" } else { b"M" })?;
    } else {
        let byte = |value: usize| u8::try_from(value.checked_add(32)?).ok();
        let code = usize::try_from(code).ok()?;
        report.push(&[ESC, b'[', b'M', byte(code)?, byte(column)?, byte(row)?])?;
    }
    Some(report)
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
        usize::try_from(back)
            .unwrap_or(usize::MAX)
            .min(history.lines)
    }

    /// Whether anything but the live screen is showing. This is the question
    /// End asks, so it is the EFFECTIVE position rather than whether the
    /// viewport was ever opened: a view with nothing left to show is at the
    /// live bottom however it got there.
    pub fn viewing(&self, history: Scrollback) -> bool {
        self.offset(history) != 0
    }

    /// Apply what `action` decided. Bytes return to the live bottom: td-term/DESIGN.md §2's
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
            Action::Scroll(Scroll::Back) => {
                current.saturating_add(page_lines(rows)).min(history.lines)
            }
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

impl Default for InputQueue {
    fn default() -> Self {
        Self::new()
    }
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

/// The packaged binary's own check of the keyboard encoder: one translation
/// of each shape and one atomic queue rejection. It touches no device, so it
/// runs wherever the artifact does.
pub fn selftest() -> Result<(), String> {
    let escape = Modes::default();
    let application = Modes {
        application_cursor: true,
    };
    let found =
        |chord: &str, modes: Modes| sequence(chord, modes).map(|found| found.as_slice().to_vec());
    if found("a", escape).as_deref() != Some(b"a")
        || found("C-c", escape).as_deref() != Some(&[0x03][..])
        || found("M-a", escape).as_deref() != Some(&[ESC, b'a'][..])
        || found("Up", escape).as_deref() != Some(b"\x1b[A")
        || found("Up", application).as_deref() != Some(b"\x1bOA")
        || found("S-Up", escape).is_some()
        || action("S-PageUp", None, escape, false) != Action::Scroll(Scroll::Back)
        || action("End", None, escape, true) != Action::Scroll(Scroll::Bottom)
    {
        return Err("keyboard encoder selftest translated a chord wrongly".into());
    }
    let mut queue = InputQueue::new();
    if !queue.push(b"a") || queue.is_empty() || queue.len() != 1 {
        return Err("keyboard queue selftest lost an admitted sequence".into());
    }
    let mut narrow = InputQueue::with_capacity(1);
    if narrow.push(b"\x1bOA") || !narrow.take_dropped() || !narrow.is_empty() {
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
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn reported(
        event: Pointer,
        at: (usize, usize),
        modifiers: PointerModifiers,
        tracking: MouseTracking,
        sgr: bool,
    ) -> Option<Vec<u8>> {
        report(event, at, modifiers, MouseMode { tracking, sgr })
            .map(|found| found.as_slice().to_vec())
    }

    #[test]
    fn a_pointer_report_is_what_the_mode_asks_for_in_its_encoding() {
        use Button::*;
        use MouseTracking::*;
        let none = PointerModifiers::default();
        let all = PointerModifiers {
            shift: true,
            alt: true,
            control: true,
        };
        let x10 = |event, at, modifiers, tracking| reported(event, at, modifiers, tracking, false);
        let sgr = |event, at, modifiers, tracking| reported(event, at, modifiers, tracking, true);
        // Off reports nothing.
        for event in [
            Pointer::Press(Left),
            Pointer::Release(Left),
            Pointer::Motion(None),
        ] {
            assert_eq!(x10(event, (0, 0), none, Off), None);
        }
        // Mode 9: presses only, with no modifiers.
        assert_eq!(
            x10(Pointer::Press(Middle), (0, 0), all, Press).as_deref(),
            Some(&b"\x1b[M!!!"[..])
        );
        assert_eq!(x10(Pointer::Release(Middle), (0, 0), none, Press), None);
        // Mode 1000: a release is button 3; modifiers add 4, 8 and 16.
        assert_eq!(
            x10(Pointer::Press(Right), (2, 4), all, Click).as_deref(),
            Some(&b"\x1b[M>%#"[..])
        );
        assert_eq!(
            x10(Pointer::Release(Right), (2, 4), none, Click).as_deref(),
            Some(&b"\x1b[M#%#"[..])
        );
        assert_eq!(x10(Pointer::Motion(Some(Left)), (2, 4), none, Click), None);
        // Mode 1002: motion with a button held, plus 32; none without.
        assert_eq!(
            x10(Pointer::Motion(Some(Left)), (2, 4), none, Drag).as_deref(),
            Some(&b"\x1b[M@%#"[..])
        );
        assert_eq!(x10(Pointer::Motion(None), (2, 4), none, Drag), None);
        // Mode 1003: motion with no button is 35.
        assert_eq!(
            x10(Pointer::Motion(None), (2, 4), none, Motion).as_deref(),
            Some(&b"\x1b[MC%#"[..])
        );
        // The wheel presses and is never released or dragged.
        assert_eq!(
            x10(Pointer::Press(WheelDown), (0, 0), none, Click).as_deref(),
            Some(&b"\x1b[Ma!!"[..])
        );
        assert_eq!(x10(Pointer::Release(WheelUp), (0, 0), none, Click), None);
        assert_eq!(
            x10(Pointer::Motion(Some(WheelUp)), (0, 0), none, Motion),
            None
        );
        // X10's byte carries a cell up to the 223rd.
        assert_eq!(
            x10(Pointer::Press(Left), (222, 222), none, Click).as_deref(),
            Some(&[0x1b, b'[', b'M', b' ', 255, 255][..])
        );
        assert_eq!(x10(Pointer::Press(Left), (0, 223), none, Click), None);
        assert_eq!(x10(Pointer::Press(Left), (223, 0), none, Click), None);
        // SGR spells any cell, keeps a release's button and ends it in m.
        assert_eq!(
            sgr(Pointer::Press(Right), (9, 99), all, Click).as_deref(),
            Some(&b"\x1b[<30;100;10M"[..])
        );
        assert_eq!(
            sgr(Pointer::Release(Right), (9, 99), none, Click).as_deref(),
            Some(&b"\x1b[<2;100;10m"[..])
        );
        assert_eq!(
            sgr(Pointer::Motion(Some(Middle)), (16_383, 16_383), all, Motion).as_deref(),
            Some(&b"\x1b[<61;16384;16384M"[..])
        );
        assert_eq!(
            sgr(Pointer::Press(WheelUp), (0, 0), none, Click).as_deref(),
            Some(&b"\x1b[<64;1;1M"[..])
        );
        // Mode 9 reports no wheel, as in xterm.
        assert_eq!(sgr(Pointer::Press(WheelUp), (0, 0), none, Press), None);
        assert_eq!(x10(Pointer::Press(WheelDown), (0, 0), none, Press), None);
        assert_eq!(
            sgr(Pointer::Press(Left), (usize::MAX, 0), none, Click),
            None
        );
        // A coordinate SGR cannot hold in MAX_REPORT is refused, not cut.
        assert_eq!(
            sgr(
                Pointer::Press(Left),
                (usize::MAX - 1, usize::MAX - 1),
                none,
                Click
            ),
            None
        );
    }

    fn normal(chord: &str) -> Option<Vec<u8>> {
        sequence(chord, Modes::default()).map(|found| found.as_slice().to_vec())
    }

    fn act(chord: &str, viewing: bool) -> Action {
        action(chord, None, Modes::default(), viewing)
    }

    #[test]
    fn alt_sends_the_case_the_keymap_resolved_and_control_ignores_it() {
        let sent = |chord: &str, resolved: Option<char>| match action(
            chord,
            resolved,
            Modes::default(),
            false,
        ) {
            Action::Bytes(bytes) => bytes.as_slice().to_vec(),
            other => panic!("{chord}: {other:?}"),
        };
        // Caps Lock under Alt: the chord spells `a`, the key resolved `A`.
        assert_eq!(sent("M-a", Some('A')), b"\x1bA");
        assert_eq!(sent("M-S-a", Some('a')), b"\x1ba");
        assert_eq!(sent("M-a", Some('a')), b"\x1ba");
        assert_eq!(sent("M-S-a", Some('A')), b"\x1bA");
        // Without a resolved letter, Shift decides as before.
        assert_eq!(sent("M-S-a", None), b"\x1bA");
        // Control's byte has no case, and a character that is not the
        // chord's own letter is not taken for it.
        assert_eq!(sent("C-a", Some('A')), b"\x01");
        assert_eq!(sent("M-a", Some('B')), b"\x1ba");
        assert_eq!(sent("M-1", Some('1')), b"\x1b1");
    }

    #[test]
    fn chords_parse_their_prefixes_in_order_and_keep_a_bare_dash() {
        let parsed = |text| chord(text).map(|c| (c.control, c.alt, c.shift, c.base));
        assert_eq!(parsed("a"), Some((false, false, false, "a")));
        assert_eq!(parsed("C-M-S-a"), Some((true, true, true, "a")));
        assert_eq!(parsed("C--"), Some((true, false, false, "-")));
        assert_eq!(parsed("-"), Some((false, false, false, "-")));
        assert_eq!(parsed("S"), Some((false, false, false, "S")));
        assert_eq!(parsed("S-Tab"), Some((false, false, true, "Tab")));
        // Out of order, a prefix is not a prefix, and the base is unknown.
        assert_eq!(parsed("S-C-a"), Some((false, false, true, "C-a")));
        assert_eq!(normal("S-C-a"), None);
        assert_eq!(parsed(""), None);
        assert_eq!(normal(""), None);
    }

    #[test]
    fn text_is_the_character_the_keymap_chose() {
        assert_eq!(normal("a"), Some(b"a".to_vec()));
        assert_eq!(normal("A"), Some(b"A".to_vec()));
        assert_eq!(normal("!"), Some(b"!".to_vec()));
        assert_eq!(normal(" "), Some(b" ".to_vec()));
        assert_eq!(normal("~"), Some(b"~".to_vec()));
        // Alt prefixes ESC; Shift under Alt is the letter's upper level.
        assert_eq!(normal("M-a"), Some(b"\x1ba".to_vec()));
        assert_eq!(normal("M-S-a"), Some(b"\x1bA".to_vec()));
        assert_eq!(normal("M-Space"), Some(b"\x1b ".to_vec()));
        assert_eq!(normal("M-!"), Some(b"\x1b!".to_vec()));
        // Nothing past printable ASCII is text.
        assert_eq!(normal("\u{7f}"), None);
        assert_eq!(normal("ab"), None);
    }

    #[test]
    fn control_reaches_the_c0_spellings_and_nothing_else() {
        for (chord, byte) in [
            ("C-a", 0x01),
            ("C-S-a", 0x01),
            ("C-z", 0x1a),
            ("C-Space", 0x00),
            ("C-@", 0x00),
            ("C-[", 0x1b),
            ("C-\\", 0x1c),
            ("C-]", 0x1d),
            ("C-^", 0x1e),
            ("C-_", 0x1f),
            ("C-?", 0x7f),
        ] {
            assert_eq!(normal(chord), Some(vec![byte]), "{chord}");
        }
        assert_eq!(normal("C-M-a"), Some(vec![ESC, 0x01]));
        for silent in [
            "C-1", "C-!", "C-Up", "C-Return", "C-Tab", "C-F1", "C-Escape",
        ] {
            assert_eq!(normal(silent), None, "{silent}");
        }
    }

    #[test]
    fn named_keys_spell_their_profile_sequences() {
        for (chord, bytes) in [
            ("Escape", &b"\x1b"[..]),
            ("S-Escape", b"\x1b"),
            ("Backspace", b"\x7f"),
            ("S-Backspace", b"\x7f"),
            ("M-Backspace", b"\x1b\x7f"),
            ("Return", b"\r"),
            ("S-Return", b"\r"),
            ("Tab", b"\t"),
            ("S-Tab", b"\x1b[Z"),
            ("M-S-Tab", b"\x1b\x1b[Z"),
            ("Up", b"\x1b[A"),
            ("Home", b"\x1b[H"),
            ("End", b"\x1b[F"),
            ("M-Left", b"\x1b\x1b[D"),
            ("Insert", b"\x1b[2~"),
            ("Delete", b"\x1b[3~"),
            ("PageUp", b"\x1b[5~"),
            ("PageDown", b"\x1b[6~"),
            ("F1", b"\x1bOP"),
            ("F4", b"\x1bOS"),
            ("F5", b"\x1b[15~"),
            ("F12", b"\x1b[24~"),
        ] {
            assert_eq!(normal(chord).as_deref(), Some(bytes), "{chord}");
        }
        let application = Modes {
            application_cursor: true,
        };
        for (chord, bytes) in [
            ("Up", &b"\x1bOA"[..]),
            ("Down", b"\x1bOB"),
            ("Right", b"\x1bOC"),
            ("Left", b"\x1bOD"),
            ("Home", b"\x1bOH"),
            ("End", b"\x1bOF"),
            ("PageUp", b"\x1b[5~"),
        ] {
            let found = sequence(chord, application).map(|s| s.as_slice().to_vec());
            assert_eq!(found.as_deref(), Some(bytes), "{chord}");
        }
        // Shift has no second spelling on navigation and function keys.
        for silent in [
            "S-Up", "S-Home", "S-Insert", "S-Delete", "S-F1", "S-PageUp", "Menu",
        ] {
            assert_eq!(normal(silent), None, "{silent}");
        }
    }

    #[test]
    fn every_named_key_survives_an_alt_prefix() {
        for (name, _) in NAMED {
            let plain = normal(name).unwrap();
            let meta = normal(&format!("M-{name}")).unwrap();
            assert_eq!(meta[0], ESC, "{name}");
            assert_eq!(&meta[1..], &plain[..], "{name}");
        }
    }

    #[test]
    fn shift_paging_scrolls_the_viewport_and_sends_nothing() {
        assert_eq!(act("S-PageUp", false), Action::Scroll(Scroll::Back));
        assert_eq!(act("S-PageDown", false), Action::Scroll(Scroll::Forward));
        for viewing in [false, true] {
            assert!(matches!(act("PageUp", viewing), Action::Bytes(_)));
            assert!(matches!(act("PageDown", viewing), Action::Bytes(_)));
        }
        for chord in ["C-S-PageUp", "M-S-PageDown"] {
            assert_eq!(act(chord, true), Action::Silent, "{chord}");
        }
    }

    #[test]
    fn end_is_the_childs_at_the_bottom_and_the_viewports_above_it() {
        assert!(matches!(act("End", false), Action::Bytes(_)));
        assert_eq!(act("End", true), Action::Scroll(Scroll::Bottom));
        assert_eq!(act("S-End", true), Action::Silent);
        assert_eq!(act("C-End", true), Action::Silent);
        assert!(matches!(act("M-End", true), Action::Bytes(_)));
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

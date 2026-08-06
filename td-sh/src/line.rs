//! The interactive line editor: the one caller of the terminal surface.
//!
//! Without it td-sh's prompt is whatever the kernel's canonical line discipline
//! gives — no cursor movement, no history, and a Ctrl-C that KILLS the shell,
//! since td-sh installs no signal handler and `SIG_DFL` for SIGINT ends the
//! process. That last is why the editor clears `ISIG` as well as `ICANON`/`ECHO`
//! (see `term.rs`): with signal generation off, Ctrl-C arrives as a byte this
//! module can act on, so the shell abandons the line instead of dying at its own
//! prompt. Raw mode is taken PER LINE and dropped before the command runs, so a
//! running child still gets the ordinary Ctrl-C.
//!
//! Ctrl-C while a command is RUNNING is a different problem and not this
//! module's: the terminal signals the whole foreground group, so it reaches the
//! shell beside the command. `process.rs`'s `InterruptibleChild` is what answers
//! it — the shell stops listening for as long as the child is alive — and the
//! terminal is in a sane mode throughout, because the guard here is dropped
//! before any command runs.
//!
//! The line is kept inside ONE terminal row and scrolled horizontally, which is
//! why the width is needed and the height is not: a redraw that never moves off
//! its row cannot lose track of where the cursor is, whatever the terminal does
//! with wrapping.
//!
//! History is per PHYSICAL LINE, as ash's is: recalling a three-line `for` loop
//! takes three Ups, and a line from a unit later abandoned with Ctrl-C is still
//! in it. Keeping whole units instead needs the parser's idea of where one ends,
//! which the editor deliberately does not have.
//!
//! Widths are counted in COLUMNS, with the East Asian Wide and Fullwidth ranges
//! worth two (`columns`). Getting that wrong is not cosmetic: a row measured at
//! half its true width wraps, and everything here rests on the row never
//! wrapping. What is still approximate is the other direction — a combining mark
//! counts one where it occupies none, so a line of them scrolls early. That is
//! the safe error, since over-counting can only shorten the row, and correcting
//! it needs full grapheme segmentation rather than a range table.

use std::io::{IsTerminal, Read, Seek, Write};
use std::os::fd::AsFd;

use crate::complete;
use crate::term;

/// What a read ended with.
#[derive(Debug)]
pub enum Input {
    /// A complete line, without its newline.
    Line(String),
    /// End of input — Ctrl-D on an empty line, or a closed stdin.
    Eof,
    /// Ctrl-C: throw the line away and prompt again.
    Interrupted,
}

/// Editing state that outlives one line: the history, and where the operator has
/// browsed to in it.
pub struct Editor {
    history: Vec<String>,
}

/// The longest escape sequence the editor will read before giving up, so a
/// stream that never sends a final byte cannot hold the loop.
///
/// Generous rather than tight, because giving up is not free: past the bound the
/// remainder is dispatched as ordinary keystrokes, which is the very thing
/// consuming through the final byte exists to prevent. No terminal sends a CSI
/// anywhere near this long -- the longest in ordinary use are mouse and status
/// replies of a dozen bytes or so -- so the bound is the backstop it reads as
/// rather than something a real reply can cross.
const MAX_ESCAPE_BYTES: usize = 256;

/// The width to assume when the terminal will not say. 80 is the width every
/// terminal emulator still defaults to, and being wrong only scrolls the line at
/// the wrong column.
const DEFAULT_WIDTH: u16 = 80;

/// How many lines of history to keep. dash keeps none, bash defaults to 500;
/// this is a bounded middle that cannot grow a long-lived shell without limit.
const HISTORY_MAX: usize = 500;

impl Editor {
    pub fn new() -> Self {
        Self { history: Vec::new() }
    }

    /// Read one line, editing it in place when stdin is a terminal that will
    /// enter raw mode, and falling back to the kernel's own line discipline when
    /// it is not. The fallback is not an error path: a serial console that will
    /// not take `TCSETS` should still get a working shell.
    /// `interruptible` is false when the shell has `trap '' INT` in force: the
    /// signal the keystroke stands in for is IGNORED, so the keystroke must be
    /// too. Passed in rather than read from the kernel because the trap table is
    /// the shell's own record of what the operator asked for, and reading it
    /// here would make this a fourth caller of the syscall module.
    /// `comp` is what Tab may offer, passed in for the same reason `Keys` is:
    /// a test must be able to state the whole world.
    pub fn read(
        &mut self,
        prompt: &str,
        interruptible: bool,
        comp: &complete::Source<'_>,
    ) -> Input {
        let stdin = std::io::stdin();
        // An UNBUFFERED handle on the same file description, taken before raw
        // mode so a failure here needs no unwind. `std::io::Stdin` is a
        // `BufReader`, and in raw mode one read returns everything available --
        // so a pasted `head -1\ndata\n` would land ENTIRELY in the shell's own
        // buffer and the child would never see `data`, which canonical mode
        // could not do because it returns at most one line per read. One syscall
        // per keystroke is the right trade for a key at a time, and it leaves
        // the rest of a paste in the KERNEL where the next reader finds it.
        let Ok(dup) = stdin.as_fd().try_clone_to_owned() else {
            return read_cooked(prompt);
        };
        let mut src = std::fs::File::from(dup);
        let Some(mut out) = sink() else {
            return read_cooked(prompt);
        };
        let Ok(raw) = term::raw(stdin.as_fd()) else {
            return read_cooked(prompt);
        };
        let tty = stdin.as_fd();
        // A second dup of the SINK for the size probe: `edit` holds the sink
        // mutably, and the width has to come from the terminal being drawn on
        // rather than the one being read -- they are the same descriptor in
        // every ordinary session and different ones the moment stdout is not a
        // terminal. If the dup fails there is still a terminal to ask.
        let probe = out.try_clone().ok();
        let mut next = || next_byte(&mut src);
        let mut width = || match &probe {
            Some(f) => term::width(f.as_fd()),
            None => term::width(tty),
        };
        let mut keys = Keys {
            next: &mut next,
            width: &mut width,
            // The two bytes the DRIVER owns, so `stty intr '^X'` moves them.
            intr: raw.intr(),
            eof: raw.eof(),
        };
        let outcome = self.edit(prompt, &mut keys, &mut out, interruptible, comp);
        // Put the terminal back BEFORE the newline that ends the line, so what
        // follows — a command's output, or the next prompt — starts on a
        // terminal in its own mode rather than in the editor's.
        drop(raw);
        if let Input::Line(text) = &outcome {
            let _ = writeln!(out);
            self.remember(text);
        }
        outcome
    }

    /// Keep `text` in the history, as ash does: blank lines are not kept, and a
    /// line identical to the previous one is not kept twice.
    fn remember(&mut self, text: &str) {
        if text.trim().is_empty() || self.history.last().map(String::as_str) == Some(text) {
            return;
        }
        if self.history.len() >= HISTORY_MAX {
            self.history.remove(0);
        }
        self.history.push(text.to_string());
    }

    fn edit<W: Write>(
        &mut self,
        prompt: &str,
        keys: &mut Keys<'_>,
        out: &mut W,
        interruptible: bool,
        src: &complete::Source<'_>,
    ) -> Input {
        let mut buf = String::new();
        // The cursor as a BYTE index, always on a character boundary.
        let mut pos = 0usize;
        // Where in the history the operator has browsed to. `len()` is "the line
        // being typed"; the draft is parked here so browsing away and back does
        // not lose it.
        let mut browse = self.history.len();
        let mut draft = String::new();
        // A byte an escape sequence turned out not to want, re-dispatched on the
        // next turn: ESC followed by Enter must submit the line, not eat it.
        let mut pending: Option<u8> = None;
        // Whether the LAST keystroke was a Tab, which is what makes the second
        // one a listing. Cleared by every other key, as ash's `lastWasTab` is.
        let mut last_tab = false;

        // Re-read on every keystroke rather than once: td-sh installs no
        // SIGWINCH handler, and `TIOCGWINSZ` is cheap next to a key press, so
        // this is what makes a terminal resized mid-line redraw correctly.
        let mut width = (keys.width)().unwrap_or(DEFAULT_WIDTH);
        draw(out, prompt, &buf, pos, width);
        loop {
            let Some(b) = pending.take().or_else(&mut *keys.next) else {
                // A closed stdin mid-line is end of input, not an empty line.
                return Input::Eof;
            };
            let was_tab = std::mem::replace(&mut last_tab, false);
            // The two bytes the DRIVER owns come from the terminal's own
            // settings, so `stty intr '^X'` moves them; the editing keys below
            // are the conventional readline bindings rather than the driver's.
            if Some(b) == keys.intr {
                // Ignored signal, ignored keystroke: `trap '' INT` says SIGINT
                // does nothing, and this byte only stands in for SIGINT.
                if !interruptible {
                    continue;
                }
                // Just `\n`: the output flags are untouched, so ONLCR is still
                // turning it into a carriage return and line feed. The `^C` echo
                // is what tells the operator the keystroke was seen.
                let _ = writeln!(out, "^C");
                return Input::Interrupted;
            }
            if Some(b) == keys.eof {
                // End of input on an empty line, delete-forward otherwise — the
                // split every shell makes, so a stray Ctrl-D mid-line cannot end
                // the session.
                if buf.is_empty() {
                    // The newline is what stops the shell exiting with the
                    // cursor parked on the prompt it drew.
                    let _ = writeln!(out);
                    return Input::Eof;
                }
                delete_forward(&mut buf, pos);
                width = (keys.width)().unwrap_or(width);
                draw(out, prompt, &buf, pos, width);
                continue;
            }
            match b {
                // Enter. Both spellings, since a terminal in raw mode may send
                // either depending on ICRNL.
                b'\r' | b'\n' => return Input::Line(buf),
                // Ctrl-A / Ctrl-E.
                0x01 => pos = 0,
                0x05 => pos = buf.len(),
                // Ctrl-B / Ctrl-F.
                0x02 => pos = prev_boundary(&buf, pos),
                0x06 => pos = next_boundary(&buf, pos),
                // Ctrl-K / Ctrl-U: kill to end / to start.
                0x0b => buf.truncate(pos),
                0x15 => {
                    buf.drain(..pos);
                    pos = 0;
                }
                // Ctrl-W: kill the word before the cursor.
                0x17 => {
                    let start = word_start(&buf, pos);
                    buf.drain(start..pos);
                    pos = start;
                }
                // Ctrl-L: clear the screen and redraw on the top line.
                0x0c => {
                    let _ = write!(out, "\x1b[H\x1b[2J");
                }
                // Ctrl-P / Ctrl-N, and the arrows below arrive here too.
                0x10 => self.browse(-1, &mut browse, &mut buf, &mut pos, &mut draft),
                0x0e => self.browse(1, &mut browse, &mut buf, &mut pos, &mut draft),
                // Backspace, both spellings.
                0x7f | 0x08 => {
                    let start = prev_boundary(&buf, pos);
                    if start != pos {
                        // Boundary-safe by the invariant `prev_boundary` keeps;
                        // `remove` shifts the tail in place rather than
                        // rebuilding the line around it.
                        buf.remove(start);
                        pos = start;
                    }
                }
                0x1b => match escape(&mut *keys.next) {
                    Escape::Key(Key::Up) => {
                        self.browse(-1, &mut browse, &mut buf, &mut pos, &mut draft)
                    }
                    Escape::Key(Key::Down) => {
                        self.browse(1, &mut browse, &mut buf, &mut pos, &mut draft)
                    }
                    Escape::Key(Key::Left) => pos = prev_boundary(&buf, pos),
                    Escape::Key(Key::Right) => pos = next_boundary(&buf, pos),
                    Escape::Key(Key::Home) => pos = 0,
                    Escape::Key(Key::End) => pos = buf.len(),
                    Escape::Key(Key::Delete) => delete_forward(&mut buf, pos),
                    Escape::Literal(b) => pending = Some(b),
                    Escape::Unknown => {}
                },
                // Tab completes the word under the cursor, and a SECOND Tab
                // lists what it could not choose between -- ash's `lastWasTab`.
                // With nothing to complete the byte is TYPED instead: `<<-EOF`
                // is a here-doc form whose whole point is the leading tab, so a
                // prompt that cannot enter one cannot enter that script. It is
                // DRAWN as a space (see `visible`) so one buffer character stays
                // one column.
                b'\t' => match complete::complete(&buf, pos, src) {
                    None => {
                        buf.insert(pos, '\t');
                        pos += 1;
                    }
                    Some(c) => {
                        // A UNIQUE match finished the word, so the next Tab is
                        // not a double-tab -- busybox clears `lastWasTab` in
                        // exactly that branch (lineedit.c:1329). Without this,
                        // a Tab after `pw<Tab>` sees an empty word past a
                        // command and dumps the whole directory.
                        last_tab = c.matches.len() > 1;
                        // Boundary-safe: `start` comes from `char_indices` and
                        // `end` is `pos`, which the invariant above keeps on a
                        // boundary.
                        if Some(c.insert.as_str()) != buf.get(c.start..c.end) {
                            buf.replace_range(c.start..c.end, &c.insert);
                            pos = c.start.saturating_add(c.insert.len());
                        } else if was_tab {
                            let _ = writeln!(out);
                            let _ = write!(out, "{}", complete::listing(&c.matches, width));
                            // The listing is on the screen, so the Tab AFTER
                            // it does nothing and the one after that lists
                            // again -- busybox's same alternation, which beeps
                            // where this is silent.
                            last_tab = false;
                        }
                    }
                },
                // Any other control byte is ignored rather than inserted: a
                // literal 0x00 or 0x1a in the line would reach the parser as
                // something no script can contain.
                0x00..=0x1f => {}
                _ => {
                    // `insert` is boundary-safe by invariant: `pos` only ever
                    // comes from 0, `buf.len()`, or the two boundary walkers.
                    let (typed, back) = character(&mut *keys.next, b);
                    if let Some(c) = typed {
                        buf.insert(pos, c);
                        pos += c.len_utf8();
                    }
                    pending = back;
                }
            }
            width = (keys.width)().unwrap_or(width);
            draw(out, prompt, &buf, pos, width);
        }
    }

    /// Step `delta` through the history, parking the line being typed at the
    /// far end so browsing away and back does not lose it.
    fn browse(
        &self,
        delta: isize,
        browse: &mut usize,
        buf: &mut String,
        pos: &mut usize,
        draft: &mut String,
    ) {
        let len = self.history.len();
        let next = match delta {
            d if d < 0 => match browse.checked_sub(1) {
                Some(n) => n,
                None => return,
            },
            _ if *browse >= len => return,
            _ => *browse + 1,
        };
        if *browse == len {
            *draft = buf.clone();
        }
        *browse = next;
        *buf = match self.history.get(next) {
            Some(line) => line.clone(),
            None => draft.clone(),
        };
        *pos = buf.len();
    }
}

/// What `edit` needs of the terminal, gathered rather than reached for.
///
/// The key dispatch is where the "`pos` is always on a character boundary"
/// invariant lives, and a violation of it is an immediate panic inside
/// `String::insert`. A test that cannot press a key cannot check that, so the
/// terminal arrives as four fields instead of as three descriptors.
struct Keys<'a> {
    next: &'a mut dyn FnMut() -> Option<u8>,
    /// `None` when the width cannot be read, so the caller keeps the last one it
    /// knew rather than snapping to the default mid-line.
    width: &'a mut dyn FnMut() -> Option<u16>,
    /// `None` when the terminal has the character DISABLED. Linux spells that
    /// `_POSIX_VDISABLE`, which is a zero byte -- and a zero taken as an ordinary
    /// binding would make NUL abandon the line or end the session.
    intr: Option<u8>,
    eof: Option<u8>,
}

/// What the bytes after an ESC turned out to be.
#[derive(PartialEq, Eq, Debug)]
enum Escape {
    Key(Key),
    /// Not a sequence: this byte is the operator's next keystroke.
    Literal(u8),
    /// A sequence with no meaning here, consumed to its terminator.
    Unknown,
}

/// The keys that arrive as an escape sequence rather than as one byte.
#[derive(PartialEq, Eq, Debug)]
enum Key {
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    Delete,
}

/// Decode the rest of a `CSI`/`SS3` sequence after the ESC.
///
/// A lone ESC blocks here until the next keystroke, because `VMIN`/`VTIME` are
/// 1/0 — the editor waits rather than guessing, which is what busybox's does. A
/// sequence this does not know is swallowed whole rather than typed into the
/// line, so an unrecognised function key does nothing instead of inserting `[15~`
/// — but a byte that begins no sequence at all comes BACK as `Literal`, so ESC
/// followed by Enter submits the line rather than losing the Enter.
fn escape<F: FnMut() -> Option<u8>>(mut next: F) -> Escape {
    let Some(intro) = next() else {
        return Escape::Unknown;
    };
    if intro != b'[' && intro != b'O' {
        return Escape::Literal(intro);
    }
    // A CSI runs: parameter bytes (0x30..=0x3f, which is the digits plus `;`),
    // then intermediates (0x20..=0x2f), then ONE final byte (0x40..=0x7e). Every
    // one of them has to be consumed here, or the tail lands in the line as
    // text: Ctrl-Left is `ESC [ 1 ; 5 D`, and stopping at the `;` would type
    // `5D` into the command. Only the FIRST parameter is kept, which is the one
    // that distinguishes Home/End/Delete.
    let mut param = 0u32;
    let mut first_done = false;
    // A bound, so a stream that never sends a final byte cannot spin here.
    for _ in 0..MAX_ESCAPE_BYTES {
        let Some(b) = next() else {
            return Escape::Unknown;
        };
        match b {
            b'0'..=b'9' if !first_done => {
                param = param.saturating_mul(10).saturating_add(u32::from(b - b'0'));
            }
            // Any other parameter byte ends the first parameter; the rest are
            // read and dropped.
            0x30..=0x3f => first_done = true,
            // Intermediates.
            0x20..=0x2f => {}
            0x40..=0x7e => {
                return match (b, param) {
                    (b'A', _) => Escape::Key(Key::Up),
                    (b'B', _) => Escape::Key(Key::Down),
                    (b'C', _) => Escape::Key(Key::Right),
                    (b'D', _) => Escape::Key(Key::Left),
                    (b'H', _) | (b'~', 1) | (b'~', 7) => Escape::Key(Key::Home),
                    (b'F', _) | (b'~', 4) | (b'~', 8) => Escape::Key(Key::End),
                    (b'~', 3) => Escape::Key(Key::Delete),
                    _ => Escape::Unknown,
                };
            }
            _ => return Escape::Unknown,
        }
    }
    Escape::Unknown
}

/// Complete a UTF-8 character whose lead byte is `first`, and any byte read that
/// turned out not to belong to it.
///
/// An invalid sequence is dropped rather than inserted: the line is a `String`,
/// so a byte that cannot be part of one has nowhere to go, and silently dropping
/// it is what leaves the rest of the line editable. Reading STOPS at the first
/// byte that is not a continuation, which is handed back rather than eaten: a
/// truncated `é` followed by `b` should lose the `é`, not the `b`, and stopping
/// early is what keeps that to the ONE byte the caller can re-dispatch.
fn character<F: FnMut() -> Option<u8>>(mut next: F, first: u8) -> (Option<char>, Option<u8>) {
    let extra = match first {
        0x00..=0x7f => 0,
        0xc2..=0xdf => 1,
        0xe0..=0xef => 2,
        0xf0..=0xf4 => 3,
        _ => return (None, None),
    };
    // A stack array: this runs once per keystroke, and a character is four
    // bytes at most.
    let mut bytes = [first, 0, 0, 0];
    for i in 1..=extra {
        let Some(b) = next() else {
            return (None, None);
        };
        if !(0x80..=0xbf).contains(&b) {
            return (None, Some(b));
        }
        let Some(slot) = bytes.get_mut(i) else {
            return (None, Some(b));
        };
        *slot = b;
    }
    let c = bytes
        .get(..=extra)
        .and_then(|b| std::str::from_utf8(b).ok())
        .and_then(|s| s.chars().next());
    (c, None)
}

fn next_byte(src: &mut std::fs::File) -> Option<u8> {
    let mut one = [0u8; 1];
    loop {
        return match src.read(&mut one) {
            Ok(1) => one.first().copied(),
            // EINTR is not end of input: every `None` here ends the SESSION, and
            // a signal that arrives while the operator is between keystrokes is
            // not them closing the terminal. Bounded by the read itself, which
            // blocks until a key or a real error.
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            _ => None,
        };
    }
}

fn delete_forward(buf: &mut String, pos: usize) {
    if pos < buf.len() {
        buf.remove(pos);
    }
}

/// The byte index one character before `pos`, or `pos` at the start.
fn prev_boundary(buf: &str, pos: usize) -> usize {
    match buf.get(..pos).and_then(|s| s.chars().next_back()) {
        Some(c) => pos.saturating_sub(c.len_utf8()),
        None => pos,
    }
}

/// The byte index one character after `pos`, or `pos` at the end.
fn next_boundary(buf: &str, pos: usize) -> usize {
    match buf.get(pos..).and_then(|s| s.chars().next()) {
        Some(c) => pos + c.len_utf8(),
        None => pos,
    }
}

/// Where the word before `pos` begins: trailing blanks, then the run of
/// non-blanks before them — readline's `unix-word-rubout`, which is what Ctrl-W
/// has meant since it was a terminal driver feature.
fn word_start(buf: &str, pos: usize) -> usize {
    let mut at = pos;
    while at > 0 {
        let prev = prev_boundary(buf, at);
        match buf.get(prev..at).and_then(|s| s.chars().next()) {
            Some(c) if c.is_whitespace() => at = prev,
            _ => break,
        }
    }
    while at > 0 {
        let prev = prev_boundary(buf, at);
        match buf.get(prev..at).and_then(|s| s.chars().next()) {
            Some(c) if !c.is_whitespace() => at = prev,
            _ => break,
        }
    }
    at
}

/// Redraw prompt and line on ONE row, scrolled so the cursor is visible.
fn draw<W: Write>(out: &mut W, prompt: &str, buf: &str, pos: usize, width: u16) {
    let (text, cursor_col) = visible(prompt, buf, pos, width);
    // `\r` to column zero, the row, `\x1b[K` to wipe whatever was longer than
    // what is being drawn now, then `\r` and a forward move to the cursor. Two
    // absolute moves rather than relative ones, so a dropped byte cannot leave
    // the cursor drifting further each keystroke.
    let mut text_out = String::with_capacity(text.len() + 16);
    text_out.push('\r');
    text_out.push_str(&text);
    text_out.push_str("\x1b[K\r");
    if cursor_col > 0 {
        use std::fmt::Write as _;
        let _ = write!(text_out, "\x1b[{cursor_col}C");
    }
    // One write, not a write per piece: a File is unbuffered, and a redraw that
    // reached the terminal in fragments would be visible as one.
    let _ = out.write_all(text_out.as_bytes());
}

/// The row to draw and the column the cursor sits in.
///
/// Split out from `draw` because it is the part with the arithmetic in it, and
/// the only part a test without a terminal can check.
fn visible(prompt: &str, buf: &str, pos: usize, width: u16) -> (String, usize) {
    let width = usize::from(width).max(1);
    // Only the LAST line of a multi-line prompt. Everything here rests on the
    // redraw never leaving its row, and a prompt with a newline in it would
    // scroll the screen on every keystroke; `\r` could then never get back to
    // where the line started.
    let prompt = prompt.rsplit('\n').next().unwrap_or(prompt);
    let prompt_cols = display_cols(prompt);
    // A prompt at least as wide as the terminal leaves nowhere to type, so it is
    // dropped for this redraw rather than allowed to squeeze the line to zero.
    let (prompt, prompt_cols) = if prompt_cols + 1 >= width {
        ("", 0)
    } else {
        (prompt, prompt_cols)
    };
    // One column is left EMPTY at the right margin. A character printed into
    // the last column arms the auto-wrap on terminals that wrap immediately
    // rather than at the next printable byte, and then the `\r` below returns to
    // column zero of the NEXT row -- leaving the prompt behind and scrolling the
    // screen once per keystroke. One column of width is the cheap side of that.
    let room = width.saturating_sub(prompt_cols).saturating_sub(1).max(1);
    // Walk BACK from the cursor while the characters fit, then forward from
    // where that stopped. Anchoring the window to the cursor rather than to the
    // line's start is what keeps a scrolled line's cursor on the row, and doing
    // it in COLUMNS rather than characters is what keeps a wide one from
    // wrapping: `漢` occupies two, so five of them on a ten-column terminal fill
    // the row that five `a`s would half fill.
    let head = buf.get(..pos).unwrap_or("");
    let mut start = pos;
    let mut used = 0usize;
    for (at, c) in head.char_indices().rev() {
        let w = columns(c);
        if used + w > room.saturating_sub(1) {
            break;
        }
        used += w;
        start = at;
    }
    let mut text = String::with_capacity(prompt.len() + room * 4);
    text.push_str(prompt);
    let mut cols = 0usize;
    for c in buf.get(start..).unwrap_or("").chars() {
        let w = columns(c);
        if cols + w > room {
            break;
        }
        cols += w;
        // A tab is drawn as a space: it is one CHARACTER in the buffer, and
        // letting the terminal advance it to the next multiple of eight would
        // put every column computed here in the wrong place. What gets
        // submitted is still the tab.
        text.push(if c == '\t' { ' ' } else { c });
    }
    (text, prompt_cols + used)
}

/// How many terminal columns `c` occupies.
///
/// Two for the East Asian Wide and Fullwidth ranges, one for everything else.
/// Counting a wide character as one is not a cosmetic error: the whole redraw
/// rests on the row never wrapping, and a row measured at half its true width
/// wraps, after which `\r` returns to a line below the prompt and the screen
/// scrolls once per keystroke. The ranges are the stable blocks, held here
/// rather than pulled from a crate, since a dependency is what this table exists
/// to avoid. A COMBINING mark counts one where it occupies none, so a line of
/// them scrolls early -- the safe direction, since over-counting can only
/// shorten the row.
fn columns(c: char) -> usize {
    const WIDE: &[(u32, u32)] = &[
        (0x1100, 0x115f),   // Hangul Jamo initial consonants
        (0x2e80, 0x303e),   // CJK radicals, Kangxi, CJK symbols and punctuation
        (0x3041, 0x33ff),   // kana, Hangul compatibility jamo, CJK compatibility
        (0x3400, 0x4dbf),   // CJK unified ideographs extension A
        (0x4e00, 0x9fff),   // CJK unified ideographs
        (0xa000, 0xa4cf),   // Yi
        (0xac00, 0xd7a3),   // Hangul syllables
        (0xf900, 0xfaff),   // CJK compatibility ideographs
        (0xfe10, 0xfe19),   // vertical forms
        (0xfe30, 0xfe6f),   // CJK compatibility forms, small form variants
        (0xff00, 0xff60),   // fullwidth forms
        (0xffe0, 0xffe6),   // fullwidth signs
        (0x1f300, 0x1f64f), // emoji: symbols/pictographs and emoticons
        (0x1f900, 0x1f9ff), // supplemental symbols and pictographs
        (0x20000, 0x3fffd), // CJK unified ideographs extensions B and beyond
    ];
    let cp = u32::from(c);
    if WIDE.iter().any(|(lo, hi)| (*lo..=*hi).contains(&cp)) {
        2
    } else {
        1
    }
}

/// What a prompt escape can ask about.
///
/// Separated from the expansion so the escape rules are a pure function of
/// strings: `render` can then be tested exhaustively without a filesystem, a
/// home directory or a uid, none of which a unit test can arrange.
struct PromptFacts {
    user: Option<String>,
    host: Option<String>,
    cwd: Option<String>,
    home: Option<String>,
    root: bool,
}

/// The effective uid, out of `/proc/self/status` rather than `getuid(2)`.
///
/// `\$` is the only escape that needs it, and reading it is what keeps this
/// off td-sh's syscall surface — the roster in UNSAFE.md §8 stays at four.
/// The EFFECTIVE uid is the second field, which is the one that decides
/// whether the shell can write where root can.
fn effective_uid() -> Option<u32> {
    effective_uid_in(&std::fs::read_to_string("/proc/self/status").ok()?)
}

/// The parse, separated so it can be tested with the four ids DIFFERING.
/// They are equal in every process the gate can run as, so a test against the
/// real `/proc/self/status` would pass whichever of them this picked.
fn effective_uid_in(status: &str) -> Option<u32> {
    for line in status.lines() {
        if let Some(ids) = line.strip_prefix("Uid:") {
            // `real effective saved fs` — the second is the one that decides
            // what the shell may write, so it is the one `\$` reports.
            return ids.split_whitespace().nth(1)?.parse().ok();
        }
    }
    None
}

/// The login name for `uid`, out of `/etc/passwd`.
///
/// By uid rather than by `$USER`, because `$USER` is inherited and survives an
/// `su`: a prompt that still said the old name after switching user would be
/// the one thing this escape exists to prevent. `$USER` is the fallback for an
/// image with no passwd file, not the first answer.
fn passwd_name(uid: u32) -> Option<String> {
    let passwd = std::fs::read_to_string("/etc/passwd").ok()?;
    for line in passwd.lines() {
        let mut fields = line.split(':');
        // `continue`, not `?`: a malformed line skips itself rather than
        // abandoning the rest of the file.
        let Some(name) = fields.next() else { continue };
        let _password = fields.next();
        if fields.next().and_then(|u| u.parse::<u32>().ok()) == Some(uid) {
            return Some(name.to_string());
        }
    }
    None
}

/// What the SHELL knows that a prompt escape may ask about.
///
/// Passed in rather than read here, because the two that matter are not facts
/// about the process. `cd` moves the shell's own `logical_cwd` and never
/// chdirs the process, so `std::env::current_dir()` answers where the shell
/// STARTED for the rest of its life; and `$USER` may have been assigned in the
/// shell without reaching the environment.
pub struct PromptEnv<'a> {
    pub home: Option<&'a str>,
    pub user: Option<&'a str>,
    pub cwd: &'a std::path::Path,
}

impl PromptFacts {
    /// Gather only what `raw` actually asks about: a prompt is rendered once
    /// per line, and the common `\w \$` has no business scanning `/etc/passwd`.
    fn gather(raw: &str, env: &PromptEnv<'_>) -> PromptFacts {
        let (mut user, mut host, mut cwd, mut root) = (false, false, false, false);
        let mut chars = raw.chars();
        while let Some(c) = chars.next() {
            if c != '\\' {
                continue;
            }
            match chars.next() {
                Some('u') => user = true,
                Some('h' | 'H') => host = true,
                Some('w' | 'W') => cwd = true,
                Some('$') => root = true,
                _ => {}
            }
        }
        let uid = if user || root { effective_uid() } else { None };
        PromptFacts {
            user: if user {
                uid.and_then(passwd_name)
                    .or_else(|| env.user.map(str::to_string))
            } else {
                None
            },
            // The kernel's own answer, so it follows `hostname` being set
            // during boot rather than whatever `/etc/hostname` was written
            // with. Reading it is why `gethostname(2)` is not needed.
            host: if host {
                std::fs::read_to_string("/proc/sys/kernel/hostname")
                    .ok()
                    .map(|h| h.trim_end().to_string())
                    .filter(|h| !h.is_empty())
                    .or_else(|| {
                        std::fs::read_to_string("/etc/hostname")
                            .ok()
                            .map(|h| h.trim_end().to_string())
                            .filter(|h| !h.is_empty())
                    })
            } else {
                None
            },
            cwd: if cwd {
                Some(env.cwd.display().to_string())
            } else {
                None
            },
            home: env.home.map(str::to_string),
            // Only `\$` asks, and an unreadable uid is not root: the prompt
            // that understates privilege is the safe way to be wrong.
            root: root && uid == Some(0),
        }
    }

    /// `\w`: the working directory with `$HOME` written `~`, as bash does.
    ///
    /// A `HOME` with a trailing slash abbreviates NOTHING, which is bash's
    /// behaviour too rather than an oversight: the prefix it compares is the
    /// variable as written.
    fn tilde_cwd(&self) -> String {
        let Some(cwd) = self.cwd.as_deref() else {
            return "?".to_string();
        };
        // `HOME=/` abbreviates NOTHING, which is bash's rule (its
        // `polite_directory_format` wants `strlen(HOME) > 1`) and not an
        // accident: under it every path begins with `$HOME`, so `~` would
        // swallow the leading slash of all of them. A root shell with `HOME=/`
        // is an ordinary td configuration, so this is reachable.
        let Some(home) = self.home.as_deref().filter(|h| h.len() > 1) else {
            return cwd.to_string();
        };
        if cwd == home {
            return "~".to_string();
        }
        match cwd.strip_prefix(home) {
            Some(rest) if rest.starts_with('/') => format!("~{rest}"),
            _ => cwd.to_string(),
        }
    }
}

/// Expand the backslash escapes in a `PS1`/`PS2` value.
///
/// Without this the shipped prompt prints as the literal `\u@\h:\w\$`, which is
/// what it did: td-sh set a bash-shaped default (`\w \$ `) and the image's
/// profile exports another, and NOTHING expanded either. Both are written the
/// way bash and busybox ash write them, so bash's meanings are the ones
/// implemented here.
///
/// Deliberately NOT implemented, and so left standing as the literal text they
/// are: the time and date escapes (`\d`, `\t`, `\T`, `\@`, `\A`). Rendering one
/// means turning a `SystemTime` into a civil date, which is a calendar — leap
/// years, the local zone, a month table — and none of td's prompts asks for it.
/// Leaving them alone is also the safer error: `\t` is bash's TIME, not a tab,
/// so a guess would silently produce whitespace where the operator asked for a
/// clock. `\!`, `\#`, `\j`, `\l`, `\s`, `\v` and `\V` are left for the same
/// reason — nothing shipped uses them, and a wrong answer looks like a right
/// one.
///
/// Two limits worth writing down rather than leaving to be discovered. `\u` is
/// answered out of `/etc/passwd`, so a user that exists only in a network
/// directory (LDAP, NIS) falls back to `$USER`; td ships no name service to
/// ask. And `\n` reaches the terminal, but the editor keeps the line being
/// edited inside ONE row and draws only the prompt's LAST row (see `visible`),
/// so a two-line `PS1` shows its second line while editing.
pub fn expand_prompt(raw: &str, env: &PromptEnv<'_>) -> String {
    render_prompt(raw, &PromptFacts::gather(raw, env))
}

/// The escape rules themselves, over facts already gathered.
fn render_prompt(raw: &str, facts: &PromptFacts) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        let Some(esc) = chars.next() else {
            // A trailing backslash is itself, as it is in bash.
            out.push('\\');
            break;
        };
        match esc {
            'u' => out.push_str(facts.user.as_deref().unwrap_or("?")),
            // `\h` stops at the first dot, `\H` does not.
            'h' => {
                let host = facts.host.as_deref().unwrap_or("?");
                out.push_str(host.split('.').next().unwrap_or(host));
            }
            'H' => out.push_str(facts.host.as_deref().unwrap_or("?")),
            'w' => out.push_str(&facts.tilde_cwd()),
            // Over the TILDE form, not the raw path: bash's `\W` in the home
            // directory itself is `~`, not the basename of `$HOME`.
            'W' => {
                let cwd = facts.tilde_cwd();
                // The root directory is its own basename; anything else is the
                // text after the last separator.
                let base = match cwd.rsplit_once('/') {
                    Some((_, tail)) if !tail.is_empty() => tail,
                    _ => cwd.as_str(),
                };
                out.push_str(if base.is_empty() { "/" } else { base });
            }
            '$' => out.push(if facts.root { '#' } else { '$' }),
            'n' => out.push('\n'),
            'r' => out.push('\r'),
            'a' => out.push('\x07'),
            'e' => out.push('\x1b'),
            '\\' => out.push('\\'),
            // bash's non-printing markers. They are DROPPED rather than
            // honoured, because `display_cols` already skips the escape
            // sequences they usually wrap; what matters is that they do not
            // print as `\[`.
            '[' | ']' => {}
            // `\nnn`, up to three octal digits, read as bash's `read_octal`
            // reads them -- which is neither "one to three" nor "exactly
            // three". bash scans a THREE-BYTE window, so a short run counts
            // only when nothing follows it: `\7` alone is a bell, `\7|` is the
            // text `\7|`. The value is then a BYTE, so it WRAPS: `\555` is
            // `m`, which the corpus names in a case of its own.
            '0'..='7' => {
                let mut digits = String::from(esc);
                while digits.len() < 3 {
                    match chars.peek().copied().filter(|c| c.is_digit(8)) {
                        Some(d) => {
                            digits.push(d);
                            chars.next();
                        }
                        None => break,
                    }
                }
                let short_run = digits.len() < 3 && chars.peek().is_some();
                let byte = digits
                    .chars()
                    .fold(0u32, |acc, c| acc * 8 + c.to_digit(8).unwrap_or(0))
                    % 0x100;
                if short_run {
                    out.push('\\');
                    out.push_str(&digits);
                } else if byte == 0 {
                    // bash emits nothing for a NUL, and a NUL in a prompt is
                    // a byte the terminal would count and not draw.
                } else if byte < 0x80 {
                    out.extend(char::from_u32(byte));
                } else {
                    // Above ASCII bash emits a RAW byte, which this `String`
                    // cannot carry: `\377` is 0xff there and would become two
                    // UTF-8 bytes here. Left as the text it was, rather than
                    // silently drawn as a different character.
                    out.push('\\');
                    out.push_str(&digits);
                }
            }
            // Anything else keeps its backslash, which is what bash does with
            // an escape it does not know.
            other => {
                out.push('\\');
                out.push(other);
            }
        }
    }
    out
}

/// How many columns `text` occupies, skipping escape sequences.
///
/// A coloured `PS1` is ordinary here — `PS1='\033[32m$ \033[0m'` is a common
/// setting — and counting its escape bytes as columns would scroll the line
/// early and leave the cursor a dozen columns off, every keystroke. Only the
/// COUNT skips them; the prompt is still drawn verbatim, so the terminal sees
/// the colours. The line being edited needs no such treatment: control bytes
/// are dropped on input, so a `\x1b` cannot get into the buffer.
pub fn display_cols(text: &str) -> usize {
    let mut cols = 0usize;
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c != '\x1b' {
            // A carriage return puts the cursor back at the left margin, so
            // what precedes it occupies nothing: `PS1='xxx\ry'` is one column
            // wide, not four. Every other control byte draws nothing and so
            // counts nothing -- BEL above all, which `\a` now puts in reach.
            match c {
                '\r' => cols = 0,
                c if (c as u32) < 0x20 || c == '\u{7f}' => {}
                c => cols += columns(c),
            }
            continue;
        }
        // CSI (`ESC [`) and SS3 (`ESC O`) run to a final byte in 0x40..=0x7e;
        // OSC (`ESC ]`) is the one a prompt uses to set a window title, and
        // runs to BEL or to ST (`ESC \`). Anything else after ESC is a single
        // character sequence already consumed by `chars.next()` above.
        match chars.next() {
            Some('[' | 'O') => {
                for f in chars.by_ref() {
                    if ('\u{40}'..='\u{7e}').contains(&f) {
                        break;
                    }
                }
            }
            Some(']') => {
                while let Some(f) = chars.next() {
                    if f == '\x07' {
                        break;
                    }
                    if f == '\x1b' && chars.next() == Some('\\') {
                        break;
                    }
                }
            }
            _ => {}
        }
    }
    cols
}

/// The descriptor the editor DRAWS on, which is not necessarily stdout.
///
/// An interactive shell whose stdout is redirected — `td-sh -i > log` — still
/// has to show the operator the line they are editing, and must not pour redraw
/// escapes into the file; readline does the same, defaulting its output to
/// stderr. Falling back to no terminal at all (both redirected) means no raw
/// mode, which is the honest answer: there is nowhere to draw.
///
/// A DUP rather than a borrow, so every write goes through one unbuffered handle
/// whose close is the editor's own and cannot take the shell's stdout with it.
fn sink() -> Option<std::fs::File> {
    pick_sink([std::io::stdout().as_fd(), std::io::stderr().as_fd()])
}

/// Split from `sink` so the half that matters is testable without a terminal:
/// that nothing which is NOT one is ever chosen. The other half — that a
/// terminal IS chosen, stdout before stderr — needs a terminal to assert, and
/// making one in-process needs `TIOCSPTLCK`, a request outside this crate's
/// roster. It is checked by driving the built shell through a pty instead.
fn pick_sink(candidates: [std::os::fd::BorrowedFd<'_>; 2]) -> Option<std::fs::File> {
    for fd in candidates {
        if fd.is_terminal() {
            // `continue`, not `return`: a dup that failed (EMFILE) says nothing
            // about the next candidate.
            if let Ok(dup) = fd.try_clone_to_owned() {
                return Some(std::fs::File::from(dup));
            }
        }
    }
    None
}

/// The kernel's own line discipline, for a stdin that is not an editable
/// terminal — a pipe, a file, or a console that would not take `TCSETS`.
///
/// Through the same unbuffered handle the `read` builtin takes, and never past
/// the line: `std::io::Stdin` is a `BufReader`, so reading a line through it
/// pulls up to 8 KiB off a descriptor the SCRIPT shares with the commands in
/// it. `printf 'read v\nDATA\n' | td-sh -i` left `read` at end of input with
/// the line it wanted sitting in the shell's own buffer, where bash hands it
/// over. Sharing the handle rather than taking a second one is what keeps the
/// two from disagreeing about position.
///
/// This defers to `read_script_line`, so which of the two readers runs follows
/// from the descriptor rather than from being interactive: `td-sh -i < file`
/// takes the block-and-rewind path, and `-i` on a pipe or on a console that
/// refused raw mode takes the byte-at-a-time one. A non-interactive stdin
/// script never reaches here at all — `main`'s parser pulls from
/// `read_script_line` directly.
fn read_cooked(prompt: &str) -> Input {
    let _ = write!(std::io::stdout(), "{prompt}");
    let _ = std::io::stdout().flush();
    match read_script_line() {
        // The terminator is not part of what the operator typed, and a console
        // may end a line CRLF. Stripped HERE rather than in the reader, because
        // in a SCRIPT those same bytes are the script's own: whether the last
        // line ended in a newline decides whether a trailing `\` is a line
        // continuation, and a carriage return inside a quoted value is data.
        ScriptLine::Line(mut line) => {
            while line.ends_with('\n') || line.ends_with('\r') {
                line.pop();
            }
            Input::Line(line)
        }
        // A failed read ends the prompt loop, which is what `read_line` did.
        ScriptLine::Eof | ScriptLine::Failed(_) => Input::Eof,
    }
}

/// What one raw line off stdin turned out to be.
pub enum ScriptLine {
    /// The bytes up to and INCLUDING the newline — or without one, at the end
    /// of input. Verbatim: a script's own bytes are not this reader's to
    /// normalise.
    Line(String),
    Eof,
    /// Reported rather than folded into `Eof`, because a script that stopped
    /// because its input FAILED is not a script that ended.
    Failed(String),
}

/// Bytes taken per read once stdin is known to be seekable. Big enough that an
/// ordinary line costs one read, small enough that the rewind after it is short.
const SCRIPT_BLOCK: usize = 256;

/// Whether stdin can be rewound, asked once. The handle is a dup taken at first
/// use, so nothing the script does to fd 0 afterwards changes the answer.
static STDIN_REWINDABLE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();

/// One line off stdin, never leaving the descriptor past the newline.
///
/// Through the same handle the `read` builtin takes, `process::stdin_raw`:
/// `std::io::Stdin` is a `BufReader`, so reading a line through it pulls up to
/// 8 KiB off a descriptor the script SHARES with the commands in it. Sharing
/// the handle rather than opening a second one is the point — two dups would
/// have one offset but two buffers, which is the bug this exists to avoid.
///
/// How the line is taken depends on whether that descriptor can be given back
/// to. A SEEKABLE stdin is read a block at a time and rewound to the byte after
/// the newline, which is what bash does; an UNSEEKABLE one — pipe, FIFO,
/// terminal — has no way to return an over-read, so it goes a byte at a time.
/// Either way the descriptor ends up exactly past the line, which is what makes
/// `cat` in `sh < script` see the lines the parser did not take.
///
/// A script given as a FILE OPERAND is still read whole (`Start::Script`);
/// everything else reaching stdin comes through here, `sh < script` included.
pub fn read_script_line() -> ScriptLine {
    let handle = match crate::process::stdin_raw() {
        Ok(handle) => handle,
        Err(e) => return ScriptLine::Failed(e.to_string()),
    };
    let seekable = *STDIN_REWINDABLE.get_or_init(|| {
        // A REGULAR FILE, and one that answers a position query. Answering is
        // not enough on its own: a descriptor can take `stream_position` and
        // still refuse the negative seek this path ends with, and by then the
        // block has been read and there is nothing to give back. A regular
        // file is the kind whose rewind is guaranteed — and it is what
        // `sh < script` is. Anything else takes the byte-at-a-time loop, which
        // is correct for every descriptor and merely slower.
        let mut probe: &std::fs::File = &handle;
        handle.metadata().is_ok_and(|m| m.file_type().is_file()) && probe.stream_position().is_ok()
    });
    let read = if seekable {
        read_line_block(&handle)
    } else {
        read_line_bytewise(&handle)
    };
    let bytes = match read {
        Ok(bytes) => bytes,
        Err(e) => return ScriptLine::Failed(e),
    };
    if bytes.is_empty() {
        return ScriptLine::Eof;
    }
    match String::from_utf8(bytes) {
        Ok(line) => ScriptLine::Line(line),
        // The message `read_to_string` gave for the same input, which is what
        // this path reported before it read a line at a time.
        Err(_) => ScriptLine::Failed("stream did not contain valid UTF-8".to_string()),
    }
}

/// A line at one syscall per byte: the only way to read a descriptor that cannot
/// be rewound, since anything over-read is taken from the script's own commands.
fn read_line_bytewise(handle: &std::fs::File) -> Result<Vec<u8>, String> {
    let mut src: &std::fs::File = handle;
    let mut bytes: Vec<u8> = Vec::new();
    let mut one = [0u8; 1];
    loop {
        match src.read(&mut one) {
            // `read_line`, which this replaces, retried an interrupted read;
            // treating it as end of input would truncate a script that has
            // more. Unreachable while td-sh installs no handler, and reachable
            // the moment `trap 'action'` is served, which UNSAFE.md §8 defers.
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.to_string()),
            Ok(0) => return Ok(bytes),
            Ok(_) => {
                // Unreachable from a conforming `Read`, and an ERROR rather
                // than the bytes so far: a script line quietly cut short is a
                // different script, where a failure stops it.
                let Some(&byte) = one.first() else {
                    return Err("stdin reported a byte that is not there".to_string());
                };
                bytes.push(byte);
                if byte == b'\n' {
                    return Ok(bytes);
                }
            }
        }
    }
}

/// A line in block reads, rewinding to the byte after the newline.
///
/// The rewind is the whole point rather than tidiness: the parser and the
/// commands it starts share one descriptor, so bytes read past the line would
/// be bytes `cat` in `sh < script` never sees. It is issued before this returns,
/// so the descriptor is ahead only for as long as one read takes — the same
/// window bash's reader has, and it is why this path needs stdin to be seekable
/// rather than merely a regular file.
fn read_line_block(handle: &std::fs::File) -> Result<Vec<u8>, String> {
    let mut src: &std::fs::File = handle;
    let mut bytes: Vec<u8> = Vec::new();
    let mut buf = [0u8; SCRIPT_BLOCK];
    loop {
        let n = match src.read(&mut buf) {
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.to_string()),
            // End of input: nothing was over-read, so there is nothing to give
            // back and the descriptor already sits where it should.
            Ok(0) => return Ok(bytes),
            Ok(n) => n,
        };
        // Both of these are unreachable from a conforming `Read` -- `n` is what
        // it just reported reading, and `nl` is an index into that same slice.
        // They fail rather than returning the bytes so far, because a script
        // line quietly cut short is a different script, where an error stops it.
        let Some(chunk) = buf.get(..n) else {
            return Err("stdin reported more bytes than were asked for".to_string());
        };
        let Some(nl) = chunk.iter().position(|&b| b == b'\n') else {
            bytes.extend_from_slice(chunk);
            continue;
        };
        let Some(upto) = chunk.get(..=nl) else {
            return Err("stdin line ended past the bytes it was found in".to_string());
        };
        bytes.extend_from_slice(upto);
        let over = n.saturating_sub(nl.saturating_add(1));
        if over > 0 {
            let back = i64::try_from(over).map_err(|e| e.to_string())?;
            src.seek(std::io::SeekFrom::Current(-back)).map_err(|e| e.to_string())?;
        }
        return Ok(bytes);
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn hist(lines: &[&str]) -> Editor {
        Editor { history: lines.iter().map(|s| (*s).to_string()).collect() }
    }

    fn facts(cwd: &str, home: &str, root: bool) -> PromptFacts {
        PromptFacts {
            user: Some("ada".to_string()),
            host: Some("box.example.com".to_string()),
            cwd: Some(cwd.to_string()),
            home: Some(home.to_string()),
            root,
        }
    }

    /// The prompt the image actually ships, and the default td-sh sets when
    /// none is exported. Both were printed VERBATIM before this existed, which
    /// is the bug: the shell wrote a prompt in a notation it could not render.
    #[test]
    fn the_shipped_prompts_expand() {
        let f = facts("/home/ada/src", "/home/ada", false);
        assert_eq!(render_prompt(r"\u@\h:\w\$ ", &f), "ada@box:~/src$ ");
        assert_eq!(render_prompt(r"\w \$ ", &f), "~/src $ ");
    }

    /// `\$` is the privilege indicator, so it has to follow the uid rather than
    /// the name -- and `\h` stops at the first dot where `\H` does not.
    #[test]
    fn prompt_root_and_host_forms() {
        let user = facts("/", "/home/ada", false);
        let root = facts("/", "/home/ada", true);
        assert_eq!(render_prompt(r"\$", &user), "$");
        assert_eq!(render_prompt(r"\$", &root), "#");
        assert_eq!(render_prompt(r"\h", &user), "box");
        assert_eq!(render_prompt(r"\H", &user), "box.example.com");
    }

    /// `\w` abbreviates `$HOME` and nothing else: a directory that merely
    /// starts with the same TEXT is a different directory, and shortening it
    /// would name a path that does not exist.
    #[test]
    fn prompt_tilde_is_a_path_prefix_not_a_string_prefix() {
        let home = "/home/ada";
        assert_eq!(render_prompt(r"\w", &facts(home, home, false)), "~");
        assert_eq!(render_prompt(r"\w", &facts("/home/ada/x", home, false)), "~/x");
        assert_eq!(
            render_prompt(r"\w", &facts("/home/adamant", home, false)),
            "/home/adamant"
        );
        assert_eq!(render_prompt(r"\W", &facts("/home/ada/x", home, false)), "x");
        assert_eq!(render_prompt(r"\W", &facts("/", home, false)), "/");
        // `\W` is the basename of the TILDE form, so the home directory itself
        // is `~` and not the last component of `$HOME`.
        assert_eq!(render_prompt(r"\W", &facts(home, home, false)), "~");
    }

    /// The character escapes, the dropped non-printing markers, and octal.
    #[test]
    fn prompt_character_escapes() {
        let f = facts("/", "/home/ada", false);
        assert_eq!(render_prompt(r"a\nb\rc\ad\ee\\f", &f), "a\nb\rc\x07d\x1be\\f");
        assert_eq!(render_prompt(r"\[\e[32m\]x\[\e[0m\]", &f), "\x1b[32mx\x1b[0m");
    }

    /// `\nnn` is read the way bash's `read_octal` reads it, which is neither
    /// "one to three digits" nor "exactly three". Every line here was measured
    /// against bash rather than reasoned about: two plausible readings each
    /// get some of them wrong.
    #[test]
    fn octal_prompt_escapes_follow_bashs_three_byte_window() {
        let f = facts("/", "/home/ada", false);
        // A short run counts only when nothing follows it.
        assert_eq!(render_prompt(r"\7", &f), "\x07");
        assert_eq!(render_prompt(r"\7|", &f), r"\7|");
        assert_eq!(render_prompt(r"\101\10\7|", &f), r"A\10\7|");
        // A fourth digit is ordinary text after a complete escape.
        assert_eq!(render_prompt(r"\1234", &f), "S4");
        // The value is a BYTE and wraps -- the corpus names this one.
        assert_eq!(render_prompt(r"\555", &f), "m");
        // A NUL emits nothing at all.
        assert_eq!(render_prompt(r"\0", &f), "");
        assert_eq!(render_prompt(r"\0x", &f), r"\0x");
        // `8` and `9` are not octal, so the escape never starts.
        assert_eq!(render_prompt(r"\8\9", &f), r"\8\9");
        // Above ASCII bash emits a raw byte, which a `String` cannot carry;
        // the escape is left as text rather than drawn as another character.
        assert_eq!(render_prompt(r"\377\200", &f), r"\377\200");
        assert_eq!(render_prompt(r"\177", &f), "\x7f");
    }

    /// `\$` reports the EFFECTIVE uid, which is the second of the four
    /// `/proc/self/status` reports. Every process the gate can run as has all
    /// four equal, so this is asserted against a sample where they differ --
    /// against the real file, any of the four would pass.
    #[test]
    fn the_effective_uid_is_the_second_of_the_four() {
        let status = "Name:\tsh\nUid:\t1000\t0\t1000\t1000\nGid:\t1000\t1000\t1000\t1000\n";
        assert_eq!(effective_uid_in(status), Some(0));
        // A real one, where they agree.
        assert_eq!(effective_uid_in("Uid:\t1001\t1001\t1001\t1001\n"), Some(1001));
        // No `Uid:` line at all, and a malformed one.
        assert_eq!(effective_uid_in("Name:\tsh\n"), None);
        assert_eq!(effective_uid_in("Uid:\t1000\n"), None);
    }

    /// `HOME=/` abbreviates nothing, as bash's `polite_directory_format` does
    /// not: under it every path starts with `$HOME`, so `~` would eat the
    /// leading slash of all of them. A root shell with `HOME=/` is ordinary.
    #[test]
    fn a_root_home_abbreviates_nothing() {
        assert_eq!(render_prompt(r"\w", &facts("/", "/", false)), "/");
        assert_eq!(render_prompt(r"\w", &facts("/etc", "/", false)), "/etc");
        assert_eq!(render_prompt(r"\W", &facts("/etc", "/", false)), "etc");
    }

    /// An escape this shell does not implement keeps its backslash rather than
    /// vanishing or guessing. `\t` is the one that matters: it is bash's TIME,
    /// so emitting a tab would put whitespace where a clock was asked for.
    #[test]
    fn unimplemented_prompt_escapes_stay_literal() {
        let f = facts("/", "/home/ada", false);
        for esc in [r"\t", r"\d", r"\T", r"\@", r"\A", r"\!", r"\#", r"\j"] {
            assert_eq!(render_prompt(esc, &f), esc, "{esc} should stay literal");
        }
        // A trailing backslash is itself.
        assert_eq!(render_prompt(r"x\", &f), r"x\");
    }

    /// Whatever the escapes render to, the editor must not count colour bytes
    /// as columns -- the prompt is measured after expansion, so this is where
    /// the two meet.
    ///
    /// Expansion is what puts these in reach: before it, a `PS1` could not
    /// contain a BEL or a bare ESC without the operator typing one. The
    /// window-title form is the case that matters, since `\[\e]0;t\a\]` is a
    /// commonplace prompt and OSC ends at BEL rather than at a CSI final byte.
    #[test]
    fn an_expanded_colour_prompt_measures_its_visible_width() {
        let f = facts("/home/ada", "/home/ada", false);
        let drawn = render_prompt(r"\[\e[32m\]\w\[\e[0m\]\$ ", &f);
        assert_eq!(display_cols(&drawn), "~$ ".len());
        // An OSC window title occupies no columns, however long it is.
        assert_eq!(display_cols(&render_prompt(r"\[\e]0;a title\a\]\$ ", &f)), 2);
        // ...and the ST-terminated spelling of the same thing.
        assert_eq!(display_cols(&render_prompt("\\[\\e]0;a title\\e\\\\\\]\\$ ", &f)), 2);
        // A carriage return returns to the margin, so what preceded it is not
        // width; a bell draws nothing.
        assert_eq!(display_cols(&render_prompt(r"xxx\ry\a", &f)), 1);
    }

    /// Cursor motion lands on CHARACTER boundaries, never inside a UTF-8
    /// sequence -- a `String` cannot hold a split one, so getting this wrong is
    /// the difference between an editor and a panic.
    #[test]
    fn the_cursor_moves_by_characters_not_bytes() {
        let s = "aéb漢";
        // a=1, é=2, b=1, 漢=3.
        assert_eq!(next_boundary(s, 0), 1);
        assert_eq!(next_boundary(s, 1), 3);
        assert_eq!(next_boundary(s, 3), 4);
        assert_eq!(next_boundary(s, 4), 7);
        // ... and at the end it stays put rather than running off.
        assert_eq!(next_boundary(s, 7), 7);
        assert_eq!(prev_boundary(s, 7), 4);
        assert_eq!(prev_boundary(s, 4), 3);
        assert_eq!(prev_boundary(s, 3), 1);
        assert_eq!(prev_boundary(s, 1), 0);
        assert_eq!(prev_boundary(s, 0), 0);
    }

    /// Delete-forward at the end is a no-op, and elsewhere takes exactly one
    /// character however many bytes it is.
    #[test]
    fn delete_forward_takes_one_character() {
        let mut s = "aéb".to_string();
        delete_forward(&mut s, 1);
        assert_eq!(s, "ab");
        delete_forward(&mut s, 2);
        assert_eq!(s, "ab", "delete at the end changed the line");
        delete_forward(&mut s, 0);
        assert_eq!(s, "b");
    }

    /// Ctrl-W: trailing blanks THEN the word, so it works from the end of a
    /// line and from the end of a word alike.
    #[test]
    fn ctrl_w_takes_the_word_before_the_cursor() {
        for (line, pos, want) in [
            ("echo one two", 12, 9),
            ("echo one two   ", 15, 9),
            ("echo one two", 8, 5),
            ("   ", 3, 0),
            ("", 0, 0),
            ("one", 3, 0),
            // From mid-word, only the part before the cursor goes.
            ("echo one two", 10, 9),
        ] {
            assert_eq!(word_start(line, pos), want, "{line:?} at {pos}");
        }
    }

    /// A line that fits is drawn whole and unscrolled, with the cursor where the
    /// prompt leaves it.
    #[test]
    fn a_short_line_is_not_scrolled() {
        let (text, col) = visible("$ ", "echo hi", 7, 80);
        assert_eq!(text, "$ echo hi");
        assert_eq!(col, 9);
        let (text, col) = visible("$ ", "echo hi", 0, 80);
        assert_eq!(text, "$ echo hi");
        assert_eq!(col, 2, "the cursor sits right after the prompt");
    }

    /// A line longer than the row scrolls to keep the cursor visible, and the
    /// drawn text never exceeds the width -- the property that keeps the redraw
    /// on one row.
    #[test]
    fn a_long_line_scrolls_to_follow_the_cursor() {
        let long: String = ('a'..='z').cycle().take(200).collect();
        for pos in [0usize, 1, 40, 199, 200] {
            let (text, col) = visible("$ ", &long, pos, 20);
            // Never INTO the last column: a character printed there arms the
            // auto-wrap on a terminal that wraps at once, and the redraw's `\r`
            // then lands a row below the prompt, every keystroke.
            assert!(text.chars().count() < 20, "row is {} wide", text.chars().count());
            assert!(col < 19, "cursor at column {col} is off the row");
        }
        // At the end, the cursor sits just inside the reserved column with the
        // tail shown: room = 20 - 2 for the prompt - 1 reserved, and the last
        // room-1 characters fit beside the column the cursor occupies.
        let (text, col) = visible("$ ", &long, 200, 20);
        assert_eq!(col, 18);
        assert!(text.ends_with(long.get(184..).unwrap_or("")), "{text}");
        assert_eq!(text.chars().count(), 18);
        // At the start, nothing is scrolled away.
        let (text, _) = visible("$ ", &long, 0, 20);
        assert!(text.starts_with("$ a"));
    }

    /// A wide character is TWO columns, and measuring it as one is what makes a
    /// row wrap -- after which `\r` returns to a line below the prompt and the
    /// screen scrolls once per keystroke.
    #[test]
    fn a_wide_character_is_measured_at_the_width_it_occupies() {
        assert_eq!(columns('a'), 1);
        assert_eq!(columns('\u{e9}'), 1);
        assert_eq!(columns('\u{6f22}'), 2);
        assert_eq!(columns('\u{3042}'), 2);
        assert_eq!(columns('\u{ff21}'), 2);
        assert_eq!(columns('\u{1f4a1}'), 2);
        assert_eq!(display_cols("\u{6f22}\u{5b57}"), 4);
        // Codex's case: five `漢` beside a two-column prompt on a ten-column
        // terminal occupy twelve, where five `a`s occupy five.
        let wide: String = std::iter::repeat('\u{6f22}').take(5).collect();
        for pos in [0usize, 3, 6, 9, 12, 15] {
            let at = prev_boundary(&wide, pos.min(wide.len()));
            let (text, col) = visible("$ ", &wide, at, 10);
            assert!(display_cols(&text) < 10, "row is {} wide: {text:?}", display_cols(&text));
            assert!(col < 10, "cursor at {col} is off a ten-column row");
        }
        // The narrow case is unchanged: five `a`s fit beside the prompt.
        let (text, _) = visible("$ ", "aaaaa", 5, 10);
        assert_eq!(text, "$ aaaaa");
    }

    /// A coloured prompt is drawn verbatim but MEASURED without its escapes,
    /// so the line neither scrolls early nor puts the cursor in the wrong
    /// column -- the failure would repeat on every keystroke.
    #[test]
    fn a_coloured_prompt_is_measured_without_its_escapes() {
        assert_eq!(display_cols("$ "), 2);
        assert_eq!(display_cols("\x1b[32m$ \x1b[0m"), 2);
        assert_eq!(display_cols("\x1b[1;32mred\x1b[m> "), 5);
        // SS3, and a lone ESC at the end, neither of which may run away.
        assert_eq!(display_cols("\x1bOA> "), 2);
        assert_eq!(display_cols("ab\x1b"), 2);

        let coloured = "\x1b[32m$ \x1b[0m";
        let (text, col) = visible(coloured, "echo hi", 7, 80);
        assert!(text.starts_with(coloured), "the colours must still be drawn");
        assert_eq!(col, 9, "measured as two columns, exactly as a plain `$ ` is");
        assert_eq!(visible("$ ", "echo hi", 7, 80).1, col);
    }

    /// A multi-line prompt is measured and drawn as its LAST line only: the
    /// whole redraw rests on never leaving the row, and `\r` cannot climb back
    /// to a row the prompt scrolled away.
    #[test]
    fn a_multiline_prompt_is_reduced_to_its_last_row() {
        let (text, col) = visible("first line\n$ ", "hi", 2, 80);
        assert_eq!(text, "$ hi");
        assert_eq!(col, 4);
    }

    /// A prompt as wide as the terminal would leave no room to type, so it is
    /// dropped for that redraw rather than squeezing the line to nothing.
    #[test]
    fn an_overlong_prompt_gives_way_to_the_line() {
        let (text, col) = visible("aVeryLongPrompt> ", "hi", 2, 10);
        assert_eq!(text, "hi");
        assert_eq!(col, 2);
        // A zero width cannot divide by zero or panic; it degenerates to one
        // column, which has room for the cursor and nothing else.
        let (text, col) = visible("$ ", "hi", 2, 0);
        assert_eq!((text.as_str(), col), ("", 0));
    }

    /// History browsing parks the draft, walks both ways, and stops at both
    /// ends rather than wrapping.
    #[test]
    fn history_browsing_parks_the_draft_and_stops_at_the_ends() {
        let ed = hist(&["first", "second"]);
        let (mut browse, mut buf, mut pos, mut draft) =
            (2usize, "typing".to_string(), 6usize, String::new());

        ed.browse(-1, &mut browse, &mut buf, &mut pos, &mut draft);
        assert_eq!((buf.as_str(), draft.as_str()), ("second", "typing"));
        assert_eq!(pos, buf.len(), "the cursor lands at the end of a recalled line");
        ed.browse(-1, &mut browse, &mut buf, &mut pos, &mut draft);
        assert_eq!(buf, "first");
        // Past the oldest entry, nothing moves.
        ed.browse(-1, &mut browse, &mut buf, &mut pos, &mut draft);
        assert_eq!((buf.as_str(), browse), ("first", 0));
        // ... and back down to the parked draft.
        ed.browse(1, &mut browse, &mut buf, &mut pos, &mut draft);
        assert_eq!(buf, "second");
        ed.browse(1, &mut browse, &mut buf, &mut pos, &mut draft);
        assert_eq!(buf, "typing", "the line being typed came back");
        ed.browse(1, &mut browse, &mut buf, &mut pos, &mut draft);
        assert_eq!((buf.as_str(), browse), ("typing", 2));
    }

    /// Blanks and immediate repeats are not kept, and the list stays bounded.
    #[test]
    fn the_history_skips_blanks_and_repeats() {
        let mut ed = Editor::new();
        ed.remember("echo one");
        ed.remember("echo one");
        ed.remember("   ");
        ed.remember("");
        ed.remember("echo two");
        assert_eq!(ed.history, vec!["echo one".to_string(), "echo two".to_string()]);

        let mut ed = Editor::new();
        for i in 0..HISTORY_MAX + 10 {
            ed.remember(&format!("line {i}"));
        }
        assert_eq!(ed.history.len(), HISTORY_MAX);
        assert_eq!(ed.history.first().map(String::as_str), Some("line 10"));
    }

    /// A multi-byte character is assembled from its lead byte and its
    /// continuations, and a byte that can begin no sequence is dropped rather
    /// than inserted -- the line is a `String`, so a byte that cannot be part of
    /// one has nowhere to go.
    #[test]
    fn a_character_is_assembled_from_its_lead_byte() {
        for (bytes, want, back) in [
            (vec![b'a'], Some('a'), None),
            (vec![0xc3u8, 0xa9], Some('\u{e9}'), None),
            (vec![0xe6, 0xbc, 0xa2], Some('\u{6f22}'), None),
            (vec![0xf0, 0x9f, 0x92, 0xa1], Some('\u{1f4a1}'), None),
            // A continuation byte with no lead, and a byte no sequence starts.
            (vec![0x80], None, None),
            (vec![0xff], None, None),
            // A lead byte whose continuations never arrive.
            (vec![0xc3], None, None),
            // A lead byte followed by something that is not a continuation:
            // the character is lost, the byte after it is HANDED BACK, and it
            // is only ever the one, since reading stops at the first.
            (vec![0xc3, b'b'], None, Some(b'b')),
            (vec![0xf0, b'x', b'y'], None, Some(b'x')),
        ] {
            let mut rest = bytes.iter().skip(1).copied();
            let first = bytes.first().copied().unwrap_or(0);
            assert_eq!(character(|| rest.next(), first), (want, back), "{bytes:02x?}");
        }
    }

    /// Escape sequences decode to keys, a numeric parameter is consumed whole so
    /// its terminator cannot be read as the next keystroke, and an unknown
    /// sequence is swallowed rather than typed into the line.
    #[test]
    fn escape_sequences_decode_to_keys() {
        for (tail, want) in [
            ("[A", Escape::Key(Key::Up)),
            ("[B", Escape::Key(Key::Down)),
            ("[C", Escape::Key(Key::Right)),
            ("[D", Escape::Key(Key::Left)),
            ("OA", Escape::Key(Key::Up)),
            ("[H", Escape::Key(Key::Home)),
            ("[F", Escape::Key(Key::End)),
            ("[1~", Escape::Key(Key::Home)),
            ("[4~", Escape::Key(Key::End)),
            ("[3~", Escape::Key(Key::Delete)),
            // Unknown, and each is consumed to its terminator.
            ("[15~", Escape::Unknown),
            ("[Z", Escape::Unknown),
            // Not a sequence at all: the byte comes back to be re-dispatched,
            // which is what makes ESC-then-Enter submit rather than swallow.
            ("x", Escape::Literal(b'x')),
            ("\r", Escape::Literal(b'\r')),
        ] {
            let mut bytes = tail.bytes();
            assert_eq!(escape(|| bytes.next()), want, "ESC {tail}");
            assert_eq!(bytes.next(), None, "ESC {tail} left bytes behind");
        }
        // A modified arrow is `ESC [ 1 ; 5 D`: the `;` ends the first parameter
        // and everything through the final byte must still be consumed, or the
        // `5D` is typed into the command line.
        for tail in ["[1;5D", "[1;2A", "[200~", "[?1049h", "[38;5;196m"] {
            let mut bytes = tail.bytes();
            let got = escape(|| bytes.next());
            assert_eq!(bytes.next(), None, "ESC {tail} left bytes behind: {got:?}");
        }
        // A stream that never sends a final byte is bounded rather than endless.
        let mut forever = "[".bytes().chain(std::iter::repeat(b'1'));
        assert_eq!(escape(|| forever.next()), Escape::Unknown);

        // A sequence that ends early gives no key rather than blocking forever
        // on a caller that has run out.
        let mut empty = std::iter::empty();
        assert_eq!(escape(|| empty.next()), Escape::Unknown);
    }

    #[test]
    fn a_redirected_stdout_is_never_drawn_on() {
        // The editor draws escape sequences. Choosing a file or a pipe would put
        // them in the operator's output and leave them typing blind, so a
        // candidate that is not a terminal is not a candidate.
        let path = std::env::temp_dir().join(format!("td-sh-sink-{}", std::process::id()));
        let one = std::fs::File::create(&path).unwrap();
        let two = std::fs::File::open("/dev/null").unwrap();
        assert!(pick_sink([one.as_fd(), two.as_fd()]).is_none());
        assert!(pick_sink([two.as_fd(), one.as_fd()]).is_none());
        let _ = std::fs::remove_file(&path);
    }

    /// Drive the whole editor with a keystroke script and no terminal at all.
    fn typed(ed: &mut Editor, keys: &[u8], width: u16, interruptible: bool) -> (Input, String) {
        // A world with nothing in it, so Tab is a literal tab -- what the
        // editor did before completion existed, and what every test here but
        // the completion ones is about.
        let c = |_: &str| Vec::new();
        let e = |_: &str, _: &str| Vec::new();
        let comp = complete::Source { commands: &c, entries: &e };
        typed_in(ed, keys, width, interruptible, &comp)
    }

    fn typed_in(
        ed: &mut Editor,
        keys: &[u8],
        width: u16,
        interruptible: bool,
        comp: &complete::Source<'_>,
    ) -> (Input, String) {
        let mut src = keys.iter().copied();
        let mut next = || src.next();
        let mut w = || Some(width);
        let mut k =
            Keys { next: &mut next, width: &mut w, intr: Some(0x03), eof: Some(0x04) };
        let mut out: Vec<u8> = Vec::new();
        let got = ed.edit("$ ", &mut k, &mut out, interruptible, comp);
        (got, String::from_utf8_lossy(&out).into_owned())
    }

    fn submitted(ed: &mut Editor, keys: &[u8]) -> String {
        match typed(ed, keys, 80, true).0 {
            Input::Line(text) => text,
            Input::Eof => "<eof>".to_string(),
            Input::Interrupted => "<int>".to_string(),
        }
    }

    /// Tab completes the word under the cursor, a SECOND Tab lists what it
    /// could not choose between, and with nothing to complete the byte is
    /// still TYPED -- which is what `<<-EOF` depends on.
    #[test]
    fn tab_completes_and_a_second_tab_lists() {
        let mut ed = Editor::new();
        let cmds: Vec<String> = ["echo", "echoes"].iter().map(|s| (*s).to_string()).collect();
        let c = |p: &str| cmds.iter().filter(|s| s.starts_with(p)).cloned().collect();
        // TWO entries, so a Tab on an empty word would LIST -- which is what
        // the unique-match case below has to not do.
        let e = |_: &str, p: &str| {
            [("elm".to_string(), true), ("zed".to_string(), false)]
                .into_iter()
                .filter(|(n, _)| n.starts_with(p))
                .collect()
        };
        let comp = complete::Source { commands: &c, entries: &e };
        // One Tab puts the shared prefix in and lists nothing.
        let (got, drawn) = typed_in(&mut ed, b"ec\t\r", 80, true, &comp);
        assert!(matches!(got, Input::Line(ref t) if t == "echo"), "{got:?}");
        assert!(!drawn.contains("echoes"), "one Tab listed: {drawn:?}");
        // A second one adds nothing, so it lists both instead.
        let (got, drawn) = typed_in(&mut ed, b"ec\t\t\r", 80, true, &comp);
        assert!(matches!(got, Input::Line(ref t) if t == "echo"), "{got:?}");
        assert!(drawn.contains("echo    echoes\n"), "no listing: {drawn:?}");
        // A key between the two Tabs clears it, as ash's `lastWasTab` is.
        let (_, drawn) = typed_in(&mut ed, b"ec\t\x06\t\r", 80, true, &comp);
        assert!(!drawn.contains("echo    echoes\n"), "listed anyway: {drawn:?}");
        // A unique match finishes the word with a space, a directory with `/`.
        let (got, _) = typed_in(&mut ed, b"echoe\t\r", 80, true, &comp);
        assert!(matches!(got, Input::Line(ref t) if t == "echoes "), "{got:?}");
        let (got, _) = typed_in(&mut ed, b"cat e\t\r", 80, true, &comp);
        assert!(matches!(got, Input::Line(ref t) if t == "cat elm/"), "{got:?}");
        // Nothing matches: the tab is typed.
        let (got, _) = typed_in(&mut ed, b"zz\t\r", 80, true, &comp);
        assert!(matches!(got, Input::Line(ref t) if t == "zz\t"), "{got:?}");
        // A UNIQUE match finished the word, so the Tab after it is not a
        // double-tab: without that, `echoe<Tab><Tab>` sees an empty word past
        // a command and dumps the whole directory.
        let (_, drawn) = typed_in(&mut ed, b"echoe\t\t\r", 80, true, &comp);
        assert!(!drawn.contains("elm"), "the second Tab listed a directory: {drawn:?}");
        // ...and the Tab after a LISTING does nothing, with the one after that
        // listing again -- so four Tabs print two listings, not three.
        let (_, drawn) = typed_in(&mut ed, b"ec\t\t\t\t\r", 80, true, &comp);
        assert_eq!(drawn.matches("echo    echoes\n").count(), 2, "{drawn:?}");
    }

    #[test]
    fn the_editing_keys_move_and_cut_the_line() {
        let mut ed = Editor::new();
        // Ctrl-A to the start, Ctrl-F twice, insert, Ctrl-E to the end.
        assert_eq!(submitted(&mut ed, b"cde\x01\x06\x06X\x05Z\r"), "cdXeZ");
        // Ctrl-K cuts to the end, Ctrl-U to the start.
        assert_eq!(submitted(&mut ed, b"keep-this\x02\x02\x02\x02\x0b\r"), "keep-");
        assert_eq!(submitted(&mut ed, b"drop me\x15kept\r"), "kept");
        // Ctrl-W takes the word before the cursor. readline's rubout skips the
        // blanks BEFORE the word and stops at the ones after it.
        assert_eq!(submitted(&mut ed, b"one two   three\x17\r"), "one two   ");
        assert_eq!(submitted(&mut ed, b"one two   \x17\r"), "one ");
        // Backspace, both spellings, and delete-forward mid-line.
        assert_eq!(submitted(&mut ed, b"abc\x7f\x08d\r"), "ad");
        assert_eq!(submitted(&mut ed, b"abc\x01\x04\r"), "bc");
    }

    #[test]
    fn the_arrows_arrive_as_escape_sequences() {
        let mut ed = Editor::new();
        assert_eq!(submitted(&mut ed, b"ac\x1b[DB\r"), "aBc");
        assert_eq!(submitted(&mut ed, b"ab\x1b[D\x1b[CX\r"), "abX");
        assert_eq!(submitted(&mut ed, b"bc\x1b[HA\r"), "Abc");
        assert_eq!(submitted(&mut ed, b"ab\x1b[H\x1b[FZ\r"), "abZ");
        // Delete (`ESC [ 3 ~`) removes forward; the parameter is what tells it
        // apart from Home, so the sequence has to be read past the digit.
        assert_eq!(submitted(&mut ed, b"abc\x1b[H\x1b[3~\r"), "bc");
        // Ctrl-Left is `ESC [ 1 ; 5 D`: consumed WHOLE, or `5D` lands in the
        // line. It moves one character here, which is what `D` means.
        assert_eq!(submitted(&mut ed, b"ac\x1b[1;5DB\r"), "aBc");
        // An unknown sequence is swallowed rather than typed.
        assert_eq!(submitted(&mut ed, b"ab\x1b[15~\r"), "ab");
        // ESC followed by a byte that starts no sequence: the byte comes back,
        // so ESC-then-Enter submits rather than eating the Enter.
        assert_eq!(submitted(&mut ed, b"ab\x1b\r"), "ab");
        assert_eq!(submitted(&mut ed, b"a\x1bb\r"), "ab");
    }

    #[test]
    fn a_tab_is_typed_and_drawn_as_one_column() {
        let mut ed = Editor::new();
        // `<<-EOF` is a here-doc whose point is the leading tab, so it has to be
        // enterable; dropping it silently is the one wrong answer.
        assert_eq!(submitted(&mut ed, b"a\tb\r"), "a\tb");
        assert_eq!(submitted(&mut ed, b"\tcat <<-EOF\r"), "\tcat <<-EOF");
        // Drawn as a space: one buffer character, one column, so every column
        // this module computes stays where the terminal put it.
        let (_, drawn) = typed(&mut ed, b"a\tb\r", 80, true);
        assert!(drawn.contains("$ a b"), "tab was not drawn as one column: {drawn:?}");
        // Backspacing over it removes one character, not a column of them.
        assert_eq!(submitted(&mut ed, b"a\tb\x7f\x7f\r"), "a");
    }

    #[test]
    fn an_ignored_interrupt_is_an_ignored_keystroke() {
        let mut ed = Editor::new();
        // `trap '' INT` in force: the byte stands in for a signal that does
        // nothing, so it does nothing here either.
        let (got, _) = typed(&mut ed, b"keep\x03 going\r", 80, false);
        assert!(matches!(got, Input::Line(ref t) if t == "keep going"), "{got:?}");
        // Interruptible: the line is abandoned and the shell told so.
        let (got, drawn) = typed(&mut ed, b"junk\x03", 80, true);
        assert!(matches!(got, Input::Interrupted), "{got:?}");
        assert!(drawn.ends_with("^C\n"), "no ^C echo: {drawn:?}");
    }

    /// `stty intr ''` disables the character rather than binding it to NUL, so a
    /// NUL keystroke is a control byte like any other -- not an interrupt, and
    /// not the end of the session.
    #[test]
    fn a_disabled_control_character_is_not_a_keystroke() {
        let mut ed = Editor::new();
        let mut src = b"a\x00b\r".iter().copied();
        let mut next = || src.next();
        let mut w = || Some(80u16);
        let mut k = Keys { next: &mut next, width: &mut w, intr: None, eof: None };
        let mut out: Vec<u8> = Vec::new();
        let c = |_: &str| Vec::new();
        let e = |_: &str, _: &str| Vec::new();
        let got = ed.edit("$ ", &mut k, &mut out, true, &complete::Source {
            commands: &c,
            entries: &e,
        });
        assert!(matches!(got, Input::Line(ref t) if t == "ab"), "{got:?}");
    }

    #[test]
    fn end_of_input_is_told_apart_from_delete_forward() {
        let mut ed = Editor::new();
        // Ctrl-D on an empty line ends the session, on its own row.
        let (got, drawn) = typed(&mut ed, b"\x04", 80, true);
        assert!(matches!(got, Input::Eof), "{got:?}");
        assert!(drawn.ends_with('\n'), "the cursor was left on the prompt: {drawn:?}");
        // Mid-line it deletes forward, so a stray one cannot end it.
        assert_eq!(submitted(&mut ed, b"abc\x02\x04\r"), "ab");
        // A closed stdin mid-line is end of input, not an empty line.
        assert!(matches!(typed(&mut ed, b"half", 80, true).0, Input::Eof));
    }

    #[test]
    fn the_history_is_browsed_and_the_draft_comes_back() {
        let mut ed = Editor::new();
        ed.remember("first");
        ed.remember("second");
        // Up twice reaches the older line; Down returns through the newer one to
        // the line that was being typed.
        assert_eq!(submitted(&mut ed, b"\x1b[A\r"), "second");
        assert_eq!(submitted(&mut ed, b"\x1b[A\x1b[A\r"), "first");
        assert_eq!(submitted(&mut ed, b"draft\x1b[A\x1b[B\r"), "draft");
        // Ctrl-P/Ctrl-N are the same two keys.
        assert_eq!(submitted(&mut ed, b"\x10\x10\x0e\r"), "second");
        // Past either end is a no-op rather than a wrap.
        assert_eq!(submitted(&mut ed, b"\x1b[A\x1b[A\x1b[A\x1b[A\r"), "first");
        assert_eq!(submitted(&mut ed, b"x\x1b[B\r"), "x");
        // A recalled line is edited from its end.
        assert_eq!(submitted(&mut ed, b"\x1b[A!\r"), "second!");
    }

    #[test]
    fn multibyte_characters_survive_editing() {
        let mut ed = Editor::new();
        // `pos` is a BYTE index kept on a character boundary; a wrong one is an
        // immediate panic inside `String::insert`, not a wrong answer.
        assert_eq!(submitted(&mut ed, "héllo\r".as_bytes()), "héllo");
        assert_eq!(submitted(&mut ed, "héllo\x7f\r".as_bytes()), "héll");
        assert_eq!(submitted(&mut ed, "aé\x7f\x7f\r".as_bytes()), "");
        assert_eq!(submitted(&mut ed, "日本\x1b[Dx\r".as_bytes()), "日x本");
        assert_eq!(submitted(&mut ed, "日本\x01\x04\r".as_bytes()), "本");
        // A lone continuation byte is not a character and is dropped.
        assert_eq!(submitted(&mut ed, b"a\x80b\r"), "ab");
    }

    #[test]
    fn no_keystroke_sequence_panics_the_editor() {
        // The reviewer's fuzz, made to ride the gate: the dispatch is where the
        // boundary invariant lives, so the property worth pinning is that
        // nothing typed can break it. A linear congruential generator rather
        // than a dependency, seeded fixed so a failure reproduces.
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        let mut rand = move || {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (seed >> 33) as u32
        };
        let mut ed = Editor::new();
        for width in [0u16, 1, 2, 3, 5, 80, 512] {
            for _ in 0..400 {
                let n = 1 + rand() % 64;
                let mut keys: Vec<u8> = (0..n)
                    .map(|_| match rand() % 8 {
                        // Control bytes and the editing keys, oversampled.
                        0..=3 => (rand() % 0x20) as u8,
                        // Escape sequences, whole and truncated.
                        4 => b'\x1b',
                        5 => b'[',
                        // Anything at all, including invalid UTF-8.
                        _ => (rand() % 0x100) as u8,
                    })
                    .collect();
                keys.push(b'\r');
                let (got, _) = typed(&mut ed, &keys, width, true);
                if let Input::Line(text) = got {
                    // The redraw's two bounds, over whatever was typed: the row
                    // never reaches the last column, and the cursor stays on the
                    // row. Both are what `\r` and one absolute move rest on.
                    for prompt in ["", "$ ", "\x1b[32m$ \x1b[0m", "日本 > "] {
                        for at in [0, text.len() / 2, text.len()] {
                            let at = prev_boundary(&text, at.min(text.len()));
                            let (row, col) = visible(prompt, &text, at, width);
                            let cols = display_cols(&row);
                            let cap = usize::from(width).max(1);
                            assert!(cols < cap || cap == 1, "{cols} >= {cap}: {row:?}");
                            assert!(col < cap, "cursor {col} off a {cap}-wide row");
                        }
                    }
                    ed.remember(&text);
                }
            }
        }
    }
}

//! Terminal plumbing: raw mode, alternate screen, key decoding, and a small
//! styled frame buffer.
//!
//! Raw mode goes through `stty(1)` rather than a termios ioctl so the crate can
//! `forbid(unsafe_code)`. All I/O is on `/dev/tty`, not stdin/stdout, so the TUI
//! works when either is redirected.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::process::{Command, Stdio};

const TTY: &str = "/dev/tty";
const ALT_SCREEN_ON: &str = "\x1b[?1049h";
const ALT_SCREEN_OFF: &str = "\x1b[?1049l";
const CURSOR_HIDE: &str = "\x1b[?25l";
const CURSOR_SHOW: &str = "\x1b[?25h";

/// Fallback geometry when `stty size` cannot be read.
const DEFAULT_ROWS: usize = 24;
const DEFAULT_COLS: usize = 80;

pub const RED: u8 = 31;
pub const GREEN: u8 = 32;
pub const YELLOW: u8 = 33;
pub const MAGENTA: u8 = 35;
pub const CYAN: u8 = 36;
// De-emphasis is `Style::dim()`, not a palette slot: SGR 2 is computed from
// whatever foreground the terminal is already using, so it stays legible on a
// theme this crate cannot see. Bright black (90) does not — most dark themes
// draw it close to the background.

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Style {
    pub fg: Option<u8>,
    pub bold: bool,
    pub dim: bool,
    pub invert: bool,
}

impl Style {
    pub const PLAIN: Style = Style { fg: None, bold: false, dim: false, invert: false };

    pub const fn fg(code: u8) -> Style {
        Style { fg: Some(code), bold: false, dim: false, invert: false }
    }

    pub const fn bold() -> Style {
        Style { fg: None, bold: true, dim: false, invert: false }
    }

    pub const fn dim() -> Style {
        Style { fg: None, bold: false, dim: true, invert: false }
    }

    pub const fn bar(code: u8) -> Style {
        Style { fg: Some(code), bold: true, dim: false, invert: true }
    }

    pub const fn with_bold(self) -> Style {
        Style { fg: self.fg, bold: true, dim: self.dim, invert: self.invert }
    }

    pub const fn with_invert(self) -> Style {
        Style { fg: self.fg, bold: self.bold, dim: self.dim, invert: true }
    }

    fn sgr(&self) -> String {
        let mut params: Vec<String> = Vec::new();
        if self.bold {
            params.push("1".to_string());
        }
        if self.dim {
            params.push("2".to_string());
        }
        if self.invert {
            params.push("7".to_string());
        }
        if let Some(code) = self.fg {
            params.push(code.to_string());
        }
        if params.is_empty() {
            String::new()
        } else {
            format!("\x1b[{}m", params.join(";"))
        }
    }
}

/// A single rendered row: plain text plus the style applied to the whole row.
#[derive(Clone)]
pub struct Line {
    pub text: String,
    pub style: Style,
}

impl Line {
    pub fn new(text: impl Into<String>, style: Style) -> Line {
        Line { text: text.into(), style }
    }

    pub fn plain(text: impl Into<String>) -> Line {
        Line::new(text, Style::PLAIN)
    }

    pub fn blank() -> Line {
        Line::plain("")
    }
}

/// Placeholder for a character that must never reach the terminal.
const REPLACEMENT: char = '\u{b7}';

/// True for anything that could steer the terminal or misrepresent the text:
/// C0 and C1 controls (U+009B is CSI on xterm) plus the bidi overrides that
/// make a diff render in an order the bytes do not have.
fn is_hostile(ch: char) -> bool {
    ch.is_control()
        || matches!(ch as u32, 0x202a..=0x202e | 0x2066..=0x2069 | 0x200e | 0x200f)
}

/// Display columns for `ch` — a dependency-free stand-in for `wcwidth(3)`.
/// Approximate at the margins, but a row that wraps scrolls the alternate
/// screen and can push a live confirmation prompt out of view.
fn char_width(ch: char) -> usize {
    match ch as u32 {
        0x0300..=0x036f
        | 0x0483..=0x0489
        | 0x1ab0..=0x1aff
        | 0x1dc0..=0x1dff
        | 0x200b..=0x200d
        | 0x20d0..=0x20f0
        | 0xfe00..=0xfe0f
        | 0xfe20..=0xfe2f => 0,
        0x1100..=0x115f
        | 0x2e80..=0x303e
        | 0x3041..=0x33ff
        | 0x3400..=0x4dbf
        | 0x4e00..=0x9fff
        | 0xa000..=0xa4cf
        | 0xa960..=0xa97f
        | 0xac00..=0xd7a3
        | 0xf900..=0xfaff
        | 0xfe10..=0xfe19
        | 0xfe30..=0xfe6f
        | 0xff00..=0xff60
        | 0xffe0..=0xffe6
        | 0x1f300..=0x1f64f
        | 0x1f680..=0x1f6ff
        | 0x1f900..=0x1f9ff
        | 0x20000..=0x2fffd
        | 0x30000..=0x3fffd => 2,
        _ => 1,
    }
}

/// Replace anything hostile, leaving the text otherwise intact. For output
/// that does not go through a [`Frame`] — `--list` and the headless `--land`
/// log still print refnames, subjects and raw git output to a terminal.
pub fn scrub(text: &str) -> String {
    text.chars()
        .map(|c| if is_hostile(c) { REPLACEMENT } else { c })
        .collect()
}

/// `scrub` every line but keep the line breaks: a newline is a control
/// character, so `scrub` alone folds a diff into one unreadable row.
pub fn scrub_lines(text: &str) -> String {
    text.lines().map(scrub).collect::<Vec<_>>().join("\n")
}

/// Expand tabs to 8-column stops, replace hostile characters and clip to
/// `cols` *display columns*. Returns the clipped text and the columns it
/// occupies. A wide character that would straddle the right edge is dropped
/// for a space, so the returned width never exceeds `cols`.
pub fn sanitize(text: &str, cols: usize) -> (String, usize) {
    let mut out = String::with_capacity(text.len().min(cols.saturating_mul(2)));
    let mut width = 0usize;
    for ch in text.chars() {
        if width >= cols {
            break;
        }
        if ch == '\t' {
            let target = ((width / 8 + 1) * 8).min(cols);
            while width < target {
                out.push(' ');
                width += 1;
            }
            continue;
        }
        let (ch, w) = if is_hostile(ch) {
            (REPLACEMENT, 1)
        } else {
            (ch, char_width(ch))
        };
        if width + w > cols {
            // A double-width character with one column left: pad, do not wrap.
            out.push(' ');
            width += 1;
            break;
        }
        out.push(ch);
        width += w;
    }
    (out, width)
}

/// Fixed-size frame assembled in one string and written with a single `write`,
/// erasing each row as it goes so redraws do not flicker.
pub struct Frame {
    pub rows: usize,
    pub cols: usize,
    buf: String,
    used: usize,
}

impl Frame {
    pub fn new(rows: usize, cols: usize) -> Frame {
        Frame {
            rows,
            cols: cols.max(1),
            buf: String::from("\x1b[H"),
            used: 0,
        }
    }

    /// Rows still free below what has been pushed.
    pub fn room(&self) -> usize {
        self.rows.saturating_sub(self.used)
    }

    pub fn push(&mut self, line: &Line) {
        self.push_parts(&line.text, line.style);
    }

    pub fn push_text(&mut self, text: &str, style: Style) {
        self.push_parts(text, style);
    }

    pub fn push_blank(&mut self) {
        self.push_parts("", Style::PLAIN);
    }

    fn push_parts(&mut self, text: &str, style: Style) {
        if self.used >= self.rows {
            return;
        }
        // Newline *before* each row but the first, so the final row never
        // scrolls the alternate screen.
        if self.used > 0 {
            self.buf.push_str("\r\n");
        }
        let (clipped, width) = sanitize(text, self.cols);
        let sgr = style.sgr();
        if !sgr.is_empty() {
            self.buf.push_str(&sgr);
        }
        self.buf.push_str(&clipped);
        // An inverted row must paint its full width, so pad before resetting.
        if style.invert {
            for _ in width..self.cols {
                self.buf.push(' ');
            }
        }
        if !sgr.is_empty() {
            self.buf.push_str("\x1b[0m");
        }
        if !style.invert {
            self.buf.push_str("\x1b[K");
        }
        self.used += 1;
    }

    /// Finish the frame, clearing any rows the content did not reach.
    pub fn finish(mut self) -> String {
        self.buf.push_str("\x1b[J");
        self.buf
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Key {
    Up,
    Down,
    Left,
    Right,
    PageUp,
    PageDown,
    Home,
    End,
    Enter,
    Esc,
    Tab,
    Backspace,
    Delete,
    Char(char),
    Ctrl(char),
    Unknown,
}

/// Decode a read buffer into keys. Escape sequences that arrive split across
/// reads decode as `Esc` plus their tail; at human typing speed a terminal
/// delivers each sequence in one read.
pub fn decode(bytes: &[u8]) -> Vec<Key> {
    let mut keys = Vec::new();
    let mut i = 0usize;
    while let Some(&b) = bytes.get(i) {
        match b {
            0x1b => {
                let (key, used) = decode_escape(bytes, i);
                keys.push(key);
                i += used;
            }
            b'\r' | b'\n' => {
                keys.push(Key::Enter);
                i += 1;
            }
            b'\t' => {
                keys.push(Key::Tab);
                i += 1;
            }
            0x7f | 0x08 => {
                keys.push(Key::Backspace);
                i += 1;
            }
            0x01..=0x1a => {
                // Ctrl-A..Ctrl-Z, minus the tab/enter/backspace cases above.
                let letter = char::from(b - 1 + b'a');
                keys.push(Key::Ctrl(letter));
                i += 1;
            }
            0x20..=0x7e => {
                keys.push(Key::Char(char::from(b)));
                i += 1;
            }
            _ => {
                // Multi-byte UTF-8, or a byte we have no meaning for.
                let len = utf8_len(b);
                match bytes.get(i..i + len).and_then(|s| std::str::from_utf8(s).ok()) {
                    Some(s) => {
                        for ch in s.chars() {
                            keys.push(Key::Char(ch));
                        }
                        i += len;
                    }
                    None => {
                        keys.push(Key::Unknown);
                        i += 1;
                    }
                }
            }
        }
    }
    keys
}

fn utf8_len(b: u8) -> usize {
    if b >= 0xf0 {
        4
    } else if b >= 0xe0 {
        3
    } else if b >= 0xc0 {
        2
    } else {
        1
    }
}

/// Decode the escape sequence starting at `start`; returns the key and how
/// many bytes it consumed (always >= 1).
fn decode_escape(bytes: &[u8], start: usize) -> (Key, usize) {
    let second = bytes.get(start + 1).copied();
    let intro = match second {
        // CSI and SS3 (application cursor mode) share the final-byte meanings.
        Some(b'[') | Some(b'O') => second,
        _ => None,
    };
    if intro.is_none() {
        return (Key::Esc, 1);
    }

    // Collect parameter bytes up to the final byte (0x40..=0x7e).
    let mut params = String::new();
    let mut idx = start + 2;
    while let Some(&b) = bytes.get(idx) {
        if (0x40..=0x7e).contains(&b) {
            let used = idx + 1 - start;
            let key = match (b, params.as_str()) {
                (b'A', _) => Key::Up,
                (b'B', _) => Key::Down,
                (b'C', _) => Key::Right,
                (b'D', _) => Key::Left,
                (b'H', _) => Key::Home,
                (b'F', _) => Key::End,
                (b'~', "1") | (b'~', "7") => Key::Home,
                (b'~', "4") | (b'~', "8") => Key::End,
                (b'~', "3") => Key::Delete,
                (b'~', "5") => Key::PageUp,
                (b'~', "6") => Key::PageDown,
                _ => Key::Unknown,
            };
            return (key, used);
        }
        params.push(char::from(b));
        idx += 1;
    }
    // Truncated sequence: consume what is there rather than looping.
    (Key::Unknown, bytes.len().saturating_sub(start).max(1))
}

/// Owns the terminal's mode for the life of the TUI and restores it on drop.
pub struct Terminal {
    input: File,
    output: File,
    saved_mode: Option<String>,
    raw: bool,
}

/// What the state machine needs from a terminal. A trait because `Terminal`
/// itself can only be built from a real `/dev/tty`: without this seam nothing
/// that decides whether a keystroke lands or deletes is reachable from a test.
pub trait Ui {
    fn size(&self) -> (usize, usize);
    fn draw(&mut self, frame: String) -> io::Result<()>;
    fn drain_input(&mut self) -> io::Result<()>;
    /// Leave the alternate screen, run `body`, come back. `&mut dyn FnMut` and
    /// not a generic so the trait stays object-safe.
    fn suspend_run(&mut self, body: &mut dyn FnMut() -> io::Result<()>) -> io::Result<io::Result<()>>;
}

impl Ui for Terminal {
    fn size(&self) -> (usize, usize) {
        Terminal::size(self)
    }

    fn draw(&mut self, frame: String) -> io::Result<()> {
        Terminal::draw(self, frame)
    }

    fn drain_input(&mut self) -> io::Result<()> {
        Terminal::drain_input(self)
    }

    fn suspend_run(
        &mut self,
        body: &mut dyn FnMut() -> io::Result<()>,
    ) -> io::Result<io::Result<()>> {
        self.suspend(body)
    }
}

impl Terminal {
    pub fn open() -> io::Result<Terminal> {
        let input = OpenOptions::new().read(true).open(TTY).map_err(|e| {
            io::Error::new(e.kind(), format!("opening {TTY} for input: {e} (no terminal?)"))
        })?;
        let output = OpenOptions::new()
            .write(true)
            .open(TTY)
            .map_err(|e| io::Error::new(e.kind(), format!("opening {TTY} for output: {e}")))?;
        let mut term = Terminal { input, output, saved_mode: None, raw: false };
        term.enter()?;
        Ok(term)
    }

    fn enter(&mut self) -> io::Result<()> {
        let saved = stty(&["-g"])?.trim().to_string();
        if saved.is_empty() {
            return Err(io::Error::other("stty -g returned no terminal settings"));
        }
        self.saved_mode = Some(saved);
        stty(&["raw", "-echo"])?;
        self.raw = true;
        self.output
            .write_all(format!("{ALT_SCREEN_ON}{CURSOR_HIDE}").as_bytes())?;
        self.output.flush()
    }

    /// Restore cooked mode and the normal screen. Idempotent.
    fn leave(&mut self) -> io::Result<()> {
        if !self.raw {
            return Ok(());
        }
        self.raw = false;
        let _ = self
            .output
            .write_all(format!("{CURSOR_SHOW}{ALT_SCREEN_OFF}").as_bytes());
        let _ = self.output.flush();
        if let Some(saved) = self.saved_mode.clone() {
            let _ = stty(&[&saved]);
        } else {
            let _ = stty(&["sane"]);
        }
        Ok(())
    }

    /// Hand the terminal back to a child process (a pager), then take it again.
    pub fn suspend<T>(&mut self, body: impl FnOnce() -> T) -> io::Result<T> {
        self.leave()?;
        let out = body();
        self.enter()?;
        Ok(out)
    }

    pub fn size(&self) -> (usize, usize) {
        let raw = match stty(&["size"]) {
            Ok(s) => s,
            Err(_) => return (DEFAULT_ROWS, DEFAULT_COLS),
        };
        let mut parts = raw.split_whitespace();
        let rows = parts.next().and_then(|s| s.parse::<usize>().ok()).unwrap_or(DEFAULT_ROWS);
        let cols = parts.next().and_then(|s| s.parse::<usize>().ok()).unwrap_or(DEFAULT_COLS);
        (rows.max(4), cols.max(20))
    }

    pub fn draw(&mut self, frame: String) -> io::Result<()> {
        self.output.write_all(frame.as_bytes())?;
        self.output.flush()
    }

    /// Block for the next batch of input. An empty result means the tty closed.
    pub fn read_keys(&mut self) -> io::Result<Vec<Key>> {
        let mut buf = [0u8; 256];
        let n = self.input.read(&mut buf)?;
        match buf.get(..n) {
            Some(bytes) if n > 0 => Ok(decode(bytes)),
            _ => Ok(Vec::new()),
        }
    }

    /// Throw away anything typed while a git command was running. A destructive
    /// confirmation must be answered by a keystroke made AFTER it appeared, and
    /// keys typed during the operation are still sitting in the tty buffer.
    pub fn drain_input(&mut self) -> io::Result<()> {
        if !self.raw {
            return Ok(());
        }
        // VMIN=0 VTIME=0 makes read() return immediately with whatever is
        // buffered, so the drain cannot block waiting for a keystroke.
        stty(&["min", "0", "time", "0"])?;
        let mut buf = [0u8; 256];
        loop {
            match self.input.read(&mut buf) {
                Ok(n) if n > 0 => continue,
                _ => break,
            }
        }
        // Restore unconditionally: left at VMIN=0 every later read returns
        // empty, which the event loop reads as a closed tty and quits.
        stty(&["min", "1", "time", "0"]).map(|_| ())
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        let _ = self.leave();
    }
}

/// Drive `stty` against the tty on its stdin — the POSIX spelling. (GNU's
/// `-F <file>` would work here but is not portable.)
fn stty(args: &[&str]) -> io::Result<String> {
    let tty = File::open(TTY)
        .map_err(|e| io::Error::new(e.kind(), format!("opening {TTY} for stty: {e}")))?;
    let out = Command::new("stty")
        .args(args)
        .stdin(Stdio::from(tty))
        .output()
        .map_err(|e| io::Error::new(e.kind(), format!("running stty: {e}")))?;
    if !out.status.success() {
        return Err(io::Error::other(format!(
            "stty {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// De-emphasis carries no colour of its own, so the status bar composes it
    /// with reverse video into a bar the terminal derives from its own theme.
    #[test]
    fn emits_the_style_compositions_the_panes_build() {
        assert_eq!(Style::PLAIN.sgr(), "");
        assert_eq!(Style::dim().sgr(), "\x1b[2m");
        assert_eq!(Style::dim().with_invert().sgr(), "\x1b[2;7m");
        assert_eq!(Style::fg(RED).with_bold().sgr(), "\x1b[1;31m");
        assert_eq!(Style::bar(CYAN).sgr(), "\x1b[1;7;36m");
    }

    #[test]
    fn decodes_arrows_and_paging() {
        assert_eq!(decode(b"\x1b[A"), vec![Key::Up]);
        assert_eq!(decode(b"\x1bOB"), vec![Key::Down]);
        assert_eq!(decode(b"\x1b[5~"), vec![Key::PageUp]);
        assert_eq!(decode(b"\x1b[6~"), vec![Key::PageDown]);
        assert_eq!(decode(b"\x1b[H"), vec![Key::Home]);
        assert_eq!(decode(b"\x1b[4~"), vec![Key::End]);
    }

    #[test]
    fn decodes_plain_and_control_keys() {
        assert_eq!(decode(b"q"), vec![Key::Char('q')]);
        assert_eq!(decode(b"\r"), vec![Key::Enter]);
        assert_eq!(decode(b"\x03"), vec![Key::Ctrl('c')]);
        assert_eq!(decode(b"\x7f"), vec![Key::Backspace]);
        assert_eq!(decode(b"\x1b"), vec![Key::Esc]);
    }

    #[test]
    fn decodes_batched_input_and_utf8() {
        assert_eq!(decode(b"jj\x1b[B"), vec![Key::Char('j'), Key::Char('j'), Key::Down]);
        assert_eq!(decode("é".as_bytes()), vec![Key::Char('é')]);
    }

    #[test]
    fn truncated_escape_does_not_loop() {
        let keys = decode(b"\x1b[");
        assert_eq!(keys.len(), 1);
    }

    #[test]
    fn sanitize_expands_tabs_and_clips() {
        assert_eq!(sanitize("ab\tc", 40).0, "ab      c");
        assert_eq!(sanitize("abcdef", 3), ("abc".to_string(), 3));
        // An escape sequence in a commit subject must not reach the terminal.
        assert_eq!(sanitize("a\x1b[2Jb", 40).0, "a\u{b7}[2Jb");
    }

    #[test]
    fn sanitize_replaces_c1_and_bidi() {
        // U+009B is CSI on xterm; the bidi overrides reorder rendered text.
        assert_eq!(sanitize("a\u{9b}2Jb", 40).0, "a\u{b7}2Jb");
        assert_eq!(sanitize("a\u{202e}b", 40).0, "a\u{b7}b");
        assert_eq!(sanitize("a\u{2066}b", 40).0, "a\u{b7}b");
    }

    #[test]
    fn sanitize_counts_display_columns_not_chars() {
        // Two double-width characters fill four columns, not two.
        assert_eq!(sanitize("世界", 40), ("世界".to_string(), 4));
        // Clipping is by column, and never exceeds the budget.
        assert_eq!(sanitize("世界", 4), ("世界".to_string(), 4));
        assert_eq!(sanitize("世界", 2), ("世".to_string(), 2));
        // A wide character with one column left pads rather than wrapping.
        assert_eq!(sanitize("世", 1), (" ".to_string(), 1));
        assert_eq!(sanitize("a世", 2), ("a ".to_string(), 2));
        // Combining marks take no columns of their own.
        assert_eq!(sanitize("e\u{301}", 40).1, 1);
    }

    #[test]
    fn every_sanitized_row_fits_its_frame() {
        for text in ["plain", "世界世界世界", "e\u{301}\u{301}x", "a\tb", "🚀🚀🚀"] {
            for cols in 1..12 {
                let (_, width) = sanitize(text, cols);
                assert!(width <= cols, "{text:?} at {cols} cols produced {width}");
            }
        }
    }

    #[test]
    fn scrub_neutralises_without_clipping() {
        assert_eq!(scrub("a\x1b[2Jb"), "a\u{b7}[2Jb");
        assert_eq!(scrub("a\u{202e}b"), "a\u{b7}b");
        assert_eq!(scrub("plain text stays"), "plain text stays");
    }

    #[test]
    fn frame_clips_rows_and_terminates() {
        let mut f = Frame::new(2, 10);
        f.push_text("one", Style::PLAIN);
        f.push_text("two", Style::PLAIN);
        f.push_text("three", Style::PLAIN);
        let out = f.finish();
        assert!(out.contains("one") && out.contains("two"));
        assert!(!out.contains("three"));
        assert!(out.ends_with("\x1b[J"));
    }

    #[test]
    fn inverted_rows_pad_to_full_width() {
        let mut f = Frame::new(1, 6);
        f.push_text("ab", Style::PLAIN.with_invert());
        assert!(f.finish().contains("ab    "));
    }
}

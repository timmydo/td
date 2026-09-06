use std::io::{self, Write};
use std::os::fd::BorrowedFd;

use crate::config::Theme;
use crate::term_sys::{self, Raw, RawOptions};

/// The screen for the duration of a session: the terminal in raw mode, the
/// mouse reporting it was asked for, and the colours every frame starts
/// with.
///
/// The lifetime is the descriptors': `Raw` restores the terminal from
/// `Drop`, so the borrow checker is what stops the handle being closed —
/// or closed and recycled — before the restore reaches it.
pub struct Terminal<'a> {
    /// Held only for its `Drop`, which is what puts the line discipline
    /// back; nothing here reads it.
    _raw: Raw<'a>,
    out: BorrowedFd<'a>,
    mouse_enabled: bool,
    base_seq: String,
}

impl<'a> Terminal<'a> {
    /// `tty` is read from and `out` is drawn on; they are the same terminal
    /// in an ordinary session and need not be.
    pub fn enter(
        tty: BorrowedFd<'a>,
        out: BorrowedFd<'a>,
        mouse: bool,
        theme: &Theme,
    ) -> Result<Self, String> {
        // Keystrokes as bytes and a read that waits for one: ISIG, ICANON
        // and ECHO off so every key is drawn by this program, ICRNL and
        // IXON off so Enter and Ctrl-S arrive as themselves, OPOST off so
        // a drawn frame is not post-processed, VMIN 1 and VTIME 0.
        let raw = term_sys::raw(tty, RawOptions::KEYS_BLOCKING)?;

        let base_seq = base_theme_sequence(theme);
        print!("\x1b[?1049h\x1b[?25l");
        if mouse {
            print!("\x1b[?1000h\x1b[?1006h");
        }
        print!("\x1b[2J\x1b[H{}", base_seq);
        io::stdout().flush().map_err(|e| e.to_string())?;

        Ok(Self {
            _raw: raw,
            out,
            mouse_enabled: mouse,
            base_seq,
        })
    }

    pub fn set_mouse(&mut self, enabled: bool) {
        if enabled && !self.mouse_enabled {
            print!("\x1b[?1000h\x1b[?1006h");
            let _ = io::stdout().flush();
            self.mouse_enabled = true;
        } else if !enabled && self.mouse_enabled {
            print!("\x1b[?1006l\x1b[?1000l");
            let _ = io::stdout().flush();
            // Keep mouse_enabled true so Drop still cleans up
        }
    }

    pub fn size(&self) -> (usize, usize) {
        match term_sys::window_size(self.out) {
            Some((cols, rows)) => (usize::from(cols), usize::from(rows)),
            None => (80, 24),
        }
    }

    pub fn draw(&self, lines: &[String]) -> Result<(), String> {
        let mut out = io::stdout();
        write!(out, "\x1b[H\x1b[2J").map_err(|e| e.to_string())?;
        for line in lines {
            write!(out, "\x1b[2K{}{}\x1b[K\x1b[0m\r\n", self.base_seq, line)
                .map_err(|e| e.to_string())?;
        }
        out.flush().map_err(|e| e.to_string())
    }
}

impl Drop for Terminal<'_> {
    fn drop(&mut self) {
        // `Raw`'s own `Drop` puts the line discipline back and records a
        // restore that did not take; `take_restore_failure` is where the
        // caller reads that, after this whole value is gone.
        if self.mouse_enabled {
            let _ = write!(io::stdout(), "\x1b[?1006l\x1b[?1000l");
        }
        let _ = write!(io::stdout(), "\x1b[0m\x1b[?25h\x1b[?1049l");
        let _ = io::stdout().flush();
    }
}

/// Whether the terminal was left in the wrong mode on the way out, so the
/// operator — the only person who can see it — is told.
pub fn restore_failure() -> Option<String> {
    term_sys::take_restore_failure()
}

fn base_theme_sequence(theme: &Theme) -> String {
    let mut seq = String::new();
    if let Some((r, g, b)) = parse_color_opt(&theme.bg) {
        seq.push_str(&format!("\x1b[48;2;{};{};{}m", r, g, b));
    }
    if let Some((r, g, b)) = parse_color_opt(&theme.fg) {
        seq.push_str(&format!("\x1b[38;2;{};{};{}m", r, g, b));
    }
    seq
}

fn parse_color_opt(v: &Option<String>) -> Option<(u8, u8, u8)> {
    v.as_deref().and_then(|hex| Theme::parse_color(hex).ok())
}

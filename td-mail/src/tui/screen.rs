use crate::config::Theme;
use crate::term_sys::{self, Raw, RawOptions};
use std::io::{self, BufWriter, Stdin, Stdout, Write};
use std::os::fd::{AsFd, BorrowedFd};
use std::sync::OnceLock;

/// The two descriptors, borrowed for the life of the process. `Raw` restores
/// the terminal from its `Drop`, so the descriptor it holds must outlive the
/// guard; a handle parked in a `OnceLock` is how a `BorrowedFd<'static>` is
/// come by without `unsafe`.
fn stdin_fd() -> BorrowedFd<'static> {
    static STDIN: OnceLock<Stdin> = OnceLock::new();
    STDIN.get_or_init(io::stdin).as_fd()
}

fn stdout_fd() -> BorrowedFd<'static> {
    static STDOUT: OnceLock<Stdout> = OnceLock::new();
    STDOUT.get_or_init(io::stdout).as_fd()
}

fn window_size() -> io::Result<(u16, u16)> {
    // `term_sys` answers columns first; the rest of this file counts rows
    // first, as `struct winsize` does.
    let (cols, rows) = term_sys::window_size(stdout_fd())
        .ok_or_else(|| io::Error::other("the terminal did not report a window size"))?;
    Ok((rows, cols))
}

pub struct Terminal {
    out: BufWriter<Stdout>,
    pub rows: u16,
    pub cols: u16,
    mouse_supported: bool,
    mouse_enabled: bool,
    theme: Theme,
    in_selection: bool,
    /// Raw mode, restored when this is dropped. Declared last so the restore
    /// happens after `Drop::drop` below has written the leaving sequences.
    _raw: Raw<'static>,
}

impl Terminal {
    pub fn new(mouse: bool, theme: Theme) -> io::Result<Self> {
        // `RawOptions::BYTES_TENTH` is the flag word this file used to build by
        // hand: BRKINT, ICRNL, INPCK, ISTRIP and IXON cleared from `c_iflag`,
        // OPOST from `c_oflag`, ECHO, ICANON, IEXTEN and ISIG from `c_lflag`,
        // CS8 set in `c_cflag`, with VMIN=0 and VTIME=1 for the tenth-of-a-
        // second read. Unlike the old code, `term_sys` reads the termios back
        // and refuses a kernel that applied only part of it.
        let raw = term_sys::raw(stdin_fd(), RawOptions::BYTES_TENTH).map_err(io::Error::other)?;

        let (rows, cols) = window_size()?;

        let mut out = BufWriter::new(io::stdout());
        // Enter alternate screen buffer, hide cursor
        write!(out, "\x1b[?1049h\x1b[?25l")?;
        // Apply base theme colors to the entire screen
        if let Some((r, g, b)) = theme.bg {
            write!(out, "\x1b[48;2;{};{};{}m", r, g, b)?;
        }
        if let Some((r, g, b)) = theme.fg {
            write!(out, "\x1b[38;2;{};{};{}m", r, g, b)?;
        }
        if mouse {
            // Enable X10 mouse tracking + SGR extended coordinates
            write!(out, "\x1b[?1000h\x1b[?1006h")?;
        }
        out.flush()?;

        Ok(Terminal {
            out,
            rows,
            cols,
            mouse_supported: mouse,
            mouse_enabled: mouse,
            theme,
            in_selection: false,
            _raw: raw,
        })
    }

    pub fn set_mouse_enabled(&mut self, enabled: bool) -> io::Result<()> {
        if !self.mouse_supported || self.mouse_enabled == enabled {
            return Ok(());
        }

        if enabled {
            write!(self.out, "\x1b[?1000h\x1b[?1006h")?;
        } else {
            write!(self.out, "\x1b[?1000l\x1b[?1006l")?;
        }
        self.mouse_enabled = enabled;
        self.out.flush()
    }

    /// Ask the terminal its size, and report a change as a resize.
    ///
    /// This replaces a SIGWINCH handler that set a flag: the handler was a
    /// signal-context write to a process-global `AtomicBool`, installed through
    /// `sigaction`, and the loop that read the flag already ticks every tenth
    /// of a second on the read timeout. Asking on that tick gives the same
    /// answer with no handler, no static, and no `unsafe`. The cost is one
    /// `ioctl` per tick and up to a tenth of a second of latency; the gain is
    /// that a resize arriving while the loop is elsewhere is not lost, since
    /// the size is compared rather than a flag consumed.
    pub fn check_resize(&mut self) -> bool {
        match window_size() {
            Ok((rows, cols)) if (rows, cols) != (self.rows, self.cols) => {
                self.rows = rows;
                self.cols = cols;
                true
            }
            _ => false,
        }
    }

    pub fn clear(&mut self) -> io::Result<()> {
        write!(self.out, "\x1b[2J\x1b[H")
    }

    pub fn move_to(&mut self, row: u16, col: u16) -> io::Result<()> {
        write!(self.out, "\x1b[{};{}H", row, col)
    }

    #[allow(dead_code)]
    pub fn clear_line(&mut self) -> io::Result<()> {
        write!(self.out, "\x1b[K")
    }

    pub fn write_str(&mut self, s: &str) -> io::Result<()> {
        write!(self.out, "{}", s)
    }

    pub fn set_reverse(&mut self) -> io::Result<()> {
        write!(self.out, "\x1b[7m")
    }

    #[allow(dead_code)]
    pub fn set_bold(&mut self) -> io::Result<()> {
        write!(self.out, "\x1b[1m")
    }

    pub fn reset_attr(&mut self) -> io::Result<()> {
        write!(self.out, "\x1b[0m")?;
        self.in_selection = false;
        // Re-apply base theme colors so the theme persists after resets
        if let Some((r, g, b)) = self.theme.bg {
            write!(self.out, "\x1b[48;2;{};{};{}m", r, g, b)?;
        }
        if let Some((r, g, b)) = self.theme.fg {
            write!(self.out, "\x1b[38;2;{};{};{}m", r, g, b)?;
        }
        Ok(())
    }

    /// Apply selection colors (for highlighted/cursor rows).
    /// Falls back to reverse video if no theme colors are set.
    pub fn set_selection(&mut self) -> io::Result<()> {
        self.in_selection = true;
        if self.theme.selection_bg.is_some() || self.theme.selection_fg.is_some() {
            if let Some((r, g, b)) = self.theme.selection_bg {
                write!(self.out, "\x1b[48;2;{};{};{}m", r, g, b)?;
            }
            if let Some((r, g, b)) = self.theme.selection_fg {
                write!(self.out, "\x1b[38;2;{};{};{}m", r, g, b)?;
            }
            Ok(())
        } else {
            self.set_reverse()
        }
    }

    /// Apply status bar colors.
    /// Falls back to reverse video if no theme colors are set.
    pub fn set_status(&mut self) -> io::Result<()> {
        if self.theme.status_bg.is_some() || self.theme.status_fg.is_some() {
            if let Some((r, g, b)) = self.theme.status_bg {
                write!(self.out, "\x1b[48;2;{};{};{}m", r, g, b)?;
            }
            if let Some((r, g, b)) = self.theme.status_fg {
                write!(self.out, "\x1b[38;2;{};{};{}m", r, g, b)?;
            }
            Ok(())
        } else {
            self.set_reverse()
        }
    }

    /// Apply header colors (bold + header_fg if set).
    pub fn set_header(&mut self) -> io::Result<()> {
        write!(self.out, "\x1b[1m")?;
        if let Some((r, g, b)) = self.theme.header_fg {
            write!(self.out, "\x1b[38;2;{};{};{}m", r, g, b)?;
        }
        Ok(())
    }

    /// Apply bold text colors (for unread items).
    /// Uses bold_fg if set, otherwise plain bold.
    /// When inside a selection, only adds bold without changing fg,
    /// so that selection_fg takes priority for contrast.
    pub fn set_bold_text(&mut self) -> io::Result<()> {
        write!(self.out, "\x1b[1m")?;
        if !self.in_selection {
            if let Some((r, g, b)) = self.theme.bold_fg {
                write!(self.out, "\x1b[38;2;{};{};{}m", r, g, b)?;
            }
        }
        Ok(())
    }

    pub fn flush(&mut self) -> io::Result<()> {
        self.out.flush()
    }

    /// Write a string truncated to fit within `max_width` columns.
    pub fn write_truncated(&mut self, s: &str, max_width: u16) -> io::Result<()> {
        let max = max_width as usize;
        if s.len() <= max {
            write!(self.out, "{}", s)
        } else {
            // Truncate at char boundary
            let mut end = max;
            while end > 0 && !s.is_char_boundary(end) {
                end -= 1;
            }
            write!(self.out, "{}", &s[..end])
        }
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        if self.mouse_enabled {
            // Disable mouse tracking
            let _ = write!(self.out, "\x1b[?1000l\x1b[?1006l");
        }
        // Reset all attributes (including custom fg/bg), show cursor, exit alternate screen
        let _ = write!(self.out, "\x1b[0m\x1b[?25h\x1b[?1049l");
        let _ = self.out.flush();

        // The terminal leaves raw mode when `_raw` drops, right after this.
        // A restore that does not take is recorded rather than reported from
        // here; `term_sys::take_restore_failure` is read on the way out.
    }
}

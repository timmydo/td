//! One error type: what happened, and whether the peer already left. Also the
//! one filter peer text and diagnostics pass through on the way to a terminal.

use std::fmt;
use std::io::{self, Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

#[derive(Debug)]
pub enum Error {
    /// The peer or the local policy broke the rules; end the connection.
    Protocol(String),
    /// The transport ended, in an orderly way or not.
    Io(io::Error),
    /// The peer sent `SSH_MSG_DISCONNECT`.
    Disconnected(String),
    /// Refused by local policy or configuration, before any protocol failure.
    Refused(String),
    /// The person at a prompt asked to stop, not merely to skip this question.
    Aborted,
}

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    pub fn protocol(message: impl Into<String>) -> Self {
        Self::Protocol(message.into())
    }

    pub fn refused(message: impl Into<String>) -> Self {
        Self::Refused(message.into())
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Protocol(message) => write!(f, "protocol error: {message}"),
            Self::Io(error) => write!(f, "{error}"),
            Self::Disconnected(message) => write!(f, "disconnected by peer: {message}"),
            Self::Refused(message) => write!(f, "{message}"),
            Self::Aborted => write!(f, "Aborted."),
        }
    }
}

impl From<io::Error> for Error {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

const REPLACEMENT: char = '?';

fn replace_controls(text: &str, keep_layout: bool) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        // RFC 4252 banners break lines with CR LF; the CR alone is an overstrike.
        if keep_layout && c == '\r' && chars.peek() == Some(&'\n') {
            continue;
        }
        let layout = keep_layout && matches!(c, '\n' | '\t');
        // Line and paragraph separators end a line for whatever reads the text
        // next, as U+0085 does, which `is_control` covers.
        let breaks_a_line = matches!(c, '\u{2028}' | '\u{2029}');
        out.push(if (c.is_control() && !layout) || breaks_a_line {
            REPLACEMENT
        } else {
            c
        });
    }
    out
}

/// What may reach a terminal as a whole message or a banner: every control
/// character replaced but the line breaks and tabs the text is laid out with.
pub fn printable(text: &str) -> String {
    replace_controls(text, true)
}

/// A peer's short string quoted into a message: one line, no controls at all.
pub fn peer_text(bytes: &[u8]) -> String {
    replace_controls(&String::from_utf8_lossy(bytes), false)
}

/// Raw mode takes the terminal's own line-end handling away, so what is printed
/// meanwhile brings its own carriage returns.
static TERMINAL_RAW: AtomicBool = AtomicBool::new(false);

pub fn set_terminal_raw(raw: bool) {
    TERMINAL_RAW.store(raw, Ordering::SeqCst);
}

fn rendered(text: &str, raw: bool) -> String {
    let clean = printable(text);
    if raw {
        format!("{}\r\n", clean.replace('\n', "\r\n"))
    } else {
        format!("{clean}\n")
    }
}

/// `std` has no `poll`, and an inherited `O_NONBLOCK` stream has no other way to
/// say when to try again.
const WOULD_BLOCK_PAUSE: Duration = Duration::from_millis(5);

/// A stream that may have come with `O_NONBLOCK` set, which is its opener's to
/// clear: `EAGAIN` from it means later, never the end of input or a lost write.
pub(crate) struct Patient<T>(pub(crate) T);

impl<R: Read> Read for Patient<R> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        loop {
            match self.0.read(out) {
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    thread::sleep(WOULD_BLOCK_PAUSE);
                }
                other => return other,
            }
        }
    }
}

impl<W: Write> Write for Patient<W> {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        loop {
            match self.0.write(data) {
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    thread::sleep(WOULD_BLOCK_PAUSE);
                }
                other => return other,
            }
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        loop {
            match self.0.flush() {
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    thread::sleep(WOULD_BLOCK_PAUSE);
                }
                other => return other,
            }
        }
    }
}

/// The one point diagnostics are printed. A failing stderr has nowhere to be
/// reported, and must not panic with the terminal raw.
pub fn diagnostic(text: &str) {
    let line = rendered(text, TERMINAL_RAW.load(Ordering::SeqCst));
    let _ = Patient(io::stderr().lock()).write_all(line.as_bytes());
}

/// A tool's primary output, whose loss is the caller's failure.
pub fn primary_output(text: &str) -> io::Result<()> {
    let mut out = Patient(io::stdout().lock());
    out.write_all(text.as_bytes())?;
    out.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOSTILE: &str = "\x1b]0;PWNED\x07\x1b[2J\u{9b}31m\x7f\x08\r";

    #[test]
    fn printable_replaces_every_control_and_keeps_layout() {
        assert_eq!(printable("a\tb\nc"), "a\tb\nc");
        assert_eq!(printable(HOSTILE), "?]0;PWNED??[2J?31m???");
        assert_eq!(printable("one\r\ntwo\r\n"), "one\ntwo\n");
        assert_eq!(printable("\rover"), "?over");
    }

    #[test]
    fn a_diagnostic_brings_its_own_carriage_returns_while_the_terminal_is_raw() {
        assert_eq!(rendered("one\ntwo", false), "one\ntwo\n");
        assert_eq!(rendered("one\ntwo", true), "one\r\ntwo\r\n");
        // The text's own never doubles: the filter has taken them out.
        assert_eq!(rendered("one\r\ntwo", true), "one\r\ntwo\r\n");
    }

    #[test]
    fn every_way_of_ending_a_line_is_filtered() {
        for text in ["a\u{2028}b", "a\u{2029}b", "a\u{0085}b"] {
            assert_eq!(peer_text(text.as_bytes()), "a?b", "{text:?}");
            assert_eq!(printable(text), "a?b", "{text:?}");
        }
    }

    #[test]
    fn peer_text_is_one_line_and_lossy() {
        assert_eq!(peer_text(b"a\nb\tc\x1b"), "a?b?c?");
        assert_eq!(peer_text(b"\xffok"), "\u{fffd}ok");
        assert_eq!(peer_text(HOSTILE.as_bytes()), "?]0;PWNED??[2J?31m???");
    }
}

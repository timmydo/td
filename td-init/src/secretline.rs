//! `secret-line PROMPT` — print PROMPT on the console once echo is off, read
//! one line with echo off, and write it, without its newline, to stdout,
//! which must be a pipe.
//!
//! This is the selector's recovery-key entry (td-install/ENCRYPTION.md, "Device-
//! bound default"): the console is a serial line whose output may be logged, so
//! the digits are never echoed. A recorder on the line itself may still log
//! what is typed; that is the review's disclosure, not this applet's to
//! prevent. It always reads `/dev/console`, the kernel's console, which in the
//! selector is the serial console its built-in command line names. A device
//! operand would let a caller point the read at any terminal, and nothing on
//! the recovery path needs one. The console is opened `O_NOCTTY`, so reading
//! it claims no session.
//!
//! The one operand is the prompt, which is not secret: 1 to `PROMPT_MAX`
//! printable ASCII bytes. The applet prints it rather than its caller so that
//! it appears only once echo is off and pending input has been discarded: a
//! key typed before the prompt was echoed under the old settings, and is
//! flushed rather than kept or joined to the entry.
//!
//! Order, and the reason for each step:
//!
//! 1. The prompt is checked, then stdout must be a pipe, checked before the
//!    console is touched: a secret written to a terminal or a file is shown or
//!    kept. A named FIFO is a pipe and is admitted; a socket is refused.
//! 2. `term::echo_off` clears `ECHO`, `ECHONL`, `ISIG`, `IEXTEN` and `EXTPROC`, sets
//!    `ICANON` and `ICRNL` (clearing `IGNCR` and `INLCR`), disables `VEOL` and
//!    `VEOL2`, applies that with `TCSETS` and reads it back, then applies it
//!    again with `TCSETSF`, which discards pending input, and reads it back
//!    again; nothing is read unless the kernel agrees. Canonical input keeps
//!    the erase and kill keys; with `ISIG` clear, `^C`, `^\` and `^Z` are
//!    input rather than signals that could kill or stop the reader with echo
//!    off; with `IEXTEN` clear, `^V` is an ordinary byte and cannot quote a
//!    newline into the record; a serial CR ends the record, and only a
//!    newline or `^D` does. See `term::echo_off` for the flush's window.
//! 3. The prompt is written, then ONE read takes one whole canonical record
//!    into a buffer of `RECORD_MAX` bytes, n_tty's `N_TTY_BUF_SIZE`. In
//!    canonical mode a read returns at most one record and leaves the rest of
//!    the input queued, and a record never exceeds that buffer: n_tty holds
//!    at most 4095 bytes of an unterminated line, dropping what is typed
//!    beyond them, and still admits its newline. So a record always arrives
//!    whole and nothing is ever drained. A record of at most `LINE_MAX` bytes
//!    plus its newline, holding no other newline, is the line. A longer one,
//!    or one holding a newline before its end, is refused with
//!    `EXIT_REFUSED`; nothing past it is consumed. A record without a newline
//!    ended at `^D`, and an empty one is end of input (`^D` alone, or a
//!    hung-up line): both refuse with `EXIT_EOF`, the partial entry
//!    discarded. Nothing is truncated.
//! 4. The saved settings are restored and read back on EVERY path that turned
//!    echo off: a line, end of input, a refused record or a read error. Then
//!    the console gets a newline, since the operator's was not echoed; after
//!    the restore, so a flow-controlled line that blocks it does so with echo
//!    back on.
//! 5. Only after the restore succeeded is the line written to stdout, in one
//!    write, so the caller's success also means the console echoes again.
//!
//! Exit status: 0 with the line on stdout; `EXIT_EOF` at end of input before a
//! newline; `EXIT_REFUSED` for an over-long record or one with a newline
//! inside; 1 for every other failure.
//! EOF is its own status because a console that returns end of input at once
//! would make a caller that re-prompts on every failure spin.
//!
//! Residuals. The record is held in one heap buffer that is zeroed on every path
//! and on drop; that is best effort in safe Rust (`zero`). The kernel's tty and
//! pipe buffers hold their own copies. This crate installs no signal handler
//! (`rt_sigaction(2)` is not on its surface), so a signal sent from elsewhere
//! whose default action terminates the process — `SIGKILL` above all — while
//! echo is off leaves the console silent until something sets it again; in the
//! selector that is at most until `kexec` or reset, which initialize the
//! console anew.

use crate::term;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::{AsFd, AsRawFd};
use std::os::unix::fs::{FileTypeExt, OpenOptionsExt};

/// The longest entry admitted, without its newline. td-protector's recovery-key
/// parser scans at most this many bytes.
pub const LINE_MAX: usize = 256;

/// n_tty's `N_TTY_BUF_SIZE`: no canonical record is longer, so one read of
/// this many bytes takes a whole record.
const RECORD_MAX: usize = 4096;

/// The longest prompt admitted.
pub const PROMPT_MAX: usize = 128;

/// End of input before a newline.
pub const EXIT_EOF: u8 = 3;
/// A record longer than `LINE_MAX` plus its newline, or holding a newline
/// before its end.
pub const EXIT_REFUSED: u8 = 4;

const CONSOLE: &str = "/dev/console";

/// `O_NOCTTY`: opening the console must not make it this process's controlling
/// terminal.
const O_NOCTTY: i32 = 0o400;

const USAGE: &str = "usage: secret-line PROMPT  (reads /dev/console; stdout must be a pipe)";

/// What `session` needs besides the settings: the line's input and its output.
pub trait Console: term::Termios {
    /// As `Read::read`: 0 is end of input.
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize>;
    /// Write the prompt or a newline; never the entry.
    fn write_out(&mut self, bytes: &[u8]) -> io::Result<()>;
}

/// A terminal device, with its settings reached through `term::Kernel`.
struct Tty {
    file: File,
    kernel: term::Kernel,
}

impl Tty {
    fn new(file: File) -> Tty {
        let kernel = term::Kernel::new(file.as_raw_fd());
        Tty { file, kernel }
    }
}

impl term::Termios for Tty {
    fn get(&mut self, out: &mut term::Bytes) -> io::Result<()> {
        self.kernel.get(out)
    }

    fn set(&mut self, termios: &term::Bytes) -> io::Result<()> {
        self.kernel.set(termios)
    }

    fn set_flushing(&mut self, termios: &term::Bytes) -> io::Result<()> {
        self.kernel.set_flushing(termios)
    }
}

impl Console for Tty {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.file.read(buf)
    }

    fn write_out(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.file.write_all(bytes)
    }
}

/// Overwrite a buffer that held secret bytes. `black_box` keeps the stores
/// observable so they are not elided as dead; best effort in safe Rust.
fn zero(buf: &mut [u8]) {
    buf.fill(0);
    std::hint::black_box(buf);
}

/// One entry: at most `LINE_MAX` bytes in a heap buffer of one whole record,
/// allocated once so the bytes are never copied by a move, and zeroed on drop.
struct Line {
    bytes: Box<[u8]>,
    len: usize,
}

impl Line {
    fn new() -> Line {
        Line {
            bytes: vec![0u8; RECORD_MAX].into_boxed_slice(),
            len: 0,
        }
    }

    fn as_bytes(&self) -> &[u8] {
        self.bytes.get(..self.len).unwrap_or(&[])
    }

    fn wipe(&mut self) {
        zero(&mut self.bytes);
        self.len = 0;
    }
}

impl Drop for Line {
    fn drop(&mut self) {
        self.wipe();
    }
}

/// How a read ended.
#[derive(Debug, PartialEq, Eq)]
enum Entry {
    /// A whole line, now in the `Line`.
    Line,
    /// End of input before a newline; any partial line is discarded.
    Eof,
    /// More than `LINE_MAX` bytes before the newline, or a newline inside.
    Refused,
}

/// The one operand: 1 to `PROMPT_MAX` printable ASCII bytes. Control bytes
/// are refused because the prompt is written to a console as-is.
fn prompt_of(args: &[String]) -> Result<&str, String> {
    let [prompt] = args else {
        return Err(format!("exactly one PROMPT operand is required\n{USAGE}"));
    };
    if prompt.is_empty() || prompt.len() > PROMPT_MAX {
        return Err(format!(
            "the prompt must be 1 to {PROMPT_MAX} bytes\n{USAGE}"
        ));
    }
    if !prompt.bytes().all(|b| (0x20..=0x7e).contains(&b)) {
        return Err(format!("the prompt must be printable ASCII\n{USAGE}"));
    }
    Ok(prompt)
}

/// One read, retrying an interrupted one.
fn read_record(console: &mut impl Console, buf: &mut [u8]) -> io::Result<usize> {
    loop {
        match console.read(buf) {
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            other => return other,
        }
    }
}

/// Read exactly one whole canonical record into `line` and judge it. The whole
/// buffer is zeroed on every path but a line's, whose bytes past the line are.
fn read_line(console: &mut impl Console, line: &mut Line) -> io::Result<Entry> {
    let n = match read_record(console, &mut line.bytes) {
        Ok(n) => n,
        Err(e) => {
            line.wipe();
            return Err(e);
        }
    };
    let record = line.bytes.get(..n).unwrap_or(&[]);
    let newline = record.iter().position(|b| *b == b'\n');
    match newline {
        Some(at) if at + 1 == n && at <= LINE_MAX => {
            if let Some(tail) = line.bytes.get_mut(at..) {
                zero(tail);
            }
            line.len = at;
            Ok(Entry::Line)
        }
        Some(_) => {
            line.wipe();
            Ok(Entry::Refused)
        }
        // Empty (end of input) or ended at `^D` without a newline.
        None => {
            line.wipe();
            Ok(Entry::Eof)
        }
    }
}

/// Echo off, the prompt, one line, and the restore on every path that follows.
fn session(console: &mut impl Console, prompt: &str, line: &mut Line) -> Result<Entry, String> {
    let saved = term::echo_off(console)?;
    let entry = match console.write_out(prompt.as_bytes()) {
        Ok(()) => read_line(console, line).map_err(|e| format!("reading {CONSOLE}: {e}")),
        Err(e) => Err(format!("writing the prompt to {CONSOLE}: {e}")),
    };
    let restored = term::restore(console, &saved);
    // Cosmetic, and after the restore: without it the next output shares the
    // entry's line, and a line that blocks it does so with echo back on.
    let _ = console.write_out(b"\n");
    match (entry, restored) {
        (Ok(entry), Ok(())) => Ok(entry),
        (Ok(_), Err(r)) => {
            line.wipe();
            Err(format!("restoring the console's settings: {r}"))
        }
        (Err(e), Ok(())) => Err(e),
        (Err(e), Err(r)) => Err(format!(
            "{e}; restoring the console's settings also failed: {r}"
        )),
    }
}

/// A duplicate of stdout, refused unless it is a pipe. Writing through this
/// `File` also keeps the line out of std's stdout buffer, which is never zeroed.
fn piped_stdout() -> Result<File, String> {
    let fd = io::stdout()
        .as_fd()
        .try_clone_to_owned()
        .map_err(|e| format!("stdout: {e}"))?;
    require_pipe(File::from(fd))
}

/// A pipe or a named FIFO, both `S_IFIFO`; a socket, terminal or file is not.
fn require_pipe(out: File) -> Result<File, String> {
    let kind = out
        .metadata()
        .map_err(|e| format!("stdout: {e}"))?
        .file_type();
    if kind.is_fifo() {
        Ok(out)
    } else {
        Err(format!(
            "stdout is not a pipe; refusing to write a secret where it could be shown or kept\n{USAGE}"
        ))
    }
}

pub fn run(args: &[String]) -> Result<u8, String> {
    let prompt = prompt_of(args)?;
    let out = piped_stdout()?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(O_NOCTTY)
        .open(CONSOLE)
        .map_err(|e| format!("{CONSOLE}: {e}"))?;
    let mut console = Tty::new(file);
    let mut line = Line::new();
    match session(&mut console, prompt, &mut line)? {
        Entry::Line => {
            (&out)
                .write_all(line.as_bytes())
                .map_err(|e| format!("writing the line to stdout: {e}"))?;
            Ok(0)
        }
        Entry::Eof => {
            crate::emit_err("secret-line: end of input before a whole line\n");
            Ok(EXIT_EOF)
        }
        Entry::Refused => {
            crate::emit_err(&format!(
                "secret-line: an entry longer than {LINE_MAX} bytes or holding a newline was refused\n"
            ));
            Ok(EXIT_REFUSED)
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]
    use super::*;
    use std::collections::VecDeque;

    /// A console that records every exchange in order and reads as a canonical
    /// terminal does: each read returns one whole record, leaving the next
    /// queued. Its settings are opaque bytes; what echo-off means is `term`'s,
    /// tested there.
    struct Script {
        settings: term::Bytes,
        original: term::Bytes,
        /// Typed before the prompt: discarded by a flushing set.
        typeahead: VecDeque<Vec<u8>>,
        records: VecDeque<Vec<u8>>,
        log: Vec<String>,
        sets: usize,
        fail_set: Option<usize>,
        fail_read_after: Option<usize>,
        reads: usize,
        interrupt_first: bool,
    }

    impl Script {
        fn new(records: &[&[u8]]) -> Script {
            // Arbitrary bytes the patch is certain to change somewhere.
            let mut settings = [0u8; std::mem::size_of::<term::Bytes>()];
            for (i, slot) in settings.iter_mut().enumerate() {
                *slot = 0xff ^ i as u8;
            }
            Script {
                settings,
                original: settings,
                typeahead: VecDeque::new(),
                records: records.iter().map(|r| r.to_vec()).collect(),
                log: Vec::new(),
                sets: 0,
                fail_set: None,
                fail_read_after: None,
                reads: 0,
                interrupt_first: false,
            }
        }

        fn echoing(&self) -> bool {
            self.settings == self.original
        }

        fn store(&mut self, termios: &term::Bytes) -> io::Result<bool> {
            self.sets += 1;
            if self.fail_set == Some(self.sets) {
                self.log.push("set-refused".into());
                return Err(io::Error::other("refused"));
            }
            self.settings = *termios;
            Ok(self.echoing())
        }
    }

    impl term::Termios for Script {
        fn get(&mut self, out: &mut term::Bytes) -> io::Result<()> {
            self.log.push("get".into());
            *out = self.settings;
            Ok(())
        }
        fn set(&mut self, termios: &term::Bytes) -> io::Result<()> {
            let echoing = self.store(termios)?;
            self.log
                .push(if echoing { "restore" } else { "silence" }.into());
            Ok(())
        }
        fn set_flushing(&mut self, termios: &term::Bytes) -> io::Result<()> {
            self.store(termios)?;
            self.typeahead.clear();
            self.log.push("flush-silence".into());
            Ok(())
        }
    }

    impl Console for Script {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            assert!(!self.echoing(), "read while the console still echoes");
            assert_eq!(buf.len(), RECORD_MAX, "one whole record's buffer");
            assert!(
                self.log.iter().any(|e| e == "prompt"),
                "read before the prompt"
            );
            if self.interrupt_first {
                self.interrupt_first = false;
                return Err(io::ErrorKind::Interrupted.into());
            }
            if self.fail_read_after == Some(self.reads) {
                self.log.push("read-error".into());
                return Err(io::Error::other("line hung up"));
            }
            self.reads += 1;
            let Some(record) = self
                .typeahead
                .pop_front()
                .or_else(|| self.records.pop_front())
            else {
                return Ok(0);
            };
            // n_tty never holds a record longer than its buffer.
            assert!(record.len() <= buf.len(), "not a canonical record");
            buf[..record.len()].copy_from_slice(&record);
            Ok(record.len())
        }
        fn write_out(&mut self, bytes: &[u8]) -> io::Result<()> {
            if bytes == b"\n" {
                self.log.push("newline".into());
            } else {
                assert!(!self.echoing(), "prompt while the console still echoes");
                self.log.push("prompt".into());
            }
            Ok(())
        }
    }

    fn entry(records: &[&[u8]]) -> (Result<Entry, String>, Vec<u8>, Script) {
        let mut console = Script::new(records);
        let mut line = Line::new();
        let got = session(&mut console, "Key: ", &mut line);
        (got, line.as_bytes().to_vec(), console)
    }

    /// The restore is the last settings exchange on every path, after the
    /// last read, and the newline follows it.
    fn restored_last(console: &Script) {
        assert!(console.echoing(), "the console was left silent");
        let tail: Vec<&str> = console
            .log
            .iter()
            .rev()
            .take(3)
            .map(String::as_str)
            .collect();
        assert_eq!(tail, ["newline", "get", "restore"], "{:?}", console.log);
    }

    #[test]
    fn a_line_is_returned_without_its_newline_and_the_rest_is_left_unread() {
        let (got, line, console) = entry(&[b"12345-67890\n", b"next\n"]);
        assert_eq!(got, Ok(Entry::Line));
        assert_eq!(line, b"12345-67890");
        assert_eq!(console.records, [b"next\n".to_vec()], "read past the line");
        restored_last(&console);
        assert_eq!(
            &console.log[..6],
            ["get", "silence", "get", "flush-silence", "get", "prompt"],
            "echo is turned off and read back, then flushed and read back, \
             before the prompt"
        );
    }

    /// Input typed before echo went off is discarded, not read as the entry.
    #[test]
    fn typeahead_before_the_prompt_is_flushed() {
        let mut console = Script::new(&[b"4242\n"]);
        console.typeahead.push_back(b"echoed-early\n".to_vec());
        let mut line = Line::new();
        assert_eq!(session(&mut console, "Key: ", &mut line), Ok(Entry::Line));
        assert_eq!(line.as_bytes(), b"4242");
    }

    #[test]
    fn an_empty_line_is_a_line() {
        let (got, line, console) = entry(&[b"\n"]);
        assert_eq!(got, Ok(Entry::Line));
        assert!(line.is_empty());
        restored_last(&console);
    }

    #[test]
    fn exactly_the_bound_is_admitted() {
        let mut input = vec![b'7'; LINE_MAX];
        input.push(b'\n');
        let (got, line, console) = entry(&[&input]);
        assert_eq!(got, Ok(Entry::Line));
        assert_eq!(line, vec![b'7'; LINE_MAX]);
        restored_last(&console);
    }

    /// One byte over is refused, never truncated, and nothing past its record
    /// is consumed.
    #[test]
    fn one_byte_over_the_bound_is_refused_and_nothing_else_consumed() {
        let mut long = vec![b'7'; LINE_MAX + 1];
        long.push(b'\n');
        let (got, line, console) = entry(&[&long, b"next\n"]);
        assert_eq!(got, Ok(Entry::Refused));
        assert!(
            line.is_empty(),
            "an over-long entry is not returned in part"
        );
        assert_eq!(console.records, [b"next\n".to_vec()]);
        assert_eq!(console.reads, 1);
        restored_last(&console);
    }

    /// A newline before a record's end — only `^V` could put one there, and
    /// echo-off clears IEXTEN — is refused, not split into two entries.
    #[test]
    fn a_newline_inside_a_record_is_refused() {
        let (got, line, console) = entry(&[b"12\n34\n"]);
        assert_eq!(got, Ok(Entry::Refused));
        assert!(line.is_empty());
        restored_last(&console);
    }

    /// The longest record n_tty delivers: 4095 bytes and a newline.
    #[test]
    fn the_longest_record_is_refused_whole() {
        let mut longest = vec![b'7'; RECORD_MAX - 1];
        longest.push(b'\n');
        let (got, line, console) = entry(&[&longest]);
        assert_eq!(got, Ok(Entry::Refused));
        assert!(line.is_empty());
        restored_last(&console);
    }

    /// `123^D` is a record without a newline: refused as end of input, with
    /// the next line left for the next entry rather than joined to it. An
    /// over-long one ended at `^D` is end of input too.
    #[test]
    fn end_of_input_is_refused_and_a_partial_line_discarded() {
        let long = vec![b'7'; LINE_MAX + 1];
        for first in [&b""[..], b"1234", &long] {
            let (got, line, console) = entry(&[first, b"456\n"]);
            assert_eq!(got, Ok(Entry::Eof), "{first:?}");
            assert!(line.is_empty());
            assert_eq!(console.records, [b"456\n".to_vec()]);
            restored_last(&console);
        }
    }

    #[test]
    fn an_interrupted_read_is_retried() {
        let mut console = Script::new(&[b"42\n"]);
        console.interrupt_first = true;
        let mut line = Line::new();
        assert_eq!(session(&mut console, "Key: ", &mut line), Ok(Entry::Line));
        assert_eq!(line.as_bytes(), b"42");
    }

    /// A read error still restores, before the newline, and wipes the buffer.
    #[test]
    fn a_read_error_restores_before_it_is_reported() {
        let mut console = Script::new(&[b"9\n"]);
        console.fail_read_after = Some(0);
        let mut line = Line::new();
        line.bytes.fill(b'7');
        let error = session(&mut console, "Key: ", &mut line).unwrap_err();
        assert!(error.contains("line hung up"), "{error}");
        assert!(line.as_bytes().is_empty());
        assert!(
            line.bytes.iter().all(|b| *b == 0),
            "the partial entry survived"
        );
        restored_last(&console);
    }

    /// Echo that cannot be turned off is never read under, and no prompt
    /// appears.
    #[test]
    fn nothing_is_read_unless_echo_turned_off() {
        let mut console = Script::new(&[b"secret\n"]);
        console.fail_set = Some(1);
        let mut line = Line::new();
        assert!(session(&mut console, "Key: ", &mut line).is_err());
        assert_eq!(console.reads, 0);
        assert_eq!(console.records.len(), 1);
        assert!(!console.log.iter().any(|e| e == "prompt" || e == "newline"));
    }

    /// A line read under a restore that failed is not handed back: the caller's
    /// success must mean the console echoes again.
    #[test]
    fn a_failed_restore_withholds_and_wipes_the_line() {
        let mut console = Script::new(&[b"secret\n"]);
        console.fail_set = Some(3);
        let mut line = Line::new();
        let error = session(&mut console, "Key: ", &mut line).unwrap_err();
        assert!(error.contains("restoring"), "{error}");
        assert!(line.bytes.iter().all(|b| *b == 0));
        let tail: Vec<&str> = console
            .log
            .iter()
            .rev()
            .take(2)
            .map(String::as_str)
            .collect();
        assert_eq!(tail, ["newline", "set-refused"]);
    }

    #[test]
    fn a_wiped_line_is_zero() {
        let mut line = Line::new();
        line.bytes.fill(0x39);
        line.len = 10;
        line.wipe();
        assert_eq!(line.len, 0);
        assert!(line.bytes.iter().all(|b| *b == 0));
        assert_eq!(line.bytes.len(), RECORD_MAX);
    }

    #[test]
    fn stdout_must_be_a_pipe_or_named_fifo() {
        let null = File::open("/dev/null").unwrap();
        let error = require_pipe(null).unwrap_err();
        assert!(error.contains("not a pipe"), "{error}");
        let (_reader, writer) = std::io::pipe().unwrap();
        assert!(require_pipe(File::from(std::os::fd::OwnedFd::from(writer))).is_ok());
        let file = File::open(std::env::current_exe().unwrap()).unwrap();
        assert!(require_pipe(file).is_err());
        let (socket, _peer) = std::os::unix::net::UnixStream::pair().unwrap();
        let socket = File::from(std::os::fd::OwnedFd::from(socket));
        assert!(require_pipe(socket).is_err(), "a socket is not a pipe");
    }

    /// The prompt is the one operand, bounded and printable; it is checked
    /// before stdout or the console is touched.
    #[test]
    fn the_prompt_is_one_bounded_printable_operand() {
        let args = |xs: &[&str]| xs.iter().map(|s| (*s).to_string()).collect::<Vec<_>>();
        assert_eq!(prompt_of(&args(&["Recovery key: "])), Ok("Recovery key: "));
        let longest = "k".repeat(PROMPT_MAX);
        assert_eq!(prompt_of(&args(&[&longest])), Ok(longest.as_str()));
        for bad in [
            args(&[]),
            args(&["a", "b"]),
            args(&[""]),
            args(&[&"k".repeat(PROMPT_MAX + 1)]),
            args(&["key\n"]),
            args(&["\u{1b}[2J"]),
            args(&["clé"]),
        ] {
            let error = prompt_of(&bad).unwrap_err();
            assert!(error.contains("usage: secret-line"), "{error}");
            assert!(run(&bad).is_err());
        }
    }

    /// The canonical reading against the kernel's own line discipline, on a
    /// pseudo-terminal (td-ui's PTY, a test-only dependency). Each boundary
    /// case is followed by an ordinary line, which must arrive intact: a
    /// reader that consumed past its record, or blocked in a drain, would
    /// swallow or join it. Typeahead before every prompt is flushed, and the
    /// session's closing newline — queued behind any echo — must arrive with
    /// nothing typed shown before it.
    ///
    /// n_tty's limit: an unterminated line holds at most 4095 bytes; what is
    /// typed beyond that is dropped and the newline still ends the record, so
    /// 4096 or 5000 bytes and a newline arrive as one 4096-byte record,
    /// refused whole.
    #[test]
    fn a_real_terminal_reads_whole_records() {
        use std::sync::mpsc;
        use std::time::Duration;

        let pty = td_ui::pty::Pty::open().unwrap();
        let mut master = pty.master().try_clone().unwrap();
        let mut output = master.try_clone().unwrap();
        let (seen, screen) = mpsc::channel::<Vec<u8>>();
        std::thread::spawn(move || {
            let mut chunk = [0u8; 512];
            while let Ok(n) = output.read(&mut chunk) {
                if n == 0 || seen.send(chunk[..n].to_vec()).is_err() {
                    break;
                }
            }
        });
        let mut shown = Vec::new();
        let wait_for = |needle: &[u8], shown: &mut Vec<u8>, from: usize| {
            while !shown[from..].windows(needle.len()).any(|w| w == needle) {
                let chunk = screen.recv_timeout(Duration::from_secs(10));
                assert!(chunk.is_ok(), "never saw {needle:?} in {shown:?}");
                shown.extend_from_slice(&chunk.unwrap());
            }
        };
        let sevens = |n: usize, end: &[u8]| {
            let mut v = vec![b'7'; n];
            v.extend_from_slice(end);
            v
        };
        let bound = sevens(LINE_MAX, b"");
        let boundary: Vec<(Vec<u8>, Entry, Vec<u8>)> = vec![
            (b"123\x04".to_vec(), Entry::Eof, vec![]),
            (b"\x04".to_vec(), Entry::Eof, vec![]),
            (sevens(LINE_MAX, b"\n"), Entry::Line, bound),
            (sevens(LINE_MAX + 1, b"\n"), Entry::Refused, vec![]),
            (sevens(LINE_MAX + 1, b"\x04"), Entry::Eof, vec![]),
            (sevens(RECORD_MAX - 1, b"\n"), Entry::Refused, vec![]),
            (sevens(RECORD_MAX, b"\n"), Entry::Refused, vec![]),
            (sevens(5000, b"\n"), Entry::Refused, vec![]),
            // IEXTEN is clear, so ^V is an ordinary byte: the record ends at
            // the newline it would have quoted, and the parser refuses 0x16.
            (b"12\x16\n".to_vec(), Entry::Line, b"12\x16".to_vec()),
        ];
        // One slave held open throughout: a PTY whose every slave is closed
        // reads as hung up on the master side.
        let held = pty.peer().unwrap();
        let mut round = 0;
        for (typed, want, line_want) in boundary {
            let next = (b"4242\n".to_vec(), Entry::Line, b"4242".to_vec());
            for (typed, want, line_want) in [(typed, want, line_want), next] {
                round += 1;
                // Typed before the prompt: flushed by the echo-off.
                master.write_all(b"999\n").unwrap();
                wait_for(b"999", &mut shown, 0);
                let slave = held.try_clone().unwrap();
                let prompt = format!("P{round}> ");
                let session_prompt = prompt.clone();
                let reader = std::thread::spawn(move || {
                    let mut tty = Tty::new(slave);
                    let mut line = Line::new();
                    let got = session(&mut tty, &session_prompt, &mut line);
                    (got, line.as_bytes().to_vec())
                });
                wait_for(prompt.as_bytes(), &mut shown, 0);
                let after_prompt = shown.len();
                master.write_all(&typed).unwrap();
                let (got, line) = reader.join().unwrap();
                assert_eq!(got, Ok(want), "round {round}");
                assert_eq!(line, line_want, "round {round}");
                wait_for(b"\r\n", &mut shown, after_prompt);
                let echoed = &shown[after_prompt..];
                assert!(
                    !echoed.iter().any(u8::is_ascii_digit),
                    "round {round} echoed what was typed: {echoed:?}"
                );
                shown.clear();
            }
        }
    }
}

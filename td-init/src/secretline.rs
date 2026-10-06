//! `secret-line PROMPT` — print PROMPT on the console's lines once echo is
//! off, read one line from whichever completes an entry first with echo off,
//! and write it, without its newline, to stdout, which must be a pipe.
//!
//! This is the selector's recovery-key entry (td-install/ENCRYPTION.md,
//! "Device-bound default" and "Keyboard console"): the digits are never
//! echoed. A recorder on a line itself may still log what is typed; that is
//! the review's disclosure, not this applet's to prevent. It takes no device
//! operand: a caller could otherwise point the read at any terminal, and
//! nothing on the recovery path needs one. Its lines are fixed: `/dev/console`,
//! which the selector's built-in command line makes the serial console, and
//! `/dev/tty1`, the first virtual terminal, which is the foreground one since
//! nothing in either initramfs switches VTs. Which terminal `/dev/console` is
//! comes from `console::read`: when it is the VT, or cannot be told, the two
//! names are one terminal and `/dev/console` is the one line. Each line is
//! opened read-write, `O_NOCTTY`, so reading it claims no session, and
//! `O_NONBLOCK`, so that no step waits on one line's output while the other
//! could be read: a line stopped by flow control (a VT stopped by Scroll Lock
//! or `^S`, a serial line held by XOFF) cannot stop entry on the other.
//!
//! The one operand is the prompt, which is not secret: 1 to `PROMPT_MAX`
//! printable ASCII bytes. The applet prints it rather than its caller so that
//! it appears only once echo is off and pending input has been discarded.
//!
//! Order, and the reason for each step:
//!
//! 1. The prompt is checked, then stdout must be a pipe, checked before any
//!    line is touched: a secret written to a terminal or a file is shown or
//!    kept. A named FIFO is a pipe and is admitted; a socket is refused.
//! 2. Every line's settings are saved before any line changes, so a restore
//!    never writes back settings the applet itself silenced. Then, serial
//!    first, `term::silence` applies the echo-off patch with `TCSETS`, reads
//!    it back, and discards pending input with `TCFLSH`: a key typed before
//!    it was echoed under the old settings and is neither kept nor joined to
//!    the entry. A line that cannot be opened, or whose settings cannot be
//!    read, is skipped untouched; one whose settings cannot be set or read
//!    back as computed is skipped with its settings restored and its input
//!    flushed. A note naming either is queued on the line that remains. With
//!    no line left the applet fails.
//! 3. Only then is the prompt queued on every line, after any note, and
//!    written without blocking: a write that would block or is interrupted
//!    leaves the queue, a short one leaves the rest, and either is finished
//!    when `poll(2)` says the line takes output. Only a hard error drops a
//!    line: a write failing with `EIO`, `ENXIO` or `EBADF`, or `poll`
//!    reporting an error without a hang-up. A dropped line is restored and
//!    flushed, and a note is queued on the line that remains. Any other
//!    write error, or a write that takes nothing, abandons that line's queue
//!    and keeps the line readable.
//! 4. One wait, with no timeout, ends at the first line holding a whole
//!    canonical record or hung up; the serial line wins a tie. That line's
//!    record is read into a buffer of `RECORD_MAX` bytes, n_tty's
//!    `N_TTY_BUF_SIZE`: a canonical read returns at most one record, and
//!    n_tty holds at most 4095 bytes of an unterminated line and still
//!    admits its newline, so a record arrives whole and nothing is drained. A
//!    record of at most `LINE_MAX` bytes plus its newline, holding no other
//!    newline, is the line. A longer one, or one holding a newline before its
//!    end, is refused with `EXIT_REFUSED`, consuming nothing after it. A
//!    record ended at `^D` without a newline, and an empty read (`^D` alone,
//!    or a hung-up line), are end of input: `EXIT_EOF`, on either line.
//!    Prompt bytes still queued are discarded.
//! 5. Every line is restored and read back on every path that silenced it,
//!    and every line, the one read included, is then flushed, discarding
//!    what was typed there with echo off, partial or whole, and anything
//!    after the record read; each restore and flush is attempted whatever
//!    the others did. Each line then gets one attempt
//!    at a newline, since the operator's was not echoed; it is never waited
//!    on, since that would hold an entry already made.
//! 6. Only once every restore and flush succeeded is the line written to
//!    stdout, in one write, so the caller's success also means the lines
//!    echo again and hold nothing typed.
//!
//! Exit status: 0 with the line on stdout; `EXIT_EOF` at end of input before a
//! newline; `EXIT_REFUSED` for an over-long record or one with a newline
//! inside; 1 for every other failure, a run with no line left among them.
//! EOF is its own status because a console that returns end of input at once
//! would make a caller that re-prompts on every failure spin.
//!
//! Residuals. The record is held in one heap buffer that is zeroed on every path
//! and on drop; that is best effort in safe Rust (`zero`). The kernel's tty and
//! pipe buffers hold their own copies. This crate installs no signal handler
//! (`rt_sigaction(2)` is not on its surface), so a signal sent from elsewhere
//! whose default action terminates the process — `SIGKILL` above all — while
//! echo is off leaves the lines silent until something sets them again; in the
//! selector that is at most until `kexec` or reset, which initialize the
//! console anew.

use crate::{console, term};
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd};
use std::os::unix::fs::{FileTypeExt, OpenOptionsExt};
use std::path::Path;

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

/// The kernel's console, the serial line in the selector.
const CONSOLE: &str = "/dev/console";
/// The first virtual terminal: the foreground one, since nothing switches.
const VT: &str = "/dev/tty1";

/// `O_NOCTTY`: opening a line must not make it this process's controlling
/// terminal.
const O_NOCTTY: i32 = 0o400;
/// `O_NONBLOCK`: no read or write waits on one line while the other could be
/// read.
const O_NONBLOCK: i32 = 0o4000;

/// The write errors that drop a line. A hung-up terminal answers `EIO`.
const EIO: i32 = 5;
const ENXIO: i32 = 6;
const EBADF: i32 = 9;

const USAGE: &str =
    "usage: secret-line PROMPT  (reads /dev/console and /dev/tty1; stdout must be a pipe)";

/// What `session` needs of a line besides its settings: non-blocking input
/// and output.
pub trait Line: term::Termios {
    /// As `Read::read`: 0 is end of input; `WouldBlock` is no record yet.
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize>;
    /// One write of the prompt, a note or a newline; never the entry.
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize>;
}

/// How `session` waits on the lines it holds: fill `found` for `lines`.
trait Wait<L> {
    fn wait(&mut self, lines: &mut [Kept<L>], found: &mut [term::Seen]) -> io::Result<()>;
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

    fn flush_input(&mut self) -> io::Result<()> {
        self.kernel.flush_input()
    }
}

impl Line for Tty {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.file.read(buf)
    }

    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.file.write(bytes)
    }
}

/// The kernel's wait, `term::wait`.
struct Polled;

impl Wait<Tty> for Polled {
    fn wait(&mut self, lines: &mut [Kept<Tty>], found: &mut [term::Seen]) -> io::Result<()> {
        let watched: Vec<(BorrowedFd<'_>, bool)> = lines
            .iter()
            .map(|kept| (kept.line.file.as_fd(), kept.queued()))
            .collect();
        term::wait(&watched, found)
    }
}

/// One line the session holds: its settings as saved, and its queued output.
struct Kept<L> {
    line: L,
    name: String,
    saved: term::Saved,
    queue: Vec<u8>,
    sent: usize,
}

impl<L: Line> Kept<L> {
    fn queued(&self) -> bool {
        self.sent < self.queue.len()
    }

    /// Write what is queued until it is done, would block, is interrupted or
    /// is short. Only a hard error is returned; any other, or a write of
    /// nothing, abandons the queue.
    fn send(&mut self) -> io::Result<()> {
        while let Some(rest) = self.queue.get(self.sent..).filter(|r| !r.is_empty()) {
            let want = rest.len();
            match self.line.write(rest) {
                // Nothing written to a line that asked for output: give up
                // its queue rather than be woken for it again and again.
                Ok(0) => {
                    self.sent = self.queue.len();
                    return Ok(());
                }
                Ok(n) => {
                    self.sent = self.sent.saturating_add(n);
                    if n < want {
                        return Ok(());
                    }
                }
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                    ) =>
                {
                    return Ok(())
                }
                Err(e) if matches!(e.raw_os_error(), Some(EIO | ENXIO | EBADF)) => return Err(e),
                Err(_) => {
                    self.sent = self.queue.len();
                    return Ok(());
                }
            }
        }
        Ok(())
    }

    /// Queue a note naming another line: before the prompt if none of it
    /// was written yet, else on a line of its own with the prompt again.
    fn tell(&mut self, note: &str, prompt: &str) {
        let text = format!("secret-line: {note}\n");
        if self.sent == 0 {
            self.queue.splice(0..0, text.into_bytes());
        } else {
            self.queue.push(b'\n');
            self.queue.extend_from_slice(text.as_bytes());
            self.queue.extend_from_slice(prompt.as_bytes());
        }
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
struct Record {
    bytes: Box<[u8]>,
    len: usize,
}

impl Record {
    fn new() -> Record {
        Record {
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

impl Drop for Record {
    fn drop(&mut self) {
        self.wipe();
    }
}

/// How a read ended.
#[derive(Debug, PartialEq, Eq)]
enum Entry {
    /// A whole line, now in the `Record`.
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
fn read_record(line: &mut impl Line, buf: &mut [u8]) -> io::Result<usize> {
    loop {
        match line.read(buf) {
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            other => return other,
        }
    }
}

/// Read exactly one whole canonical record into `record` and judge it. The
/// whole buffer is zeroed on every path but a line's, whose bytes past the
/// line are.
fn read_line(line: &mut impl Line, record: &mut Record) -> io::Result<Entry> {
    let n = match read_record(line, &mut record.bytes) {
        Ok(n) => n,
        Err(e) => {
            record.wipe();
            return Err(e);
        }
    };
    let bytes = record.bytes.get(..n).unwrap_or(&[]);
    let newline = bytes.iter().position(|b| *b == b'\n');
    match newline {
        Some(at) if at + 1 == n && at <= LINE_MAX => {
            if let Some(tail) = record.bytes.get_mut(at..) {
                zero(tail);
            }
            record.len = at;
            Ok(Entry::Line)
        }
        Some(_) => {
            record.wipe();
            Ok(Entry::Refused)
        }
        // Empty (end of input) or ended at `^D` without a newline.
        None => {
            record.wipe();
            Ok(Entry::Eof)
        }
    }
}

/// The lines to offer: `/dev/console` alone when it is the VT or cannot be
/// told, else `/dev/console` and then the VT.
fn line_names(console: console::Console) -> &'static [&'static str] {
    if console.vt_is_second() {
        &[CONSOLE, VT]
    } else {
        &[CONSOLE]
    }
}

/// Save every line, then silence each, serial first; queue the notes and
/// the prompt on what is kept.
fn prepare<L: Line>(
    opened: Vec<(L, String)>,
    mut notes: Vec<String>,
    prompt: &str,
) -> Result<Vec<Kept<L>>, String> {
    let mut saved = Vec::with_capacity(opened.len());
    for (mut line, name) in opened {
        match term::save(&mut line) {
            Ok(settings) => saved.push((line, name, settings)),
            Err(e) => notes.push(format!("{name} is not offered: {e}")),
        }
    }
    let mut kept = Vec::with_capacity(saved.len());
    for (mut line, name, settings) in saved {
        match term::silence(&mut line, &settings) {
            Ok(()) => kept.push(Kept {
                line,
                name,
                saved: settings,
                queue: Vec::new(),
                sent: 0,
            }),
            Err(e) => notes.push(format!("{name} is not offered: {e}")),
        }
    }
    if kept.is_empty() {
        return Err(no_line(&notes));
    }
    for line in &mut kept {
        for note in &notes {
            line.queue
                .extend_from_slice(format!("secret-line: {note}\n").as_bytes());
        }
        line.queue.extend_from_slice(prompt.as_bytes());
    }
    Ok(kept)
}

fn no_line(notes: &[String]) -> String {
    format!("no line to read the entry from: {}", notes.join("; "))
}

/// Drop the line at `at` after a hard error: restore and flush it, and tell
/// the line that remains. With none left, the session fails.
fn drop_line<L: Line>(
    kept: &mut Vec<Kept<L>>,
    at: usize,
    error: &str,
    prompt: &str,
) -> Result<(), String> {
    if at >= kept.len() {
        return Ok(());
    }
    let mut gone = kept.remove(at);
    let note = match term::put_back(&mut gone.line, &gone.saved) {
        Ok(()) => format!("{} was dropped: {error}", gone.name),
        Err(r) => format!(
            "{} was dropped: {error}; restoring or flushing it also failed: {r}",
            gone.name
        ),
    };
    if kept.is_empty() {
        return Err(no_line(&[note]));
    }
    for line in kept.iter_mut() {
        line.tell(&note, prompt);
    }
    Ok(())
}

/// Silence the lines, show the prompt, read the first whole record from
/// either line, and restore every line on every path that follows.
fn session<L: Line>(
    opened: Vec<(L, String)>,
    notes: Vec<String>,
    prompt: &str,
    record: &mut Record,
    waiter: &mut impl Wait<L>,
) -> Result<Entry, String> {
    let mut kept = prepare(opened, notes, prompt)?;
    let mut found = vec![term::Seen::default(); kept.len()];
    let mut at = kept.len();
    while at > 0 {
        at -= 1;
        let sent = kept.get_mut(at).map_or(Ok(()), Kept::send);
        if let Err(e) = sent {
            drop_line(&mut kept, at, &format!("writing: {e}"), prompt)?;
        }
    }
    loop {
        found.truncate(kept.len());
        found.fill(term::Seen::default());
        if let Err(e) = waiter.wait(&mut kept, &mut found) {
            return finish(&mut kept, Err(format!("poll: {e}")), record);
        }
        // A record, or a hang-up, ends the wait; the serial line wins a tie.
        if let Some(at) = found.iter().position(|seen| seen.input) {
            let Some(line) = kept.get_mut(at) else {
                continue;
            };
            let name = line.name.clone();
            let entry = match read_line(&mut line.line, record) {
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => continue,
                other => other.map_err(|e| format!("reading {name}: {e}")),
            };
            return finish(&mut kept, entry, record);
        }
        // Back to front, so a drop does not move a line still to be seen.
        let mut at = kept.len();
        while at > 0 {
            at -= 1;
            let Some(seen) = found.get(at).copied() else {
                continue;
            };
            if seen.failed {
                drop_line(&mut kept, at, "poll reported an error", prompt)?;
            } else if seen.output {
                let sent = kept.get_mut(at).map_or(Ok(()), Kept::send);
                if let Err(e) = sent {
                    drop_line(&mut kept, at, &format!("writing: {e}"), prompt)?;
                }
            }
        }
    }
}

/// Restore every line, then flush every line, the one read included, so
/// neither a second record (a pasted key's CR LF) nor keys typed after the
/// entry stay queued unechoed for the next reader; try one newline on each,
/// and only then let `entry` stand. Each restore and each flush is
/// attempted whatever the others did.
fn finish<L: Line>(
    kept: &mut [Kept<L>],
    entry: Result<Entry, String>,
    record: &mut Record,
) -> Result<Entry, String> {
    let mut failures = Vec::new();
    for line in kept.iter_mut() {
        if let Err(e) = term::restore(&mut line.line, &line.saved) {
            failures.push(format!("restoring {}'s settings: {e}", line.name));
        }
    }
    for line in kept.iter_mut() {
        if let Err(e) = term::flush(&mut line.line) {
            failures.push(format!("flushing {}: {e}", line.name));
        }
    }
    // Cosmetic, after the restore, and tried once: a stopped line is not
    // waited on, since that would hold an entry already made.
    for line in kept.iter_mut() {
        let _ = line.line.write(b"\n");
    }
    if failures.is_empty() {
        return entry;
    }
    record.wipe();
    let failed = failures.join("; ");
    match entry {
        Ok(_) => Err(failed),
        Err(e) => Err(format!("{e}; {failed}")),
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

/// One line, read-write, `O_NOCTTY` and `O_NONBLOCK`.
fn open_line(path: &str) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(O_NOCTTY | O_NONBLOCK)
        .open(path)
}

/// Open `names`, noting each that does not open.
fn open_lines(names: &[&str]) -> (Vec<(Tty, String)>, Vec<String>) {
    let mut opened = Vec::with_capacity(names.len());
    let mut notes = Vec::new();
    for name in names {
        match open_line(name) {
            Ok(file) => opened.push((Tty::new(file), (*name).to_string())),
            Err(e) => notes.push(format!("{name} is not offered: {e}")),
        }
    }
    (opened, notes)
}

pub fn run(args: &[String]) -> Result<u8, String> {
    let prompt = prompt_of(args)?;
    let out = piped_stdout()?;
    let (opened, notes) = open_lines(line_names(console::read(Path::new(console::ACTIVE))));
    let mut record = Record::new();
    match session(opened, notes, prompt, &mut record, &mut Polled)? {
        Entry::Line => {
            (&out)
                .write_all(record.as_bytes())
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
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
    use super::*;
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::rc::Rc;

    type Log = Rc<RefCell<Vec<String>>>;

    /// A line that reads as a canonical terminal does: each read returns one
    /// whole record, leaving the next queued, and a partial line is never
    /// readable. Its settings are opaque bytes; what echo-off means is
    /// `term`'s, tested there. Every exchange goes to one log shared by the
    /// lines of a session, prefixed with the line's name.
    struct Script {
        name: &'static str,
        log: Log,
        settings: term::Bytes,
        original: term::Bytes,
        /// Typed before the flush: discarded by it.
        typeahead: VecDeque<Vec<u8>>,
        /// Whole records, readable.
        records: VecDeque<Vec<u8>>,
        /// An unterminated line: never readable, discarded by a flush.
        partial: Vec<u8>,
        hung: bool,
        shown: Vec<u8>,
        stopped: bool,
        short: Option<usize>,
        /// Every write but the closing newline takes nothing.
        takes_nothing: bool,
        write_error: Option<i32>,
        poll_error: bool,
        fail_get: bool,
        sets: usize,
        fail_set: Option<usize>,
        reads: usize,
    }

    impl Script {
        fn new(name: &'static str, log: &Log) -> Script {
            // Arbitrary bytes the patch is certain to change somewhere.
            let mut settings = [0u8; std::mem::size_of::<term::Bytes>()];
            for (i, slot) in settings.iter_mut().enumerate() {
                *slot = 0xff ^ i as u8;
            }
            Script {
                name,
                log: Rc::clone(log),
                settings,
                original: settings,
                typeahead: VecDeque::new(),
                records: VecDeque::new(),
                partial: Vec::new(),
                hung: false,
                shown: Vec::new(),
                stopped: false,
                short: None,
                takes_nothing: false,
                write_error: None,
                poll_error: false,
                fail_get: false,
                sets: 0,
                fail_set: None,
                reads: 0,
            }
        }

        fn echoing(&self) -> bool {
            self.settings == self.original
        }

        fn note(&self, what: &str) {
            self.log.borrow_mut().push(format!("{}:{what}", self.name));
        }
    }

    impl term::Termios for Script {
        fn get(&mut self, out: &mut term::Bytes) -> io::Result<()> {
            self.note("get");
            if self.fail_get {
                return Err(io::Error::other("not a terminal"));
            }
            *out = self.settings;
            Ok(())
        }
        fn set(&mut self, termios: &term::Bytes) -> io::Result<()> {
            self.sets += 1;
            if self.fail_set == Some(self.sets) {
                self.note("set-refused");
                return Err(io::Error::other("refused"));
            }
            self.settings = *termios;
            self.note(if self.echoing() { "restore" } else { "silence" });
            Ok(())
        }
        fn flush_input(&mut self) -> io::Result<()> {
            self.note("flush");
            self.typeahead.clear();
            self.records.clear();
            self.partial.clear();
            Ok(())
        }
    }

    impl Line for Script {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            assert!(!self.echoing(), "{} read while it still echoes", self.name);
            assert_eq!(buf.len(), RECORD_MAX, "one whole record's buffer");
            assert!(self.typeahead.is_empty(), "read before the flush");
            self.reads += 1;
            self.note("read");
            let Some(record) = self.records.pop_front() else {
                if self.hung {
                    return Ok(0);
                }
                return Err(io::ErrorKind::WouldBlock.into());
            };
            // n_tty never holds a record longer than its buffer.
            assert!(record.len() <= buf.len(), "not a canonical record");
            buf[..record.len()].copy_from_slice(&record);
            Ok(record.len())
        }
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if let Some(errno) = self.write_error {
                self.note("write-error");
                return Err(io::Error::from_raw_os_error(errno));
            }
            if self.stopped {
                return Err(io::ErrorKind::WouldBlock.into());
            }
            if self.takes_nothing && bytes != b"\n" {
                self.note("wrote-nothing");
                return Ok(0);
            }
            if bytes != b"\n" {
                assert!(!self.echoing(), "{} prompted while it echoes", self.name);
            }
            let n = self.short.map_or(bytes.len(), |s| s.min(bytes.len()));
            self.shown.extend_from_slice(&bytes[..n]);
            self.note(if bytes == b"\n" { "newline" } else { "wrote" });
            Ok(n)
        }
    }

    /// What happens on a line while the session waits.
    enum Event {
        Type(&'static str, &'static [u8]),
        Partial(&'static str, &'static [u8]),
        Resume(&'static str),
        Fail(&'static str),
        HangUp(&'static str),
        /// Two events between one wait and the next.
        Both(Box<Event>, Box<Event>),
    }

    /// Applies one event per wait until some line is ready; a wait with
    /// nothing ready and nothing left to happen would block forever.
    struct Timeline {
        events: VecDeque<Event>,
    }

    impl Timeline {
        fn new(events: Vec<Event>) -> Timeline {
            Timeline {
                events: events.into(),
            }
        }
    }

    impl Event {
        fn apply(self, lines: &mut [Kept<Probe>]) {
            if let Event::Both(first, second) = self {
                first.apply(lines);
                second.apply(lines);
                return;
            }
            let name = match &self {
                Event::Both(..) => return,
                Event::Type(name, _)
                | Event::Partial(name, _)
                | Event::Resume(name)
                | Event::Fail(name)
                | Event::HangUp(name) => *name,
            };
            let at = lines
                .iter_mut()
                .position(|k| k.line.script().name == name)
                .expect("an event on a line the session no longer holds");
            let line = lines[at].line.script();
            match self {
                Event::Type(_, bytes) => {
                    let mut record = std::mem::take(&mut line.partial);
                    record.extend_from_slice(bytes);
                    line.records.push_back(record);
                }
                Event::Partial(_, bytes) => line.partial.extend_from_slice(bytes),
                Event::Resume(_) => line.stopped = false,
                Event::Fail(_) => line.poll_error = true,
                Event::HangUp(_) => line.hung = true,
                Event::Both(..) => {}
            }
        }
    }

    impl Wait<Probe> for Timeline {
        fn wait(&mut self, lines: &mut [Kept<Probe>], found: &mut [term::Seen]) -> io::Result<()> {
            loop {
                let mut any = false;
                for (kept, seen) in lines.iter_mut().zip(found.iter_mut()) {
                    let queued = kept.queued();
                    let line = kept.line.script();
                    *seen = term::Seen {
                        input: !line.records.is_empty() || line.hung,
                        output: queued && !line.stopped,
                        failed: line.poll_error,
                    };
                    any |= seen.input || seen.output || seen.failed;
                }
                if any {
                    return Ok(());
                }
                let event = self
                    .events
                    .pop_front()
                    .expect("the wait would block forever");
                event.apply(lines);
            }
        }
    }

    const PROMPT: &str = "Key: ";

    /// A session's result, its entry and its lines as left, by name.
    struct Ran {
        got: Result<Entry, String>,
        line: Vec<u8>,
        lines: Vec<Script>,
        log: Vec<String>,
    }

    impl Ran {
        fn line(&self, name: &str) -> &Script {
            self.lines.iter().find(|l| l.name == name).unwrap()
        }
        fn shown(&self, name: &str) -> String {
            String::from_utf8(self.line(name).shown.clone()).unwrap()
        }
    }

    /// Run a session over the console and, beside it, the VT, each set up by
    /// `setup`, the lines captured as the session drops them back. The
    /// session owns the lines, so a probe line rides beside each one to hand
    /// its final state back through the shared log's owner.
    fn run_two(setup: impl FnOnce(&mut Script, &mut Script), events: Vec<Event>) -> Ran {
        run_lines(
            &["console", "vt"],
            |lines| {
                let (a, b) = lines.split_at_mut(1);
                setup(&mut a[0], &mut b[0]);
            },
            events,
        )
    }

    fn run_lines(
        names: &[&'static str],
        setup: impl FnOnce(&mut [Script]),
        events: Vec<Event>,
    ) -> Ran {
        let log: Log = Rc::new(RefCell::new(Vec::new()));
        let mut lines: Vec<Script> = names.iter().map(|n| Script::new(n, &log)).collect();
        setup(&mut lines);
        let mut slots = Vec::new();
        let mut opened = Vec::new();
        for line in lines {
            let slot = Rc::new(RefCell::new(None));
            slots.push(Rc::clone(&slot));
            let name = line.name.to_string();
            opened.push((Probe(Some(line), slot), name));
        }
        let mut record = Record::new();
        let got = session(
            opened,
            Vec::new(),
            PROMPT,
            &mut record,
            &mut Timeline::new(events),
        );
        let lines = slots
            .into_iter()
            .map(|slot| slot.borrow_mut().take().unwrap())
            .collect();
        let log = log.borrow().clone();
        Ran {
            got,
            line: record.as_bytes().to_vec(),
            lines,
            log,
        }
    }

    /// A scripted line that hands itself back when the session drops it.
    struct Probe(Option<Script>, Rc<RefCell<Option<Script>>>);

    impl Probe {
        fn script(&mut self) -> &mut Script {
            self.0.as_mut().unwrap()
        }
    }

    impl Drop for Probe {
        fn drop(&mut self) {
            *self.1.borrow_mut() = self.0.take();
        }
    }

    impl term::Termios for Probe {
        fn get(&mut self, out: &mut term::Bytes) -> io::Result<()> {
            self.script().get(out)
        }
        fn set(&mut self, termios: &term::Bytes) -> io::Result<()> {
            self.script().set(termios)
        }
        fn flush_input(&mut self) -> io::Result<()> {
            self.script().flush_input()
        }
    }

    impl Line for Probe {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.script().read(buf)
        }
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.script().write(bytes)
        }
    }

    fn restored(line: &Script) {
        assert!(line.echoing(), "{} was left silent", line.name);
    }

    #[test]
    fn every_line_is_saved_before_any_is_silenced_serial_first() {
        let ran = run_two(|_, _| {}, vec![Event::Type("console", b"4242\n")]);
        assert_eq!(ran.got, Ok(Entry::Line));
        assert_eq!(ran.line, b"4242");
        assert_eq!(
            &ran.log[..8],
            [
                "console:get",
                "vt:get",
                "console:silence",
                "console:get",
                "console:flush",
                "vt:silence",
                "vt:get",
                "vt:flush",
            ],
            "{:?}",
            ran.log
        );
        for name in ["console", "vt"] {
            assert_eq!(ran.shown(name), format!("{PROMPT}\n"), "{name}");
            restored(ran.line(name));
        }
    }

    /// Whichever line completes a record is read; the other is restored and
    /// its partial entry discarded, never joined to a later read.
    #[test]
    fn an_entry_on_either_line_wins_and_the_other_is_restored_and_flushed() {
        for (typed, other) in [("console", "vt"), ("vt", "console")] {
            let ran = run_two(
                |_, _| {},
                vec![
                    Event::Partial(other, b"123"),
                    Event::Type(typed, b"98765\n"),
                ],
            );
            assert_eq!(ran.got, Ok(Entry::Line), "{typed}");
            assert_eq!(ran.line, b"98765");
            assert!(
                ran.line(other).partial.is_empty(),
                "{other} kept its digits"
            );
            assert_eq!(ran.line(other).reads, 0);
            restored(ran.line(typed));
            restored(ran.line(other));
            let read = ran.log.iter().position(|e| *e == format!("{typed}:read"));
            let flushed = ran.log.iter().rposition(|e| *e == format!("{other}:flush"));
            assert!(read < flushed, "{:?}", ran.log);
            let flushes = |name: &str| {
                ran.log
                    .iter()
                    .filter(|e| **e == format!("{name}:flush"))
                    .count()
            };
            // Each line is flushed at its silence and after its restore, the
            // line read as well as the other.
            assert_eq!((flushes(typed), flushes(other)), (2, 2), "{:?}", ran.log);
            assert_eq!(
                ran.log[ran.log.len() - 2..],
                ["console:newline", "vt:newline"]
            );
        }
    }

    /// Both lines ready at once: the serial line is read and the VT's whole
    /// record is discarded with the flush.
    #[test]
    fn a_tie_reads_the_serial_line() {
        let ran = run_two(
            |_, _| {},
            vec![Event::Both(
                Box::new(Event::Type("vt", b"222\n")),
                Box::new(Event::Type("console", b"111\n")),
            )],
        );
        assert_eq!(ran.line, b"111");
        assert!(ran.line("vt").records.is_empty());
    }

    #[test]
    fn typeahead_before_the_prompt_is_flushed() {
        let ran = run_two(
            |console, vt| {
                console.typeahead.push_back(b"echoed-early\n".to_vec());
                vt.typeahead.push_back(b"also-early\n".to_vec());
            },
            vec![Event::Type("vt", b"4242\n")],
        );
        assert_eq!(ran.line, b"4242");
    }

    /// A stopped line only delays its own prompt: the other line is offered
    /// at once, what is typed on the stopped one is still read, and its
    /// prompt, never written, is discarded with the wait.
    #[test]
    fn a_stopped_line_delays_its_prompt_but_its_input_is_read() {
        let ran = run_two(|_, vt| vt.stopped = true, vec![Event::Type("vt", b"777\n")]);
        assert_eq!(ran.got, Ok(Entry::Line));
        assert_eq!(ran.line, b"777");
        assert_eq!(ran.shown("console"), format!("{PROMPT}\n"));
        assert_eq!(ran.shown("vt"), "", "a stopped line is not waited on");
        restored(ran.line("vt"));
        // Resumed before the entry: its prompt is finished on POLLOUT.
        let ran = run_two(
            |_, vt| vt.stopped = true,
            vec![Event::Resume("vt"), Event::Type("console", b"5\n")],
        );
        assert_eq!(ran.line, b"5");
        assert_eq!(ran.shown("vt"), format!("{PROMPT}\n"));
    }

    /// A short write leaves the rest from its offset, finished across waits.
    #[test]
    fn a_short_write_is_finished_from_its_offset() {
        let ran = run_two(
            |console, _| console.short = Some(2),
            vec![Event::Type("vt", b"6\n")],
        );
        assert_eq!(ran.line, b"6");
        assert_eq!(ran.shown("console"), format!("{PROMPT}\n"));
    }

    /// A hard write error drops that line, restored, and the other line
    /// names it before its prompt.
    #[test]
    fn a_hard_write_error_drops_a_line() {
        for errno in [EIO, ENXIO, EBADF] {
            let ran = run_two(
                |_, vt| vt.write_error = Some(errno),
                vec![Event::Type("console", b"31\n")],
            );
            assert_eq!(ran.got, Ok(Entry::Line), "{errno}");
            restored(ran.line("vt"));
            let console = ran.shown("console");
            assert!(
                console.starts_with("secret-line: vt was dropped: writing: ")
                    && console.ends_with(&format!("\n{PROMPT}\n")),
                "{console:?}"
            );
        }
        // Any other write error abandons the queue but keeps the line.
        let ran = run_two(
            |_, vt| vt.write_error = Some(22),
            vec![Event::Type("vt", b"8\n")],
        );
        assert_eq!(ran.line, b"8");
    }

    /// A poll error drops the line; with none left the session fails, every
    /// line restored.
    #[test]
    fn a_poll_error_drops_a_line_and_none_left_fails() {
        let ran = run_two(
            |_, _| {},
            vec![Event::Fail("vt"), Event::Type("console", b"9\n")],
        );
        assert_eq!(ran.line, b"9");
        assert!(ran
            .shown("console")
            .contains("\nsecret-line: vt was dropped: poll reported an error\nKey: "));
        let ran = run_two(|_, _| {}, vec![Event::Fail("vt"), Event::Fail("console")]);
        let error = ran.got.as_ref().unwrap_err().clone();
        assert!(
            error.starts_with("no line to read the entry from"),
            "{error}"
        );
        restored(ran.line("vt"));
        restored(ran.line("console"));
    }

    /// End of input on either line ends the entry: `^D` after a partial
    /// line, `^D` alone, or a hang-up.
    #[test]
    fn end_of_input_on_either_line_refuses() {
        for line in ["console", "vt"] {
            for event in [
                Event::Type(line, b"1234"),
                Event::Type(line, b""),
                Event::HangUp(line),
            ] {
                let ran = run_two(|_, _| {}, vec![event]);
                assert_eq!(ran.got, Ok(Entry::Eof), "{line}");
                assert!(ran.line.is_empty());
                restored(ran.line("console"));
                restored(ran.line("vt"));
            }
        }
    }

    /// The bound and the refusals are per line, whichever line it is.
    #[test]
    fn the_bound_and_its_refusals_hold_on_either_line() {
        let mut exact = vec![b'7'; LINE_MAX];
        exact.push(b'\n');
        let exact: &'static [u8] = exact.leak();
        let mut long = vec![b'7'; LINE_MAX + 1];
        long.push(b'\n');
        let long: &'static [u8] = long.leak();
        for line in ["console", "vt"] {
            let ran = run_two(|_, _| {}, vec![Event::Type(line, exact)]);
            assert_eq!((ran.got, ran.line.len()), (Ok(Entry::Line), LINE_MAX));
            for refused in [long, b"12\n34\n"] {
                let ran = run_two(|_, _| {}, vec![Event::Type(line, refused)]);
                assert_eq!(ran.got, Ok(Entry::Refused), "{line}");
                assert!(ran.line.is_empty());
            }
            let ran = run_two(|_, _| {}, vec![Event::Type(line, b"\n")]);
            assert_eq!((ran.got, ran.line.len()), (Ok(Entry::Line), 0));
        }
    }

    /// A line whose settings cannot be read, set or read back is skipped
    /// before the prompt, restored, and named on the line that remains.
    #[test]
    fn a_line_that_cannot_be_silenced_is_skipped_and_named() {
        let ran = run_two(
            |_, vt| vt.fail_get = true,
            vec![Event::Type("console", b"1\n")],
        );
        assert_eq!(ran.line, b"1");
        assert_eq!(ran.shown("vt"), "");
        assert!(ran
            .shown("console")
            .starts_with("secret-line: vt is not offered: TCGETS: not a terminal\nKey: "));
        let ran = run_two(
            |console, _| console.fail_set = Some(1),
            vec![Event::Type("vt", b"2\n")],
        );
        assert_eq!(ran.line, b"2");
        restored(ran.line("console"));
        assert!(ran
            .shown("vt")
            .starts_with("secret-line: console is not offered: TCSETS"));
        assert_eq!(ran.shown("console"), "");
    }

    /// With no line, nothing is prompted or read and the session fails.
    #[test]
    fn no_line_fails_without_a_prompt() {
        let ran = run_two(
            |console, vt| {
                console.fail_set = Some(1);
                vt.fail_get = true;
            },
            vec![],
        );
        assert!(ran.got.is_err());
        assert!(!ran
            .log
            .iter()
            .any(|e| e.ends_with(":wrote") || e.ends_with(":read")));
        restored(ran.line("console"));
        let mut none = Record::new();
        let got = session::<Probe>(
            Vec::new(),
            vec!["/dev/tty1 is not offered: gone".into()],
            PROMPT,
            &mut none,
            &mut Timeline::new(vec![]),
        );
        assert!(got.unwrap_err().contains("/dev/tty1 is not offered"));
    }

    /// A line read under a restore that failed is not handed back.
    #[test]
    fn a_failed_restore_withholds_and_wipes_the_line() {
        let ran = run_two(
            |_, vt| vt.fail_set = Some(2),
            vec![Event::Type("console", b"secret\n")],
        );
        let error = ran.got.as_ref().unwrap_err().clone();
        assert!(error.contains("restoring vt's settings"), "{error}");
        assert!(ran.line.is_empty());
    }

    /// The line read is flushed after its restore too: a second record (a
    /// pasted key's CR LF, ICRNL making two) or keys typed after the entry
    /// would otherwise wait, unechoed, for the next reader there.
    #[test]
    fn the_line_read_is_flushed_after_its_record() {
        let ran = run_two(
            |_, _| {},
            vec![Event::Both(
                Box::new(Event::Type("console", b"4242\n")),
                Box::new(Event::Type("console", b"\n")),
            )],
        );
        assert_eq!(ran.got, Ok(Entry::Line));
        assert_eq!(ran.line, b"4242");
        let console = ran.line("console");
        assert_eq!(console.reads, 1);
        assert!(console.records.is_empty(), "a second record stayed queued");
        let at = |e: &str| ran.log.iter().rposition(|x| x == e);
        assert!(
            at("console:read") < at("console:restore")
                && at("console:restore") < at("console:flush")
                && at("console:flush") < at("console:newline"),
            "{:?}",
            ran.log
        );
    }

    /// A dropped line whose restore fails is still flushed: its unechoed
    /// partial entry does not wait for a later reader while the other line
    /// takes the entry.
    #[test]
    fn a_dropped_line_whose_restore_fails_is_still_flushed() {
        let ran = run_two(
            |_, vt| vt.fail_set = Some(2),
            vec![
                Event::Partial("vt", b"55"),
                Event::Fail("vt"),
                Event::Type("console", b"4242\n"),
            ],
        );
        assert_eq!(ran.got, Ok(Entry::Line));
        assert_eq!(ran.line, b"4242");
        let vt = ran.line("vt");
        assert!(vt.partial.is_empty(), "the dropped line kept its digits");
        let refused = ran.log.iter().position(|e| e == "vt:set-refused");
        let flushed = ran.log.iter().rposition(|e| e == "vt:flush");
        assert!(refused.is_some() && refused < flushed, "{:?}", ran.log);
        assert!(
            ran.shown("console")
                .contains("restoring or flushing it also failed"),
            "{}",
            ran.shown("console")
        );
    }

    /// At the end, a restore that fails does not skip that line's flush, on
    /// the line read or the other; the entry is withheld and wiped.
    #[test]
    fn a_failed_final_restore_still_flushes_every_line() {
        for (failing, other) in [("console", "vt"), ("vt", "console")] {
            let ran = run_two(
                |console, vt| {
                    let line = if failing == "console" { console } else { vt };
                    line.fail_set = Some(2);
                },
                vec![
                    Event::Partial(other, b"55"),
                    Event::Partial(failing, b"66"),
                    Event::Both(
                        Box::new(Event::Type("console", b"4242\n")),
                        Box::new(Event::Type("console", b"\n")),
                    ),
                ],
            );
            assert!(ran.got.is_err(), "{failing}");
            assert!(ran.line.is_empty(), "the entry was not wiped");
            for name in ["console", "vt"] {
                let line = ran.line(name);
                assert!(
                    line.partial.is_empty() && line.records.is_empty(),
                    "{name} kept input with {failing}'s restore refused"
                );
            }
            let refused = ran
                .log
                .iter()
                .position(|e| *e == format!("{failing}:set-refused"));
            let flushed = ran
                .log
                .iter()
                .rposition(|e| *e == format!("{failing}:flush"));
            assert!(refused.is_some() && refused < flushed, "{:?}", ran.log);
        }
    }

    /// A line whose write takes nothing gives up its queue at once rather
    /// than be woken for output again and again; it stays readable.
    #[test]
    fn a_write_of_nothing_abandons_that_lines_queue() {
        let ran = run_two(
            |console, _| console.takes_nothing = true,
            vec![Event::Type("console", b"7\n")],
        );
        assert_eq!(ran.got, Ok(Entry::Line));
        assert_eq!(ran.line, b"7");
        let nothing = ran.log.iter().filter(|e| *e == "console:wrote-nothing");
        assert_eq!(nothing.count(), 1, "{:?}", ran.log);
        assert_eq!(ran.shown("vt"), format!("{PROMPT}\n"));
    }

    /// One line alone, as when `/dev/console` is the VT: today's applet
    /// but for what it waits in.
    #[test]
    fn one_line_alone_reads_as_before() {
        let ran = run_lines(
            &["console"],
            |_| {},
            vec![Event::Type("console", b"12345-67890\n")],
        );
        assert_eq!(ran.got, Ok(Entry::Line));
        assert_eq!(ran.line, b"12345-67890");
        assert_eq!(
            ran.log,
            [
                "console:get",
                "console:silence",
                "console:get",
                "console:flush",
                "console:wrote",
                "console:read",
                "console:restore",
                "console:get",
                "console:flush",
                "console:newline",
            ]
        );
    }

    /// The two names are one terminal when `active` says the VT is
    /// `/dev/console`, or cannot be read: one line, saved and restored once.
    #[test]
    fn one_terminal_under_two_names_is_one_line() {
        let dir = std::env::temp_dir().join(format!("td-init-secret-line-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let active = dir.join("active");
        for (text, names) in [
            (&b"tty0 ttyS0\n"[..], &[CONSOLE, VT][..]),
            (b"ttyS0 tty0\n", &[CONSOLE][..]),
            (b"ttyS0 tty1\n", &[CONSOLE][..]),
            (b"tty0 ttyS0", &[CONSOLE][..]),
        ] {
            std::fs::write(&active, text).unwrap();
            assert_eq!(line_names(console::read(&active)), names, "{text:?}");
        }
        assert_eq!(line_names(console::read(&dir.join("absent"))), [CONSOLE]);
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!((CONSOLE, VT), ("/dev/console", "/dev/tty1"));
    }

    #[test]
    fn a_wiped_record_is_zero() {
        let mut record = Record::new();
        record.bytes.fill(0x39);
        record.len = 10;
        record.wipe();
        assert_eq!(record.len, 0);
        assert!(record.bytes.iter().all(|b| *b == 0));
        assert_eq!(record.bytes.len(), RECORD_MAX);
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
    /// before stdout or any line is touched.
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

    // ── against the kernel's line discipline ────────────────────────────────

    use std::sync::mpsc;
    use std::time::Duration;

    /// One pseudo-terminal standing in for a line (td-ui's PTY, a test-only
    /// dependency): its master, a reader of everything shown on it, and the
    /// slave reopened as the applet opens a line, through the descriptor's
    /// `/proc` link with `O_NOCTTY` and `O_NONBLOCK`.
    struct Pty {
        master: File,
        screen: mpsc::Receiver<Vec<u8>>,
        shown: Vec<u8>,
        /// A slave held open throughout: a PTY whose every slave is closed
        /// reads as hung up on the master side.
        held: File,
        _pty: td_ui::pty::Pty,
    }

    impl Pty {
        fn open() -> Pty {
            let pty = td_ui::pty::Pty::open().unwrap();
            let master = pty.master().try_clone().unwrap();
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
            let held = pty.peer().unwrap();
            Pty {
                master,
                screen,
                shown: Vec::new(),
                held,
                _pty: pty,
            }
        }

        fn path(&self) -> String {
            format!("/proc/self/fd/{}", self.held.as_raw_fd())
        }

        fn line(&self) -> Tty {
            Tty::new(open_line(&self.path()).unwrap())
        }

        fn type_in(&mut self, bytes: &[u8]) {
            self.master.write_all(bytes).unwrap();
        }

        /// Wait until `needle` appears after `from`, at most ten seconds.
        fn wait_for(&mut self, needle: &[u8], from: usize) {
            while !self.shown[from.min(self.shown.len())..]
                .windows(needle.len())
                .any(|w| w == needle)
            {
                let chunk = self.screen.recv_timeout(Duration::from_secs(10));
                assert!(chunk.is_ok(), "never saw {needle:?} in {:?}", self.shown);
                self.shown.extend_from_slice(&chunk.unwrap());
            }
        }

        /// Everything shown within `quiet` of the last byte.
        fn settle(&mut self, quiet: Duration) {
            while let Ok(chunk) = self.screen.recv_timeout(quiet) {
                self.shown.extend_from_slice(&chunk);
            }
        }

        /// Stop the line with XOFF and wait until n_tty has taken it: a
        /// non-blocking probe write to the slave then answers `EAGAIN`.
        /// n_tty takes input in order, so whatever was typed before the
        /// XOFF has reached it too.
        fn stop(&mut self) {
            self.type_in(b"\x13");
            let mut probe = open_line(&self.path()).unwrap();
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            loop {
                if let Err(e) = probe.write(b".") {
                    assert_eq!(e.kind(), io::ErrorKind::WouldBlock, "probe write: {e}");
                    return;
                }
                assert!(std::time::Instant::now() < deadline, "XOFF never took");
                std::thread::sleep(Duration::from_millis(5));
            }
        }

        /// The slave's settings as a fresh reader finds them.
        fn settings(&self) -> term::Bytes {
            let mut line = self.line();
            let mut out = [0u8; std::mem::size_of::<term::Bytes>()];
            term::Termios::get(&mut line, &mut out).unwrap();
            out
        }
    }

    /// Run a session over `lines`' paths on a thread, as `run` does.
    fn spawn_session(
        lines: &[&Pty],
        prompt: &str,
    ) -> std::thread::JoinHandle<(Result<Entry, String>, Vec<u8>)> {
        let opened: Vec<(Tty, String)> = lines.iter().map(|p| (p.line(), p.path())).collect();
        let prompt = prompt.to_string();
        std::thread::spawn(move || {
            let mut record = Record::new();
            let got = session(opened, Vec::new(), &prompt, &mut record, &mut Polled);
            (got, record.as_bytes().to_vec())
        })
    }

    /// The canonical reading against the kernel's own line discipline. Each
    /// boundary case is followed by an ordinary line, which must arrive
    /// intact: a reader that consumed past its record would swallow or join
    /// it. Typeahead before every prompt is flushed, and the session's
    /// closing newline must arrive with nothing typed shown before it.
    ///
    /// n_tty's limit: an unterminated line holds at most 4095 bytes; what is
    /// typed beyond that is dropped and the newline still ends the record, so
    /// 4096 or 5000 bytes and a newline arrive as one 4096-byte record,
    /// refused whole.
    #[test]
    fn a_real_terminal_reads_whole_records() {
        let mut pty = Pty::open();
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
        let mut round = 0;
        for (typed, want, line_want) in boundary {
            let next = (b"4242\n".to_vec(), Entry::Line, b"4242".to_vec());
            for (typed, want, line_want) in [(typed, want, line_want), next] {
                round += 1;
                // Typed before the prompt: flushed by the echo-off.
                pty.type_in(b"999\n");
                pty.wait_for(b"999", 0);
                let prompt = format!("P{round}> ");
                let reader = spawn_session(&[&pty], &prompt);
                pty.wait_for(prompt.as_bytes(), 0);
                let after_prompt = pty.shown.len();
                pty.type_in(&typed);
                let (got, line) = reader.join().unwrap();
                assert_eq!(got, Ok(want), "round {round}");
                assert_eq!(line, line_want, "round {round}");
                pty.wait_for(b"\r\n", after_prompt);
                let echoed = &pty.shown[after_prompt..];
                assert!(
                    !echoed.iter().any(u8::is_ascii_digit),
                    "round {round} echoed what was typed: {echoed:?}"
                );
                pty.shown.clear();
            }
        }
    }

    /// Two terminals: the entry made on either is read; the other line is
    /// put back as it was found, and its unechoed partial entry is gone, so
    /// the next reader there sees only what is typed after.
    #[test]
    fn two_real_terminals_read_the_first_entry_and_restore_the_other() {
        for typed_on in [0usize, 1] {
            let mut ptys = [Pty::open(), Pty::open()];
            let before = [ptys[0].settings(), ptys[1].settings()];
            let reader = spawn_session(&[&ptys[0], &ptys[1]], "K> ");
            for pty in &mut ptys {
                pty.wait_for(b"K> ", 0);
            }
            let other = 1 - typed_on;
            // A PTY's input reaches n_tty from a work queue: the XOFF after
            // the partial `55` shows when both have landed, before the entry,
            // so the flush is what discards it.
            ptys[other].type_in(b"55");
            ptys[other].stop();
            ptys[typed_on].type_in(b"31415\n");
            let (got, line) = reader.join().unwrap();
            assert_eq!(got, Ok(Entry::Line), "line {typed_on}");
            assert_eq!(line, b"31415");
            assert_eq!([ptys[0].settings(), ptys[1].settings()], before);
            for pty in &mut ptys {
                pty.settle(Duration::from_millis(200));
                assert!(
                    !pty.shown.iter().any(u8::is_ascii_digit),
                    "a line echoed what was typed: {:?}",
                    pty.shown
                );
            }
            // The partial `55` was flushed: a reader now gets only this line.
            ptys[other].type_in(b"next\n");
            let mut fresh = ptys[other].held.try_clone().unwrap();
            let mut buf = [0u8; 64];
            let n = fresh.read(&mut buf).unwrap();
            assert_eq!(&buf[..n], b"next\n", "the partial entry survived");
        }
    }

    /// A line stopped by XOFF (n_tty's `IXON`, on by default) takes no prompt
    /// and holds nothing up: the other line is prompted at once, and an entry
    /// typed on the stopped line is still read. Resumed, its prompt arrives.
    #[test]
    fn a_stopped_real_terminal_delays_only_its_own_prompt() {
        let mut ptys = [Pty::open(), Pty::open()];
        ptys[1].stop();
        let reader = spawn_session(&[&ptys[0], &ptys[1]], "S> ");
        ptys[0].wait_for(b"S> ", 0);
        ptys[1].settle(Duration::from_millis(300));
        assert!(
            !ptys[1].shown.windows(3).any(|w| w == b"S> "),
            "a stopped line showed its prompt"
        );
        ptys[1].type_in(b"2718\n");
        let (got, line) = reader.join().unwrap();
        assert_eq!((got, line), (Ok(Entry::Line), b"2718".to_vec()));

        // Stopped, then resumed: the prompt is finished on POLLOUT.
        let mut ptys = [Pty::open(), Pty::open()];
        ptys[1].stop();
        let reader = spawn_session(&[&ptys[0], &ptys[1]], "R> ");
        ptys[0].wait_for(b"R> ", 0);
        ptys[1].type_in(b"\x11");
        ptys[1].wait_for(b"R> ", 0);
        ptys[0].type_in(b"1\n");
        let (got, line) = reader.join().unwrap();
        assert_eq!((got, line), (Ok(Entry::Line), b"1".to_vec()));
    }

    /// A key pasted with CR LF is two records under ICRNL; the first is the
    /// entry and the second must not wait, unechoed, for the next reader.
    #[test]
    fn a_pasted_crlf_leaves_nothing_on_the_terminal_read() {
        let mut pty = Pty::open();
        let reader = spawn_session(&[&pty], "C> ");
        pty.wait_for(b"C> ", 0);
        pty.type_in(b"1618\r\n");
        let (got, line) = reader.join().unwrap();
        assert_eq!((got, line), (Ok(Entry::Line), b"1618".to_vec()));
        pty.type_in(b"next\n");
        let mut fresh = pty.held.try_clone().unwrap();
        let mut buf = [0u8; 64];
        let n = fresh.read(&mut buf).unwrap();
        assert_eq!(
            &buf[..n],
            b"next\n",
            "the pasted line's second record survived"
        );
    }

    /// End of input and the bound hold on the second terminal as on the
    /// first.
    #[test]
    fn end_of_input_and_the_bound_hold_on_either_real_terminal() {
        for (typed, want) in [
            (&b"12\x04"[..], Entry::Eof),
            (b"\x04", Entry::Eof),
            (&[b'7'; LINE_MAX + 1][..], Entry::Refused),
        ] {
            let mut ptys = [Pty::open(), Pty::open()];
            let reader = spawn_session(&[&ptys[0], &ptys[1]], "E> ");
            ptys[1].wait_for(b"E> ", 0);
            ptys[1].type_in(typed);
            if want == Entry::Refused {
                ptys[1].type_in(b"\n");
            }
            let (got, line) = reader.join().unwrap();
            assert_eq!(got, Ok(want));
            assert!(line.is_empty());
        }
    }
}

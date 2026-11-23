//! `less` — the console pager.
//!
//! Full screen: each command redraws the page in place, so the reader moves back
//! as well as forward, jumps to either end, and searches, rather than watching
//! the input scroll past a `--More--` prompt that can only go one way.
//!
//! Bytes, not text: a pager is pointed at logs and `/proc` files, and rejecting a
//! whole file over one stray byte would make it unviewable. `cat` made the same
//! call for the same reason. But nothing from the file reaches the terminal as a
//! control: colour and attribute changes (`ESC [ ... m`) pass through, as
//! `less -R` does, so `git log` keeps its colours; any other C0 control is shown
//! in caret form (`^[`, `^G`), and a C1 control or a byte that is not well-formed
//! UTF-8 as `<9B>`. That holds for `-F`'s one-screen copy and the prompt row too.
//! The filter assumes a UTF-8 terminal, as td's consoles are: a character such as
//! `Û` (`C3 9B`) is sent whole, and an 8-bit terminal would read its second byte
//! as a C1 control.
//! Searches match the text as shown, with the colour sequences taken out.
//!
//! ## Memory
//!
//! Scrolling back means reaching what scrolled past. A regular file is read in
//! place, by offset: there is no line index, the screen's top is a byte offset,
//! and moving back scans backward for the previous newline, so paging a 10 GiB
//! log holds one screen of it. A pipe cannot be re-read, so what arrives is kept
//! up to `SPOOL_CAP`; past that the oldest is dropped, scrolling back stops where
//! the kept input starts, and the prompt says so there. Either way no line costs
//! more than `MAX_LINE`: a longer one is shown as several.
//!
//! ## Terminal
//!
//! Single keystrokes when the terminal will allow it, which is what `term.rs`'s
//! raw mode buys and the crate's one `ioctl` surface exists for; otherwise each
//! command is a line ended by Enter. The page is drawn on the normal screen, never
//! the alternate one: `ISIG` stays on, so Ctrl-C ends a pager stuck reading a
//! huge pipe, and a pager killed that way must not leave the reader on a blank
//! alternate screen. Quitting leaves the last page in place, as `less -X` does.
//!
//! Commands are read between pages. A pipe that pauses holds the page it is
//! filling — the rows that have arrived are shown — until it writes again or
//! Ctrl-C; `std` has no way to wait on the pipe and the keyboard at once.
//!
//! **Not a pager when stdout is not a terminal.** `less | grep` and `less > file`
//! copy through untouched, as every pager does — otherwise the prompts land in the
//! data.

use std::io::{self, BufRead, BufReader, IsTerminal, Read, Write};
use std::os::fd::AsFd;
use std::os::unix::fs::FileExt;

use crate::allocation;
use crate::term;

/// Rows and columns to assume when the terminal will not say and the environment
/// does not either.
const DEFAULT_ROWS: u16 = 24;
const DEFAULT_COLS: u16 = 80;

/// Below two rows a page is not a page; above the ceiling a bad `LINES` would make
/// one page the whole file.
const MIN_ROWS: u16 = 2;
const MAX_ROWS: u16 = 1000;

/// Copy buffer for the non-terminal path. One page of memory, so the promise of
/// constant memory holds for a file of any size.
const COPY_BUF: usize = 4096;

/// The most bytes one piece of a line may cost. A log with no newline in it — a
/// truncated write, a binary blob, `/dev/zero` — is shown in pieces of this size,
/// measured from the line's start in both directions so forward and backward
/// agree.
const MAX_LINE: usize = 64 * 1024;

/// How much of a pipe is kept for scrolling back.
const SPOOL_CAP: usize = 64 * 1024 * 1024;

/// One read from a pipe.
const SPOOL_READ: usize = 16 * 1024;

/// The smallest cap a spool takes. Trimming keeps the newest half, and a line
/// being read must keep its start: it began at most `MAX_LINE` before the read
/// that trims, which adds at most `SPOOL_READ`.
const MIN_SPOOL: usize = 4 * (MAX_LINE + SPOOL_READ);

/// One read while walking a file, in either direction.
const SCAN_BLOCK: usize = 4096;

const TAB: usize = 8;

/// A search pattern longer than this is not being typed by a person.
const MAX_PATTERN: usize = 256;

/// A count prefix saturates here rather than overflowing.
const MAX_COUNT: u64 = 1_000_000_000;

/// The letters of a fuller `less` that take an argument, which may follow in the
/// same word: reading `LESS`, a word stops at one of these.
const TAKES_ARGUMENT: &[u8] = b"bhjkoOpPtTxyzD#\"";

const USAGE: &str =
    "usage: less [-FRXiI] [+COMMAND] [FILE...]  (q quits, h lists the keys; `-` is stdin)";

const HELP: &str =
    "q quit  SPACE/b page  j/k line  d/u half  g/G ends  / ? search  n/N again  :n/:p file";

const SPACES: &[u8] = b"        ";

const HEX: &[u8] = b"0123456789ABCDEF";

pub fn run(args: &[String]) -> Result<u8, String> {
    let env = std::env::var("LESS").ok();
    let parsed = match parse(args, env.as_deref()) {
        Ok(parsed) => parsed,
        Err(Refused::Option(bad)) => {
            let _ = writeln!(
                io::stderr().lock(),
                "less: unrecognised option '{bad}'\n{USAGE}"
            );
            // 2, not 1: the crate reserves 2 for a usage error, and `test` and the
            // multicall's own unknown-applet path already answer that way.
            return Ok(2);
        }
        Err(Refused::Memory) => {
            crate::emit_err("less: out of memory\n");
            return Ok(1);
        }
    };
    let mut out = io::stdout();
    if !out.is_terminal() {
        return Ok(copy_through(&parsed.files, &mut out));
    }
    // Commands come from the terminal, not from stdin — stdin may BE the data.
    // Without one there is nothing to page against, so degrade to copying.
    let Ok(tty) = std::fs::File::open("/dev/tty") else {
        return Ok(copy_through(&parsed.files, &mut out));
    };
    // Raw mode is optional: `None` just means commands need Enter.
    let raw = term::raw(tty.as_fd()).ok();
    // BORROWS the terminal — `&File` reads just as well as `File`. Moving it here
    // would close the descriptor when this reader drops, which happens BEFORE the
    // guard restores through it, and the restore would fail EBADF into a `let _`:
    // every interactive run would end at a shell with no echo and no line editing.
    //
    // Capacity ONE for the same reason `less` is not `cat`: in raw mode a `read`
    // returns whatever has been typed, and a bigger buffer would swallow whatever
    // the reader typed ahead for their SHELL and throw it away on exit.
    let commands = BufReader::with_capacity(1, &tty);
    // Asked before every page, so a resized window is redrawn to fit on the next
    // keystroke.
    let mut measure = || {
        let (rows, cols) = geometry(tty.as_fd());
        Screen { rows, cols }
    };
    let mut open = |name: &str| Store::open(name);
    let mut out = out.lock();
    let terminal = Terminal {
        out: &mut out,
        commands,
        raw: raw.is_some(),
        measure: &mut measure,
        typed_ahead: parsed.start.unwrap_or("").as_bytes(),
    };
    match page_files(&parsed.files, &mut open, terminal, parsed.options) {
        Ok(status) => Ok(status),
        Err(e) => {
            let _ = writeln!(io::stderr().lock(), "less: {e}");
            Ok(1)
        }
    }
}

/// The flags this pager honours. `R` and `X` are accepted because they ask for
/// what it always does — colour sequences through, no terminal initialisation —
/// and git's default `LESS=FRX` names them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Options {
    /// `-F`: input that fits on one screen is printed and the pager exits.
    quit_if_one_screen: bool,
    /// `-i`: searches ignore case unless the pattern has an upper-case letter.
    smart_case: bool,
    /// `-I`: searches ignore case.
    ignore_case: bool,
}

impl Options {
    /// Apply one option letter, reporting whether it is one this pager knows.
    fn set(&mut self, letter: u8) -> bool {
        match letter {
            b'F' => self.quit_if_one_screen = true,
            b'i' => self.smart_case = true,
            b'I' => self.ignore_case = true,
            b'R' | b'X' => {}
            _ => return false,
        }
        true
    }

    /// The `LESS` variable, read leniently: it is the reader's standing
    /// preference, written for a fuller `less`, and a letter this one lacks is
    /// skipped rather than refused. The first word may omit its dash, as git's
    /// `FRX` does; later words count only when they start with one. A word stops
    /// at a letter that takes an argument, since what follows is that argument —
    /// `-Ps File` is a prompt string, not `-F`.
    fn absorb(&mut self, value: &str) {
        for (index, word) in value.split_ascii_whitespace().enumerate() {
            if word.starts_with("--") || (index > 0 && !word.starts_with('-')) {
                continue;
            }
            for letter in word.trim_start_matches('-').bytes() {
                if TAKES_ARGUMENT.contains(&letter) {
                    break;
                }
                let _ = self.set(letter);
            }
        }
    }

    fn fold(&self, pattern: &[u8]) -> bool {
        self.ignore_case || (self.smart_case && !pattern.iter().any(u8::is_ascii_uppercase))
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Refused<'a> {
    Option(&'a str),
    Memory,
}

#[derive(Debug, PartialEq, Eq)]
struct Parsed<'a> {
    options: Options,
    files: Vec<&'a str>,
    /// `+COMMAND`: keys run as if typed before the first one, `+G` to open at the
    /// end, `+/pattern` at a match.
    start: Option<&'a str>,
}

/// The options and operands. A flag on the command line that this pager does not
/// honour is refused rather than ignored: silently paging without what was asked
/// for would be worse. `--` ends the options, so a file really named `-N` (or
/// `+G`) is still reachable.
fn parse<'a>(args: &'a [String], env: Option<&str>) -> Result<Parsed<'a>, Refused<'a>> {
    let mut options = Options::default();
    if let Some(value) = env {
        options.absorb(value);
    }
    let mut files: Vec<&'a str> = Vec::new();
    let mut start = None;
    let mut options_done = false;
    for a in args {
        if !options_done && a == "--" {
            options_done = true;
            continue;
        }
        if !options_done && a.len() > 1 && a.starts_with('-') {
            if a.starts_with("--") || !a.bytes().skip(1).all(|letter| options.set(letter)) {
                return Err(Refused::Option(a));
            }
            continue;
        }
        if !options_done {
            if let Some(command) = a.strip_prefix('+') {
                start = Some(command);
                continue;
            }
        }
        allocation::push(&mut files, a.as_str()).map_err(|_| Refused::Memory)?;
    }
    if files.is_empty() {
        allocation::push(&mut files, "-").map_err(|_| Refused::Memory)?;
    }
    Ok(Parsed {
        options,
        files,
        start,
    })
}

/// What a page is measured against.
#[derive(Clone, Copy, Debug)]
struct Screen {
    rows: u16,
    cols: u16,
}

impl Screen {
    /// Rows of text: one is kept for the prompt, and never zero however small the
    /// screen.
    fn page(self) -> usize {
        usize::from(self.rows).saturating_sub(1).max(1)
    }

    fn width(self) -> usize {
        usize::from(self.cols.max(1))
    }
}

/// The terminal's size, else `LINES`/`COLUMNS`, else the classic 24x80.
fn geometry(tty: std::os::fd::BorrowedFd<'_>) -> (u16, u16) {
    if let Some((rows, cols)) = term::size(tty) {
        return (clamp_rows(rows), cols.max(1));
    }
    let rows = env_u16("LINES").map_or(DEFAULT_ROWS, clamp_rows);
    let cols = env_u16("COLUMNS").unwrap_or(DEFAULT_COLS).max(1);
    (rows, cols)
}

fn env_u16(name: &str) -> Option<u16> {
    std::env::var(name).ok()?.trim().parse().ok()
}

/// A bad value is IGNORED rather than refused: a pager that will not start because
/// the environment is odd is worse than one that guesses 24.
fn clamp_rows(rows: u16) -> u16 {
    if (MIN_ROWS..=MAX_ROWS).contains(&rows) {
        rows
    } else {
        DEFAULT_ROWS
    }
}

fn out_of_memory(_: allocation::Error) -> io::Error {
    io::Error::from(io::ErrorKind::OutOfMemory)
}

fn put(frame: &mut Vec<u8>, bytes: &[u8]) -> io::Result<()> {
    allocation::extend_from_slice(frame, bytes).map_err(out_of_memory)
}

fn report(name: &str, err: &io::Error) {
    let _ = writeln!(io::stderr().lock(), "less: {name}: {err}");
}

/// A source that is read once, front to back.
enum Input {
    Stdin(io::Stdin),
    File(std::fs::File),
}

impl Read for Input {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Input::Stdin(s) => s.read(buf),
            Input::File(f) => f.read(buf),
        }
    }
}

fn open_input(name: &str) -> io::Result<Input> {
    if name == "-" {
        return Ok(Input::Stdin(io::stdin()));
    }
    std::fs::File::open(name).map(Input::File)
}

/// Write, reporting whether the sink is still OPEN.
///
/// A closed reader is a clean exit, exactly as in `cat`: `less x | head` must not
/// look like a failure. But it must not look like success either — swallowing
/// EPIPE into `Ok(())` leaves the caller looping, so `less huge.log | head -1`
/// reads the whole log to write it all nowhere, and against a producer that never
/// ends the pipeline never does. The `false` is what stops the loop.
fn write(out: &mut impl Write, bytes: &[u8]) -> io::Result<bool> {
    match out.write_all(bytes).and_then(|()| out.flush()) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::BrokenPipe => Ok(false),
        Err(e) => Err(e),
    }
}

/// The non-terminal path: stream, never buffer. `less big.log | grep x` must
/// start producing before the log ends, and must not hold the log in memory.
fn copy_through(files: &[&str], out: &mut impl Write) -> u8 {
    let mut status = 0u8;
    let mut buf = [0u8; COPY_BUF];
    for name in files {
        let mut input = match open_input(name) {
            Ok(input) => input,
            Err(e) => {
                report(name, &e);
                status = 1;
                continue;
            }
        };
        loop {
            let n = match input.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => {
                    report(name, &e);
                    status = 1;
                    break;
                }
            };
            match write(out, buf.get(..n).unwrap_or_default()) {
                Ok(true) => {}
                Ok(false) => return status,
                Err(e) => {
                    report(name, &e);
                    return 1;
                }
            }
        }
    }
    status
}

/// A pipe's input as far as it has been read, of which at most `cap` bytes are
/// kept.
struct Spool<R> {
    input: R,
    held: Vec<u8>,
    /// The offset of `held`'s first byte: what has been dropped from the front.
    base: u64,
    done: bool,
    cap: usize,
}

/// Where a file's bytes come from.
enum Source<R> {
    /// A regular file, read in place.
    Seek(std::fs::File),
    /// Anything that can only be read once: a pipe, a terminal, a `/proc` file
    /// whose size says nothing.
    Spool(Spool<R>),
}

/// A file's bytes, read by offset.
struct Store<R> {
    source: Source<R>,
    /// A line start, and how far past it is known to hold no newline. Walking back
    /// through the pieces of one long line starts from here, rather than scanning
    /// back to the line's start again for every piece.
    long: Option<(u64, u64)>,
}

impl Store<Input> {
    fn open(name: &str) -> io::Result<Self> {
        let input = open_input(name)?;
        if let Input::File(file) = &input {
            let meta = file.metadata()?;
            if meta.is_dir() {
                // Opening a directory succeeds; reading it is what fails. Said
                // here, where it is still about the operand.
                return Err(io::Error::from(io::ErrorKind::IsADirectory));
            }
            if meta.is_file() && meta.len() > 0 {
                if let Input::File(file) = input {
                    return Ok(Store::new(Source::Seek(file)));
                }
            }
        }
        Ok(Store::spool(input, SPOOL_CAP))
    }
}

impl<R: Read> Store<R> {
    fn new(source: Source<R>) -> Self {
        Store { source, long: None }
    }

    fn spool(input: R, cap: usize) -> Self {
        Store::new(Source::Spool(Spool {
            input,
            held: Vec::new(),
            base: 0,
            done: false,
            cap: cap.max(MIN_SPOOL),
        }))
    }

    /// The first offset that can still be read.
    fn first(&self) -> u64 {
        match &self.source {
            Source::Seek(_) => 0,
            Source::Spool(s) => s.base,
        }
    }

    /// Whether the line at `at` can be read without waiting on a pipe.
    fn ready(&self, at: u64) -> bool {
        match &self.source {
            Source::Seek(_) => true,
            Source::Spool(s) => {
                if s.done {
                    return true;
                }
                let from = usize::try_from(at.saturating_sub(s.base)).unwrap_or(usize::MAX);
                let kept = s.held.get(from..).unwrap_or_default();
                kept.len() > MAX_LINE || kept.contains(&b'\n')
            }
        }
    }

    /// Bytes at `at`, as many as fit and are there; `Ok(0)` past the end. A pipe
    /// is read further only as far as a request needs.
    fn read_at(&mut self, at: u64, buf: &mut [u8]) -> io::Result<usize> {
        match &mut self.source {
            Source::Seek(file) => loop {
                match file.read_at(buf, at) {
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    other => return other,
                }
            },
            Source::Spool(s) => loop {
                let end = s.base.saturating_add(s.held.len() as u64);
                if at < end {
                    // Below `base` was dropped. Every walk starts from `first`, so
                    // this is not asked for; answering "nothing" keeps it harmless.
                    let Some(from) = at.checked_sub(s.base) else {
                        return Ok(0);
                    };
                    let from = usize::try_from(from).unwrap_or(usize::MAX);
                    let kept = s.held.get(from..).unwrap_or_default();
                    let n = kept.len().min(buf.len());
                    if let (Some(dst), Some(src)) = (buf.get_mut(..n), kept.get(..n)) {
                        dst.copy_from_slice(src);
                    }
                    return Ok(n);
                }
                if s.done {
                    return Ok(0);
                }
                s.pull()?;
            },
        }
    }

    /// Fill as much of `buf` as there is.
    fn read_full(&mut self, at: u64, buf: &mut [u8]) -> io::Result<usize> {
        let mut got = 0usize;
        while let Some(rest) = buf.get_mut(got..) {
            if rest.is_empty() {
                break;
            }
            let n = self.read_at(at.saturating_add(got as u64), rest)?;
            if n == 0 {
                break;
            }
            got += n;
        }
        Ok(got)
    }

    /// The offset just past the last byte, reading a pipe to its end.
    ///
    /// A file is walked to its end from its recorded size, so a log that grew is
    /// followed. If the byte before that size is not there, the size is not
    /// trusted: the file shrank (a log rotated by truncation), or reports more
    /// than it holds (sysfs says 4096 for a short file). The walk then starts
    /// from `from`, an offset known to have held data, when that is inside the
    /// size, and from the start otherwise.
    fn end(&mut self, from: u64) -> io::Result<u64> {
        self.long = None;
        let size = match &mut self.source {
            Source::Seek(file) => file.metadata()?.len(),
            Source::Spool(s) => {
                while !s.done {
                    s.pull()?;
                }
                return Ok(s.base.saturating_add(s.held.len() as u64));
            }
        };
        let mut probe = [0u8; 1];
        let mut at = if size > 0 && self.read_at(size - 1, &mut probe)? == 1 {
            size
        } else if from < size {
            from
        } else {
            0
        };
        let mut block = [0u8; SPOOL_READ];
        loop {
            let n = self.read_at(at, &mut block)?;
            if n == 0 {
                return Ok(at);
            }
            at = at.saturating_add(n as u64);
        }
    }
}

impl<R: Read> Spool<R> {
    /// One more read from the pipe. Past `cap`, all but the newest half is
    /// dropped: far enough behind every position being read (each is within
    /// `MAX_LINE` of the newest byte, see `MIN_SPOOL`) that none of them goes with
    /// it.
    fn pull(&mut self) -> io::Result<()> {
        let mut chunk = [0u8; SPOOL_READ];
        let n = loop {
            match self.input.read(&mut chunk) {
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                other => break other?,
            }
        };
        if n == 0 {
            self.done = true;
            return Ok(());
        }
        allocation::extend_from_slice(&mut self.held, chunk.get(..n).unwrap_or_default())
            .map_err(out_of_memory)?;
        if self.held.len() > self.cap {
            let cut = self.held.len() - self.cap / 2;
            self.held.drain(..cut);
            self.base = self.base.saturating_add(cut as u64);
        }
        Ok(())
    }
}

/// The line starting at `at`, newline included, into `buf` — or its next piece,
/// `MAX_LINE` bytes of it. Returns the offset after it, which is `at` at the end.
///
/// A newline straight after a full piece belongs to that piece: otherwise a line
/// of exactly `MAX_LINE` bytes would end in an empty row of its own.
fn line_at<R: Read>(store: &mut Store<R>, at: u64, buf: &mut Vec<u8>) -> io::Result<u64> {
    buf.clear();
    let mut pos = at;
    let mut block = [0u8; SCAN_BLOCK];
    loop {
        let want = SCAN_BLOCK.min(MAX_LINE.saturating_sub(buf.len()));
        let Some(dst) = block.get_mut(..want).filter(|d| !d.is_empty()) else {
            let mut next = [0u8; 1];
            if store.read_at(pos, &mut next)? == 1 && next == *b"\n" {
                allocation::push(buf, b'\n').map_err(out_of_memory)?;
                pos = pos.saturating_add(1);
            }
            return Ok(pos);
        };
        let n = store.read_at(pos, dst)?;
        if n == 0 {
            return Ok(pos);
        }
        let got = block.get(..n).unwrap_or_default();
        let (take, complete) = match got.iter().position(|b| *b == b'\n') {
            Some(i) => (i + 1, true),
            None => (n, false),
        };
        put(buf, got.get(..take).unwrap_or_default())?;
        pos = pos.saturating_add(take as u64);
        if complete {
            return Ok(pos);
        }
    }
}

/// Where the line before the one at `off` starts, or `None` at the first.
///
/// The byte at `off - 1` ends that line. Its true start is after the last newline
/// before it; a line longer than `MAX_LINE` is then cut into pieces from that
/// start, exactly as `line_at` cuts it reading forward, a newline going with the
/// piece before it.
fn prev_start<R: Read>(store: &mut Store<R>, off: u64) -> io::Result<Option<u64>> {
    let first = store.first();
    if off <= first {
        return Ok(None);
    }
    let last = off - 1;
    let start = match store.long {
        Some((start, clean)) if start >= first && start <= last && last <= clean => start,
        _ => {
            let mut end = last;
            let mut start = first;
            let mut block = [0u8; SCAN_BLOCK];
            while end > first {
                let from = end.saturating_sub(SCAN_BLOCK as u64).max(first);
                let len = usize::try_from(end - from).unwrap_or(SCAN_BLOCK);
                let n = store.read_full(from, block.get_mut(..len).unwrap_or_default())?;
                if let Some(i) = block
                    .get(..n)
                    .unwrap_or_default()
                    .iter()
                    .rposition(|b| *b == b'\n')
                {
                    start = from.saturating_add(i as u64 + 1);
                    break;
                }
                end = from;
            }
            store.long = Some((start, last));
            start
        }
    };
    let mut byte = [0u8; 1];
    let ends_line = store.read_at(last, &mut byte)? == 1 && byte == *b"\n";
    let tail = if ends_line && last > start {
        last - 1
    } else {
        last
    };
    let piece = MAX_LINE as u64;
    Ok(Some(start + (tail - start) / piece * piece))
}

/// The bytes a line shows: without its newline, or the CR before one.
/// A CR with no newline after it is not a line end, and is shown as `^M`.
fn shown(line: &[u8]) -> &[u8] {
    match line.strip_suffix(b"\n") {
        Some(text) => text.strip_suffix(b"\r").unwrap_or(text),
        None => line,
    }
}

/// What one run of a line's bytes is on screen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Piece {
    /// A printable character, sent as is.
    Text(usize),
    Tab,
    /// A C0 control byte, shown as `^` and a letter.
    Caret,
    /// A byte that is not a printable character — a C1 control, or not
    /// well-formed UTF-8 — shown as `<9B>`.
    Hex,
    /// `ESC [ ... m`, a colour change: sent through, zero columns.
    Sgr,
}

/// The piece starting at `i`, and how many bytes it spans.
fn piece(text: &[u8], i: usize) -> (Piece, usize) {
    match text.get(i).copied().unwrap_or(b' ') {
        b'\t' => (Piece::Tab, 1),
        0x1b => match sgr_len(text, i) {
            Some(n) => (Piece::Sgr, n),
            None => (Piece::Caret, 1),
        },
        0..=0x1f | 0x7f => (Piece::Caret, 1),
        0x20..=0x7e => (Piece::Text(1), 1),
        // U+0080..U+009F are the C1 controls: a terminal in UTF-8 mode takes
        // U+009B as CSI exactly as it takes ESC [.
        _ => match utf8(text, i) {
            Some((n, c)) if c >= 0xa0 => (Piece::Text(1), n),
            _ => (Piece::Hex, 1),
        },
    }
}

/// The well-formed UTF-8 sequence at `i`: its length and code point. Overlong
/// forms, surrogates and code points past U+10FFFF are not.
fn utf8(text: &[u8], i: usize) -> Option<(usize, u32)> {
    let lead = *text.get(i)?;
    let (len, mut c, low, high) = match lead {
        0xc2..=0xdf => (2, u32::from(lead & 0x1f), 0x80, 0xbf),
        0xe0 => (3, 0, 0xa0, 0xbf),
        0xe1..=0xec | 0xee..=0xef => (3, u32::from(lead & 0x0f), 0x80, 0xbf),
        0xed => (3, 0x0d, 0x80, 0x9f),
        0xf0 => (4, 0, 0x90, 0xbf),
        0xf1..=0xf3 => (4, u32::from(lead & 0x07), 0x80, 0xbf),
        0xf4 => (4, 4, 0x80, 0x8f),
        _ => return None,
    };
    for k in 1..len {
        let b = *text.get(i + k)?;
        let (min, max) = if k == 1 { (low, high) } else { (0x80, 0xbf) };
        if !(min..=max).contains(&b) {
            return None;
        }
        c = (c << 6) | u32::from(b & 0x3f);
    }
    Some((len, c))
}

/// The length of the colour sequence at `i`, if that is what is there. Bounded,
/// so a stray `ESC [` before a run of digits is not read as one forever.
fn sgr_len(text: &[u8], i: usize) -> Option<usize> {
    if text.get(i + 1) != Some(&b'[') {
        return None;
    }
    for j in i + 2..text.len().min(i + 64) {
        match text.get(j)? {
            b'0'..=b'9' | b';' | b':' => {}
            b'm' => return Some(j - i + 1),
            _ => return None,
        }
    }
    None
}

/// Columns a piece takes at column `col` of a `cols`-wide row. A tab runs to the
/// next stop but never past the edge; at the edge it takes a whole stop, which is
/// what moves it to the next row.
fn width(p: Piece, col: usize, cols: usize) -> usize {
    match p {
        Piece::Text(w) => w,
        Piece::Tab if col >= cols => TAB,
        Piece::Tab => (TAB - col % TAB).min(cols - col),
        Piece::Caret => 2,
        Piece::Hex => 4,
        Piece::Sgr => 0,
    }
}

/// Append a piece as it is shown. A tab is sent as a tab: where columns matter,
/// `render_row` expands it.
fn put_shown(frame: &mut Vec<u8>, p: Piece, bytes: &[u8]) -> io::Result<()> {
    let b = bytes.first().copied().unwrap_or(b'?');
    match p {
        Piece::Caret => put(frame, &[b'^', b ^ 0x40]),
        Piece::Hex => {
            let digit = |n: u8| HEX.get(usize::from(n)).copied().unwrap_or(b'?');
            put(frame, &[b'<', digit(b >> 4), digit(b & 0x0f), b'>'])
        }
        Piece::Text(_) | Piece::Tab | Piece::Sgr => put(frame, bytes),
    }
}

/// Where each screen row of `text` starts, wrapped at `cols`. Always at least one
/// row: an empty line still takes one.
///
/// A character the terminal draws two columns wide is counted as one, so such a
/// row can reach past the edge; the page is drawn with autowrap off, so it is cut
/// there rather than pushing every row below it down.
fn breaks(text: &[u8], cols: usize, out: &mut Vec<usize>) -> io::Result<()> {
    out.clear();
    allocation::push(out, 0).map_err(out_of_memory)?;
    let (mut i, mut col) = (0usize, 0usize);
    while i < text.len() {
        let (p, n) = piece(text, i);
        let w = width(p, col, cols);
        if col > 0 && col + w > cols {
            allocation::push(out, i).map_err(out_of_memory)?;
            col = 0;
            continue;
        }
        col += w;
        i += n;
    }
    Ok(())
}

/// `text` with its colour sequences taken out, which is what a search matches,
/// and where each byte left came from.
fn plain_text(text: &[u8], plain: &mut Vec<u8>, map: &mut Vec<usize>) -> io::Result<()> {
    plain.clear();
    map.clear();
    let mut i = 0usize;
    while i < text.len() {
        let (p, n) = piece(text, i);
        if p != Piece::Sgr {
            for k in i..i + n {
                allocation::push(plain, text.get(k).copied().unwrap_or(b' '))
                    .map_err(out_of_memory)?;
                allocation::push(map, k).map_err(out_of_memory)?;
            }
        }
        i += n;
    }
    Ok(())
}

/// Every place `pattern` occurs in `text`, without overlaps. The pattern is
/// literal text.
fn find_all(
    text: &[u8],
    pattern: &[u8],
    fold: bool,
    out: &mut Vec<(usize, usize)>,
) -> io::Result<()> {
    out.clear();
    if pattern.is_empty() {
        return Ok(());
    }
    let mut i = 0usize;
    while i + pattern.len() <= text.len() {
        if same(text.get(i..i + pattern.len()), pattern, fold) {
            allocation::push(out, (i, i + pattern.len())).map_err(out_of_memory)?;
            i += pattern.len();
        } else {
            i += 1;
        }
    }
    Ok(())
}

/// The matches of the search in `text`, as ranges of `text`'s own bytes: found in
/// the plain text, then mapped back, so a match that spans a colour change covers
/// it.
fn find_matches(
    text: &[u8],
    find: &Find,
    plain: &mut Vec<u8>,
    map: &mut Vec<usize>,
    out: &mut Vec<(usize, usize)>,
) -> io::Result<()> {
    out.clear();
    if find.pattern.is_empty() {
        return Ok(());
    }
    plain_text(text, plain, map)?;
    find_all(plain, &find.pattern, find.fold, out)?;
    for m in out.iter_mut() {
        let start = map.get(m.0).copied().unwrap_or(0);
        let end = map.get(m.1.saturating_sub(1)).map_or(start, |e| e + 1);
        *m = (start, end);
    }
    Ok(())
}

fn contains(text: &[u8], pattern: &[u8], fold: bool) -> bool {
    !pattern.is_empty()
        && text
            .windows(pattern.len())
            .any(|w| same(Some(w), pattern, fold))
}

fn same(window: Option<&[u8]>, pattern: &[u8], fold: bool) -> bool {
    window.is_some_and(|w| {
        if fold {
            w.eq_ignore_ascii_case(pattern)
        } else {
            w == pattern
        }
    })
}

/// A place on the page: the line at byte offset `line`, from its `row`th screen
/// row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Pos {
    line: u64,
    row: usize,
}

/// One file being paged.
struct View<R> {
    store: Store<R>,
    top: Pos,
}

impl<R: Read> View<R> {
    fn new(store: Store<R>) -> Self {
        let line = store.first();
        View {
            store,
            top: Pos { line, row: 0 },
        }
    }
}

/// Buffers reused by every page, so moving about allocates nothing once they
/// have grown to fit.
#[derive(Default)]
struct Work {
    line: Vec<u8>,
    breaks: Vec<usize>,
    ring: Vec<Pos>,
    matches: Vec<(usize, usize)>,
    plain: Vec<u8>,
    map: Vec<usize>,
    frame: Vec<u8>,
}

/// Read the line at `at` and lay it out. `None` at the end.
fn layout<R: Read>(
    store: &mut Store<R>,
    work: &mut Work,
    at: u64,
    cols: usize,
) -> io::Result<Option<u64>> {
    let next = line_at(store, at, &mut work.line)?;
    if next == at {
        return Ok(None);
    }
    breaks(shown(&work.line), cols, &mut work.breaks)?;
    Ok(Some(next))
}

/// Keep the top on a row that exists: the input may have been dropped from under
/// it (a spooled pipe), the file truncated under it, or the window resized under
/// a wrapped line.
fn settle<R: Read>(view: &mut View<R>, work: &mut Work, screen: Screen) -> io::Result<()> {
    let first = view.store.first();
    if view.top.line < first {
        view.top = Pos {
            line: first,
            row: 0,
        };
    }
    match layout(&mut view.store, work, view.top.line, screen.width())? {
        Some(_) => view.top.row = view.top.row.min(work.breaks.len().saturating_sub(1)),
        None if view.top.line > first => to_end(view, work, screen)?,
        None => view.top.row = 0,
    }
    Ok(())
}

/// Move `n` rows down, but never so far that the page stops being full: at the
/// end, the last row sits on the bottom of the page, as in `less`.
///
/// One walk, keeping the last page of positions in a ring: if the input ends
/// before `n` rows and a page, the answer is a page back from its last row.
fn forward<R: Read>(view: &mut View<R>, work: &mut Work, n: u64, screen: Screen) -> io::Result<()> {
    let page = screen.page();
    let cols = screen.width();
    work.ring.clear();
    allocation::resize(&mut work.ring, page, view.top).map_err(out_of_memory)?;
    let need = n.saturating_add(page as u64);
    let mut seen = 0u64;
    let mut target = view.top;
    let Pos { mut line, mut row } = view.top;
    'walk: while let Some(next) = layout(&mut view.store, work, line, cols)? {
        for r in row..work.breaks.len() {
            let here = Pos { line, row: r };
            if seen == n {
                target = here;
            }
            if let Some(slot) = work.ring.get_mut((seen % page as u64) as usize) {
                *slot = here;
            }
            seen += 1;
            if seen >= need {
                break 'walk;
            }
        }
        line = next;
        row = 0;
    }
    if seen >= need {
        view.top = target;
    } else if seen > page as u64 {
        let oldest = (seen - page as u64) % page as u64;
        if let Some(here) = work.ring.get(oldest as usize) {
            view.top = *here;
        }
    }
    Ok(())
}

/// Move `n` rows up, stopping at the first.
fn backward<R: Read>(
    view: &mut View<R>,
    work: &mut Work,
    n: u64,
    screen: Screen,
) -> io::Result<()> {
    let mut left = n;
    while left > 0 {
        if view.top.row > 0 {
            let step = left.min(view.top.row as u64);
            view.top.row -= step as usize;
            left -= step;
            continue;
        }
        let Some(prev) = prev_start(&mut view.store, view.top.line)? else {
            break;
        };
        if layout(&mut view.store, work, prev, screen.width())?.is_none() {
            break;
        }
        view.top = Pos {
            line: prev,
            row: work.breaks.len().saturating_sub(1),
        };
        left -= 1;
    }
    Ok(())
}

/// If the page from the top runs out before the screen does, move up until it is
/// full or the first line is at the top.
fn fill<R: Read>(view: &mut View<R>, work: &mut Work, screen: Screen) -> io::Result<()> {
    let page = screen.page();
    let mut rows = 0usize;
    let Pos { mut line, row } = view.top;
    let mut skip = row;
    while rows < page {
        let Some(next) = layout(&mut view.store, work, line, screen.width())? else {
            break;
        };
        rows += work.breaks.len().saturating_sub(skip);
        skip = 0;
        line = next;
    }
    if rows < page {
        backward(view, work, (page - rows) as u64, screen)?;
    }
    Ok(())
}

/// The last page.
fn to_end<R: Read>(view: &mut View<R>, work: &mut Work, screen: Screen) -> io::Result<()> {
    let end = view.store.end(view.top.line)?;
    let first = view.store.first();
    let Some(last) = prev_start(&mut view.store, end)? else {
        view.top = Pos {
            line: first,
            row: 0,
        };
        return Ok(());
    };
    let rows = match layout(&mut view.store, work, last, screen.width())? {
        Some(_) => work.breaks.len(),
        None => 1,
    };
    view.top = Pos {
        line: last,
        row: rows.saturating_sub(1),
    };
    backward(view, work, screen.page().saturating_sub(1) as u64, screen)
}

/// Line `n`, counting from one, at the top; the last page if there are fewer.
fn to_line<R: Read>(view: &mut View<R>, work: &mut Work, n: u64, screen: Screen) -> io::Result<()> {
    let mut line = view.store.first();
    for _ in 1..n {
        let next = line_at(&mut view.store, line, &mut work.line)?;
        if next == line {
            return to_end(view, work, screen);
        }
        line = next;
    }
    view.top = Pos { line, row: 0 };
    settle(view, work, screen)?;
    fill(view, work, screen)
}

/// The search in force.
#[derive(Default)]
struct Find {
    pattern: Vec<u8>,
    fold: bool,
    backwards: bool,
    /// The line the last match was on: `n` goes on from it while it is on the
    /// page, so a match `fill` moved off the top is not found again.
    last: Option<u64>,
}

/// Where a search starts, and which way it goes.
#[derive(Clone, Copy)]
struct Start {
    at: u64,
    /// Whether the line at `at` is searched too: a new search takes the top line
    /// (the bottom one, backwards); `n` and `N` go on past it.
    inclusive: bool,
    backwards: bool,
}

/// Put the `count`th line containing the pattern from `start` at the top.
/// `false`, and nothing moved, if there is no such line.
fn search<R: Read>(
    view: &mut View<R>,
    work: &mut Work,
    find: &mut Find,
    start: Start,
    count: u64,
    screen: Screen,
) -> io::Result<bool> {
    let mut at = start.at;
    let mut include = start.inclusive;
    for _ in 0..count {
        loop {
            if !include {
                at = if start.backwards {
                    match prev_start(&mut view.store, at)? {
                        Some(prev) => prev,
                        None => return Ok(false),
                    }
                } else {
                    let next = line_at(&mut view.store, at, &mut work.line)?;
                    if next == at {
                        return Ok(false);
                    }
                    next
                };
            }
            include = false;
            if line_at(&mut view.store, at, &mut work.line)? == at {
                return Ok(false);
            }
            let Work {
                line, plain, map, ..
            } = &mut *work;
            plain_text(shown(line), plain, map)?;
            if contains(plain, &find.pattern, find.fold) {
                break;
            }
        }
    }
    find.last = Some(at);
    view.top = Pos { line: at, row: 0 };
    fill(view, work, screen)?;
    Ok(true)
}

/// Search, saying on the prompt row why nothing moved.
fn run_search<R: Read>(
    view: &mut View<R>,
    work: &mut Work,
    find: &mut Find,
    start: Start,
    count: Option<u64>,
    screen: Screen,
) -> io::Result<Option<&'static str>> {
    if find.pattern.is_empty() {
        return Ok(Some("No previous search pattern"));
    }
    if search(view, work, find, start, count.unwrap_or(1).max(1), screen)? {
        Ok(None)
    } else {
        Ok(Some("Pattern not found"))
    }
}

/// Append one screen row of `text`, bytes `from..to`, with the matches lit.
///
/// The colour changes earlier in the line are replayed first, from the last reset,
/// so a row that starts mid-line is drawn in the colour it is in. Every row ends
/// with a reset, so no colour bleeds into the cleared rest of the row or the next.
fn render_row(
    frame: &mut Vec<u8>,
    text: &[u8],
    from: usize,
    to: usize,
    cols: usize,
    matches: &[(usize, usize)],
) -> io::Result<()> {
    let mut replay = 0usize;
    let mut i = 0usize;
    while i < from {
        let (p, n) = piece(text, i);
        if p == Piece::Sgr && matches!(text.get(i..i + n), Some(b"\x1b[m" | b"\x1b[0m")) {
            replay = i;
        }
        i += n;
    }
    let mut i = replay;
    while i < from {
        let (p, n) = piece(text, i);
        if p == Piece::Sgr {
            put(frame, text.get(i..i + n).unwrap_or_default())?;
        }
        i += n;
    }
    let mut next_match = matches
        .iter()
        .position(|&(_, end)| end > from)
        .unwrap_or(matches.len());
    let mut lit = false;
    let mut col = 0usize;
    let mut i = from;
    while i < to {
        let (p, n) = piece(text, i);
        while matches.get(next_match).is_some_and(|&(_, end)| end <= i) {
            next_match += 1;
        }
        let want = matches
            .get(next_match)
            .is_some_and(|&(start, _)| start <= i);
        if want != lit {
            put(frame, if want { b"\x1b[7m" } else { b"\x1b[27m" })?;
            lit = want;
        }
        let w = width(p, col, cols);
        if p == Piece::Tab {
            put(frame, SPACES.get(..w).unwrap_or(SPACES))?;
        } else {
            put_shown(frame, p, text.get(i..i + n).unwrap_or_default())?;
        }
        // The file's own reset must not end a match half way.
        if p == Piece::Sgr && lit {
            put(frame, b"\x1b[7m")?;
        }
        col += w;
        i += n;
    }
    put(frame, b"\x1b[m\x1b[K\r\n")
}

/// Write what the frame holds so far, before a read that may wait on a pipe, so
/// the rows that have arrived are seen. A failed write is kept for the caller.
///
/// Autowrap goes back on before the wait and off again after it: Ctrl-C is how
/// a reader leaves a pipe that has paused, and a pager killed there must not
/// leave the terminal not wrapping, which `stty sane` does not cure. Every flush
/// is at the start of a row, where the switch moves nothing.
fn flush_partial<W: Write>(frame: &mut Vec<u8>, out: &mut W, failed: &mut Option<io::Error>) {
    if failed.is_none() {
        let sent = put(frame, b"\x1b[?7h")
            .and_then(|()| out.write_all(frame))
            .and_then(|()| out.flush());
        if let Err(e) = sent {
            *failed = Some(e);
        }
    }
    frame.clear();
    if let Err(e) = put(frame, b"\x1b[?7l") {
        failed.get_or_insert(e);
    }
}

/// What a page showed.
struct Drawn {
    /// The input ends on it.
    at_end: bool,
    /// Where its last line starts.
    bottom: u64,
}

/// Append the page from the top to `work.frame`.
fn render<R: Read, W: Write>(
    view: &mut View<R>,
    work: &mut Work,
    screen: Screen,
    find: &Find,
    out: &mut W,
    failed: &mut Option<io::Error>,
) -> io::Result<Drawn> {
    let page = screen.page();
    let cols = screen.width();
    let mut drawn = 0usize;
    let Pos { mut line, mut row } = view.top;
    let mut bottom = line;
    let mut more = false;
    while drawn < page {
        if !view.store.ready(line) {
            flush_partial(&mut work.frame, out, failed);
        }
        let Some(next) = layout(&mut view.store, work, line, cols)? else {
            break;
        };
        let Work {
            line: bytes,
            breaks,
            matches,
            plain,
            map,
            frame,
            ..
        } = &mut *work;
        let text = shown(bytes);
        find_matches(text, find, plain, map, matches)?;
        bottom = line;
        let mut r = row;
        while r < breaks.len() && drawn < page {
            let from = breaks.get(r).copied().unwrap_or(text.len());
            let to = breaks.get(r + 1).copied().unwrap_or(text.len());
            render_row(frame, text, from, to, cols, matches)?;
            r += 1;
            drawn += 1;
        }
        if r < breaks.len() {
            more = true;
            break;
        }
        line = next;
        row = 0;
        if drawn == page {
            // Whether anything follows decides `(END)`. Asking a pipe may wait on
            // it, so the page goes out first and only the prompt waits.
            if !view.store.ready(line) {
                flush_partial(&mut work.frame, out, failed);
            }
            more = line_at(&mut view.store, line, &mut work.line)? != line;
        }
    }
    while drawn < page {
        put(&mut work.frame, b"~\x1b[K\r\n")?;
        drawn += 1;
    }
    Ok(Drawn {
        at_end: !more,
        bottom,
    })
}

/// A short message for the prompt row, held without allocating.
struct Message {
    bytes: [u8; 160],
    len: usize,
}

impl Message {
    fn new() -> Self {
        Message {
            bytes: [0; 160],
            len: 0,
        }
    }

    /// Cut short, silently, if it does not fit.
    fn set(&mut self, args: std::fmt::Arguments<'_>) {
        let mut cursor = io::Cursor::new(self.bytes.as_mut_slice());
        let _ = cursor.write_fmt(args);
        self.len = usize::try_from(cursor.position()).unwrap_or(0);
    }

    fn text(&self) -> &[u8] {
        self.bytes.get(..self.len).unwrap_or_default()
    }

    fn clear(&mut self) {
        self.len = 0;
    }
}

/// The prompt row. A name or a message can hold anything, so whatever is not a
/// printable character is shown as `?` rather than sent.
fn prompt(frame: &mut Vec<u8>, parts: &[&[u8]], standout: bool) -> io::Result<()> {
    if standout {
        put(frame, b"\x1b[7m")?;
    }
    for part in parts {
        let mut i = 0usize;
        while i < part.len() {
            let (p, n) = piece(part, i);
            match p {
                Piece::Text(_) => put(frame, part.get(i..i + n).unwrap_or_default())?,
                _ => put(frame, b"?")?,
            }
            i += n;
        }
    }
    if standout {
        put(frame, b"\x1b[27m")?;
    }
    put(frame, b"\x1b[K")
}

/// One key, decoded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Key {
    Byte(u8),
    Up,
    Down,
    PageUp,
    PageDown,
    Home,
    End,
    /// An escape sequence this pager has no use for.
    Ignored,
}

/// Command input. In raw mode a key is a byte; otherwise commands come a line at
/// a time, ended by Enter. `line` also holds a `+COMMAND`, read before either.
struct Keys<C> {
    source: C,
    raw: bool,
    line: Vec<u8>,
    at: usize,
    /// ESC just abandoned a pattern: if it began an escape sequence (an arrow,
    /// Home), the rest of that sequence is not a command.
    after_escape: bool,
}

impl<C: BufRead> Keys<C> {
    /// One byte of command; `None` once the source has closed, which quits — a
    /// command source that has closed can never say "continue".
    fn byte(&mut self) -> io::Result<Option<u8>> {
        if let Some(b) = self.line.get(self.at).copied() {
            self.at += 1;
            return Ok(Some(b));
        }
        if self.raw {
            let mut b = [0u8; 1];
            loop {
                match self.source.read(&mut b) {
                    Ok(0) => return Ok(None),
                    Ok(_) => return Ok(b.first().copied()),
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(e) => return Err(e),
                }
            }
        }
        if !self.refill()? {
            return Ok(None);
        }
        let b = self.line.get(self.at).copied();
        self.at += 1;
        Ok(b)
    }

    /// The next typed line. A bare Enter is kept, as one line forward; otherwise
    /// the newline only submitted the line and is dropped.
    fn refill(&mut self) -> io::Result<bool> {
        self.line.clear();
        self.at = 0;
        loop {
            let available = match self.source.fill_buf() {
                Ok(a) => a,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            };
            if available.is_empty() {
                return Ok(!self.line.is_empty());
            }
            let room = (MAX_PATTERN + 16).saturating_sub(self.line.len());
            let (take, complete) = match available.iter().position(|b| *b == b'\n') {
                Some(i) => (i + 1, true),
                None => (available.len(), false),
            };
            put(
                &mut self.line,
                available.get(..take.min(room)).unwrap_or_default(),
            )?;
            self.source.consume(take);
            if complete {
                break;
            }
        }
        if self.line != b"\n" && self.line.last() == Some(&b'\n') {
            self.line.pop();
        }
        Ok(true)
    }

    /// Whether part of a line (typed, or a `+COMMAND`) is still to be read.
    fn buffered(&self) -> bool {
        self.at < self.line.len()
    }

    /// What remains of the line, which after `/` is the pattern.
    fn rest_of_line(&mut self) -> &[u8] {
        let rest = self.line.get(self.at..).unwrap_or_default();
        self.at = self.line.len();
        rest
    }

    fn key(&mut self) -> io::Result<Option<Key>> {
        let Some(b) = self.byte()? else {
            return Ok(None);
        };
        if std::mem::take(&mut self.after_escape) && (b == b'[' || b == b'O') {
            return Ok(self.sequence()?.map(|_| Key::Ignored));
        }
        if b != 0x1b {
            return Ok(Some(Key::Byte(b)));
        }
        let Some(intro) = self.byte()? else {
            return Ok(None);
        };
        if intro != b'[' && intro != b'O' {
            // `ESC v` and `ESC SPACE` are `less`'s own two-key page moves.
            return Ok(Some(match intro {
                b'v' => Key::PageUp,
                b' ' => Key::PageDown,
                _ => Key::Ignored,
            }));
        }
        self.sequence()
    }

    /// The rest of `ESC [` or `ESC O`: parameters, then the final byte. Bounded,
    /// so a stray ESC [ cannot swallow the commands after it for long.
    fn sequence(&mut self) -> io::Result<Option<Key>> {
        let mut param = 0u32;
        for _ in 0..16 {
            let Some(c) = self.byte()? else {
                return Ok(None);
            };
            match c {
                b'0'..=b'9' => param = param.saturating_mul(10).saturating_add(u32::from(c - b'0')),
                b';' => {}
                0x40..=0x7e => {
                    return Ok(Some(match (c, param) {
                        (b'A', _) => Key::Up,
                        (b'B', _) => Key::Down,
                        (b'H', _) | (b'~', 1 | 7) => Key::Home,
                        (b'F', _) | (b'~', 4 | 8) => Key::End,
                        (b'~', 5) => Key::PageUp,
                        (b'~', 6) => Key::PageDown,
                        _ => Key::Ignored,
                    }))
                }
                _ => return Ok(Some(Key::Ignored)),
            }
        }
        Ok(Some(Key::Ignored))
    }
}

enum Entry {
    Typed,
    Cancelled,
    Closed,
}

/// Read a search pattern after `/` or `?` into `pattern`, echoing it on the prompt
/// row in raw mode. An empty entry keeps the previous pattern, as `less` does.
fn enter_pattern<C: BufRead, W: Write>(
    keys: &mut Keys<C>,
    out: &mut W,
    lead: u8,
    pattern: &mut Vec<u8>,
) -> io::Result<Entry> {
    let mut typed: Vec<u8> = allocation::with_capacity(MAX_PATTERN).map_err(out_of_memory)?;
    if keys.raw && !keys.buffered() {
        out.write_all(&[b'\r', lead])?;
        out.write_all(b"\x1b[K")?;
        out.flush()?;
        loop {
            let Some(b) = keys.byte()? else {
                return Ok(Entry::Closed);
            };
            match b {
                b'\r' | b'\n' => break,
                // Erasing past the start abandons the search, as in `less`. One
                // character goes, not one byte.
                0x7f | 0x08 => {
                    if typed.is_empty() {
                        return Ok(Entry::Cancelled);
                    }
                    while typed.last().is_some_and(|b| (0x80..0xc0).contains(b)) {
                        typed.pop();
                    }
                    typed.pop();
                    out.write_all(b"\x08 \x08")?;
                }
                0x1b => {
                    keys.after_escape = true;
                    return Ok(Entry::Cancelled);
                }
                0x15 => {
                    typed.clear();
                    out.write_all(&[b'\r', lead])?;
                    out.write_all(b"\x1b[K")?;
                }
                _ if b < 0x20 => {}
                _ if typed.len() < MAX_PATTERN => {
                    allocation::push(&mut typed, b).map_err(out_of_memory)?;
                    out.write_all(&[b])?;
                }
                _ => {}
            }
            out.flush()?;
        }
    } else {
        let rest = keys.rest_of_line();
        put(
            &mut typed,
            rest.get(..rest.len().min(MAX_PATTERN)).unwrap_or_default(),
        )?;
    }
    if !typed.is_empty() {
        pattern.clear();
        put(pattern, &typed)?;
    }
    Ok(Entry::Typed)
}

/// What a session draws on and reads from. Split from `run` so the whole pager is
/// reachable from a test: what `run` adds is a real terminal, and none of the
/// behaviour worth asserting is about having one.
struct Terminal<'a, W, C> {
    out: &'a mut W,
    commands: C,
    raw: bool,
    measure: &'a mut dyn FnMut() -> Screen,
    /// Keys run before any are read: the `+COMMAND`.
    typed_ahead: &'a [u8],
}

/// An operand, as far as the session has got with it.
enum Slot<R> {
    Unopened,
    Open(View<R>),
    /// It would not open, or failed while being read; passed over from then on.
    Dead,
}

/// The operands, opened as they are reached.
struct Files<'a, R> {
    names: &'a [&'a str],
    open: &'a mut dyn FnMut(&str) -> io::Result<Store<R>>,
    slots: Vec<Slot<R>>,
    status: u8,
}

impl<'a, R: Read> Files<'a, R> {
    fn name(&self, i: usize) -> &'a str {
        self.names.get(i).copied().unwrap_or("-")
    }

    /// The nearest operand from `from` (itself too, if `inclusive`), onward or
    /// back, that is open or opens. One that will not is named on the prompt row
    /// — and on stderr if `loud`, for when no page is ever drawn — and passed
    /// over. `Err` says whether any was.
    fn step(
        &mut self,
        from: usize,
        onward: bool,
        inclusive: bool,
        message: &mut Message,
        loud: bool,
    ) -> Result<usize, bool> {
        let mut at = from;
        let mut take = inclusive;
        let mut failed = false;
        loop {
            if !take {
                let next = if onward {
                    at.checked_add(1)
                } else {
                    at.checked_sub(1)
                };
                at = next.ok_or(failed)?;
            }
            take = false;
            let name = self.name(at);
            let slot = self.slots.get_mut(at).ok_or(failed)?;
            match slot {
                Slot::Open(_) => return Ok(at),
                Slot::Dead => {}
                Slot::Unopened => match (self.open)(name) {
                    Ok(store) => {
                        *slot = Slot::Open(View::new(store));
                        return Ok(at);
                    }
                    Err(e) => {
                        if loud {
                            report(name, &e);
                        }
                        message.set(format_args!("{name}: {e}"));
                        *slot = Slot::Dead;
                        self.status = 1;
                        failed = true;
                    }
                },
            }
        }
    }
}

/// Why a page or a command could not go on.
enum Fault {
    /// The operand could not be read: it is passed over.
    Input(io::Error),
    /// The terminal could not be written or read: the session ends.
    Output(io::Error),
}

/// One run of the pager over its operands.
struct Session<'a, R, W, C> {
    files: Files<'a, R>,
    current: usize,
    out: &'a mut W,
    keys: Keys<C>,
    measure: &'a mut dyn FnMut() -> Screen,
    options: Options,
    work: Work,
    find: Find,
    message: Message,
    /// The next page clears the screen first. In line mode every page does, since
    /// the Enter that ended each command has scrolled the screen.
    clear: bool,
    raw: bool,
    /// Where the last line on the page starts: a new `?` searches from it.
    bottom: u64,
    /// A read error that left nothing to page, said once the terminal is back.
    lost: Option<(usize, io::Error)>,
}

/// Page `names` until the reader quits. Returns 1 if any operand could not be
/// opened or read, which does not stop the others being paged.
fn page_files<R: Read, W: Write, C: BufRead>(
    names: &[&str],
    open: &mut dyn FnMut(&str) -> io::Result<Store<R>>,
    terminal: Terminal<'_, W, C>,
    options: Options,
) -> io::Result<u8> {
    let Terminal {
        out,
        commands,
        raw,
        measure,
        typed_ahead,
    } = terminal;
    let mut slots = allocation::with_capacity(names.len()).map_err(out_of_memory)?;
    for _ in names {
        allocation::push(&mut slots, Slot::Unopened).map_err(out_of_memory)?;
    }
    let mut files = Files {
        names,
        open,
        slots,
        status: 0,
    };
    let mut message = Message::new();
    let Ok(current) = files.step(0, true, true, &mut message, true) else {
        return Ok(files.status);
    };
    let mut work = Work::default();
    if options.quit_if_one_screen && names.len() == 1 {
        if let Some(Slot::Open(view)) = files.slots.get_mut(current) {
            let shown = match fits(view, &mut work, measure()) {
                Ok(true) => copy_clean(&mut view.store, &mut work, out),
                Ok(false) => Ok(false),
                Err(e) => Err(Fault::Input(e)),
            };
            match shown {
                Ok(true) => return Ok(files.status),
                Ok(false) => {}
                Err(Fault::Input(e)) => {
                    report(files.name(current), &e);
                    return Ok(1);
                }
                Err(Fault::Output(e)) => return Err(e),
            }
        }
    }
    let mut line = Vec::new();
    put(&mut line, typed_ahead)?;
    let session = Session {
        files,
        current,
        out,
        keys: Keys {
            source: commands,
            raw,
            line,
            at: 0,
            after_escape: false,
        },
        measure,
        options,
        work,
        find: Find::default(),
        message,
        clear: true,
        raw,
        bottom: 0,
        lost: None,
    };
    session.run()
}

impl<R: Read, W: Write, C: BufRead> Session<'_, R, W, C> {
    fn run(mut self) -> io::Result<u8> {
        let mut redraw = true;
        let mut count: Option<u64> = None;
        let result = loop {
            let screen = (self.measure)();
            if redraw {
                match self.draw(screen) {
                    Ok(()) => {}
                    Err(Fault::Input(e)) => {
                        if self.lose(e) {
                            continue;
                        }
                        break Ok(());
                    }
                    Err(Fault::Output(e)) => break Err(e),
                }
            }
            redraw = true;
            let key = match self.keys.key() {
                Ok(Some(key)) => key,
                Ok(None) => break Ok(()),
                Err(e) => break Err(e),
            };
            let n = count.take();
            if let Key::Byte(d @ b'0'..=b'9') = key {
                // A count is not a command: the page stays as it is until one
                // comes.
                let digit = u64::from(d - b'0');
                count = Some(
                    n.unwrap_or(0)
                        .saturating_mul(10)
                        .saturating_add(digit)
                        .min(MAX_COUNT),
                );
                redraw = false;
                continue;
            }
            match self.command(key, n, screen) {
                Ok(true) => {}
                Ok(false) => break Ok(()),
                Err(Fault::Input(e)) => {
                    if !self.lose(e) {
                        break Ok(());
                    }
                }
                Err(Fault::Output(e)) => break Err(e),
            }
        };
        // Leave the last page where it is and clear only the prompt under it, so
        // the shell's prompt lands there.
        let restored = write(self.out, b"\r\x1b[K\x1b[?7h");
        result?;
        restored?;
        if let Some((at, e)) = &self.lost {
            report(self.files.name(*at), e);
        }
        Ok(self.files.status)
    }

    /// The operand being paged could not be read. Name it on the prompt row, pass
    /// it over, and go on with the nearest other one; `false` if there is none.
    fn lose(&mut self, e: io::Error) -> bool {
        let at = self.current;
        let name = self.files.name(at);
        self.message.set(format_args!("{name}: {e}"));
        self.files.status = 1;
        if let Some(slot) = self.files.slots.get_mut(at) {
            *slot = Slot::Dead;
        }
        self.find.last = None;
        self.clear = true;
        let next = match self.files.step(at, true, false, &mut self.message, false) {
            Ok(to) => Ok(to),
            Err(_) => self.files.step(at, false, false, &mut self.message, false),
        };
        match next {
            Ok(to) => {
                self.current = to;
                true
            }
            Err(_) => {
                self.lost = Some((at, e));
                false
            }
        }
    }

    /// Draw one whole screen: the page, then the prompt row.
    fn draw(&mut self, screen: Screen) -> Result<(), Fault> {
        // A message stays up while a `+COMMAND` is still running: the reader has
        // not had a page to read it on yet.
        let keep_message = self.keys.buffered();
        let Session {
            files,
            current,
            out,
            work,
            find,
            message,
            clear,
            raw,
            bottom,
            ..
        } = self;
        let name = files.name(*current);
        let next = files.names.get(*current + 1).copied();
        let Some(Slot::Open(view)) = files.slots.get_mut(*current) else {
            return Ok(());
        };
        work.frame.clear();
        // Autowrap off while drawing, so a row the terminal measures wider than
        // this pager did is cut at the edge instead of pushing every row below it
        // down.
        let head: &[u8] = if *clear {
            b"\x1b[?7l\x1b[H\x1b[2J"
        } else {
            b"\x1b[?7l\x1b[H"
        };
        put(&mut work.frame, head).map_err(Fault::Output)?;
        let mut failed = None;
        if !view.store.ready(view.top.line) {
            flush_partial(&mut work.frame, *out, &mut failed);
        }
        settle(view, work, screen).map_err(Fault::Input)?;
        let drawn = render(view, work, screen, find, *out, &mut failed).map_err(Fault::Input)?;
        *bottom = drawn.bottom;
        let first = view.store.first();
        let frame = &mut work.frame;
        let written = if !message.text().is_empty() {
            prompt(frame, &[message.text()], true)
        } else if first > 0
            && view.top
                == (Pos {
                    line: first,
                    row: 0,
                })
        {
            prompt(frame, &[b"(input before this was not kept)"], true)
        } else if drawn.at_end {
            match next {
                Some(next) => prompt(frame, &[b"(END) - Next: ", next.as_bytes()], true),
                None => prompt(frame, &[b"(END)"], true),
            }
        } else if view.top == (Pos { line: 0, row: 0 }) && name != "-" {
            prompt(frame, &[name.as_bytes()], true)
        } else {
            prompt(frame, &[b":"], false)
        };
        written
            .and_then(|()| put(frame, b"\x1b[?7h"))
            .map_err(Fault::Output)?;
        if let Some(e) = failed {
            return Err(Fault::Output(e));
        }
        out.write_all(frame)
            .and_then(|()| out.flush())
            .map_err(Fault::Output)?;
        *clear = !*raw;
        if !keep_message {
            message.clear();
        }
        Ok(())
    }

    /// Carry out one command; `false` to quit.
    fn command(&mut self, key: Key, n: Option<u64>, screen: Screen) -> Result<bool, Fault> {
        let page = screen.page() as u64;
        let half = (page / 2).max(1);
        let Session {
            files,
            current,
            out,
            keys,
            options,
            work,
            find,
            message,
            clear,
            bottom,
            ..
        } = self;
        let Some(Slot::Open(view)) = files.slots.get_mut(*current) else {
            return Ok(false);
        };
        let input = Fault::Input;
        let output = Fault::Output;
        match key {
            Key::Byte(b'q' | b'Q') => return Ok(false),
            Key::Byte(b'Z') => {
                if matches!(keys.byte().map_err(output)?, Some(b'Z') | None) {
                    return Ok(false);
                }
            }
            Key::Byte(b' ' | b'f' | b'z' | 0x06 | 0x16) | Key::PageDown => {
                forward(view, work, n.unwrap_or(page), screen).map_err(input)?
            }
            Key::Byte(b'b' | 0x02) | Key::PageUp => {
                backward(view, work, n.unwrap_or(page), screen).map_err(input)?
            }
            Key::Byte(b'j' | b'e' | b'\r' | b'\n' | 0x05 | 0x0e) | Key::Down => {
                forward(view, work, n.unwrap_or(1), screen).map_err(input)?
            }
            Key::Byte(b'k' | b'y' | 0x19 | 0x10 | 0x0b) | Key::Up => {
                backward(view, work, n.unwrap_or(1), screen).map_err(input)?
            }
            Key::Byte(b'd' | 0x04) => {
                forward(view, work, n.unwrap_or(half), screen).map_err(input)?
            }
            Key::Byte(b'u' | 0x15) => {
                backward(view, work, n.unwrap_or(half), screen).map_err(input)?
            }
            Key::Byte(b'g' | b'<') | Key::Home => {
                to_line(view, work, n.unwrap_or(1).max(1), screen).map_err(input)?
            }
            Key::Byte(b'G' | b'>') | Key::End => match n {
                Some(line) => to_line(view, work, line.max(1), screen).map_err(input)?,
                None => to_end(view, work, screen).map_err(input)?,
            },
            Key::Byte(lead @ (b'/' | b'?')) => {
                match enter_pattern(keys, &mut **out, lead, &mut find.pattern).map_err(output)? {
                    Entry::Closed => return Ok(false),
                    Entry::Cancelled => {}
                    Entry::Typed => {
                        find.backwards = lead == b'?';
                        find.fold = options.fold(&find.pattern);
                        let start = Start {
                            at: if find.backwards {
                                *bottom
                            } else {
                                view.top.line
                            },
                            inclusive: true,
                            backwards: find.backwards,
                        };
                        let note = run_search(view, work, find, start, n, screen).map_err(input)?;
                        if let Some(note) = note {
                            message.set(format_args!("{note}"));
                        }
                    }
                }
            }
            Key::Byte(b'n' | b'N') => {
                let backwards = find.backwards != (key == Key::Byte(b'N'));
                let at = match find.last {
                    Some(last) if last >= view.top.line && last <= *bottom => last,
                    _ => view.top.line,
                };
                let start = Start {
                    at,
                    inclusive: false,
                    backwards,
                };
                if let Some(note) = run_search(view, work, find, start, n, screen).map_err(input)? {
                    message.set(format_args!("{note}"));
                }
            }
            Key::Byte(b':') => {
                let (from, onward, inclusive, end) = match keys.byte().map_err(output)? {
                    Some(b'q' | b'Q') | None => return Ok(false),
                    Some(b'n') => (*current, true, false, "No next file"),
                    Some(b'p') => (*current, false, false, "No previous file"),
                    Some(b'x') => (0, true, true, ""),
                    Some(_) => return Ok(true),
                };
                match files.step(from, onward, inclusive, message, false) {
                    Ok(to) => {
                        if to != *current {
                            find.last = None;
                        }
                        *current = to;
                    }
                    Err(false) => message.set(format_args!("{end}")),
                    Err(true) => {}
                }
            }
            Key::Byte(b'r' | b'R' | 0x0c | 0x12) => *clear = true,
            Key::Byte(b'h' | b'H') => message.set(format_args!("{HELP}")),
            Key::Byte(_) | Key::Ignored => {}
        }
        Ok(true)
    }
}

/// Whether the whole input fits on one page.
fn fits<R: Read>(view: &mut View<R>, work: &mut Work, screen: Screen) -> io::Result<bool> {
    let page = screen.page();
    let mut rows = 0usize;
    let mut line = view.top.line;
    while let Some(next) = layout(&mut view.store, work, line, screen.width())? {
        rows += work.breaks.len();
        if rows > page {
            return Ok(false);
        }
        line = next;
    }
    Ok(true)
}

/// `-F`'s one screen: the input line by line, with no page, prompt or wrapping,
/// but shown exactly as a page shows it — colours through, every other control
/// in caret or hex form. Returns `true` (the input is shown).
fn copy_clean<R: Read, W: Write>(
    store: &mut Store<R>,
    work: &mut Work,
    out: &mut W,
) -> Result<bool, Fault> {
    let mut at = store.first();
    loop {
        let next = line_at(store, at, &mut work.line).map_err(Fault::Input)?;
        if next == at {
            out.flush().map_err(Fault::Output)?;
            return Ok(true);
        }
        work.frame.clear();
        let text = shown(&work.line);
        let mut i = 0usize;
        while i < text.len() {
            let (p, n) = piece(text, i);
            put_shown(&mut work.frame, p, text.get(i..i + n).unwrap_or_default())
                .map_err(Fault::Output)?;
            i += n;
        }
        put(
            &mut work.frame,
            work.line.get(text.len()..).unwrap_or_default(),
        )
        .map_err(Fault::Output)?;
        if !write(out, &work.frame).map_err(Fault::Output)? {
            return Ok(true);
        }
        at = next;
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::panic
    )]
    use super::*;
    use std::cell::RefCell;
    use std::io::Cursor;
    use std::rc::Rc;

    /// Five rows of text and the prompt.
    const SMALL: Screen = Screen { rows: 6, cols: 80 };

    /// An in-memory pipe; a `broken` one fails every read.
    struct Fake {
        data: Cursor<Vec<u8>>,
        broken: bool,
    }

    impl Read for Fake {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.broken {
                return Err(io::Error::other("bad sector"));
            }
            self.data.read(buf)
        }
    }

    fn fake(text: &[u8], broken: bool) -> Fake {
        Fake {
            data: Cursor::new(text.to_vec()),
            broken,
        }
    }

    fn numbered(lines: usize) -> Vec<u8> {
        let mut text = Vec::new();
        for n in 1..=lines {
            text.extend_from_slice(format!("line{n}\n").as_bytes());
        }
        text
    }

    fn spool(text: &[u8]) -> Store<Fake> {
        Store::spool(fake(text, false), SPOOL_CAP)
    }

    /// A whole session over in-memory operands, `keys` typed, the command source
    /// closing after them (which quits). An operand named `broken` opens and then
    /// fails to read.
    fn run_full(
        names: &[&str],
        docs: &[(&str, &[u8])],
        ahead: &[u8],
        keys: &[u8],
        screen: Screen,
        raw: bool,
        options: Options,
    ) -> (Vec<u8>, u8) {
        let mut open = |name: &str| -> io::Result<Store<Fake>> {
            match docs.iter().position(|d| d.0 == name) {
                Some(i) => Ok(Store::spool(fake(docs[i].1, name == "broken"), SPOOL_CAP)),
                None => Err(io::Error::from(io::ErrorKind::NotFound)),
            }
        };
        let mut out = Vec::new();
        let mut measure = || screen;
        let terminal = Terminal {
            out: &mut out,
            commands: Cursor::new(keys.to_vec()),
            raw,
            measure: &mut measure,
            typed_ahead: ahead,
        };
        let status = page_files(names, &mut open, terminal, options).unwrap();
        (out, status)
    }

    fn run_session(
        names: &[&str],
        docs: &[(&str, &[u8])],
        keys: &[u8],
        screen: Screen,
        raw: bool,
        options: Options,
    ) -> (Vec<u8>, u8) {
        run_full(names, docs, b"", keys, screen, raw, options)
    }

    fn session(text: &[u8], keys: &[u8]) -> Vec<String> {
        let (out, _) = run_session(
            &["t"],
            &[("t", text)],
            keys,
            SMALL,
            true,
            Options::default(),
        );
        last_screen(&out)
    }

    /// The last page drawn, from its cursor-home on.
    fn last_frame(out: &[u8]) -> &[u8] {
        let start = out
            .windows(3)
            .rposition(|w| w == b"\x1b[H")
            .expect("no page was drawn");
        &out[start..]
    }

    /// The screen the last page left: escape sequences dropped, one string per
    /// row, the prompt row last.
    fn last_screen(out: &[u8]) -> Vec<String> {
        let frame = last_frame(out);
        let mut plain = Vec::new();
        let mut i = 0;
        while i < frame.len() {
            if frame[i] == 0x1b && frame.get(i + 1) == Some(&b'[') {
                i += 2;
                while i < frame.len() && !(0x40..=0x7e).contains(&frame[i]) {
                    i += 1;
                }
                i += 1;
                continue;
            }
            plain.push(frame[i]);
            i += 1;
        }
        String::from_utf8_lossy(&plain)
            .split("\r\n")
            .map(|row| row.replace('\r', ""))
            .collect()
    }

    fn rows(first: usize, last: usize) -> Vec<String> {
        (first..=last).map(|n| format!("line{n}")).collect()
    }

    fn page(text: &[String], prompt: &str) -> Vec<String> {
        let mut all = text.to_vec();
        all.push(prompt.to_string());
        all
    }

    fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
        haystack.windows(needle.len()).any(|w| w == needle)
    }

    #[test]
    fn the_first_page_shows_the_top_and_the_name() {
        assert_eq!(session(&numbered(100), b""), page(&rows(1, 5), "t"));
    }

    /// The point of the change: back as well as forward.
    #[test]
    fn space_pages_forward_and_b_pages_back() {
        assert_eq!(session(&numbered(100), b" "), page(&rows(6, 10), ":"));
        assert_eq!(session(&numbered(100), b"  b"), page(&rows(6, 10), ":"));
        assert_eq!(session(&numbered(100), b" b"), page(&rows(1, 5), "t"));
        assert_eq!(
            session(&numbered(100), b"bbb"),
            page(&rows(1, 5), "t"),
            "back from the top stays at the top"
        );
    }

    #[test]
    fn j_and_k_move_one_row() {
        assert_eq!(session(&numbered(100), b"j"), page(&rows(2, 6), ":"));
        assert_eq!(session(&numbered(100), b"jjk"), page(&rows(2, 6), ":"));
        assert_eq!(session(&numbered(100), b"\r\n"), page(&rows(3, 7), ":"));
        assert_eq!(session(&numbered(100), b"d"), page(&rows(3, 7), ":"));
        assert_eq!(session(&numbered(100), b"ddu"), page(&rows(3, 7), ":"));
    }

    #[test]
    fn a_count_multiplies_the_move() {
        assert_eq!(session(&numbered(100), b"10j"), page(&rows(11, 15), ":"));
        assert_eq!(session(&numbered(100), b"10j3k"), page(&rows(8, 12), ":"));
        assert_eq!(session(&numbered(100), b"50g"), page(&rows(50, 54), ":"));
        assert_eq!(session(&numbered(100), b"50G"), page(&rows(50, 54), ":"));
    }

    #[test]
    fn the_ends_are_one_key_away() {
        let text = numbered(100);
        assert_eq!(session(&text, b"G"), page(&rows(96, 100), "(END)"));
        assert_eq!(session(&text, b"Gg"), page(&rows(1, 5), "t"));
        assert_eq!(session(&text, b"G<"), page(&rows(1, 5), "t"));
        assert_eq!(session(&text, b">"), page(&rows(96, 100), "(END)"));
        assert_eq!(
            session(&text, b"200g"),
            page(&rows(96, 100), "(END)"),
            "a line past the end is the last page"
        );
        assert_eq!(
            session(&text, b"98g"),
            page(&rows(96, 100), "(END)"),
            "a line on the last page keeps the page full"
        );
    }

    /// `+COMMAND` runs before the first key, as if typed.
    #[test]
    fn a_plus_command_runs_first() {
        let text = numbered(100);
        let docs: &[(&str, &[u8])] = &[("t", &text)];
        let opened = |ahead: &[u8], raw: bool| {
            let (out, _) = run_full(&["t"], docs, ahead, b"", SMALL, raw, Options::default());
            last_screen(&out)
        };
        assert_eq!(opened(b"G", true), page(&rows(96, 100), "(END)"));
        assert_eq!(opened(b"/line42", true), page(&rows(42, 46), ":"));
        assert_eq!(opened(b"/line42", false), page(&rows(42, 46), ":"));
        assert_eq!(opened(b"30g", true), page(&rows(30, 34), ":"));
    }

    /// Paging forward stops with the last line on the bottom row, and pressing on
    /// does nothing — it does not quit, as the old pager did.
    #[test]
    fn forward_stops_at_the_end_without_quitting() {
        assert_eq!(session(&numbered(7), b" "), page(&rows(3, 7), "(END)"));
        assert_eq!(session(&numbered(7), b"    jj"), page(&rows(3, 7), "(END)"));
        assert_eq!(session(&numbered(7), b"    b"), page(&rows(1, 5), "t"));
        assert_eq!(
            session(&numbered(5), b""),
            page(&rows(1, 5), "(END)"),
            "a page boundary at the end is the end"
        );
    }

    #[test]
    fn a_short_input_is_padded_with_tildes() {
        let mut want = rows(1, 2);
        want.extend(["~", "~", "~"].map(String::from));
        assert_eq!(session(&numbered(2), b""), page(&want, "(END)"));
        assert_eq!(session(&numbered(2), b" jjbk"), page(&want, "(END)"));
        let empty = session(b"", b"");
        assert_eq!(empty, page(&["~"; 5].map(String::from), "(END)"));
    }

    #[test]
    fn an_unterminated_last_line_is_not_lost() {
        let want = ["a", "b", "c", "~", "~"].map(String::from);
        assert_eq!(session(b"a\nb\nc", b""), page(&want, "(END)"));
        assert_eq!(
            session(b"a\r\nb\r\n", b"")[..2],
            ["a", "b"],
            "CRLF shows no CR"
        );
        assert_eq!(
            session(b"a\nlast\r", b"")[..2],
            ["a", "last^M"],
            "a CR with no newline after it is not a line end"
        );
        let f = Options {
            quit_if_one_screen: true,
            ..Options::default()
        };
        let (out, _) = run_session(&["t"], &[("t", b"a\nlast\r")], b"", SMALL, true, f);
        assert_eq!(out, b"a\nlast^M", "nor is it sent raw by -F");
    }

    #[test]
    fn search_moves_the_matching_line_to_the_top() {
        let text = numbered(100);
        assert_eq!(session(&text, b"/line42\r"), page(&rows(42, 46), ":"));
        assert_eq!(session(&text, b"/line4\r"), page(&rows(4, 8), ":"));
        assert_eq!(session(&text, b"/line4\rn"), page(&rows(40, 44), ":"));
        assert_eq!(session(&text, b"/line4\rnN"), page(&rows(4, 8), ":"));
        assert_eq!(session(&text, b"/line4\r3n"), page(&rows(42, 46), ":"));
        assert_eq!(session(&text, b"G?line50\r"), page(&rows(50, 54), ":"));
        // `n` repeats in the search's own direction, `N` against it.
        assert_eq!(session(&text, b"G?line5\r"), page(&rows(59, 63), ":"));
        assert_eq!(session(&text, b"G?line5\rn"), page(&rows(58, 62), ":"));
        assert_eq!(session(&text, b"G?line5\rnN"), page(&rows(59, 63), ":"));
        assert_eq!(
            session(&text, b"/line9\n"),
            page(&rows(9, 13), ":"),
            "a newline ends the pattern as Enter does"
        );
        assert_eq!(
            session(&text, b"/line42\rg/\r"),
            page(&rows(42, 46), ":"),
            "an empty pattern repeats the last"
        );
    }

    /// A new search takes the line it starts on; `n` goes on from the match.
    #[test]
    fn a_new_search_includes_the_line_it_starts_on() {
        let mut text = b"needle\n".to_vec();
        text.extend_from_slice(&numbered(20));
        let s = session(&text, b"/needle\r");
        assert_eq!(s[0], "needle");
        assert_eq!(s[5], "t", "the match on the top line is found");
        assert_eq!(session(&text, b"/needle\rn")[5], "Pattern not found");
        // `?` starts from the bottom line.
        assert_eq!(session(&numbered(100), b"?line5\r"), page(&rows(5, 9), ":"));
    }

    /// A match the page had to be pulled up past is not found again by `n`.
    #[test]
    fn n_goes_on_from_the_match_not_the_top() {
        let text = numbered(100);
        let s = session(&text, b"/line9\rnnnnnnnnnn");
        assert_eq!(s, page(&rows(96, 100), "(END)"), "the tenth n is line99");
        assert_eq!(
            session(&text, b"/line9\rnnnnnnnnnnn")[5],
            "Pattern not found",
            "and nothing after it"
        );
        assert_eq!(
            session(&text, b"/line9\rnnnnnnnnnnN"),
            page(&rows(96, 100), "(END)"),
            "N goes back from line99 to line98, which is on the last page"
        );
    }

    /// Colour sequences are not text: a search neither stops inside one nor is
    /// stopped by one splitting a word.
    #[test]
    fn search_reads_through_colour() {
        let text: &[u8] = b"\x1b[31mred\x1b[m\nplain 31\nfi\x1b[1mne\x1b[m\nx\ny\nz\nw\n";
        assert_eq!(session(text, b"/31\r")[0], "plain 31");
        assert_eq!(session(text, b"/m\r")[5], "Pattern not found");
        assert_eq!(session(text, b"/fine\r")[0], "fine");
        let (out, _) = run_session(
            &["t"],
            &[("t", text)],
            b"/red\r",
            SMALL,
            true,
            Options::default(),
        );
        assert!(
            contains_bytes(last_frame(&out), b"\x1b[31m\x1b[7mred\x1b[27m\x1b[m"),
            "{:?}",
            String::from_utf8_lossy(last_frame(&out))
        );
    }

    #[test]
    fn a_failed_search_says_so_and_stays() {
        let text = numbered(100);
        assert_eq!(
            session(&text, b"jj/nope\r"),
            page(&rows(3, 7), "Pattern not found")
        );
        assert_eq!(
            session(&text, b"n"),
            page(&rows(1, 5), "No previous search pattern")
        );
        assert_eq!(
            session(&text, b"/line9\x1bj"),
            page(&rows(2, 6), ":"),
            "ESC abandons the pattern"
        );
        assert_eq!(
            session(&text, b"/x\x7f\x7fj"),
            page(&rows(2, 6), ":"),
            "erasing past the start abandons it too"
        );
        assert_eq!(
            session(&text, b"/line\x1b[Hj"),
            page(&rows(2, 6), ":"),
            "a key's escape sequence after ESC is not read as commands"
        );
        assert_eq!(
            session(&text, "/é\x7f\x7fj".as_bytes()),
            page(&rows(2, 6), ":"),
            "erasing takes a whole character"
        );
    }

    #[test]
    fn case_folding_follows_the_options() {
        let text: &[u8] = b"alpha\nBETA\ngamma\nbeta\nx\ny\nz\nw\nv\n";
        let top = |options: Options, keys: &[u8]| {
            let (out, _) = run_session(&["t"], &[("t", text)], keys, SMALL, true, options);
            last_screen(&out)[0].clone()
        };
        assert_eq!(top(Options::default(), b"/beta\r"), "beta");
        let smart = Options {
            smart_case: true,
            ..Options::default()
        };
        assert_eq!(
            top(smart, b"/beta\r"),
            "BETA",
            "-i folds a lower-case pattern"
        );
        assert_eq!(
            top(smart, b"/Beta\r"),
            "alpha",
            "...but not one with capitals"
        );
        let always = Options {
            ignore_case: true,
            ..Options::default()
        };
        assert_eq!(top(always, b"/Beta\r"), "BETA");
    }

    #[test]
    fn matches_are_shown_in_reverse_video() {
        let (out, _) = run_session(
            &["t"],
            &[("t", b"one two one\n")],
            b"/one\r",
            SMALL,
            true,
            Options::default(),
        );
        let frame = last_frame(&out);
        let row = b"\x1b[7mone\x1b[27m two \x1b[7mone\x1b[m\x1b[K";
        assert!(
            contains_bytes(frame, row),
            "{:?}",
            String::from_utf8_lossy(frame)
        );
        // The file's own reset inside a match does not end the highlight.
        let mut frame = Vec::new();
        render_row(&mut frame, b"ab\x1b[mcd", 0, 7, 80, &[(0, 7)]).unwrap();
        assert_eq!(frame, b"\x1b[7mab\x1b[m\x1b[7mcd\x1b[m\x1b[K\r\n");
    }

    #[test]
    fn arrows_and_paging_keys_work() {
        let text = numbered(100);
        assert_eq!(
            session(&text, b"\x1b[B\x1b[B\x1b[A"),
            page(&rows(2, 6), ":")
        );
        assert_eq!(session(&text, b"\x1bOB"), page(&rows(2, 6), ":"));
        assert_eq!(session(&text, b"\x1b[6~"), page(&rows(6, 10), ":"));
        assert_eq!(session(&text, b"\x1b[6~\x1b[5~"), page(&rows(1, 5), "t"));
        assert_eq!(session(&text, b"\x1b \x1bv"), page(&rows(1, 5), "t"));
        assert_eq!(session(&text, b"\x1b[F"), page(&rows(96, 100), "(END)"));
        assert_eq!(session(&text, b"\x1b[4~\x1b[1~"), page(&rows(1, 5), "t"));
        assert_eq!(
            session(&text, b"\x1b[1;5Cj"),
            page(&rows(2, 6), ":"),
            "an unknown sequence is consumed whole, not read as keys"
        );
    }

    /// q quits, and so do ZZ, :q and a closed command source; the page stays on
    /// the screen and only the prompt is cleared.
    #[test]
    fn quitting_leaves_the_page_and_clears_the_prompt() {
        let text = numbered(100);
        for keys in [&b"q"[..], b"Q", b"ZZ", b":q", b""] {
            let (out, status) = run_session(
                &["t"],
                &[("t", &text)],
                keys,
                SMALL,
                true,
                Options::default(),
            );
            assert_eq!(status, 0);
            assert!(out.ends_with(b"\r\x1b[K\x1b[?7h"), "{keys:?}");
        }
        assert_eq!(
            session(&text, b"qj"),
            page(&rows(1, 5), "t"),
            "nothing after q is read"
        );
    }

    #[test]
    fn help_lists_the_keys() {
        let screen = session(&numbered(100), b"h");
        assert!(screen[5].starts_with("q quit"), "{screen:?}");
        assert_eq!(session(&numbered(100), b"hj"), page(&rows(2, 6), ":"));
    }

    /// A long line wraps, and a row is the unit every move counts in.
    #[test]
    fn a_wrapped_line_is_scrolled_by_rows() {
        let mut text = vec![b'x'; 200];
        text.push(b'\n');
        text.extend_from_slice(&numbered(10));
        let x = |n: usize| "x".repeat(n);
        let top = session(&text, b"");
        assert_eq!(top[..4], [x(80), x(80), x(40), "line1".to_string()]);
        let one = session(&text, b"j");
        assert_eq!(one[..3], [x(80), x(40), "line1".to_string()]);
        let back = session(&text, b"jjjk");
        assert_eq!(back[..2], [x(40), "line1".to_string()]);
        let up = session(&text, b"jjjkkk");
        assert_eq!(up[..1], [x(80)]);
        assert_eq!(up[5], "t");
        let end = session(&text, b"Gbb");
        assert_eq!(end[..1], [x(80)], "back from the end reaches the top");
    }

    /// Control bytes are shown, not sent; colour changes are sent through.
    #[test]
    fn control_bytes_become_carets_and_colours_pass() {
        let (out, _) = run_session(
            &["t"],
            &[("t", b"a\x1b[31mred\x1b[m\x07bell\x1b]0;title\x07\n")],
            b"",
            SMALL,
            true,
            Options::default(),
        );
        let frame = last_frame(&out);
        let row = b"a\x1b[31mred\x1b[m^Gbell^[]0;title^G\x1b[m\x1b[K";
        assert!(
            contains_bytes(frame, row),
            "{:?}",
            String::from_utf8_lossy(frame)
        );
        assert!(!frame.contains(&0x07), "a bell reached the terminal");
        assert!(
            !contains_bytes(frame, b"\x1b]"),
            "an OSC sequence reached the terminal"
        );
    }

    /// A C1 control is a control too: U+009B is CSI to a UTF-8 terminal, and a
    /// raw 0x9B is CSI to an 8-bit one. Neither is sent; nor is any byte that is
    /// not well-formed UTF-8. Real characters are.
    #[test]
    fn c1_controls_and_stray_bytes_are_shown_in_hex() {
        let text = "a\u{9b}2Jb é ✓ \u{1F600}\n".as_bytes().to_vec();
        let mut text = text;
        text.extend_from_slice(b"\x9b2J \xff\xc0\xaf \xed\xa0\x80\n");
        let (out, _) = run_session(
            &["t"],
            &[("t", &text)],
            b"",
            SMALL,
            true,
            Options::default(),
        );
        let s = last_screen(&out);
        assert_eq!(s[0], "a<C2><9B>2Jb é ✓ \u{1F600}");
        assert_eq!(s[1], "<9B>2J <FF><C0><AF> <ED><A0><80>");
        let frame = last_frame(&out);
        assert!(!frame.contains(&0x9b), "a C1 byte reached the terminal");
        let mut at = Vec::new();
        breaks(b"\xff\xff", 8, &mut at).unwrap();
        assert_eq!(
            at,
            [0],
            "each stray byte takes the four columns it is shown in"
        );
        breaks(b"\xff\xff\xff", 8, &mut at).unwrap();
        assert_eq!(at, [0, 2]);
    }

    /// A row that starts in the middle of a coloured line is drawn in its colour.
    #[test]
    fn a_wrapped_row_keeps_its_colour() {
        let mut text = b"\x1b[32m".to_vec();
        text.extend_from_slice(&[b'g'; 100]);
        text.extend_from_slice(b"\x1b[m\n");
        text.extend_from_slice(&numbered(10));
        let (out, _) = run_session(
            &["t"],
            &[("t", &text)],
            b"j",
            SMALL,
            true,
            Options::default(),
        );
        let frame = last_frame(&out);
        assert!(
            frame.starts_with(b"\x1b[H\x1b[32mgggg"),
            "{:?}",
            String::from_utf8_lossy(frame)
        );
    }

    #[test]
    fn a_line_wraps_by_the_columns_it_really_takes() {
        let mut at = Vec::new();
        let mut cut = |text: &[u8], cols: usize| {
            breaks(text, cols, &mut at).unwrap();
            at.clone()
        };
        assert_eq!(cut(b"", 80), [0], "an empty line still takes a row");
        assert_eq!(cut(&[b'x'; 80], 80), [0], "exactly the width is one row");
        assert_eq!(cut(&[b'x'; 81], 80), [0, 80]);
        assert_eq!(cut(&[b'x'; 240], 80), [0, 80, 160]);
        // A tab runs to the next stop of eight.
        assert_eq!(cut(b"\t\t", 8), [0, 1]);
        assert_eq!(cut(b"a\tb", 8), [0, 2]);
        assert_eq!(cut(b"abc\tde", 10), [0], "abc, a tab to 8, then two");
        assert_eq!(cut(b"abc\tde", 9), [0, 5]);
        // UTF-8 costs a column per character, not per byte.
        assert_eq!(cut("éééé".as_bytes(), 4), [0]);
        assert_eq!(cut("ééééé".as_bytes(), 4), [0, 8]);
        // A caret takes two; a colour change none.
        assert_eq!(cut(b"\x01\x01", 3), [0, 1]);
        assert_eq!(cut(b"ab\x1b[1;31mcd", 4), [0]);
        // A one-column screen still makes progress.
        assert_eq!(cut(b"\x01\x01", 1), [0, 1]);
    }

    #[test]
    fn colour_sequences_are_recognised_and_bounded() {
        assert_eq!(sgr_len(b"\x1b[m", 0), Some(3));
        assert_eq!(sgr_len(b"\x1b[1;38:5:208m", 0), Some(13));
        assert_eq!(sgr_len(b"\x1b[2J", 0), None, "only colour passes");
        assert_eq!(sgr_len(b"\x1b[", 0), None);
        let mut long = b"\x1b[".to_vec();
        long.extend_from_slice(&[b'1'; 100]);
        long.push(b'm');
        assert_eq!(sgr_len(&long, 0), None);
    }

    #[test]
    fn utf8_is_decoded_strictly() {
        assert_eq!(utf8("é".as_bytes(), 0), Some((2, 0xe9)));
        assert_eq!(utf8("✓".as_bytes(), 0), Some((3, 0x2713)));
        assert_eq!(utf8("\u{10FFFF}".as_bytes(), 0), Some((4, 0x10ffff)));
        assert_eq!(utf8(b"\xc0\xaf", 0), None, "overlong");
        assert_eq!(utf8(b"\xe0\x80\xaf", 0), None, "overlong");
        assert_eq!(utf8(b"\xed\xa0\x80", 0), None, "a surrogate");
        assert_eq!(utf8(b"\xf4\x90\x80\x80", 0), None, "past U+10FFFF");
        assert_eq!(utf8(b"\xe2\x9c", 0), None, "cut short");
        assert_eq!(utf8(b"\x9c", 0), None, "a continuation alone");
    }

    #[test]
    fn quit_if_one_screen_prints_short_input_as_a_page_would_show_it() {
        let f = Options {
            quit_if_one_screen: true,
            ..Options::default()
        };
        let short: &[u8] = b"one\n\x1b[31mtwo\x1b[m\n";
        let (out, status) = run_session(&["t"], &[("t", short)], b"", SMALL, true, f);
        assert_eq!(status, 0);
        assert_eq!(out, short, "no page, no prompt, colours through");
        let (out, _) = run_session(&["t"], &[("t", &numbered(5))], b"", SMALL, true, f);
        assert_eq!(out, numbered(5), "exactly a page fits");
        let hostile: &[u8] = b"x\x07\x1b]0;t\x07\x1b[2J\xc2\x9b\r\nend";
        let (out, _) = run_session(&["t"], &[("t", hostile)], b"", SMALL, true, f);
        assert_eq!(out, b"x^G^[]0;t^G^[[2J<C2><9B>\r\nend", "nothing sent raw");
        let (out, _) = run_session(&["t"], &[("t", &numbered(6))], b"", SMALL, true, f);
        assert_eq!(
            last_screen(&out),
            page(&rows(1, 5), "t"),
            "a page and a line does not"
        );
    }

    #[test]
    fn options_are_refused_unless_honoured() {
        let args = |list: &[&str]| list.iter().map(|a| a.to_string()).collect::<Vec<_>>();
        let a = args(&[]);
        let parsed = parse(&a, None).unwrap();
        assert_eq!(
            (parsed.options, parsed.files, parsed.start),
            (Options::default(), vec!["-"], None)
        );
        let a = args(&["-FRX", "-i", "+G", "f"]);
        let parsed = parse(&a, None).unwrap();
        let options = parsed.options;
        assert!(options.quit_if_one_screen && options.smart_case && !options.ignore_case);
        assert_eq!(parsed.files, ["f"]);
        assert_eq!(parsed.start, Some("G"));
        for bad in ["-N", "--squeeze", "-S", "-r", "-FN"] {
            let a = args(&[bad]);
            assert_eq!(parse(&a, None), Err(Refused::Option(bad)), "{bad}");
        }
        let a = args(&["--", "-N", "+G"]);
        assert_eq!(
            parse(&a, None).unwrap().files,
            ["-N", "+G"],
            "`--` ends the options"
        );
        let a = args(&["--"]);
        assert_eq!(parse(&a, None).unwrap().files, ["-"]);
        let a = args(&["-"]);
        assert_eq!(parse(&a, None).unwrap().files, ["-"], "`-` is stdin");
    }

    #[test]
    fn the_less_variable_is_read_leniently() {
        let read = |value: &str| {
            let mut o = Options::default();
            o.absorb(value);
            o
        };
        assert!(
            read("FRX").quit_if_one_screen,
            "git's default, with no dash"
        );
        assert!(read("-R -F").quit_if_one_screen);
        assert!(
            read("-SRF").quit_if_one_screen,
            "an unknown letter is skipped"
        );
        assert!(read("-NF").quit_if_one_screen);
        assert_eq!(
            read("-Ps File"),
            Options::default(),
            "a prompt string is not -F"
        );
        assert_eq!(read("-x4F"), Options::default(), "nor a tab stop");
        assert!(read("--mouse -i").smart_case);
        assert_eq!(read("  "), Options::default());
        let a: Vec<String> = Vec::new();
        assert!(
            parse(&a, Some("-N -F")).unwrap().options.quit_if_one_screen,
            "never refused"
        );
    }

    #[test]
    fn several_files_are_stepped_through() {
        let a: &[u8] = b"a1\na2\n";
        let b: &[u8] = b"b1\n";
        let docs = [("a", a), ("b", b)];
        let screen = |keys: &[u8]| {
            let (out, status) =
                run_session(&["a", "b"], &docs, keys, SMALL, true, Options::default());
            (last_screen(&out), status)
        };
        let (s, status) = screen(b"");
        assert_eq!(status, 0);
        assert_eq!(s[..2], ["a1", "a2"]);
        assert_eq!(s[5], "(END) - Next: b");
        assert_eq!(screen(b":n").0[..2], ["b1", "~"]);
        assert_eq!(screen(b":n").0[5], "(END)");
        assert_eq!(screen(b":n:p").0[..2], ["a1", "a2"]);
        assert_eq!(screen(b":n:n").0[5], "No next file");
        assert_eq!(screen(b":p").0[5], "No previous file");
        assert_eq!(screen(b":n:x").0[0], "a1");
    }

    /// An operand that will not open is named where the reader will see it — on
    /// the first page's prompt row, since the page clears the screen stderr wrote
    /// to — and passed over.
    #[test]
    fn an_operand_that_will_not_open_is_reported_and_passed_over() {
        let a: &[u8] = b"a1\n";
        let (out, status) = run_session(
            &["gone", "a"],
            &[("a", a)],
            b"",
            SMALL,
            true,
            Options::default(),
        );
        assert_eq!(status, 1);
        let s = last_screen(&out);
        assert_eq!(s[0], "a1");
        assert!(s[5].starts_with("gone: "), "{s:?}");
        let (out, status) = run_session(
            &["a", "gone"],
            &[("a", a)],
            b":n",
            SMALL,
            true,
            Options::default(),
        );
        assert_eq!(status, 1);
        let s = last_screen(&out);
        assert_eq!(s[0], "a1", "it stays on the file it could show");
        assert!(s[5].starts_with("gone: "), "{s:?}");
        let (out, status) = run_session(&["gone"], &[], b"", SMALL, true, Options::default());
        assert_eq!((out.len(), status), (0, 1), "nothing to page draws nothing");
        // A `+COMMAND`'s own page must not take the message away unread.
        let docs: &[(&str, &[u8])] = &[("a", a)];
        let (out, _) = run_full(
            &["gone", "a"],
            docs,
            b"G",
            b"",
            SMALL,
            true,
            Options::default(),
        );
        let s = last_screen(&out);
        assert!(s[5].starts_with("gone: "), "{s:?}");
    }

    /// An operand that fails while being read is named, passed over, and the
    /// session goes on with the others.
    #[test]
    fn an_operand_that_fails_to_read_is_passed_over() {
        let a: &[u8] = b"a1\n";
        let docs: &[(&str, &[u8])] = &[("broken", b"x\n"), ("a", a)];
        let (out, status) =
            run_session(&["broken", "a"], docs, b"", SMALL, true, Options::default());
        assert_eq!(status, 1);
        let s = last_screen(&out);
        assert_eq!(s[0], "a1");
        assert_eq!(s[5], "broken: bad sector");
        let (out, status) = run_session(
            &["a", "broken"],
            docs,
            b":n",
            SMALL,
            true,
            Options::default(),
        );
        assert_eq!(status, 1);
        let s = last_screen(&out);
        assert_eq!((s[0].as_str(), s[5].as_str()), ("a1", "broken: bad sector"));
        let (out, status) = run_session(&["broken"], docs, b"", SMALL, true, Options::default());
        assert_eq!(status, 1, "with nothing left it ends, failed");
        assert!(
            out.ends_with(b"\r\x1b[K\x1b[?7h"),
            "and the terminal is restored"
        );
    }

    /// Without raw mode a command is a line: Enter alone is one line forward.
    #[test]
    fn line_mode_takes_a_command_per_line() {
        let text = numbered(100);
        let lines = |keys: &[u8]| {
            let (out, _) = run_session(
                &["t"],
                &[("t", &text)],
                keys,
                SMALL,
                false,
                Options::default(),
            );
            last_screen(&out)
        };
        assert_eq!(lines(b"j\n"), page(&rows(2, 6), ":"));
        assert_eq!(lines(b"\n\n"), page(&rows(3, 7), ":"));
        assert_eq!(lines(b" \nb\n"), page(&rows(1, 5), "t"));
        assert_eq!(lines(b"/line9\n"), page(&rows(9, 13), ":"));
        assert_eq!(lines(b"10j\n"), page(&rows(11, 15), ":"));
        assert_eq!(lines(b"q\nj\n"), page(&rows(1, 5), "t"));
        let (out, _) = run_session(
            &["t"],
            &[("t", &text)],
            b"j\n",
            SMALL,
            false,
            Options::default(),
        );
        let pages = out.windows(7).filter(|w| w == b"\x1b[H\x1b[2J").count();
        assert_eq!(
            pages, 2,
            "every page clears, since each Enter scrolled the screen"
        );
    }

    /// A shared sink, so a reader can see what had been drawn when it was asked.
    struct Shared(Rc<RefCell<Vec<u8>>>);

    impl Write for Shared {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.borrow_mut().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// A pipe that delivers its first chunk, and records what the terminal showed
    /// at the moment the pager waited for the second.
    struct Slow {
        chunks: Vec<Vec<u8>>,
        next: usize,
        drawn: Rc<RefCell<Vec<u8>>>,
        seen: Rc<RefCell<Option<Vec<u8>>>>,
    }

    impl Read for Slow {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.next == 1 && self.seen.borrow().is_none() {
                *self.seen.borrow_mut() = Some(self.drawn.borrow().clone());
            }
            let Some(chunk) = self.chunks.get(self.next) else {
                return Ok(0);
            };
            self.next += 1;
            buf[..chunk.len()].copy_from_slice(chunk);
            Ok(chunk.len())
        }
    }

    /// The rows a pipe has sent are on the screen before the pager waits for
    /// more, rather than held back until a page is complete.
    #[test]
    fn a_slow_pipe_shows_what_has_arrived() {
        let drawn = Rc::new(RefCell::new(Vec::new()));
        let seen = Rc::new(RefCell::new(None));
        let slow = Slow {
            chunks: vec![b"line1\nline2\n".to_vec(), numbered(20)],
            next: 0,
            drawn: drawn.clone(),
            seen: seen.clone(),
        };
        let mut slow = Some(slow);
        let mut open = |_: &str| -> io::Result<Store<Slow>> {
            Ok(Store::spool(slow.take().unwrap(), SPOOL_CAP))
        };
        let mut sink = Shared(drawn.clone());
        let mut measure = || SMALL;
        let terminal = Terminal {
            out: &mut sink,
            commands: Cursor::new(b"q".to_vec()),
            raw: true,
            measure: &mut measure,
            typed_ahead: b"",
        };
        page_files(&["-"], &mut open, terminal, Options::default()).unwrap();
        let seen = seen.borrow().clone().expect("the pager never waited");
        assert!(
            contains_bytes(&seen, b"line1"),
            "{:?}",
            String::from_utf8_lossy(&seen)
        );
        assert!(contains_bytes(&seen, b"line2"));
        assert!(
            seen.ends_with(b"\x1b[?7h"),
            "autowrap must be back on while the pager waits, where Ctrl-C may end it"
        );
        assert_eq!(
            last_screen(&drawn.borrow())[..3],
            ["line1", "line2", "line1"],
            "and the page is finished once the rest arrives"
        );
    }

    fn top_line<R: Read>(view: &mut View<R>) -> Vec<u8> {
        let mut line = Vec::new();
        line_at(&mut view.store, view.top.line, &mut line).unwrap();
        line
    }

    #[test]
    fn a_regular_file_is_read_in_place() {
        let dir = td_fs::TempDir::new("td-less-seek").unwrap();
        let path = dir.path().join("f");
        std::fs::write(&path, numbered(5000)).unwrap();
        let store = Store::open(path.to_str().unwrap()).unwrap();
        assert!(matches!(store.source, Source::Seek(_)));
        let mut view = View::new(store);
        let mut work = Work::default();
        to_end(&mut view, &mut work, SMALL).unwrap();
        assert_eq!(top_line(&mut view), b"line4996\n");
        backward(&mut view, &mut work, 4995, SMALL).unwrap();
        assert_eq!(view.top, Pos { line: 0, row: 0 });
        forward(&mut view, &mut work, 10_000, SMALL).unwrap();
        assert_eq!(
            top_line(&mut view),
            b"line4996\n",
            "forward stops a page from the end"
        );
        assert!(Store::open(dir.path().join("none").to_str().unwrap()).is_err());
        std::fs::write(dir.path().join("empty"), b"").unwrap();
        let empty = Store::open(dir.path().join("empty").to_str().unwrap()).unwrap();
        assert!(
            matches!(empty.source, Source::Spool(_)),
            "an empty file reports no size worth trusting"
        );
        let dir_err = Store::open(dir.path().to_str().unwrap()).err().unwrap();
        assert_eq!(dir_err.kind(), io::ErrorKind::IsADirectory);
        dir.cleanup().unwrap();
    }

    /// A file that grew since it was opened is followed to its new end, and one
    /// truncated under the page (a log rotated by copy and truncate) is too.
    #[test]
    fn the_end_of_a_changed_file_is_followed() {
        let dir = td_fs::TempDir::new("td-less-grow").unwrap();
        let path = dir.path().join("f");
        std::fs::write(&path, numbered(10)).unwrap();
        let mut view = View::new(Store::open(path.to_str().unwrap()).unwrap());
        let mut work = Work::default();
        to_end(&mut view, &mut work, SMALL).unwrap();
        assert_eq!(top_line(&mut view), b"line6\n");
        std::fs::write(&path, numbered(20_000)).unwrap();
        to_end(&mut view, &mut work, SMALL).unwrap();
        assert_eq!(top_line(&mut view), b"line19996\n");
        std::fs::write(&path, numbered(20)).unwrap();
        settle(&mut view, &mut work, SMALL).unwrap();
        assert_eq!(
            top_line(&mut view),
            b"line16\n",
            "the page moves back onto the file"
        );
        to_end(&mut view, &mut work, SMALL).unwrap();
        assert_eq!(top_line(&mut view), b"line16\n");
        std::fs::write(&path, b"").unwrap();
        settle(&mut view, &mut work, SMALL).unwrap();
        assert_eq!(view.top, Pos { line: 0, row: 0 });
        dir.cleanup().unwrap();
    }

    /// A line longer than `MAX_LINE` is cut into pieces from its start, and
    /// walking back finds the same pieces walking forward did.
    #[test]
    fn an_endless_line_is_cut_the_same_both_ways() {
        let mut text = vec![b'x'; MAX_LINE * 2 + 10];
        text.extend_from_slice(b"\nend\n");
        let mut store = spool(&text);
        let mut buf = Vec::new();
        let mut starts = vec![0u64];
        loop {
            let at = *starts.last().unwrap();
            let next = line_at(&mut store, at, &mut buf).unwrap();
            assert!(buf.len() <= MAX_LINE + 1, "the cap must bound the buffer");
            if next == at {
                break;
            }
            starts.push(next);
        }
        let m = MAX_LINE as u64;
        assert_eq!(starts, [0, m, 2 * m, 2 * m + 11, 2 * m + 15]);
        for pair in starts.windows(2) {
            assert_eq!(prev_start(&mut store, pair[1]).unwrap(), Some(pair[0]));
        }
        // ...and walking back the whole way, which reuses what the first scan
        // learned about the long line.
        for pair in starts.windows(2).rev() {
            assert_eq!(prev_start(&mut store, pair[1]).unwrap(), Some(pair[0]));
        }
        assert_eq!(prev_start(&mut store, 0).unwrap(), None);
    }

    /// A line of exactly `MAX_LINE` bytes keeps its newline, rather than ending in
    /// an empty piece of its own; so does one of twice that.
    #[test]
    fn a_line_of_exactly_max_line_has_no_empty_tail() {
        let m = MAX_LINE as u64;
        let mut buf = Vec::new();
        let mut exact = vec![b'y'; MAX_LINE];
        exact.extend_from_slice(b"\nz\n");
        let mut store = spool(&exact);
        assert_eq!(line_at(&mut store, 0, &mut buf).unwrap(), m + 1);
        assert_eq!(line_at(&mut store, m + 1, &mut buf).unwrap(), m + 3);
        assert_eq!(prev_start(&mut store, m + 3).unwrap(), Some(m + 1));
        assert_eq!(prev_start(&mut store, m + 1).unwrap(), Some(0));
        let mut double = vec![b'y'; MAX_LINE * 2];
        double.push(b'\n');
        let mut store = spool(&double);
        assert_eq!(line_at(&mut store, 0, &mut buf).unwrap(), m);
        assert_eq!(line_at(&mut store, m, &mut buf).unwrap(), 2 * m + 1);
        assert_eq!(prev_start(&mut store, 2 * m + 1).unwrap(), Some(m));
        assert_eq!(prev_start(&mut store, m).unwrap(), Some(0));
    }

    /// Walking back across many read blocks finds each line start.
    #[test]
    fn walking_back_crosses_read_blocks() {
        let mut text = Vec::new();
        for n in 0..20u8 {
            text.extend_from_slice(&vec![b'a' + n; SCAN_BLOCK + 7]);
            text.push(b'\n');
        }
        let mut store = spool(&text);
        let mut at = text.len() as u64;
        let mut seen = 0;
        while let Some(prev) = prev_start(&mut store, at).unwrap() {
            assert_eq!(prev % (SCAN_BLOCK as u64 + 8), 0);
            at = prev;
            seen += 1;
        }
        assert_eq!(seen, 20);
    }

    /// A pipe is kept up to its cap; past that the oldest input goes, the page
    /// cannot be moved above what is kept, and the prompt says why.
    #[test]
    fn a_pipe_is_kept_only_up_to_its_cap() {
        // The cap is raised to its floor, which 200,000 lines are well past.
        let mut view = View::new(Store::spool(fake(&numbered(200_000), false), 1));
        let mut work = Work::default();
        to_end(&mut view, &mut work, SMALL).unwrap();
        assert_eq!(top_line(&mut view), b"line199996\n");
        let Source::Spool(s) = &view.store.source else {
            panic!("not spooled")
        };
        assert_eq!(s.cap, MIN_SPOOL);
        assert!(s.held.len() <= MIN_SPOOL, "held {}", s.held.len());
        let kept = view.store.first();
        assert!(kept > 0);
        to_line(&mut view, &mut work, 1, SMALL).unwrap();
        assert_eq!(view.top.line, kept, "the top of what is kept");
        backward(&mut view, &mut work, 100, SMALL).unwrap();
        assert_eq!(view.top.line, kept);
        // A top that was dropped from under the page is moved onto what is kept.
        view.top = Pos { line: 0, row: 3 };
        settle(&mut view, &mut work, SMALL).unwrap();
        assert_eq!(view.top, Pos { line: kept, row: 0 });
        // ...and the prompt row says the input above is gone.
        let text = numbered(200_000);
        let mut doc = Some(fake(&text, false));
        let mut open =
            |_: &str| -> io::Result<Store<Fake>> { Ok(Store::spool(doc.take().unwrap(), 1)) };
        let mut out = Vec::new();
        let mut measure = || SMALL;
        let terminal = Terminal {
            out: &mut out,
            commands: Cursor::new(b"Gg".to_vec()),
            raw: true,
            measure: &mut measure,
            typed_ahead: b"",
        };
        page_files(&["-"], &mut open, terminal, Options::default()).unwrap();
        assert_eq!(last_screen(&out)[5], "(input before this was not kept)");
    }

    #[test]
    fn the_row_clamp_ignores_nonsense() {
        assert_eq!(clamp_rows(25), 25);
        assert_eq!(clamp_rows(2), 2);
        assert_eq!(clamp_rows(0), DEFAULT_ROWS, "zero is not a screen");
        assert_eq!(clamp_rows(1), DEFAULT_ROWS, "one row leaves no page");
        assert_eq!(clamp_rows(60000), DEFAULT_ROWS, "absurd is not a screen");
    }

    /// The smallest screen still moves rather than looping.
    #[test]
    fn a_tiny_screen_still_advances() {
        let tiny = Screen { rows: 1, cols: 1 };
        let (out, _) = run_session(
            &["t"],
            &[("t", &numbered(3))],
            b"  ",
            tiny,
            true,
            Options::default(),
        );
        // `line1` is five one-column rows, and each space moves one.
        assert_eq!(last_screen(&out)[0], "n");
    }

    /// A sink that is already gone, and counts what was pushed at it.
    #[derive(Default)]
    struct ClosedPipe {
        pushes: usize,
    }

    impl Write for ClosedPipe {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            self.pushes += 1;
            Err(io::Error::from(io::ErrorKind::BrokenPipe))
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// A closed reader STOPS the copy instead of draining the input into it.
    ///
    /// EPIPE must not look like a failure — `less x | head` is not an error — but
    /// it must not look like success either: swallowed, it leaves the loop reading
    /// the whole input to write it all nowhere.
    #[test]
    fn a_closed_sink_stops_the_copy_through_path() {
        let dir = td_fs::TempDir::new("td-less-pipe").unwrap();
        let path = dir.path().join("f");
        std::fs::write(&path, vec![b'x'; COPY_BUF * 20]).unwrap();
        let mut out = ClosedPipe::default();
        let status = copy_through(&[path.to_str().unwrap()], &mut out);
        assert_eq!(status, 0, "a reader that left is not a failure");
        assert_eq!(out.pushes, 1, "it must stop at the FIRST refused write");
        dir.cleanup().unwrap();
    }

    /// `copy_through` streams and passes bytes through unchanged.
    #[test]
    fn the_non_terminal_path_copies_bytes_through() {
        let dir = td_fs::TempDir::new("td-less-copy").unwrap();
        let path = dir.path().join("f");
        // Larger than one copy buffer, so the loop really loops.
        let data: Vec<u8> = (0..COPY_BUF * 3).map(|i| (i % 251) as u8).collect();
        std::fs::write(&path, &data).unwrap();
        let name = path.to_str().unwrap();
        let mut out: Vec<u8> = Vec::new();
        assert_eq!(copy_through(&[name], &mut out), 0);
        assert_eq!(out, data, "every byte, unchanged and unpaged");
        // A missing operand is reported and the others still copy.
        let mut out: Vec<u8> = Vec::new();
        assert_eq!(copy_through(&["/no/such/file", name], &mut out), 1);
        assert_eq!(out, data, "...and the readable operand is still copied");
        dir.cleanup().unwrap();
    }
}

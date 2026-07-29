//! `less` — the console pager, replacing the `more` busybox was carried for.
//!
//! Bytes, not text: a pager is pointed at logs and `/proc` files, and rejecting a
//! whole file over one stray byte would make it unviewable. `cat` made the same
//! call for the same reason.
//!
//! ## Shape of the thing
//!
//! Single keystrokes when the terminal will allow it — space pages, `j` advances
//! one line, `q` quits — which is what `term.rs`'s raw mode buys and the crate's
//! one `ioctl` surface exists for. When raw mode cannot be had the same commands
//! work a line at a time, terminated by Enter, so a terminal that will not switch
//! degrades instead of failing.
//!
//! **Forward only.** Scrolling back means holding what scrolled past, and a pager
//! that buffers has to answer "how much" — unbounded is a `less` of a growing log
//! that eats the machine, bounded silently loses the top. Streaming has neither
//! problem and constant memory, which is a claim the non-terminal path has to
//! honour too, so it copies through a fixed buffer rather than slurping.
//!
//! **Not a pager when stdout is not a terminal.** `less | grep` and `less > file`
//! copy through untouched, as every pager does — otherwise the prompts land in the
//! data.

use std::io::{BufRead, BufReader, Cursor, IsTerminal, Read, Write};
use std::os::fd::AsFd;

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

pub fn run(args: &[String]) -> Result<u8, String> {
    let files = match parse(args) {
        Ok(files) => files,
        Err(why) => {
            crate::emit_err(&format!("less: {why}\n"));
            // 2, not 1: the crate reserves 2 for a usage error, and `test` and the
            // multicall's own unknown-applet path already answer that way.
            return Ok(2);
        }
    };
    let mut out = std::io::stdout();
    if !out.is_terminal() {
        return copy_through(&files, &mut out);
    }
    // Commands come from the terminal, not from stdin — stdin may BE the data.
    // Without one there is nothing to page against, so degrade to copying.
    let Ok(tty) = std::fs::File::open("/dev/tty") else {
        return copy_through(&files, &mut out);
    };
    let (rows, cols) = geometry(tty.as_fd());
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
    let mut commands = BufReader::with_capacity(1, &tty);
    page_files(
        &files,
        &mut out,
        &mut commands,
        Screen { rows, cols },
        raw.is_some(),
    )
}

/// Page every operand onto ONE screen budget.
///
/// Split from `run` so the whole multi-file decision is reachable from a test:
/// what `run` adds is a terminal and its geometry, and none of the behaviour
/// worth asserting is about having one.
fn page_files(
    files: &[String],
    out: &mut impl Write,
    commands: &mut impl BufRead,
    screen: Screen,
    raw: bool,
) -> Result<u8, String> {
    let mut status = 0u8;
    // Rows already spent on THIS screen, carried ACROSS operands. Reset per file
    // and `less /etc/*` over forty one-line files emits forty files' worth with no
    // prompt at all — the pager degrades to `cat` for exactly the invocation that
    // wants one most.
    let mut used = 0usize;
    for (index, name) in files.iter().enumerate() {
        let input = match open(name) {
            Ok(input) => input,
            Err(why) => {
                crate::emit_err(&format!("less: {why}\n"));
                status = 1;
                continue;
            }
        };
        // Name each file only when there is more than one, as `more` does. The
        // header is CHAINED onto the front of the file rather than written
        // separately, so it is paged, wrapped and row-counted as ordinary input: a
        // header that does not fit waits for a keystroke like anything else, and a
        // long store path that wraps costs the rows it really takes.
        let head = if files.len() > 1 {
            format!(
                "{}::::::::::\n{name}\n::::::::::\n",
                if index > 0 { "\n" } else { "" }
            )
        } else {
            String::new()
        };
        let mut source = Cursor::new(head.into_bytes()).chain(input);
        match paginate(&mut source, out, commands, screen, raw, used) {
            Ok((Stopped::Quit, _)) => break,
            Ok((Stopped::Eof, left)) => used = left,
            Err(why) => {
                // A read error on one operand is not a reason to abandon the rest,
                // which is what `copy_through` already does.
                crate::emit_err(&format!("less: {why}\n"));
                status = 1;
            }
        }
    }
    Ok(status)
}

fn usage() -> String {
    "usage: less [FILE...]  (space pages, j one line, q quits; `-` is stdin)".to_string()
}

/// The operands. No options are served: every `less` flag is either a terminal
/// behaviour this cannot do or a search this does not have, and accepting one
/// silently would be worse than refusing it. `--` ends the options, so a file
/// really named `-N` is still reachable.
fn parse(args: &[String]) -> Result<Vec<String>, String> {
    let mut files = Vec::new();
    let mut options_done = false;
    for a in args {
        if !options_done && a == "--" {
            options_done = true;
            continue;
        }
        if !options_done && a.len() > 1 && a.starts_with('-') {
            return Err(format!("unrecognised option '{a}'\n{}", usage()));
        }
        files.push(a.clone());
    }
    if files.is_empty() {
        files.push("-".to_string());
    }
    Ok(files)
}

/// What a page is measured against.
#[derive(Clone, Copy)]
struct Screen {
    rows: u16,
    cols: u16,
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

fn open(name: &str) -> Result<Box<dyn BufRead>, String> {
    if name == "-" {
        return Ok(Box::new(BufReader::new(std::io::stdin())));
    }
    match std::fs::File::open(name) {
        Ok(f) => Ok(Box::new(BufReader::new(f))),
        Err(e) => Err(format!("{name}: {e}")),
    }
}

/// Write, reporting whether the sink is still OPEN.
///
/// A closed reader is a clean exit, exactly as in `cat`: `less x | head` must not
/// look like a failure. But it must not look like success either — swallowing
/// EPIPE into `Ok(())` leaves the caller looping, so `less huge.log | head -1`
/// reads the whole log to write it all nowhere, and against a producer that never
/// ends the pipeline never does. The `false` is what stops the loop.
fn write(out: &mut impl Write, bytes: &[u8]) -> Result<bool, String> {
    match out.write_all(bytes).and_then(|()| out.flush()) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => Ok(false),
        Err(e) => Err(e.to_string()),
    }
}

/// The non-terminal path: stream, never buffer. `less big.log | grep x` must
/// start producing before the log ends, and must not hold the log in memory.
fn copy_through(files: &[String], out: &mut impl Write) -> Result<u8, String> {
    let mut status = 0u8;
    let mut buf = [0u8; COPY_BUF];
    for name in files {
        let mut input = match open(name) {
            Ok(input) => input,
            Err(why) => {
                crate::emit_err(&format!("less: {why}\n"));
                status = 1;
                continue;
            }
        };
        loop {
            match input.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => match buf.get(..n) {
                    Some(chunk) => {
                        if !write(out, chunk)? {
                            return Ok(status);
                        }
                    }
                    None => break,
                },
                Err(e) => {
                    crate::emit_err(&format!("less: {name}: {e}\n"));
                    status = 1;
                    break;
                }
            }
        }
    }
    Ok(status)
}

#[derive(Debug, PartialEq, Eq)]
enum Stopped {
    Eof,
    Quit,
}

#[derive(Debug, PartialEq, Eq)]
enum Command {
    Page,
    Line,
    Quit,
}

/// The most bytes charged to one screen line before it is treated as a unit of
/// its own.
///
/// `read_until` is unbounded: a log with no newline in it — a truncated write, a
/// binary blob, `/dev/zero` — grows one `Vec` until the machine gives out, which
/// is exactly the failure this pager's forward-only design exists to avoid. A cap
/// this size holds any real line; a monstrous one simply pages through in pieces.
const MAX_LINE: usize = 64 * 1024;

/// How many COLUMNS a run of bytes occupies on the terminal.
///
/// A tab is one byte and up to eight columns, so counting bytes UNDER-counts it —
/// and under-counting is the direction that overflows the page and scrolls its top
/// away, the one thing a pager exists to prevent. So tabs are expanded to the next
/// multiple of eight. Everything else counts one per byte, which OVER-counts a
/// UTF-8 character; that direction only ends the page early.
fn display_cols(bytes: &[u8]) -> usize {
    let mut cols = 0usize;
    for b in bytes {
        match *b {
            b'\t' => cols = cols / 8 * 8 + 8,
            b'\n' | b'\r' => {}
            _ => cols += 1,
        }
    }
    cols
}

/// How many screen ROWS those columns occupy once the terminal wraps them.
fn rows_used(bytes: &[u8], cols: u16) -> usize {
    let width = usize::from(cols.max(1));
    // A line exactly `cols` wide occupies one row, not two: the wrap happens on
    // the column AFTER the last one.
    display_cols(bytes).saturating_sub(1) / width + 1
}

/// Read one line, or `MAX_LINE` bytes of one, whichever comes first.
///
/// Returns the number of bytes read; `Ok(0)` is end of input. The cap is what
/// keeps memory constant — see `MAX_LINE`.
fn read_line_capped(input: &mut impl BufRead, out: &mut Vec<u8>) -> Result<usize, String> {
    out.clear();
    loop {
        let available = match input.fill_buf() {
            Ok(a) => a,
            Err(e) => return Err(e.to_string()),
        };
        if available.is_empty() {
            return Ok(out.len());
        }
        let room = MAX_LINE.saturating_sub(out.len());
        let limit = available.len().min(room);
        let slice = available.get(..limit).unwrap_or_default();
        // `position`, not the obvious lookup method: this file is embedded verbatim
        // into the recipe, and the ladder guard scans that text for host-tool names
        // the method happens to spell.
        let (taken, complete) = match slice.iter().position(|b| *b == b'\n') {
            Some(at) => (at + 1, true),
            None => (limit, false),
        };
        out.extend_from_slice(slice.get(..taken).unwrap_or_default());
        input.consume(taken);
        if complete || out.len() >= MAX_LINE {
            return Ok(out.len());
        }
    }
}

/// Page `input` to `out`, taking one command per screenful.
///
/// Split from `run` so the whole decision is reachable from a test: the terminal
/// is what `run` supplies, and none of the behaviour worth asserting is about
/// having one.
fn paginate(
    input: &mut impl BufRead,
    out: &mut impl Write,
    commands: &mut impl BufRead,
    screen: Screen,
    raw: bool,
    already_used: usize,
) -> Result<(Stopped, usize), String> {
    // One row is kept for the prompt, and never zero however small the screen.
    let per_page = usize::from(screen.rows).saturating_sub(1).max(1);
    // Allocated once, reused every line: this is the pager's inner loop.
    let mut line: Vec<u8> = Vec::with_capacity(256);
    // The header is charged to the page, not subtracted from the budget as well.
    let mut used = already_used;
    let mut advance = per_page;
    loop {
        while used < advance {
            if read_line_capped(input, &mut line)? == 0 {
                return Ok((Stopped::Eof, used));
            }
            if !write(out, &line)? {
                return Ok((Stopped::Quit, used));
            }
            used += rows_used(&line, screen.cols);
        }
        // Peeking here is what makes an input ending exactly on a page boundary not
        // prompt. It costs one thing: a producer that emits exactly a screenful and
        // then PAUSES holds the prompt back until it writes again. That is narrower
        // than it sounds — a producer pausing mid-page already blocks the read, as
        // it would under `cat` — and Ctrl-C still works, because `ISIG` is
        // deliberately left enabled. Prompting first instead would trade this for
        // prompting at a clean end of input, which every reader would meet and this
        // one almost none.
        if !more_follows(input)? {
            return Ok((Stopped::Eof, used));
        }
        if !write(out, b"--More--")? {
            return Ok((Stopped::Quit, used));
        }
        match next_command(commands, raw)? {
            Command::Quit => {
                let _ = write(out, b"\n")?;
                return Ok((Stopped::Quit, used));
            }
            // Raw mode echoes nothing, so the prompt has to be erased. In line mode
            // the reader's Enter already moved the cursor on.
            Command::Page => {
                if !clear_prompt(out, raw)? {
                    return Ok((Stopped::Quit, used));
                }
                used = 0;
                advance = per_page;
            }
            Command::Line => {
                if !clear_prompt(out, raw)? {
                    return Ok((Stopped::Quit, used));
                }
                used = per_page.saturating_sub(1);
                advance = per_page;
            }
        }
    }
}

fn clear_prompt(out: &mut impl Write, raw: bool) -> Result<bool, String> {
    if raw {
        write(out, b"\r        \r")
    } else {
        Ok(true)
    }
}

/// Whether anything remains, WITHOUT consuming it.
fn more_follows(input: &mut impl BufRead) -> Result<bool, String> {
    match input.fill_buf() {
        Ok(buf) => Ok(!buf.is_empty()),
        Err(e) => Err(e.to_string()),
    }
}

/// One command. In raw mode that is a single byte; otherwise a line, terminated by
/// Enter. EOF quits either way, because a command source that has closed can never
/// say "continue" — treating it as "page forward" would spin the whole file past.
fn next_command(commands: &mut impl BufRead, raw: bool) -> Result<Command, String> {
    if raw {
        let mut byte = [0u8; 1];
        return match commands.read(&mut byte) {
            Ok(0) => Ok(Command::Quit),
            Ok(_) => Ok(classify(byte.first().copied().unwrap_or(b' '))),
            Err(e) => Err(e.to_string()),
        };
    }
    let mut reply = String::new();
    match commands.read_line(&mut reply) {
        Ok(0) => Ok(Command::Quit),
        // The FIRST byte, untrimmed: a bare Enter is the `\n` that means one line,
        // and trimming it away would silently turn every Enter into a page.
        Ok(_) => Ok(reply.bytes().next().map_or(Command::Page, classify)),
        Err(e) => Err(e.to_string()),
    }
}

/// Anything unrecognised pages forward, which is what every pager does with a
/// stray key and is the harmless answer: the reader sees more of what they asked
/// for, rather than the pager exiting on a typo.
fn classify(key: u8) -> Command {
    match key {
        b'q' | b'Q' => Command::Quit,
        b'j' | b'\r' | b'\n' => Command::Line,
        _ => Command::Page,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use std::io::Cursor;

    const WIDE: Screen = Screen { rows: 6, cols: 80 };

    fn body(lines: usize) -> String {
        let mut s = String::new();
        for n in 1..=lines {
            s.push_str(&format!("line{n}\n"));
        }
        s
    }

    fn page_raw(text: &str, keys: &str, screen: Screen) -> (String, Stopped) {
        run_pager(text, keys, screen, true, 0)
    }

    fn run_pager(
        text: &str,
        commands: &str,
        screen: Screen,
        raw: bool,
        used: usize,
    ) -> (String, Stopped) {
        let mut input = Cursor::new(text.to_string().into_bytes());
        let mut cmds = Cursor::new(commands.to_string().into_bytes());
        let mut out: Vec<u8> = Vec::new();
        let (stopped, _) = paginate(&mut input, &mut out, &mut cmds, screen, raw, used).unwrap();
        (String::from_utf8_lossy(&out).into_owned(), stopped)
    }

    /// It STOPS. A pager that emits everything before reading a command is just
    /// `cat`, and that is the whole function of this applet.
    #[test]
    fn output_halts_at_the_first_screenful() {
        let (out, stopped) = page_raw(&body(100), "", WIDE);
        assert!(out.contains("line5"), "the first page must be shown");
        assert!(!out.contains("line6"), "it must not run past the page: {out:?}");
        assert_eq!(stopped, Stopped::Quit, "an exhausted command source quits");
    }

    /// A single keystroke advances a page — no Enter, which is what raw mode buys.
    #[test]
    fn one_keystroke_pages_in_raw_mode() {
        let (out, _) = page_raw(&body(100), " ", WIDE);
        assert!(out.contains("line10"), "space alone must deliver a second page");
        assert!(!out.contains("line11"), "and exactly one: {out:?}");
    }

    /// `j` advances ONE line, not a page.
    #[test]
    fn j_advances_a_single_line() {
        let (out, _) = page_raw(&body(100), "j", WIDE);
        assert!(out.contains("line6"), "j must show the next line");
        assert!(!out.contains("line7"), "but only one: {out:?}");
    }

    /// `q` stops, and stops the whole run.
    #[test]
    fn q_quits_in_both_modes() {
        assert_eq!(page_raw(&body(100), "q", WIDE).1, Stopped::Quit);
        assert_eq!(page_raw(&body(100), "Q", WIDE).1, Stopped::Quit);
        assert_eq!(run_pager(&body(100), "q\n", WIDE, false, 0).1, Stopped::Quit);
        // ...and an unrecognised key pages on rather than exiting on a typo.
        let (out, _) = page_raw(&body(100), "z", WIDE);
        assert!(out.contains("line10"), "an unknown key pages forward: {out:?}");
    }

    /// Line mode is the fallback when raw mode could not be had, and Enter pages.
    #[test]
    fn line_mode_still_pages() {
        let (out, _) = run_pager(&body(100), "\n\n", WIDE, false, 0);
        // Enter is `j` — one line each — so two commands show two more lines.
        assert!(out.contains("line7"), "two Enters advance two lines: {out:?}");
        assert!(!out.contains("line8"));
        let (out, _) = run_pager(&body(100), " \n", WIDE, false, 0);
        assert!(out.contains("line10"), "space in line mode pages: {out:?}");
    }

    /// Input shorter than a page is shown whole, with NO prompt.
    #[test]
    fn a_short_input_never_prompts() {
        let (out, stopped) = page_raw("only\ntwo\n", "", WIDE);
        assert_eq!(out, "only\ntwo\n");
        assert_eq!(stopped, Stopped::Eof);
        assert!(!out.contains("--More--"), "nothing to page, so nothing to ask");
    }

    /// An input ending EXACTLY on a page boundary does not prompt either — the one
    /// case a naive loop always gets wrong.
    #[test]
    fn a_page_boundary_at_eof_does_not_prompt() {
        let (out, stopped) = page_raw(&body(5), "", WIDE);
        assert_eq!(stopped, Stopped::Eof, "five lines in five rows is done");
        assert!(!out.contains("--More--"), "must not prompt at a clean end: {out:?}");
    }

    /// The prompt appears when there IS more.
    #[test]
    fn the_prompt_appears_only_when_more_follows() {
        let (out, _) = page_raw(&body(6), "q", WIDE);
        assert!(out.contains("--More--"), "a sixth line means a prompt: {out:?}");
    }

    /// A last line with no trailing newline is not lost.
    #[test]
    fn an_unterminated_last_line_is_not_lost() {
        let (out, stopped) = page_raw("a\nb\nc", "", WIDE);
        assert_eq!(out, "a\nb\nc");
        assert_eq!(stopped, Stopped::Eof);
    }

    /// Bytes, not text: a stray non-UTF-8 byte is shown, not refused.
    ///
    /// The sibling `cat` made the same call for the same reason — a pager pointed
    /// at a log must not become unusable because one byte is not text.
    #[test]
    fn invalid_utf8_is_paged_rather_than_refused() {
        let mut input = Cursor::new(vec![b'o', b'k', b'\n', 0xff, 0xfe, b'\n']);
        let mut cmds = Cursor::new(Vec::new());
        let mut out: Vec<u8> = Vec::new();
        let (stopped, _) = paginate(&mut input, &mut out, &mut cmds, WIDE, true, 0).unwrap();
        assert_eq!(stopped, Stopped::Eof);
        assert_eq!(out, vec![b'o', b'k', b'\n', 0xff, 0xfe, b'\n'], "bytes pass through");
    }

    /// A WRAPPED line costs the rows it really occupies, or the page overflows and
    /// the top scrolls away — the one thing a pager exists to prevent.
    #[test]
    fn a_wrapped_line_costs_the_rows_it_occupies() {
        let x = |n: usize| vec![b'x'; n];
        assert_eq!(rows_used(b"", 80), 1, "an empty line still occupies one row");
        assert_eq!(rows_used(&x(1), 80), 1);
        assert_eq!(rows_used(&x(80), 80), 1, "exactly the width is one row");
        assert_eq!(rows_used(&x(81), 80), 2, "one over wraps");
        assert_eq!(rows_used(&x(240), 80), 3);
        // The trailing newline is not a column, or every full-width line would
        // claim a second row it does not use.
        assert_eq!(rows_used(b"12345678\n", 8), 1, "the newline is not a column");
        // A TAB is one byte and up to eight columns. Counting it as one byte
        // under-counts, and under-counting overflows the page and scrolls its top
        // away — the direction that actually loses the reader's data.
        assert_eq!(rows_used(b"\t", 8), 1);
        assert_eq!(rows_used(b"\t\t", 8), 2, "two tabs are sixteen columns");
        assert_eq!(rows_used(b"a\tb", 8), 2, "a tab advances to the next stop");
        assert_eq!(display_cols(b"a\tb"), 9, "1 col, tab to 8, then 1 more");
        assert_eq!(display_cols(b"\t"), 8);
        assert_eq!(display_cols(b"12345678\t"), 16, "a full stop advances a whole tab");
        // ...and the pager charges for it: three 80-column lines fill a 5-row page.
        let wide = format!("{}\n{}\n", "x".repeat(160), "y".repeat(160));
        let (out, stopped) = page_raw(&wide, "", Screen { rows: 6, cols: 80 });
        assert_eq!(stopped, Stopped::Eof, "two lines of two rows each fit");
        assert!(out.contains("yyy"));
    }

    /// A multi-file header is charged against the first page, not added to it.
    #[test]
    fn a_header_costs_rows_from_the_page_it_precedes() {
        let (out, _) = run_pager(&body(100), "", WIDE, true, 3);
        assert!(out.contains("line2"), "five rows less a three-row header: {out:?}");
        assert!(!out.contains("line3"), "the header must not push lines off: {out:?}");
    }

    /// Geometry falls back in order, and nonsense is ignored.
    #[test]
    fn the_row_clamp_ignores_nonsense() {
        assert_eq!(clamp_rows(25), 25);
        assert_eq!(clamp_rows(2), 2);
        assert_eq!(clamp_rows(0), DEFAULT_ROWS, "zero is not a screen");
        assert_eq!(clamp_rows(1), DEFAULT_ROWS, "one row leaves no page");
        assert_eq!(clamp_rows(60000), DEFAULT_ROWS, "absurd is not a screen");
    }

    /// A one-row screen still makes progress rather than looping forever.
    #[test]
    fn a_tiny_screen_still_advances() {
        let (out, stopped) = page_raw(&body(3), "  ", Screen { rows: 1, cols: 80 });
        assert!(out.contains("line1"));
        assert!(matches!(stopped, Stopped::Eof | Stopped::Quit));
    }

    /// Operands, `--`, and the refusal of options this cannot honour.
    #[test]
    fn options_are_refused_and_double_dash_ends_them() {
        assert_eq!(parse(&[]).unwrap(), vec!["-".to_string()]);
        assert_eq!(parse(&["-".to_string()]).unwrap(), vec!["-".to_string()]);
        for bad in ["-N", "--squeeze", "-S"] {
            assert!(parse(&[bad.to_string()]).is_err(), "{bad} must be refused");
        }
        // `--` is consumed, and what follows is an operand however it is spelled.
        assert_eq!(
            parse(&["--".to_string(), "-N".to_string()]).unwrap(),
            vec!["-N".to_string()],
            "`--` must make a file named -N reachable, not manufacture an operand"
        );
        assert_eq!(parse(&["--".to_string()]).unwrap(), vec!["-".to_string()]);
    }

    /// A sink that is already gone, and counts what was pushed at it.
    #[derive(Default)]
    struct ClosedPipe {
        pushes: usize,
    }

    impl Write for ClosedPipe {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.pushes += 1;
            let _ = buf;
            Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe))
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// A closed reader STOPS the pager instead of draining the input into it.
    ///
    /// EPIPE must not look like a failure — `less x | head` is not an error — but
    /// it must not look like success either: swallowed, it leaves the loop reading
    /// the whole input to write it all nowhere, and against a producer that never
    /// ends the pipeline never does.
    #[test]
    fn a_closed_sink_stops_the_pager_rather_than_draining_the_input() {
        let text = body(20_000);
        let total = text.len() as u64;
        let mut input = Cursor::new(text.into_bytes());
        let mut cmds = Cursor::new(Vec::new());
        let mut out = ClosedPipe::default();
        let (stopped, _) = paginate(&mut input, &mut out, &mut cmds, WIDE, true, 0).unwrap();
        assert_eq!(stopped, Stopped::Quit, "a closed sink ends the run");
        assert_eq!(out.pushes, 1, "it must stop at the FIRST refused write");
        assert!(
            input.position() < total / 100,
            "the input was drained into a closed pipe: read {} of {total} bytes",
            input.position()
        );
    }

    /// The same, on the non-terminal path.
    #[test]
    fn a_closed_sink_stops_the_copy_through_path() {
        let dir = std::env::temp_dir().join(format!("td-less-pipe-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("f");
        std::fs::write(&path, vec![b'x'; COPY_BUF * 20]).unwrap();
        let mut out = ClosedPipe::default();
        let status = copy_through(&[path.to_string_lossy().into_owned()], &mut out).unwrap();
        assert_eq!(status, 0, "a reader that left is not a failure");
        assert_eq!(out.pushes, 1, "it must stop at the FIRST refused write");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A line with no newline in it does NOT grow without bound.
    ///
    /// `read_until` would take the whole thing. A truncated write, a binary blob or
    /// `/dev/zero` is then one `Vec` that grows until the machine gives out — which
    /// is precisely the failure the forward-only design exists to avoid, arriving
    /// by another road.
    #[test]
    fn an_endless_line_is_capped_rather_than_buffered_whole() {
        let huge = vec![b'x'; MAX_LINE * 3 + 17];
        let mut input = Cursor::new(huge);
        let mut line: Vec<u8> = Vec::new();
        assert_eq!(read_line_capped(&mut input, &mut line).unwrap(), MAX_LINE);
        assert_eq!(line.len(), MAX_LINE, "the cap must bound the buffer");
        assert_eq!(read_line_capped(&mut input, &mut line).unwrap(), MAX_LINE);
        assert_eq!(read_line_capped(&mut input, &mut line).unwrap(), MAX_LINE);
        assert_eq!(read_line_capped(&mut input, &mut line).unwrap(), 17, "the tail");
        assert_eq!(read_line_capped(&mut input, &mut line).unwrap(), 0, "then EOF");
        // ...and a NORMAL line is still returned whole, newline included.
        let mut small = Cursor::new(b"one\ntwo\n".to_vec());
        assert_eq!(read_line_capped(&mut small, &mut line).unwrap(), 4);
        assert_eq!(line, b"one\n");
        assert_eq!(read_line_capped(&mut small, &mut line).unwrap(), 4);
        assert_eq!(line, b"two\n");
        assert_eq!(read_line_capped(&mut small, &mut line).unwrap(), 0);
    }

    /// ...and the pager charges a WRAPPED line the rows it really covers, so what
    /// follows it waits for a keystroke instead of scrolling it away.
    #[test]
    fn an_over_long_line_prompts_before_what_follows_it() {
        let mut wide = vec![b'x'; 4000];
        wide.push(b'\n');
        wide.extend_from_slice(body(10).as_bytes());
        let mut input = Cursor::new(wide);
        let mut cmds = Cursor::new(b"q".to_vec());
        let mut out: Vec<u8> = Vec::new();
        let (stopped, _) = paginate(&mut input, &mut out, &mut cmds, WIDE, true, 0).unwrap();
        assert_eq!(stopped, Stopped::Quit, "it must stop and ask, not run on");
        let shown = String::from_utf8_lossy(&out);
        assert!(
            shown.contains("--More--"),
            "50 wrapped rows into a 6-row screen must prompt: {shown:.60}"
        );
        assert!(
            !shown.contains("line1"),
            "the line after it must not be shown before the reader asks"
        );
    }

    /// Rows spent on one operand are still spent when the next one starts.
    ///
    /// The screen does not reset because the file did. Counting per file makes
    /// `less /etc/*` emit every one of forty short files with no prompt at all —
    /// the applet degrades to `cat` for exactly the invocation a pager is for.
    #[test]
    fn rows_carry_across_operands() {
        let dir = std::env::temp_dir().join(format!("td-less-multi-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::create_dir_all(&dir);
        let mut names = Vec::new();
        for n in 0..6 {
            let p = dir.join(format!("f{n}"));
            std::fs::write(&p, format!("body{n}\n")).unwrap();
            names.push(p.to_string_lossy().into_owned());
        }
        let mut cmds = Cursor::new(b"q".to_vec());
        let mut out: Vec<u8> = Vec::new();
        // Six one-line files, each with a 3-4 row header, onto a 6-row screen.
        let status = page_files(&names, &mut out, &mut cmds, WIDE, true).unwrap();
        assert_eq!(status, 0);
        let shown = String::from_utf8_lossy(&out);
        assert!(
            shown.contains("--More--"),
            "six files onto one screen must PROMPT, not run them all past: {shown:?}"
        );
        // `q` at that first prompt ends the whole run, so the later files must not
        // have been emitted at all.
        assert!(shown.contains("body0"), "the first file is shown");
        assert!(
            !shown.contains("body5"),
            "quitting at the prompt must stop the run, not resume at the next file"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A single operand still pages exactly as before, and an unreadable one in
    /// the middle is reported without abandoning the rest.
    #[test]
    fn one_bad_operand_does_not_end_the_run() {
        let dir = std::env::temp_dir().join(format!("td-less-bad-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::create_dir_all(&dir);
        let good = dir.join("good");
        std::fs::write(&good, "kept\n").unwrap();
        let names = vec![
            "/no/such/file".to_string(),
            good.to_string_lossy().into_owned(),
        ];
        let mut cmds = Cursor::new(Vec::new());
        let mut out: Vec<u8> = Vec::new();
        let status = page_files(&names, &mut out, &mut cmds, WIDE, true).unwrap();
        assert_eq!(status, 1, "the unreadable operand is reported");
        assert!(
            String::from_utf8_lossy(&out).contains("kept"),
            "...and the readable one is still paged"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `paginate` reports the rows it left on the screen, which is what makes the
    /// carry above possible at all.
    #[test]
    fn paginate_reports_the_rows_it_left_behind() {
        let mut input = Cursor::new(b"a\nb\n".to_vec());
        let mut cmds = Cursor::new(Vec::new());
        let mut out: Vec<u8> = Vec::new();
        let (stopped, used) = paginate(&mut input, &mut out, &mut cmds, WIDE, true, 0).unwrap();
        assert_eq!(stopped, Stopped::Eof);
        assert_eq!(used, 2, "two lines occupy two rows");
        // ...and it starts from what it was handed.
        let mut input = Cursor::new(b"a\n".to_vec());
        let (_, used) = paginate(&mut input, &mut out, &mut cmds, WIDE, true, 3).unwrap();
        assert_eq!(used, 4, "three already spent, plus one more");
    }

    /// `copy_through` streams and passes bytes through unchanged.
    #[test]
    fn the_non_terminal_path_copies_bytes_through() {
        let dir = std::env::temp_dir().join(format!("td-less-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("f");
        // Larger than one copy buffer, so the loop really loops.
        let data: Vec<u8> = (0..COPY_BUF * 3).map(|i| (i % 251) as u8).collect();
        std::fs::write(&path, &data).unwrap();
        let mut out: Vec<u8> = Vec::new();
        let status = copy_through(&[path.to_string_lossy().into_owned()], &mut out).unwrap();
        assert_eq!(status, 0);
        assert_eq!(out, data, "every byte, unchanged and unpaged");
        // A missing operand is reported and the others still copy.
        let mut out2: Vec<u8> = Vec::new();
        let status = copy_through(
            &["/no/such/file".to_string(), path.to_string_lossy().into_owned()],
            &mut out2,
        )
        .unwrap();
        assert_eq!(status, 1, "the failure is reported");
        assert_eq!(out2, data, "...and the readable operand is still copied");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

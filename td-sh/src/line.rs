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
use std::num::NonZeroUsize;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use crate::complete;
use crate::exec::Shell;
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
    /// How many lines are kept, in memory and in the file. `HISTFILESIZE`
    /// names it; `HISTORY_MAX` is the ceiling and the default.
    ///
    /// NON-ZERO in the type, because `remember` drops the oldest line by index
    /// when the list is full and `remove(0)` on an empty one is a panic in a
    /// crate that aborts on those. `HISTFILESIZE` floors at one, so this is a
    /// bound the type keeps rather than a bug being fixed.
    max: NonZeroUsize,
    /// Where the history outlives the session, and how many lines this session
    /// believes are in that file. `None` when nothing is persisted, which is
    /// every non-interactive shell and any one that cannot name a file.
    hist: Option<HistoryFile>,
    /// What the kill keys took, for Ctrl-Y to put back. On the EDITOR rather
    /// than on a line, because readline's outlives the line that filled it:
    /// killing on one line and yanking on the next is what makes it a way to
    /// move text between commands rather than only within one.
    kill: String,
}

struct HistoryFile {
    path: PathBuf,
    /// Counted rather than measured: nothing but the trim threshold reads it,
    /// and the only way to measure it is to read the file, which is the very
    /// work the threshold exists to do rarely.
    lines: usize,
    /// Set when the file was found ending mid-line, so the next append writes
    /// a newline first rather than gluing its command onto the fragment.
    unterminated: bool,
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
/// It is also the CEILING `HISTFILESIZE` is clamped to, as busybox clamps to
/// its own `MAX_HISTORY` — 255 there, so a value between the two is honoured
/// here and capped there.
const HISTORY_MAX: usize = 500;

/// How far the file may run past what is kept before it is rewritten. busybox's
/// number (`max_history * 4`), and the reason is the same: trimming on every
/// command would turn one append into a whole-file read and rewrite.
const TRIM_FACTOR: usize = 4;

/// `HISTORY_MAX` as the type the editor holds it in. `match` rather than
/// `unwrap_or`, which is not const yet.
const DEFAULT_MAX: NonZeroUsize = match NonZeroUsize::new(HISTORY_MAX) {
    Some(n) => n,
    None => NonZeroUsize::MIN,
};

/// The most of a history file that is ever read into memory. A megabyte is
/// thousands of commands where hundreds are kept, so it never clips a real
/// history — it is the bound on a `HISTFILE` that names something else.
const HISTORY_READ_MAX: u64 = 1 << 20;

impl Editor {
    pub fn new() -> Self {
        Self { history: Vec::new(), max: DEFAULT_MAX, hist: None, kill: String::new() }
    }

    /// Take the session's history file: load what is in it, and append every
    /// line entered from here on.
    ///
    /// Resolved ONCE, as ash resolves it once at the top of an interactive
    /// session (ash.c:14802) — assigning `HISTFILE` later does not move the
    /// file, here or there. When nobody set it the default is written BACK
    /// into the variable, so `echo $HISTFILE` names the file that is in use
    /// rather than nothing.
    pub fn open_history(&mut self, sh: &mut Shell) {
        self.max = history_size(sh);
        let Some(path) = history_path(sh) else {
            return;
        };
        // A file that cannot be read is still the file to append to: it may be
        // unreadable this instant and writable the next, and refusing to keep
        // history because of one failed open would be the worse answer.
        let loaded = read_history(&path, self.max.get());
        if let Some(l) = &loaded {
            self.history.clone_from(&l.kept);
        }
        self.hist = Some(HistoryFile {
            path,
            lines: loaded.as_ref().map_or(0, |l| l.total),
            unterminated: loaded.is_some_and(|l| !l.terminated),
        });
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
        let outcome = self.read_line(prompt, interruptible, comp);
        // ONE place a line is remembered, so the cooked fallback keeps history
        // too. It used to be inside the raw path, past three early returns, and
        // a terminal that would not take `TCSETS` therefore had an operator
        // typing commands that reached neither Ctrl-P nor the file.
        if let Input::Line(text) = &outcome {
            self.remember(text);
        }
        outcome
    }

    fn read_line(
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
        if matches!(outcome, Input::Line(_)) {
            let _ = writeln!(out);
        }
        outcome
    }

    /// Keep `text` in the history: a line identical to the previous one is not
    /// kept twice, as ash does, and a blank one is not kept at all. That second
    /// rule is slightly wider than busybox's, which drops only a line of zero
    /// length — a line of spaces is not a command, and keeping it would make it
    /// the first thing Ctrl-P offers.
    fn remember(&mut self, text: &str) {
        if text.trim().is_empty() || self.history.last().map(String::as_str) == Some(text) {
            return;
        }
        if self.history.len() >= self.max.get() {
            self.history.remove(0);
        }
        self.history.push(text.to_string());
        self.append(text);
    }

    /// Append the line to the history file, if there is one.
    ///
    /// APPEND, per line, rather than rewriting the file when the shell exits:
    /// two shells open at once then INTERLEAVE their lines instead of the one
    /// that exits last discarding everything the other did. It is also what
    /// survives a shell that is killed rather than exited. busybox's default
    /// build does exactly this (lineedit.c:1608) and its `SAVE_ON_EXIT` option
    /// is what gives both up.
    fn append(&mut self, text: &str) {
        let max = self.max.get();
        let Some(h) = self.hist.as_mut() else {
            return;
        };
        // ONE write of the line and its newline together: `O_APPEND` makes a
        // single write atomic against a concurrent appender, and two writes
        // would let another shell's line land between them.
        let mut buf = String::with_capacity(text.len().saturating_add(2));
        // A file left ending mid-line -- edited by hand, or cut short by a
        // full disk -- gets that line ended first, or this command is glued
        // onto the fragment and the pair reads back as one command nobody
        // typed. Once, since everything after this writes its own newline.
        if std::mem::take(&mut h.unterminated) {
            buf.push('\n');
        }
        buf.push_str(text);
        buf.push('\n');
        // 0600 because a shell history is a record of what someone typed,
        // passwords on argv included; busybox opens it with the same mode.
        let opened =
            std::fs::OpenOptions::new().append(true).create(true).mode(0o600).open(&h.path);
        let Ok(mut f) = opened else {
            return;
        };
        if f.write_all(buf.as_bytes()).is_err() {
            return;
        }
        h.lines = h.lines.saturating_add(1);
        if h.lines > max.saturating_mul(TRIM_FACTOR) {
            if let Some(n) = rewrite_history(&h.path, max) {
                h.lines = n;
            }
        }
    }

    /// Take `text` into the kill buffer, and say whether anything was taken.
    ///
    /// `before` is whether the text came from BEFORE the cursor, which is what
    /// decides the side an accumulating kill lands on: readline appends a
    /// forward kill and prepends a backward one, so `^W ^W` yanks the two words
    /// back in the order they were typed rather than reversed. `run` is whether
    /// the previous keystroke was itself a kill; without it every kill would
    /// stand alone and the buffer could only ever hold one.
    ///
    /// A kill that takes NOTHING leaves the buffer alone -- `^K` at the end of
    /// a line must not throw away what is in it -- and `false` here is what
    /// then ends the run, both measured against bash 5.2.
    fn kill_text(&mut self, text: &str, before: bool, run: bool) -> bool {
        if text.is_empty() {
            return false;
        }
        if !run {
            self.kill.clear();
        }
        // Index 0 is a character boundary in every string, so the prepend
        // cannot be the panicking half of `insert_str`.
        match before {
            true => self.kill.insert_str(0, text),
            false => self.kill.push_str(text),
        }
        true
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
        // Whether the last keystroke was a kill that took something, which is
        // what makes the next one ACCUMULATE rather than replace. Local to the
        // line, unlike the buffer itself: bash starts a fresh run on a new
        // line even though the buffer survives.
        let mut last_kill = false;

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
            let was_kill = std::mem::replace(&mut last_kill, false);
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
                // Ctrl-K / Ctrl-U: kill to end / to start. What they take goes
                // to the kill buffer for Ctrl-Y, which is the difference
                // between a mistyped line being lost and being moved. The
                // `get` is the lint's form of the slice beside it and NOT a
                // handled case: the `truncate`/`drain` on the next line rests
                // on the same index, so both hold or neither does.
                0x0b => {
                    last_kill = self.kill_text(buf.get(pos..).unwrap_or_default(), false, was_kill);
                    buf.truncate(pos);
                }
                0x15 => {
                    last_kill = self.kill_text(buf.get(..pos).unwrap_or_default(), true, was_kill);
                    buf.drain(..pos);
                    pos = 0;
                }
                // Ctrl-W: kill the word before the cursor.
                0x17 => {
                    let start = word_start(&buf, pos);
                    last_kill =
                        self.kill_text(buf.get(start..pos).unwrap_or_default(), true, was_kill);
                    buf.drain(start..pos);
                    pos = start;
                }
                // Ctrl-Y: put the kill buffer back at the cursor, which then
                // sits AFTER it -- so a second Ctrl-Y inserts a second copy
                // rather than overwriting the first. `pos` is a boundary and
                // the buffer holds whole characters, so the insert cannot
                // split one and the byte count lands on a boundary too.
                0x19 => {
                    buf.insert_str(pos, &self.kill);
                    pos = pos.saturating_add(self.kill.len());
                }
                // Ctrl-L: clear the screen and redraw on the top line.
                0x0c => {
                    let _ = write!(out, "\x1b[H\x1b[2J");
                }
                // Ctrl-R: reverse incremental search. The key that ENDED it
                // comes back to be dispatched here, which is what makes Enter
                // run what was found and Ctrl-A go to its start.
                0x12 => {
                    pending = self.reverse_search(
                        prompt,
                        keys,
                        out,
                        &mut buf,
                        &mut pos,
                        &mut browse,
                        &mut draft,
                        &mut width,
                    );
                    if pending.is_none() {
                        return Input::Eof;
                    }
                    continue;
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
                    // The META prefix: a terminal sends Alt-<key> as ESC and
                    // then the key, so this is where readline's word bindings
                    // live. Both cases, because readline answers to either.
                    Escape::Literal(b'b' | b'B') => pos = word_back(&buf, pos),
                    Escape::Literal(b'f' | b'F') => pos = word_forward(&buf, pos),
                    Escape::Literal(b'd' | b'D') => {
                        let end = word_forward(&buf, pos);
                        last_kill =
                            self.kill_text(buf.get(pos..end).unwrap_or_default(), false, was_kill);
                        buf.drain(pos..end);
                    }
                    // Alt-Backspace, whichever byte the terminal sends for it.
                    // Both had a meaning here before: a one-character delete.
                    Escape::Literal(0x7f | 0x08) => {
                        let start = word_back(&buf, pos);
                        last_kill =
                            self.kill_text(buf.get(start..pos).unwrap_or_default(), true, was_kill);
                        buf.drain(start..pos);
                        pos = start;
                    }
                    // Not a binding: the byte comes back as an ordinary key,
                    // which is what makes ESC then Enter submit the line.
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

    /// Ctrl-R: readline's reverse incremental search, which busybox mimics
    /// (`reverse_i_search`, lineedit.c:2325). The prompt becomes
    /// `(reverse-i-search)'…': `, every printable key narrows the pattern, and
    /// Ctrl-R again steps to the next OLDER match. The cursor lands on the
    /// match inside the line rather than at either end of it.
    ///
    /// Returns the byte that ENDED the search, for the caller to dispatch as
    /// an ordinary key — busybox's `goto again`, and the whole ergonomics of
    /// the thing: Enter runs what was found, Ctrl-A goes to its start, Ctrl-C
    /// abandons it, and none of that is written here. `None` is end of input.
    #[allow(clippy::too_many_arguments)]
    fn reverse_search<W: Write>(
        &self,
        prompt: &str,
        keys: &mut Keys<'_>,
        out: &mut W,
        buf: &mut String,
        pos: &mut usize,
        browse: &mut usize,
        draft: &mut String,
        width: &mut u16,
    ) -> Option<u8> {
        let mut pattern = String::new();
        let mut pending: Option<u8> = None;
        loop {
            *width = (keys.width)().unwrap_or(*width);
            draw(out, &format!("(reverse-i-search)'{pattern}': "), buf, *pos, *width);
            let Some(b) = pending.take().or_else(&mut *keys.next) else {
                // Closed stdin. Put the real prompt back on the way out, so
                // EVERY exit from the search leaves one -- otherwise the last
                // thing on the screen is a search that is no longer running.
                draw(out, prompt, buf, *pos, *width);
                return None;
            };
            // What the pattern goes back to if this key finds nothing. A
            // shortening key leaves it already shorter, and truncating to a
            // length past the end does nothing, so one line covers both.
            let restore = pattern.len();
            let mut older = false;
            // The two bytes the DRIVER owns are the caller's here too. With
            // `ISIG` cleared the EDITOR is what implements them, so a search
            // that took them as pattern text would be a hole in that
            // emulation exactly where the operator had moved the key --
            // `stty intr x`, and Ctrl-R makes `x` unable to interrupt.
            if Some(b) == keys.intr || Some(b) == keys.eof {
                draw(out, prompt, buf, *pos, *width);
                return Some(b);
            }
            match b {
                // Backspace, both spellings: a CHARACTER off the pattern.
                0x7f | 0x08 => {
                    pattern.pop();
                }
                0x12 => older = true,
                // Every other control byte ends the search and belongs to the
                // caller. The real prompt goes back FIRST: if the key is Enter
                // the caller returns the line at once and never redraws, and
                // the search prompt would be what stays above the output.
                0x00..=0x1f => {
                    draw(out, prompt, buf, *pos, *width);
                    return Some(b);
                }
                _ => {
                    let (typed, back) = character(&mut *keys.next, b);
                    if let Some(c) = typed {
                        pattern.push(c);
                    }
                    pending = back;
                }
            }
            // From where the operator already is, INCLUSIVE — except after a
            // Ctrl-R, which starts one older. That exception is what makes
            // repeated Ctrl-R walk back through the matches instead of
            // finding the same one every time.
            let from = match older {
                true => browse.checked_sub(1),
                false => Some(*browse),
            };
            match from.and_then(|f| self.search_back(&pattern, f)) {
                Some((h, at)) => {
                    // Park the draft before leaving it, exactly as `browse`
                    // does, or arrowing back down to it restores a stale one.
                    if *browse == self.history.len() {
                        draft.clone_from(buf);
                    }
                    if let Some(line) = self.history.get(h) {
                        buf.clone_from(line);
                    }
                    *browse = h;
                    *pos = at;
                }
                None => {
                    // The key that found nothing is TAKEN BACK, so what is on
                    // screen always matches something. Silence would leave the
                    // operator unable to tell a rejected key from an ignored
                    // one, which is what the bell is for.
                    pattern.truncate(restore);
                    let _ = out.write_all(b"\x07");
                }
            }
        }
    }

    /// The newest history line at or before `from` containing `pattern`, and
    /// where in it the match starts.
    ///
    /// `match_indices` rather than the obvious search method, whose bare name
    /// is a token the ladder's host-tool guard refuses anywhere in these
    /// sources.
    fn search_back(&self, pattern: &str, from: usize) -> Option<(usize, usize)> {
        for i in (0..=from).rev() {
            let Some(line) = self.history.get(i) else {
                continue;
            };
            if let Some((at, _)) = line.match_indices(pattern).next() {
                return Some((i, at));
            }
        }
        None
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

/// What a history file held.
struct Loaded {
    /// The last `max` non-empty lines.
    kept: Vec<String>,
    /// How many non-empty lines there were altogether, which is what the trim
    /// threshold is measured against.
    total: usize,
    /// Whether the last byte is a newline, an empty file counting as yes. A
    /// file cut off mid-line would otherwise have the next append GLUE the new
    /// command onto the fragment, and `betagamma` is a history entry naming a
    /// command nobody typed.
    terminated: bool,
}

/// Read the file, or `None` if it cannot be read — which is NOT the same as an
/// absent one, and the difference is the whole of a trim's safety: a trim that
/// read nothing would otherwise write nothing over a history that is merely
/// unreadable this instant.
///
/// Lossy rather than refusing a file that is not UTF-8, which is the rule the
/// rest of this shell already applies to input it did not write: a history
/// carrying one undecodable byte should not cost the operator every other line
/// in it. Empty lines are dropped, as busybox drops them on load.
fn read_history(path: &Path, max: usize) -> Option<Loaded> {
    let mut f = match std::fs::File::open(path) {
        Ok(f) => f,
        // Absent is an empty history, which is what a first session has.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Some(Loaded { kept: Vec::new(), total: 0, terminated: true });
        }
        Err(_) => return None,
    };
    // BOUNDED, and from the END. `HISTFILE` is a variable, so the file behind
    // it need not be a history at all: `std::fs::read` on `/dev/zero` returns
    // when the allocator gives up, which is a shell that dies at its first
    // prompt because of an assignment. The window is the tail because the tail
    // is what is kept; `take` is what bounds the device that has no end, since
    // its length reads as zero and the seek does nothing.
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let clipped = len > HISTORY_READ_MAX;
    if clipped {
        let _ = f.seek(std::io::SeekFrom::End(-(HISTORY_READ_MAX as i64)));
    }
    let mut bytes = Vec::new();
    if f.take(HISTORY_READ_MAX).read_to_end(&mut bytes).is_err() {
        return None;
    }
    let terminated = bytes.last().is_none_or(|b| *b == b'\n');
    let text = String::from_utf8_lossy(&bytes);
    // The window opened mid-file, so its first line is whatever was left of
    // the line the cut landed in -- half a command, which is not one.
    let text = match clipped {
        true => text.split_once('\n').map_or("", |(_, rest)| rest).to_string(),
        false => text.into_owned(),
    };
    // COUNTED first and collected second, so only the lines actually kept are
    // allocated and none of them is ever moved. Dropping the oldest as it went
    // would shift the whole vector once per line past the cap, and a file at
    // the trim threshold is four times the cap by construction -- so that is
    // the size this function most often runs at, not the exception.
    let total = text.lines().filter(|l| !l.is_empty()).count();
    let kept = text
        .lines()
        .filter(|l| !l.is_empty())
        .skip(total.saturating_sub(max))
        .map(str::to_string)
        .collect();
    Some(Loaded { kept, total, terminated })
}

/// Rewrite the file with only the lines that are kept, and answer how many
/// that was. `None` if nothing was rewritten, which leaves the caller's count
/// alone and so retries on the next command rather than never again.
///
/// Through a temporary and a `rename`, so a concurrent reader sees the old file
/// or the new one and never a half-written one. The temporary carries this
/// process's pid, as busybox's does, so two shells trimming at the same moment
/// do not write the same path. Unlike busybox, a failed rename takes the
/// temporary with it rather than leaving it beside the history for good.
fn rewrite_history(path: &Path, max: usize) -> Option<usize> {
    // Re-READ rather than writing this session's own memory out: another shell
    // may have appended lines this one has never seen, and they are history
    // too. It is the same read busybox does before its rewrite.
    //
    // A file that cannot be READ is not one to rewrite. Without this a
    // transient EACCES makes the read answer "empty" and the rewrite put that
    // over the history -- a trim that erases it, and reports success.
    let loaded = read_history(path, max)?;
    let mut body = String::new();
    for line in &loaded.kept {
        body.push_str(line);
        body.push('\n');
    }
    // Over what the path RESOLVES to. Appends follow a symlinked `HISTFILE`
    // to its target; renaming over the link itself would replace the link
    // with a regular file, and every later append would go there instead --
    // the target silently stopping at the last line before the first trim.
    // Putting the temporary beside the target is also what keeps the rename
    // within one filesystem.
    let target = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let mut name = target.as_os_str().to_os_string();
    name.push(format!(".{}.new", std::process::id()));
    let tmp = PathBuf::from(name);
    // `create_new`, so the open FAILS rather than following something already
    // at that name. The path is predictable -- a directory and a pid -- so
    // anyone who can write to the history's directory could leave a symlink
    // there, and `create(true).truncate(true)` would follow it and write the
    // operator's history over whatever it points at. Nothing is removed on
    // this arm: the file that is there is not ours.
    let opened = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&tmp);
    let Ok(mut f) = opened else {
        return None;
    };
    // Flushed to the DEVICE before the rename rather than only to the kernel.
    // The rename is atomic for the NAME and says nothing about the data behind
    // it, so a crash just after can leave the history naming blocks that were
    // never written -- an empty file where the whole history was. busybox's
    // `fclose` does not do this either, and this is the one place the cost is
    // affordable: a trim happens once every four times the kept size.
    // The trim puts a NEW inode in place of the old one, so without this every
    // trim would silently reset whatever mode the operator had set on their
    // history to this function's idea of one. 0600 is the fallback and not the
    // answer: the file that is there already has the mode it should keep.
    if let Ok(m) = std::fs::metadata(&target) {
        let _ = f.set_permissions(m.permissions());
    }
    if f.write_all(body.as_bytes()).is_err() || f.sync_data().is_err() {
        let _ = std::fs::remove_file(&tmp);
        return None;
    }
    drop(f);
    if std::fs::rename(&tmp, &target).is_err() {
        let _ = std::fs::remove_file(&tmp);
        return None;
    }
    Some(loaded.kept.len())
}

/// `$HISTFILE`, or `$HOME/.ash_history` written back into the variable.
///
/// An EMPTY `HISTFILE` names no file and is not defaulted, which is how ash
/// behaves for a different reason: it only defaults an UNSET one, then hands
/// the empty string to `fopen`, which fails. Same outcome, without the failed
/// open.
/// A RELATIVE one is resolved against the shell's directory rather than the
/// process's, for the reason completion resolves its own: `cd` moves `sh.cwd`
/// and never `chdir`s, and a profile that cd's runs BEFORE this. The variable
/// keeps what was written into it; only the file this session opens is
/// absolute.
fn history_path(sh: &mut Shell) -> Option<PathBuf> {
    if let Some(f) = sh.get_var("HISTFILE") {
        return (!f.is_empty()).then(|| sh.resolve(&f));
    }
    // An empty `HOME` names no history either, where busybox would join it to
    // nothing and write `/.ash_history` — a file in the root of the
    // filesystem, from a variable nobody set.
    let home = sh.get_var("HOME").filter(|h| !h.is_empty())?;
    let path = Path::new(&home).join(".ash_history");
    // Only when the assignment can SUCCEED. A readonly `HISTFILE` refusing one
    // is a diagnostic on stderr before the first prompt, about a default the
    // operator never asked for, and the history it names is used either way.
    if !sh.vars.get("HISTFILE").is_some_and(|v| v.readonly) {
        let _ = sh.set_var("HISTFILE", &path.to_string_lossy());
    }
    Some(sh.resolve(&path.to_string_lossy()))
}

/// `$HISTFILESIZE`, on ash's exact rule (`size_from_HISTFILESIZE`,
/// lineedit.c:1403): unset is the built-in maximum, anything above it is
/// capped, and zero or below asks for ONE line rather than for none — so
/// `HISTFILESIZE=0` does not turn history off.
///
/// Read with `atoi`'s tolerance, which is what ash uses: leading ASCII blanks
/// and a sign, then digits up to the first character that is not one, so `50x`
/// is fifty and `x` is zero. `parse` would reject both and make them one line.
///
/// One value does NOT match: where C's `atoi` overflows an `int`, glibc's
/// wraps to a negative and busybox reads `HISTFILESIZE=4000000000` as one
/// line. This saturates to the ceiling instead, which is the more defensible
/// reading of a number that large and not worth reproducing signed overflow
/// for.
fn history_size(sh: &Shell) -> NonZeroUsize {
    let Some(v) = sh.get_var("HISTFILESIZE") else {
        return DEFAULT_MAX;
    };
    let s = v.trim_start_matches(|c: char| c.is_ascii_whitespace());
    let (neg, digits) = match s.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, s.strip_prefix('+').unwrap_or(s)),
    };
    let mut n: usize = 0;
    for c in digits.chars() {
        let Some(d) = c.to_digit(10) else {
            break;
        };
        n = n.saturating_mul(10).saturating_add(d as usize);
    }
    match (neg, NonZeroUsize::new(n)) {
        (false, Some(n)) => n.min(DEFAULT_MAX),
        // Zero or below asks for ONE line, not for none.
        _ => NonZeroUsize::MIN,
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

/// Where the word at or after `pos` ends: the non-alphanumerics first, then the
/// run of alphanumerics — readline's `forward-word`.
///
/// ALPHANUMERIC, which is deliberately not `word_start`'s blanks-only rule:
/// `foo-bar` is one word to Ctrl-W and two to these, and both are shipped.
/// Rust's classifier is not glibc's — see
/// `the_word_characters_are_rusts_not_readlines`.
fn word_forward(buf: &str, pos: usize) -> usize {
    let mut at = pos;
    while at < buf.len() {
        match buf.get(at..).and_then(|s| s.chars().next()) {
            Some(c) if !c.is_alphanumeric() => at = next_boundary(buf, at),
            _ => break,
        }
    }
    while at < buf.len() {
        match buf.get(at..).and_then(|s| s.chars().next()) {
            Some(c) if c.is_alphanumeric() => at = next_boundary(buf, at),
            _ => break,
        }
    }
    at
}

/// Where the word before `pos` begins, by the same rule backwards —
/// readline's `backward-word`.
fn word_back(buf: &str, pos: usize) -> usize {
    let mut at = pos;
    while at > 0 {
        let prev = prev_boundary(buf, at);
        match buf.get(prev..at).and_then(|s| s.chars().next()) {
            Some(c) if !c.is_alphanumeric() => at = prev,
            _ => break,
        }
    }
    while at > 0 {
        let prev = prev_boundary(buf, at);
        match buf.get(prev..at).and_then(|s| s.chars().next()) {
            Some(c) if c.is_alphanumeric() => at = prev,
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
        Err(e) => return ScriptLine::Failed(crate::exec::strerror(&e)),
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
            Err(e) => return Err(crate::exec::strerror(&e)),
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
            Err(e) => return Err(crate::exec::strerror(&e)),
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
            src.seek(std::io::SeekFrom::Current(-back))
                .map_err(|e| crate::exec::strerror(&e))?;
        }
        return Ok(bytes);
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn hist(lines: &[&str]) -> Editor {
        Editor {
            history: lines.iter().map(|s| (*s).to_string()).collect(),
            max: DEFAULT_MAX,
            hist: None,
            kill: String::new(),
        }
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

    /// Alt-B / Alt-F walk ALPHANUMERIC words, which is not Ctrl-W's rule: the
    /// non-word characters first, then the run of word characters.
    #[test]
    fn the_alt_words_are_alphanumeric_runs() {
        for (line, pos, want) in [
            ("echo foo bar", 0, 4),
            ("echo foo bar", 4, 8),
            // `-` is not alphanumeric, so `foo-bar` is TWO words here where
            // `word_start` sees one.
            ("echo foo-bar", 4, 8),
            ("echo foo", 8, 8),
            ("", 0, 0),
            ("   ", 0, 3),
            ("echo   foo", 4, 10),
            // The leading skip is over NON-WORD characters, not over blanks:
            // starting on punctuation has to cross it, or the walk cannot
            // move at all from here.
            ("echo -bar", 5, 9),
            // `_` is not alphanumeric either, which is where this rule and
            // most editors' idea of a word part company.
            ("echo foo_bar", 4, 8),
            // A CJK character is alphanumeric, so it is word text.
            ("日本 x", 0, 6),
        ] {
            assert_eq!(word_forward(line, pos), want, "forward {line:?} at {pos}");
        }
        for (line, pos, want) in [
            ("echo foo bar", 12, 9),
            ("echo foo bar", 9, 5),
            ("echo foo-bar", 12, 9),
            ("echo foo", 0, 0),
            ("echo foo bar   ", 15, 9),
            ("", 0, 0),
            ("echo f", 6, 5),
            // Ending on punctuation, the mirror of the forward case above.
            ("echo foo-", 9, 5),
            ("echo foo_bar", 12, 9),
            ("echo 日本 x", 12, 5),
        ] {
            assert_eq!(word_back(line, pos), want, "back {line:?} at {pos}");
        }
    }

    /// Where this classifier and readline's PART COMPANY, recorded rather than
    /// left to be discovered. `char::is_alphanumeric` is Alphabetic plus the
    /// three Unicode number categories; glibc's `iswalnum` is a different set
    /// and is not even locale-independent, so exact parity would need category
    /// tables `std` does not expose and a dependency this crate will not take.
    /// Every row was measured against bash 5.2 through a PTY.
    #[test]
    fn the_word_characters_are_rusts_not_readlines() {
        // `A<c>B` walked back from the end: a word character makes the whole
        // run one word, a separator stops the walk at the `B`.
        let one_word = |c: char| {
            let line = format!("A{c}B");
            word_back(&line, line.len()) == 0
        };
        for c in ['a', 'α', '日', '1', 'Ⅷ'] {
            assert!(one_word(c), "{c:?} should be word text");
        }
        for c in ['_', '-', ' ', '/', '.'] {
            assert!(!one_word(c), "{c:?} should not be word text");
        }
        // Rust takes the `No` category and bash does not...
        for c in ['²', '½', '①'] {
            assert!(one_word(c), "{c:?} is word text here but not in bash");
        }
        // ...and refuses a nonspacing combining mark, which bash accepts. So
        // DECOMPOSED text is where the two visibly differ: `é` written as
        // `e` + U+0301 is two words here and one there.
        assert!(!one_word('\u{301}'), "a combining mark is a separator here");
    }

    /// Alt-B and Alt-F, end to end. Every case here is what bash 5.2 does with
    /// the same keys, and readline answers to either case of the letter.
    #[test]
    fn the_alt_keys_move_by_a_word() {
        let mut ed = Editor::new();
        assert_eq!(submitted(&mut ed, b"echo foo bar baz\x1bbX\r"), "echo foo bar Xbaz");
        assert_eq!(submitted(&mut ed, b"echo foo bar baz\x1bb\x1bbX\r"), "echo foo Xbar baz");
        assert_eq!(submitted(&mut ed, b"echo foo-bar\x1bbX\r"), "echo foo-Xbar");
        // At either end the move is a no-op rather than an error.
        assert_eq!(submitted(&mut ed, b"echo foo\x01\x1bbX\r"), "Xecho foo");
        assert_eq!(submitted(&mut ed, b"echo foo\x1bfX\r"), "echo fooX");
        assert_eq!(submitted(&mut ed, b"echo foo bar\x01\x1bfX\r"), "echoX foo bar");
        assert_eq!(submitted(&mut ed, b"echo foo bar\x01\x1bf\x1bfX\r"), "echo fooX bar");
        // Uppercase is the same binding -- for all three letters, since each
        // is a separate arm and an unbound one types its byte instead.
        assert_eq!(submitted(&mut ed, b"echo foo bar\x1bBX\r"), "echo foo Xbar");
        assert_eq!(submitted(&mut ed, b"echo foo bar\x01\x1bFX\r"), "echoX foo bar");
        let mut ed = Editor::new();
        assert_eq!(submitted(&mut ed, b"echo aa bb\x01\x1bD\x05\x19\r"), " aa bbecho");
        // The cursor moves by BYTES over characters that are not one byte.
        assert_eq!(submitted(&mut ed, "echo 日本 x\x1bb\x1bbX\r".as_bytes()), "echo X日本 x");
    }

    /// Alt-Backspace and Ctrl-W take DIFFERENT words, which is the whole
    /// reason both exist: one takes a path, the other its last segment.
    #[test]
    fn alt_backspace_and_ctrl_w_take_different_words() {
        let mut ed = Editor::new();
        assert_eq!(submitted(&mut ed, b"echo foo-bar\x1b\x7fX\r"), "echo foo-X");
        assert_eq!(submitted(&mut ed, b"echo foo-bar\x17X\r"), "echo X");
        // `ESC ^H` is the other spelling of the same key.
        assert_eq!(submitted(&mut ed, b"echo foo-bar\x1b\x08X\r"), "echo foo-X");
    }

    /// The Alt- kills feed the SAME buffer as Ctrl-K/U/W and join the same
    /// accumulation run, on the side each of them took from.
    #[test]
    fn the_alt_kills_feed_the_kill_buffer() {
        let mut ed = Editor::new();
        assert_eq!(submitted(&mut ed, b"echo foo-bar\x1b\x7f\x19\r"), "echo foo-bar");
        // Alt-D is a FORWARD kill, so a second one appends: `aa` then ` bb`.
        let mut ed = Editor::new();
        let mut keys = b"echo aa bb cc\x01".to_vec();
        keys.extend(std::iter::repeat_n(b'\x06', 5));
        keys.extend_from_slice(b"\x1bd\x1bd\x05\x19\r");
        assert_eq!(submitted(&mut ed, &keys), "echo  ccaa bb");
        // ...and a Ctrl-W after one prepends to what it took, so the two
        // kinds of kill share one run rather than one each.
        let mut ed = Editor::new();
        let mut keys = b"echo aa bb cc\x01".to_vec();
        keys.extend(std::iter::repeat_n(b'\x06', 5));
        keys.extend_from_slice(b"\x1bd\x17\x05\x19\r");
        assert_eq!(submitted(&mut ed, &keys), " bb ccecho aa");
        // An Alt-D with nothing in front of it keeps the buffer, as every
        // other empty kill does.
        let mut ed = Editor::new();
        assert_eq!(submitted(&mut ed, b"echo aa bb\x17\x05\x1bd\x19\r"), "echo aa bb");
        // Alt-Backspace is a BACKWARD kill, which only shows when it is the
        // SECOND kill of a run -- the same blind spot Ctrl-K has, since a
        // first kill lands the same way whichever side it is put on. Alt-D
        // first, so the buffer it prepends to is not empty.
        let mut ed = Editor::new();
        let mut keys = b"echo aa bb\x01".to_vec();
        keys.extend(std::iter::repeat_n(b'\x06', 5));
        keys.extend_from_slice(b"\x1bd\x1b\x7f\x05\x19\r");
        assert_eq!(submitted(&mut ed, &keys), " bbecho aa");
    }

    /// A meta byte that is NOT a binding still comes back as an ordinary key.
    /// This is where td-sh diverges from readline deliberately: bash discards
    /// it, and discarding it would take ESC-then-Enter with it.
    #[test]
    fn an_unbound_meta_byte_is_still_typed() {
        let mut ed = Editor::new();
        assert_eq!(submitted(&mut ed, b"echo hi\x1bzZ\r"), "echo hizZ");
        assert_eq!(submitted(&mut ed, b"echo hi\x1b\r"), "echo hi");
    }

    /// What a kill key takes, Ctrl-Y puts back -- and the cursor lands AFTER
    /// it, so a second Ctrl-Y inserts a second copy rather than overwriting.
    #[test]
    fn a_kill_can_be_yanked_back() {
        let mut ed = Editor::new();
        assert_eq!(submitted(&mut ed, b"echo abc def\x17\x19\r"), "echo abc def");
        // The `X` proves where the cursor was left.
        assert_eq!(submitted(&mut ed, b"echo abc \x17\x19X\r"), "echo abc X");
        assert_eq!(submitted(&mut ed, b"echo abc def\x17\x19\x19\r"), "echo abc defdef");
        // Ctrl-Y with nothing killed is a no-op, not an empty insert that
        // moves the cursor.
        let mut ed = Editor::new();
        assert_eq!(submitted(&mut ed, b"abc\x19X\r"), "abcX");
    }

    /// Consecutive kills ACCUMULATE, and on the side they were taken from --
    /// so `^W ^W` yanks the two words back in the order they were typed rather
    /// than reversed. Every case here is what bash 5.2 does with the same keys.
    #[test]
    fn consecutive_kills_accumulate_in_typing_order() {
        let mut ed = Editor::new();
        assert_eq!(submitted(&mut ed, b"echo abc def ghi\x17\x17\x19\r"), "echo abc def ghi");
        assert_eq!(submitted(&mut ed, b"echo a b c d\x17\x17\x17\x19\r"), "echo a b c d");
        // A FORWARD kill appends where a backward one prepends: Ctrl-K takes
        // `abc def`, Ctrl-U then takes `echo ` from in front of it.
        assert_eq!(
            submitted(&mut ed, b"echo abc def\x01\x06\x06\x06\x06\x06\x0b\x15\x19\r"),
            "echo abc def"
        );
        // The same pair the other way round, which is the only way a Ctrl-K
        // is ever the SECOND kill of a run and so the only way its side
        // shows: Ctrl-U leaves the rest of the line with the cursor at its
        // start, and the Ctrl-K after it appends rather than prepending.
        assert_eq!(
            submitted(&mut ed, b"echo abc def\x01\x06\x06\x06\x06\x06\x15\x0b\x19\r"),
            "echo abc def"
        );
    }

    /// The run is only ever CONSECUTIVE kills: any other key ends it, and the
    /// next kill starts the buffer over. A yank is one of those other keys,
    /// and so is a history recall, which replaces the whole line.
    #[test]
    fn a_key_that_is_not_a_kill_ends_the_run() {
        let mut ed = Editor::new();
        // Ctrl-B Ctrl-F leaves the cursor exactly where it was, so what breaks
        // the run is the KEY rather than the movement.
        assert_eq!(submitted(&mut ed, b"echo abc def ghi\x17\x02\x06\x17\x19\r"), "echo abc def ");
        // A yank between two kills likewise: the second Ctrl-W starts fresh,
        // so the third accumulates onto it alone.
        assert_eq!(submitted(&mut ed, b"echo abc def\x17\x19\x17\x17\x19\r"), "echo abc def");
        // Ctrl-P: the recalled line's own Ctrl-W must not prepend to what was
        // killed off the line it replaced.
        let mut ed = Editor::new();
        ed.remember("echo hello world");
        assert_eq!(submitted(&mut ed, b"echo aa bb\x17\x10\x17\x19\r"), "echo hello world");
    }

    /// A kill that takes NOTHING keeps the buffer -- Ctrl-K at the end of a
    /// line must not throw away what is in it -- but still ends the run.
    #[test]
    fn a_kill_that_takes_nothing_keeps_the_buffer_and_ends_the_run() {
        let mut ed = Editor::new();
        assert_eq!(submitted(&mut ed, b"echo abc\x01\x0b\x0b\x19\r"), "echo abc");
        // The empty Ctrl-K sits between two kills: with the run ended, the
        // second Ctrl-W replaces `def` rather than prepending to it.
        let mut ed = Editor::new();
        assert_eq!(submitted(&mut ed, b"echo abc def\x17\x0b\x17\x19\r"), "echo abc ");
        // ...and with the run ALREADY closed, which is the case an operator
        // reaches by killing, moving, and pressing Ctrl-K at the end of a
        // line. Both branches keep the buffer, and only one of them was
        // reachable from the two cases above.
        let mut ed = Editor::new();
        assert_eq!(submitted(&mut ed, b"echo aa bb\x17\x02\x06\x0b\x19\r"), "echo aa bb");
    }

    /// Every kill ends the run when it takes nothing, not only Ctrl-K. An
    /// empty Ctrl-W or Ctrl-U is what the cursor at column 0 gives.
    #[test]
    fn an_empty_kill_of_any_kind_ends_the_run() {
        // Ctrl-U leaves the cursor at column 0, so the key after it kills
        // nothing; the Ctrl-K then starts the buffer over rather than
        // appending `bb` to `echo aa `.
        for empty in [b'\x17', b'\x15', b'\x7f', b'\x04'] {
            let mut ed = Editor::new();
            let mut keys = b"echo aa bb\x01\x06\x06\x06\x06\x06\x06\x06\x06\x15".to_vec();
            keys.push(empty);
            keys.extend_from_slice(b"\x0b\x19\r");
            // Only Ctrl-D takes anything there: backspace at column 0 has
            // nothing before the cursor either, and it ends the run all the
            // same, which is the property.
            let want = match empty {
                b'\x04' => "b",
                _ => "bb",
            };
            assert_eq!(submitted(&mut ed, &keys), want, "key {empty:#04x}");
        }
    }

    /// Deleting a character is not killing it: neither key feeds the buffer,
    /// and neither clears it.
    #[test]
    fn backspace_and_delete_forward_are_not_kills() {
        let mut ed = Editor::new();
        // Ctrl-K fills the buffer with `abc` and Ctrl-Y puts it back; the
        // backspace then takes the `c` without the buffer noticing.
        assert_eq!(submitted(&mut ed, b"abc\x01\x0b\x19\x7f\x19\r"), "ababc");
        let mut ed = Editor::new();
        // Same with delete-forward, which is Ctrl-D anywhere but an empty line.
        assert_eq!(submitted(&mut ed, b"abc\x01\x0b\x19\x01\x04\x19\r"), "abcbc");
    }

    /// Both halves work at the CURSOR: Ctrl-W takes the word in front of it
    /// rather than the rest of the line, and Ctrl-Y puts the buffer there
    /// rather than at the end.
    #[test]
    fn the_kill_and_the_yank_act_at_the_cursor() {
        let mut ed = Editor::new();
        // Cursor between `def` and `ghi`: the Ctrl-W takes `def` alone, and
        // Ctrl-E then puts the yank at the end so the two are told apart.
        let mut keys = b"echo abc def ghi\x01".to_vec();
        keys.extend(std::iter::repeat_n(b'\x06', 12));
        keys.extend_from_slice(b"\x17\x05\x19\r");
        assert_eq!(submitted(&mut ed, &keys), "echo abc  ghidef");
        // A yank with text after the cursor lands where the cursor is.
        let mut ed = Editor::new();
        assert_eq!(submitted(&mut ed, b"echo aa bb\x17\r"), "echo aa ");
        assert_eq!(submitted(&mut ed, b"echo XY\x01\x06\x06\x06\x06\x06\x19\r"), "echo bbXY");
    }

    /// The buffer outlives the LINE, which is what makes the kill keys a way
    /// to move text between commands.
    #[test]
    fn the_kill_buffer_outlives_the_line() {
        let mut ed = Editor::new();
        assert_eq!(submitted(&mut ed, b"echo abc def\x17\r"), "echo abc ");
        assert_eq!(submitted(&mut ed, b"echo one \x19\r"), "echo one def");
        // The RUN does not, though nothing can tell: a fresh line's buffer is
        // empty, so its first kill takes nothing and closes the run itself
        // whatever it inherited. This pins only that the Ctrl-W replaces
        // `def` rather than prepending `two` to it.
        assert_eq!(submitted(&mut ed, b"echo one two\x17\x19\r"), "echo one two");
    }

    /// The buffer is text rather than bytes, so a kill that ends mid-character
    /// is not representable and a yank cannot split one. The `X` is what pins
    /// the cursor: it moves by BYTES, and a count of CHARACTERS would leave it
    /// inside the `本` here rather than past the `x`.
    #[test]
    fn a_multibyte_kill_round_trips() {
        let mut ed = Editor::new();
        let keys = "echo 日本語 x\x17\x17\x19X\r".as_bytes();
        assert_eq!(submitted(&mut ed, keys), "echo 日本語 xX");
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
        // ESC followed by a byte that starts no sequence and is no BINDING:
        // the byte comes back, so ESC-then-Enter submits rather than eating
        // the Enter. `z` rather than `b`, which is Alt-B -- see
        // `an_unbound_meta_byte_is_still_typed`.
        assert_eq!(submitted(&mut ed, b"ab\x1b\r"), "ab");
        assert_eq!(submitted(&mut ed, b"a\x1bz\r"), "az");
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

    fn hist_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("td-sh-hist-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// A line entered is appended to the file as it is entered, and the next
    /// session starts with it. Appending per line rather than rewriting at
    /// exit is what lets two shells INTERLEAVE instead of the last one out
    /// discarding the other's lines.
    #[test]
    fn the_history_outlives_the_session_and_two_shells_interleave() {
        let dir = hist_dir("outlives");
        let path = dir.join("h");
        let mut sh = crate::exec::Shell::new_for_test();
        sh.set_var("HISTFILE", &path.to_string_lossy()).unwrap();

        let mut a = Editor::new();
        a.open_history(&mut sh);
        a.remember("one");
        // Written as it was entered, not held until something closes.
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "one\n");

        // A SECOND shell, opened while the first is still running.
        let mut b = Editor::new();
        b.open_history(&mut sh);
        assert_eq!(b.history, ["one"]);
        b.remember("two");
        a.remember("three");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "one\ntwo\nthree\n");

        // A third session sees all three, in the order they were typed.
        let mut c = Editor::new();
        c.open_history(&mut sh);
        assert_eq!(c.history, ["one", "two", "three"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The file is trimmed LAZILY -- only once it runs `TRIM_FACTOR` times
    /// past what is kept -- and the trim keeps the newest lines. Rewriting on
    /// every command would turn one append into a whole-file read and write.
    #[test]
    fn the_file_is_trimmed_lazily_and_keeps_the_newest() {
        let dir = hist_dir("trim");
        let path = dir.join("h");
        let mut sh = crate::exec::Shell::new_for_test();
        sh.set_var("HISTFILE", &path.to_string_lossy()).unwrap();
        sh.set_var("HISTFILESIZE", "4").unwrap();
        let mut ed = Editor::new();
        ed.open_history(&mut sh);
        assert_eq!(ed.max.get(), 4);

        // Up to the threshold the file only grows: 16 lines is 4 * 4, and the
        // trim fires when the count goes PAST it.
        for i in 0..16 {
            ed.remember(&format!("line{i}"));
        }
        assert_eq!(std::fs::read_to_string(&path).unwrap().lines().count(), 16);
        ed.remember("line16");
        // Now rewritten to the last four, newest last.
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "line13\nline14\nline15\nline16\n");
        // ...and the in-memory list agrees with the file.
        assert_eq!(ed.history, ["line13", "line14", "line15", "line16"]);
        // No temporary left beside it.
        let left: Vec<_> = std::fs::read_dir(&dir).unwrap().flatten().map(|e| e.file_name()).collect();
        assert_eq!(left.len(), 1, "a temporary was left behind: {left:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A trim writes what is in the FILE, not what this session remembers, so
    /// a sibling shell's lines survive it.
    #[test]
    fn a_trim_keeps_a_sibling_shells_lines() {
        let dir = hist_dir("sibling");
        let path = dir.join("h");
        let mut sh = crate::exec::Shell::new_for_test();
        sh.set_var("HISTFILE", &path.to_string_lossy()).unwrap();
        sh.set_var("HISTFILESIZE", "2").unwrap();
        let mut a = Editor::new();
        a.open_history(&mut sh);
        for i in 0..8 {
            a.remember(&format!("a{i}"));
        }
        // A sibling appends without this session knowing.
        {
            let mut f = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
            f.write_all(b"sibling\n").unwrap();
        }
        // ...and the next line takes the count past 2 * 4 and trims.
        a.remember("a8");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "sibling\na8\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A trim writes through a name anyone who can write to the directory can
    /// guess -- a pid -- so it must not FOLLOW what is already there. With a
    /// symlink in the way the trim declines and the target is untouched;
    /// `create(true).truncate(true)` would have written the operator's history
    /// over it.
    #[test]
    fn a_trim_refuses_to_follow_a_symlink_at_its_temporary() {
        let dir = hist_dir("symlink");
        let path = dir.join("h");
        let decoy = dir.join("decoy");
        std::fs::write(&decoy, "PRECIOUS\n").unwrap();
        let mut tmp = path.as_os_str().to_os_string();
        tmp.push(format!(".{}.new", std::process::id()));
        std::os::unix::fs::symlink(&decoy, PathBuf::from(&tmp)).unwrap();

        let mut sh = crate::exec::Shell::new_for_test();
        sh.set_var("HISTFILE", &path.to_string_lossy()).unwrap();
        sh.set_var("HISTFILESIZE", "2").unwrap();
        let mut ed = Editor::new();
        ed.open_history(&mut sh);
        for i in 0..10 {
            ed.remember(&format!("x{i}"));
        }
        assert_eq!(std::fs::read_to_string(&decoy).unwrap(), "PRECIOUS\n", "the decoy was written");
        // The trim declined, so the history is whole rather than truncated --
        // and the symlink is still there, since it was never ours to remove.
        assert_eq!(std::fs::read_to_string(&path).unwrap().lines().count(), 10);
        assert!(std::fs::symlink_metadata(PathBuf::from(&tmp)).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A trim renames over what the path RESOLVES to, so a symlinked
    /// `HISTFILE` is still a symlink afterwards. Renaming over the link would
    /// replace it with a regular file and leave the target at whatever line it
    /// held when the first trim ran, with nothing saying so.
    #[test]
    fn a_trim_keeps_a_symlinked_history_a_symlink() {
        let dir = hist_dir("symtarget");
        let real = dir.join("real");
        let link = dir.join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let mut sh = crate::exec::Shell::new_for_test();
        sh.set_var("HISTFILE", &link.to_string_lossy()).unwrap();
        sh.set_var("HISTFILESIZE", "2").unwrap();
        let mut ed = Editor::new();
        ed.open_history(&mut sh);
        for i in 0..10 {
            ed.remember(&format!("y{i}"));
        }
        assert!(std::fs::symlink_metadata(&link).unwrap().is_symlink(), "the link was replaced");
        // The trim fired on `y8`, keeping two, and `y9` was appended after it:
        // between trims the file is allowed to stand above the kept size,
        // which is what makes trimming lazy.
        assert_eq!(std::fs::read_to_string(&real).unwrap(), "y7\ny8\ny9\n");
        // ...and appending after the trim still reaches the target.
        ed.remember("after");
        assert_eq!(std::fs::read_to_string(&real).unwrap(), "y7\ny8\ny9\nafter\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A `HISTFILE` naming something that is not a history cannot cost the
    /// shell its first prompt: the read is bounded, and bounded from the END,
    /// since the end is what is kept.
    #[test]
    fn the_load_is_bounded_and_reads_the_tail() {
        let dir = hist_dir("bounded");
        let path = dir.join("big");
        // Past the window, so the read seeks: the last lines are the ones
        // that come back, and the fragment the cut landed in does not.
        let mut body = String::new();
        let mut n = 0u32;
        while body.len() < (HISTORY_READ_MAX as usize) + 4096 {
            body.push_str(&format!("cmd{n}\n"));
            n += 1;
        }
        std::fs::write(&path, &body).unwrap();
        let kept = read_history(&path, 3).unwrap().kept;
        assert_eq!(kept, [format!("cmd{}", n - 3), format!("cmd{}", n - 2), format!("cmd{}", n - 1)]);
        // Every line that came back is whole -- the partial one at the window's
        // edge was dropped rather than offered as a command.
        let all = read_history(&path, 100_000).unwrap().kept;
        assert!(all.iter().all(|l| l.starts_with("cmd")), "a fragment survived: {:?}", all.first());
        // A file with no end at all returns rather than growing without bound.
        if std::fs::metadata("/dev/zero").is_ok() {
            let kept = read_history(Path::new("/dev/zero"), 5).unwrap().kept;
            assert!(kept.len() <= 5);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A relative `HISTFILE` follows the SHELL's directory, not the process's
    /// -- `cd` moves only the former, and a profile that cd's runs before the
    /// history is opened.
    #[test]
    fn a_relative_history_file_follows_the_shell() {
        let dir = hist_dir("relative");
        let mut sh = crate::exec::Shell::new_for_test();
        sh.cwd = dir.clone();
        sh.set_var("HISTFILE", "kept-here").unwrap();
        assert_eq!(history_path(&mut sh), Some(dir.join("kept-here")));
        // The VARIABLE keeps what was written into it; only the file this
        // session opens is absolute.
        assert_eq!(sh.get_var("HISTFILE").as_deref(), Some("kept-here"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The file is created 0600 and KEEPS whatever mode it has across a trim,
    /// which puts a new inode in place of the old one. Asserted as "nothing
    /// for group or other" rather than as an exact number, since the mode a
    /// create asks for is masked by the process umask.
    #[test]
    fn the_history_is_private_and_a_trim_does_not_reset_its_mode() {
        use std::os::unix::fs::PermissionsExt;
        let dir = hist_dir("mode");
        let path = dir.join("h");
        let mut sh = crate::exec::Shell::new_for_test();
        sh.set_var("HISTFILE", &path.to_string_lossy()).unwrap();
        sh.set_var("HISTFILESIZE", "2").unwrap();
        let mut ed = Editor::new();
        ed.open_history(&mut sh);
        ed.remember("secret");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o077, 0, "a history readable by anyone else: {:o}", mode);
        // A mode the operator chose survives the trim.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o400)).unwrap();
        for i in 0..10 {
            ed.remember(&format!("z{i}"));
        }
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o400, "the trim reset the mode: {mode:o}");
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A file left ending mid-line gets that line ended before the next one is
    /// appended. Without it the two GLUE, and the pair reads back as a single
    /// history entry naming a command nobody typed.
    #[test]
    fn a_file_cut_off_mid_line_does_not_glue_the_next_command_on() {
        let dir = hist_dir("glue");
        let path = dir.join("h");
        std::fs::write(&path, b"alpha\nbeta").unwrap();
        let mut sh = crate::exec::Shell::new_for_test();
        sh.set_var("HISTFILE", &path.to_string_lossy()).unwrap();
        let mut ed = Editor::new();
        ed.open_history(&mut sh);
        ed.remember("gamma");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "alpha\nbeta\ngamma\n");
        // Once, not before every line.
        ed.remember("delta");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "alpha\nbeta\ngamma\ndelta\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A trim that cannot READ the file must not write over it. Reading
    /// nothing and rewriting that is a trim that ERASES a history which was
    /// merely unreadable for an instant, and reports success.
    #[test]
    fn a_trim_that_cannot_read_leaves_the_file_alone() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let dir = hist_dir("unreadable");
        let path = dir.join("h");
        std::fs::write(&path, "keep1\nkeep2\n").unwrap();
        // Root can read anything, so there is no unreadable file to make.
        if std::fs::metadata(&path).unwrap().uid() == 0 {
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
        assert!(read_history(&path, 2).is_none(), "an unreadable file read as empty");
        assert!(rewrite_history(&path, 2).is_none(), "an unreadable file was rewritten");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "keep1\nkeep2\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A readonly `HISTFILE` that nobody set still gets a working history: the
    /// default is used and the assignment that would fail is not attempted.
    #[test]
    fn a_readonly_histfile_still_gets_a_history() {
        let dir = hist_dir("readonly");
        let mut sh = crate::exec::Shell::new_for_test();
        sh.set_var("HOME", &dir.to_string_lossy()).unwrap();
        sh.set_var("HISTFILE", "placeholder").unwrap();
        // Readonly AND unset, which is what `readonly HISTFILE` in a profile
        // leaves behind.
        if let Some(v) = sh.vars.get_mut("HISTFILE") {
            v.readonly = true;
            v.value = None;
        }
        assert_eq!(history_path(&mut sh), Some(dir.join(".ash_history")));
        assert_eq!(sh.get_var("HISTFILE"), None, "the readonly name was written to");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `HISTFILE` is resolved once and defaulted INTO the variable, and
    /// `HISTFILESIZE` follows ash's clamp: capped above, one line below, and
    /// `atoi`'s tolerance for what is not a number.
    #[test]
    fn the_history_variables_follow_ash() {
        let dir = hist_dir("vars");
        let mut sh = crate::exec::Shell::new_for_test();
        sh.set_var("HOME", &dir.to_string_lossy()).unwrap();
        // Unset: defaulted to $HOME/.ash_history and written back, so
        // `echo $HISTFILE` names the file in use.
        assert_eq!(history_path(&mut sh), Some(dir.join(".ash_history")));
        assert_eq!(sh.get_var("HISTFILE").as_deref(), Some(&*dir.join(".ash_history").to_string_lossy()));
        // Set-and-empty names no file, and is not defaulted over.
        sh.set_var("HISTFILE", "").unwrap();
        assert_eq!(history_path(&mut sh), None);

        let mut s = crate::exec::Shell::new_for_test();
        assert_eq!(history_size(&s).get(), HISTORY_MAX, "unset is the maximum");
        s.set_var("HISTFILESIZE", "10").unwrap();
        assert_eq!(history_size(&s).get(), 10);
        s.set_var("HISTFILESIZE", "0").unwrap();
        assert_eq!(history_size(&s).get(), 1, "zero asks for one line, not for none");
        s.set_var("HISTFILESIZE", "-5").unwrap();
        assert_eq!(history_size(&s).get(), 1);
        s.set_var("HISTFILESIZE", "99999").unwrap();
        assert_eq!(history_size(&s).get(), HISTORY_MAX, "capped at the built-in maximum");
        s.set_var("HISTFILESIZE", "50x").unwrap();
        assert_eq!(history_size(&s).get(), 50, "atoi stops at the first non-digit");
        s.set_var("HISTFILESIZE", "x").unwrap();
        assert_eq!(history_size(&s).get(), 1, "...and reads nothing as zero");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Loading keeps the LAST `max` lines, drops empty ones, and counts what
    /// the whole file held -- the count being what the trim threshold is
    /// measured against. A file that is not UTF-8 is read lossily rather than
    /// costing the operator every line in it.
    #[test]
    fn loading_keeps_the_newest_lines_and_counts_the_rest() {
        let dir = hist_dir("load");
        let path = dir.join("h");
        std::fs::write(&path, "a\n\nb\nc\n\nd\n").unwrap();
        let l = read_history(&path, 2).unwrap();
        assert_eq!(l.kept, ["c", "d"]);
        assert_eq!(l.total, 4);
        assert_eq!(read_history(&path, 99).unwrap().total, 4);
        // Keeping none is what a cap of none means, not keeping everything.
        assert!(read_history(&path, 0).unwrap().kept.is_empty());
        // A missing file is an empty history; UNREADABLE is not the same
        // answer, and a trim turns on the difference.
        let l = read_history(&dir.join("nope"), 9).unwrap();
        assert!(l.kept.is_empty() && l.total == 0);
        // Undecodable bytes become U+FFFD; the other lines are still there.
        std::fs::write(&path, b"ok\n\xff\xfe\nlast\n").unwrap();
        let l = read_history(&path, 9).unwrap();
        assert_eq!(l.total, 3);
        assert_eq!(l.kept.first().map(String::as_str), Some("ok"));
        assert_eq!(l.kept.last().map(String::as_str), Some("last"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The LAST search prompt in a drawn transcript. `drawn` accumulates every
    /// redraw, so it holds every PREFIX of the pattern -- asserting that it
    /// `contains` one says only that it was passed through on the way, which
    /// is true whether or not the key that should have shortened it worked.
    fn last_search(drawn: &str) -> String {
        let mut pattern = String::new();
        for (i, m) in drawn.match_indices("(reverse-i-search)'") {
            let Some(rest) = drawn.get(i.saturating_add(m.len())..) else {
                continue;
            };
            if let Some((p, _)) = rest.split_once("': ") {
                pattern = p.to_string();
            }
        }
        pattern
    }

    /// Ctrl-R searches backwards as it is typed, puts the cursor ON the match,
    /// and steps to the next older one on another Ctrl-R.
    #[test]
    fn reverse_search_walks_back_through_the_matches() {
        let mut ed = Editor::new();
        for line in ["echo one", "grep alpha", "echo two", "ls -l"] {
            ed.remember(line);
        }
        // `\x12` then `ec`: the newest line containing `ec` is `echo two`.
        assert_eq!(submitted(&mut ed, b"\x12ec\r"), "echo two");
        // A second Ctrl-R steps PAST it to the older `echo one`.
        assert_eq!(submitted(&mut ed, b"\x12ec\x12\r"), "echo one");
        // ...and a third finds nothing older, so it stays where it was.
        assert_eq!(submitted(&mut ed, b"\x12ec\x12\x12\r"), "echo one");
        // The search prompt is readline's, and the cursor sits on the match:
        // `alpha` starts at column 5 of `grep alpha`, so with a two-character
        // prompt the cursor is at column 7.
        let (_, drawn) = typed(&mut ed, b"\x12alpha\r", 80, true);
        assert!(drawn.contains("(reverse-i-search)'alpha': "), "{drawn:?}");
        assert!(drawn.contains("$ grep alpha\x1b[K\r\x1b[7C"), "cursor not on the match: {drawn:?}");
    }

    /// Backspace widens the pattern again, and a key that would match nothing
    /// is TAKEN BACK -- so what is on screen always matches something -- with
    /// a bell, since otherwise a rejected key and an ignored one look alike.
    #[test]
    fn reverse_search_takes_back_a_key_that_matches_nothing() {
        let mut ed = Editor::new();
        ed.remember("echo hello");
        ed.remember("ls -l");
        // `echo hello` has no `x`, so the `x` is dropped and `ech` stands.
        let (got, drawn) = typed(&mut ed, b"\x12echx\r", 80, true);
        assert!(matches!(got, Input::Line(ref t) if t == "echo hello"), "{got:?}");
        assert!(drawn.contains('\x07'), "no bell for a key that matched nothing");
        assert_eq!(last_search(&drawn), "ech", "the failed key stuck");

        // Backspace widens the pattern, and the proof is that a WIDER one
        // reaches a different line: `echo t` is on `echo two`, and backing
        // off the `t` to type `one` moves to `echo one`.
        let mut ed = Editor::new();
        ed.remember("echo one");
        ed.remember("echo two");
        assert_eq!(submitted(&mut ed, b"\x12echo t\x7fone\r"), "echo one");
        // ...and `^H` is the same key, not a control byte that ends the
        // search and then eats a character out of the recalled line.
        assert_eq!(submitted(&mut ed, b"\x12echo t\x08one\r"), "echo one");
        // The prompt ends where the pattern really is, not where it passed.
        let (_, drawn) = typed(&mut ed, b"\x12echo t\x7f\r", 80, true);
        assert_eq!(last_search(&drawn), "echo ");
    }

    /// The search starts from where the operator ALREADY IS, not from the
    /// newest line -- the other half of the rule whose exception is Ctrl-R.
    #[test]
    fn reverse_search_starts_from_where_the_operator_is() {
        let mut ed = Editor::new();
        for line in ["echo one", "grep alpha", "echo two", "ls -l"] {
            ed.remember(line);
        }
        // Three Ctrl-P's land on `grep alpha`, which is OLDER than `echo two`.
        // Searching `ec` from there must reach `echo one` behind it; a search
        // that restarted from the newest line would answer `echo two`.
        assert_eq!(submitted(&mut ed, b"\x10\x10\x10\x12ec\r"), "echo one");
    }

    /// Within a line it is the FIRST occurrence, as busybox's `strstr` is --
    /// which is what puts the cursor at the start of what was searched for.
    #[test]
    fn reverse_search_takes_the_first_match_in_the_line() {
        let mut ed = Editor::new();
        ed.remember("echo echo hi");
        // Ctrl-L ends the search without moving the cursor, so the `X` lands
        // where the match put it: at the FIRST `echo`, not the second.
        assert_eq!(submitted(&mut ed, b"\x12echo\x0cX\r"), "Xecho echo hi");
    }

    /// A byte `character` read and did not want comes BACK, for the reason
    /// `escape`'s does: a truncated multi-byte sequence must cost the
    /// character, not the keystroke after it.
    #[test]
    fn a_pushed_back_byte_is_not_lost_by_the_search() {
        let mut ed = Editor::new();
        ed.remember("echo hello");
        // `\xc3` starts a two-byte character; the `l` after it is not a
        // continuation byte, so the character is dropped and the `l` is the
        // next key -- leaving `hell`, not `hel`.
        let (got, drawn) = typed(&mut ed, b"\x12hel\xc3l\r", 80, true);
        assert!(matches!(got, Input::Line(ref t) if t == "echo hello"), "{got:?}");
        assert_eq!(last_search(&drawn), "hell", "the pushed-back byte was lost");
    }

    /// The key that ends the search is DISPATCHED, not eaten: that is what
    /// makes Enter run what was found and Ctrl-A go to its start.
    #[test]
    fn the_key_that_ends_a_reverse_search_is_dispatched() {
        let mut ed = Editor::new();
        ed.remember("echo hello");
        // Ctrl-A leaves the search and moves to the start, so what is typed
        // next lands there.
        assert_eq!(submitted(&mut ed, b"\x12hello\x01X\r"), "Xecho hello");
        // Ctrl-C abandons the line, as it does anywhere else.
        assert!(matches!(typed(&mut ed, b"\x12hello\x03", 80, true).0, Input::Interrupted));
        // An arrow key arrives as ESC and is re-dispatched whole, so it browses
        // rather than typing `[A` into the line.
        assert_eq!(submitted(&mut ed, b"\x12hello\x1b[A\r"), "echo hello");
        // A search that found nothing leaves the line alone.
        assert_eq!(submitted(&mut ed, b"draft\x12zz\r"), "draft");
        // ...and the real prompt is back before the line is handed over.
        let (_, drawn) = typed(&mut ed, b"\x12hello\r", 80, true);
        assert!(drawn.ends_with("$ echo hello\x1b[K\r\x1b[7C"), "prompt not restored: {drawn:?}");
    }

    /// A multi-byte pattern is matched by CHARACTER and the cursor lands on a
    /// character boundary: `pos` off a boundary is an abort inside
    /// `String::insert`, and the search is the one place it comes from a
    /// string offset rather than from the two boundary walkers.
    #[test]
    fn reverse_search_handles_multibyte_patterns() {
        let mut ed = Editor::new();
        ed.remember("echo 日本語");
        ed.remember("ls");
        // `本` is at byte 8 of `echo 日本語`; typing it must leave the cursor
        // there and not inside the character before it.
        assert_eq!(submitted(&mut ed, b"\x12\xe6\x9c\xac\r"), "echo 日本語");
        let (_, drawn) = typed(&mut ed, b"\x12\xe6\x9c\xac\r", 80, true);
        assert!(drawn.contains("(reverse-i-search)'本': "), "{drawn:?}");
        // Ctrl-L ends the search without moving the cursor, and typing there
        // then inserts ON the boundary rather than aborting inside `insert`.
        // (Typing WITHOUT leaving would narrow the pattern instead: inside a
        // search every printable key is pattern text.)
        assert_eq!(submitted(&mut ed, b"\x12\xe6\x9c\xac\x0cX\r"), "echo 日X本語");
        // Backspacing the pattern takes a whole CHARACTER, so it is empty
        // again rather than half of one -- and an empty pattern matches at
        // the start of the line it is already on.
        assert_eq!(submitted(&mut ed, b"\x12\xe6\x9c\xac\x7f\x0cq\r"), "qecho 日本語");
    }

    /// A REBOUND interrupt or end-of-input byte ends the search too. With
    /// `ISIG` cleared the editor is what implements those two, so taking them
    /// as pattern text would be a hole in the emulation exactly where the
    /// operator had moved the key.
    #[test]
    fn a_rebound_control_byte_still_ends_a_reverse_search() {
        let mut ed = Editor::new();
        ed.remember("echo hello");
        // `stty intr x`: inside a search, `x` interrupts rather than being
        // searched for -- and `\x03` is then an ordinary byte.
        let mut src = b"\x12hellox".iter().copied();
        let mut next = || src.next();
        let mut w = || Some(80u16);
        let mut k = Keys { next: &mut next, width: &mut w, intr: Some(b'x'), eof: Some(0x04) };
        let mut out: Vec<u8> = Vec::new();
        let c = |_: &str| Vec::new();
        let e = |_: &str, _: &str| Vec::new();
        let got = ed.edit("$ ", &mut k, &mut out, true, &complete::Source {
            commands: &c,
            entries: &e,
        });
        assert!(matches!(got, Input::Interrupted), "{got:?}");
    }

    /// Searching away from the line being typed parks it, so arrowing back
    /// down to it returns the draft rather than a stale one.
    #[test]
    fn a_reverse_search_parks_the_draft() {
        let mut ed = Editor::new();
        ed.remember("echo hello");
        // Type a draft, search away from it, then Down twice to come back.
        assert_eq!(submitted(&mut ed, b"mydraft\x12hello\x0e\r"), "mydraft");
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

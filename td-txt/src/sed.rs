//! `sed` — the stream editor.
//!
//! POSIX `sed` plus the GNU surface the vendored GNU sed corpus and td's scripts
//! use: `first~step` / `addr,+N` / `addr,~N` / `0,/re/` addresses, the `s///`
//! case-conversion escapes (`\U\L\u\l\E`), `T`/`Q`/`R`/`W`/`z`/`F`, `-s`, `-i`,
//! `-E`, and `-z`. Byte-oriented in the C locale, like `grep`.
//!
//! Not implemented, deliberately: `e` (executing a shell command) — td-txt is a
//! static binary with no shell dependency, and a sed that can spawn `/bin/sh`
//! would put one back. It is a diagnosed error, not a silent no-op.
//!
//! Inputs are read whole. `$` (last line) must be known before the last line is
//! processed, which a streaming reader can only answer with a lookahead buffer;
//! reading whole files keeps that honest and matches `grep`.
//!
//! Every compiled regex in a script lives in one table, and an address or `s///`
//! holds an index into it. That is what makes the empty regex (`//`, `s//x/`)
//! expressible: it means "the last regex APPLIED", which is a runtime property,
//! not a syntactic one.

use std::collections::BTreeMap;
use std::io::{Read, Seek, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use crate::regex::{Captures, Options, Regex};
use crate::util::{
    errmsg, number, posixly_correct, print_line, records, show, Input, Out, VERSION,
};

const USAGE: &str =
    "usage: sed [-nrEsz] [-i[SUFFIX]] [-l N] [-e SCRIPT] [-f FILE] [SCRIPT] [FILE]...";

/// What a bare `l` folds at when NEITHER input to the fallback width said
/// otherwise — `COLS` seeds it and `-l` overrides it. GNU's own constant.
const DEFAULT_LINE_WRAP: usize = 70;

// ---- script model --------------------------------------------------------

#[derive(Debug)]
enum AddrKind {
    Line(u64),
    Last,
    /// Index into the script's regex table; `None` is the empty regex `//`.
    Rx(Option<usize>),
    Step { first: u64, step: u64 },
    /// `0` as the start of a range, so the end regex may match on line 1.
    Zero,
    /// What a `+`/`~` with a step of 0 leaves behind: an address that matches
    /// every line, which is how `sed -n '+p'` prints all of them.
    Always,
}

#[derive(Debug)]
enum Addr2 {
    Kind(AddrKind),
    Plus(u64),
    Multiple(u64),
}

#[derive(Debug, Default)]
struct Addr {
    a1: Option<AddrKind>,
    a2: Option<Addr2>,
    negate: bool,
}

impl Addr {
    fn is_range(&self) -> bool {
        self.a2.is_some()
    }
}

#[derive(Debug)]
struct Subst {
    re: Option<usize>,
    replacement: Vec<Repl>,
    global: bool,
    print: bool,
    occurrence: u64,
    wfile: Option<Vec<u8>>,
}

/// One piece of an `s///` replacement.
#[derive(Debug)]
enum Repl {
    Literal(Vec<u8>),
    Group(usize),
    Case(CaseOp),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CaseOp {
    Upper,
    Lower,
    UpperOne,
    LowerOne,
    End,
}

/// A branch destination: a label name until `resolve_labels` turns it into a
/// command index. `None` means the end of the script.
#[derive(Debug)]
enum Target {
    Name(Vec<u8>),
    At(Option<usize>),
}

#[derive(Debug)]
enum Kind {
    Block(usize), // index just past the matching `}`
    BlockEnd,
    Label,
    Branch(Target),
    BranchIfSub(Target),
    BranchIfNoSub(Target),
    /// `None` is a text GNU writes NOTHING for, which only a script ENDING in a
    /// bare backslash produces: `sed 'a\\'` appends no line at all where `sed 'a\\\\'`
    /// appends an empty one, and `sed 'c\\'` still deletes.
    Append(Option<Vec<u8>>),
    Insert(Option<Vec<u8>>),
    Change(Option<Vec<u8>>),
    Delete,
    DeleteFirstLine,
    Get,
    GetAppend,
    Hold,
    HoldAppend,
    Exchange,
    /// `None` is "no width argument", which defers to `-l`'s value at RUN time —
    /// GNU reads that global when `l` executes, so `sed -e l -l 12` folds at 12.
    List(Option<usize>),
    Next,
    NextAppend,
    Print,
    PrintFirstLine,
    Quit(i32, bool), // (exit status, auto-print first)
    ReadFile(Vec<u8>),
    /// `0r` — the same read, written where the command RUNS rather than queued.
    PrependFile(Vec<u8>),
    ReadLine(Vec<u8>),
    Subst(Subst),
    Transliterate(Box<[u8; 256]>),
    Write(Vec<u8>),
    WriteFirstLine(Vec<u8>),
    LineNumber,
    FileName,
    Zap,
    Comment,
}

#[derive(Debug)]
struct Cmd {
    addr: Addr,
    kind: Kind,
}

#[derive(Debug, Default)]
struct Script {
    cmds: Vec<Cmd>,
    regexes: Vec<Regex>,
    labels: BTreeMap<Vec<u8>, usize>,
    /// The `w` targets, ALREADY OPEN: GNU opens one while it parses the command
    /// that names it, so compiling a script is what creates and truncates them.
    /// Keyed by filename because GNU keeps one output per name, not per command.
    wfiles: BTreeMap<Vec<u8>, WFile>,
}

/// GNU's tri-state. `Closed` is not `Inactive`: a range whose start address is a
/// LINE NUMBER must not re-arm once it has closed, because `line >= start` would
/// otherwise be true forever after.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum RangeState {
    #[default]
    Inactive,
    /// Running; carries the line the range started on, which is what `+N`/`~N`
    /// ends are measured from.
    Active(u64),
    Closed,
}

// ---- script parsing ------------------------------------------------------

struct ScriptParser<'a> {
    src: &'a [u8],
    pos: usize,
    ere: bool,
    /// `--posix`. Reaches the regex compiler, which drops GNU's extensions under
    /// it (see `Options::posix`), and `normalize_regex`, which stops decoding the
    /// escape vocabulary inside a bracket expression.
    posix: bool,
    /// `--sandbox`. Read at the `r`/`R`/`w`/`W` commands and the `s///w` flag, and
    /// checked BEFORE the target is opened, because refusing the command is GNU's
    /// answer whether or not the file could have been opened.
    sandbox: bool,
    /// `-z`. Only the `M` flag reads it: see `Options::reg_newline`.
    null_data: bool,
    /// Offsets of the newlines that JOIN `-e`/`-f` parts. GNU ends a script part
    /// like it ends the script for one question only -- whether `a`/`i`/`c` was
    /// given any text -- so `sed -e a -e p` is an error where the one-argument
    /// `sed 'a<newline>p'` is not, while `sed -e 'a\' -e text` still spans the
    /// boundary because the backslash asked to. See `parse_text`.
    part_ends: Vec<usize>,
    regexes: Vec<Regex>,
    wfiles: BTreeMap<Vec<u8>, WFile>,
}

impl ScriptParser<'_> {
    fn peek(&self) -> Option<u8> {
        self.src.get(self.pos).copied()
    }

    fn bump(&mut self) -> Option<u8> {
        let b = self.peek();
        if b.is_some() {
            self.pos += 1;
        }
        b
    }

    fn eat(&mut self, b: u8) -> bool {
        if self.peek() == Some(b) {
            self.pos += 1;
            return true;
        }
        false
    }

    /// Is the parser at a newline that ENDS an `-e`/`-f` part? See `part_ends`.
    fn at_part_end(&self) -> bool {
        self.peek() == Some(b'\n') && self.part_ends.contains(&self.pos)
    }

    fn skip_blank(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t')) {
            self.pos += 1;
        }
    }

    /// What GNU skips where a COMMAND is expected: `;` plus its whole `ISSPACE`
    /// set, which is six bytes -- four more than the blanks `skip_blank`
    /// crosses. The asymmetry with `end_of_cmd` is GNU's own: `read_end_of_cmd`
    /// reads through `in_nonblank`, so a `\r` AFTER a command is `extra
    /// characters after command` while one BEFORE it is nothing at all.
    fn skip_separators(&mut self) {
        while matches!(
            self.peek(),
            Some(b' ' | b'\t' | b'\n' | b'\x0b' | b'\x0c' | b'\r' | b';')
        ) {
            self.pos += 1;
        }
    }

    /// A command that does not consume the rest of the line must be SEPARATED from
    /// the next one: `sed -n 'gp'` is an error in GNU, not `g` then `p`, and so is
    /// `g p`. `}` and `#` end a command as much as `;` and a newline do, and none of
    /// them is consumed here — the caller's loop handles them.
    fn end_of_cmd(&mut self) -> Result<(), String> {
        self.skip_blank();
        match self.peek() {
            None | Some(b'\n' | b';' | b'}' | b'#') => Ok(()),
            Some(_) => Err("extra characters after command".to_string()),
        }
    }

    fn rest_of_line(&mut self) -> Vec<u8> {
        let start = self.pos;
        while !matches!(self.peek(), None | Some(b'\n')) {
            self.pos += 1;
        }
        let text = self.src.get(start..self.pos).unwrap_or_default().to_vec();
        self.eat(b'\n');
        text
    }

    fn parse_number(&mut self) -> Option<u64> {
        let start = self.pos;
        let mut n: u64 = 0;
        while let Some(b) = self.peek() {
            if !b.is_ascii_digit() {
                break;
            }
            // GNU's `in_integer` accumulates into a `countT` and lets it WRAP, so
            // a number past 2^64 is its remainder and not a clamp: `2^64+1` is
            // line 1 and `2^64` is line 0, which is then refused as an address.
            // Saturating instead was a silent wrong ANSWER -- the script ran,
            // matched nothing, and said nothing.
            n = n.wrapping_mul(10).wrapping_add(u64::from(b - b'0'));
            self.pos += 1;
        }
        if self.pos == start {
            return None;
        }
        Some(n)
    }

    fn add_regex(&mut self, raw: &[u8], icase: bool, multiline: bool) -> Result<Option<usize>, String> {
        if raw.is_empty() {
            // `//` reuses whatever matched last, which has its own flags — so GNU
            // refuses to be given new ones rather than silently dropping them.
            if icase || multiline {
                return Err("cannot specify modifiers on empty regexp".to_string());
            }
            return Ok(None);
        }
        // `M` is relative to the RECORD separator, which `N` can put inside the
        // pattern space: under `-z`, `N` joins with a NUL and GNU anchors there.
        let opts = Options {
            ere: self.ere,
            icase,
            strict_repeats: true,
            posix: self.posix,
            // sed has only glibc, which never satisfies a mid-branch `$`.
            glibc_engine: true,
            // sed lexes one regex at a time, and has no `-x`/`-w` to wrap it.
            lex_continues: false,
            reg_newline: multiline.then_some(separator_for(self.null_data)),
        };
        let re = Regex::compile(&normalize_regex(raw, self.posix)?, opts)
            .map_err(|e| e.msg)?;
        self.regexes.push(re);
        Ok(Some(self.regexes.len() - 1))
    }

    /// A `/re/` or `\cREc` address, consuming the `I`/`M` modifiers that follow --
    /// except under `--posix`, which withdraws them; see the body.
    fn parse_regex_addr(&mut self, delim: u8) -> Result<AddrKind, String> {
        let raw = self.read_delimited(delim, "unterminated address regex")?;
        let mut icase = false;
        let mut multiline = false;
        // `I` and `M` after an address regex are GNU's, so `--posix` leaves them
        // unread and each is met as a command: `--posix -n '/A/Ip'` is
        // `unknown command: `I''. The s/// flags of the same names go too, but
        // through the `s` parser's own catch-all rather than here.
        while !self.posix {
            match self.peek() {
                Some(b'I') => {
                    icase = true;
                    self.pos += 1;
                }
                Some(b'M') => {
                    multiline = true;
                    self.pos += 1;
                }
                _ => break,
            }
        }
        Ok(AddrKind::Rx(self.add_regex(&raw, icase, multiline)?))
    }

    /// Text up to the next unescaped `delim`, every escape but `\delim` preserved
    /// for the regex compiler. The order of the three tests below IS the rule GNU's
    /// `match_slash` implements: a bare newline ends the half, then the delimiter,
    /// then a backslash -- so `\<delim>` collapses whatever class the delimiter is,
    /// and a `\` may itself delimit (`sed -n '\\^\p'`).
    fn read_delimited(&mut self, delim: u8, unterminated: &str) -> Result<Vec<u8>, String> {
        let mut out = Vec::new();
        let mut in_bracket = false;
        let mut bracket_body = 0usize; // index in `out` where the set's members start
        loop {
            let Some(b) = self.bump() else {
                return Err(unterminated.to_string());
            };
            // Ahead of bracket state too, so `s/[<newline>]/X/` is refused.
            if b == b'\n' {
                return Err(unterminated.to_string());
            }
            if b == delim && !in_bracket {
                return Ok(out);
            }
            if b == b'\\' && !in_bracket {
                let Some(n) = self.bump() else {
                    return Err(unterminated.to_string());
                };
                if n == delim {
                    out.push(n); // `\/` in a `/…/` is a literal delimiter
                } else if n == b'\n' {
                    out.push(b'\n');
                } else {
                    out.push(b'\\');
                    out.push(n);
                }
                continue;
            }
            if in_bracket {
                out.push(b);
                // `[:`, `[.` and `[=` open a sub-expression running to that same
                // character before a `]`, so the `]` of `[:alpha:]` does not end
                // the set and a delimiter after one is still a member. GNU's
                // reader CONSUMES the byte it tests after the kind character and
                // does not put it back, where the regex compiler peeks it — which
                // is why `[[...]]` is a set holding `.` to grep and unterminated
                // to sed, the reader having eaten the `.]` its name needed.
                if b == b'[' {
                    if let Some(kind) = self.peek().filter(|c| matches!(c, b':' | b'.' | b'=')) {
                        self.pos += 1;
                        out.push(kind);
                        loop {
                            let c = self.sub_expr_byte(&mut out, unterminated)?;
                            if c != kind {
                                continue;
                            }
                            if self.sub_expr_byte(&mut out, unterminated)? == b']' {
                                break;
                            }
                        }
                    }
                    continue;
                }
                if b == b']' && out.len() > bracket_body + 1 {
                    in_bracket = false;
                }
                continue;
            }
            if b == b'[' {
                in_bracket = true;
                out.push(b);
                if self.eat(b'^') {
                    out.push(b'^');
                }
                bracket_body = out.len();
                continue;
            }
            out.push(b);
        }
    }

    /// One byte of a bracket sub-expression, kept in `out`. A bare newline ends
    /// the half here as it does everywhere else in `read_delimited`.
    fn sub_expr_byte(&mut self, out: &mut Vec<u8>, unterminated: &str) -> Result<u8, String> {
        let Some(c) = self.bump() else {
            return Err(unterminated.to_string());
        };
        if c == b'\n' {
            return Err(unterminated.to_string());
        }
        out.push(c);
        Ok(c)
    }

    /// The command spellings `--posix` withdraws, each then reported by the
    /// parser's ordinary "not a command" path rather than a message of its own --
    /// which is also how GNU words it. `q`, `r`, `w` and `t` stay; only their
    /// upper-case GNU siblings go, so the combined arms below cannot be gated as
    /// a whole and this is asked BEFORE the dispatch instead.
    fn posix_drops_command(&self, c: u8) -> bool {
        self.posix && matches!(c, b'v' | b'Q' | b'T' | b'z' | b'F' | b'W' | b'R' | b'e')
    }

    /// The commands `--posix` gives at most ONE address, GNU otherwise taking a
    /// range. Four are POSIX's own; `l` is NOT -- POSIX.1 defines it `[2addr]l`,
    /// so that one is GNU's posix-mode behaviour, taken from the oracle rather
    /// than from the standard. `q`/`Q` are restricted in both modes and check
    /// themselves; `c` is not in the list, a range being the whole point of it.
    fn posix_limits_addresses(&self, c: u8) -> bool {
        self.posix && matches!(c, b'=' | b'a' | b'i' | b'l' | b'r')
    }

    fn parse_addr_kind(&mut self) -> Result<Option<AddrKind>, String> {
        match self.peek() {
            Some(b'$') => {
                self.pos += 1;
                Ok(Some(AddrKind::Last))
            }
            Some(b'/') => {
                self.pos += 1;
                Ok(Some(self.parse_regex_addr(b'/')?))
            }
            Some(b'\\') => {
                self.pos += 1;
                let delim = self.bump().ok_or_else(|| "unterminated address".to_string())?;
                Ok(Some(self.parse_regex_addr(delim)?))
            }
            // `+N`/`~N` is an address FORM in any position, not only after a
            // comma -- GNU reads both through one `compile_address` -- but a
            // stride with nothing to count FROM is refused as a first address.
            // Step 0 is not a stride: it leaves the address with no type at all,
            // and an untyped address matches every line, so `+p` and `~p` run
            // and print all of them. Absent digits ARE 0, as everywhere else
            // GNU reads a step, so a bare `+` is that same always-match; a
            // number past 2^64 WRAPS like GNU's, so `+2^64` is `+0`. The
            // message names the first address because only a first address
            // reaches here -- `parse_addr` takes `+`/`~` after a comma itself.
            Some(b'+' | b'~') if !self.posix => {
                self.pos += 1;
                self.skip_blank();
                match self.parse_number().unwrap_or(0) {
                    0 => Ok(Some(AddrKind::Always)),
                    _ => Err("invalid usage of +N or ~N as first address".to_string()),
                }
            }
            Some(b) if b.is_ascii_digit() => {
                let n = self.parse_number().unwrap_or(0);
                if !self.posix {
                    // GNU reads the step with `in_integer(in_nonblank())`, so
                    // blanks fall either side of the `~` and ABSENT digits are
                    // 0 rather than an error -- `2 ~ 2`, `1~ 2` and a bare `2~`
                    // are all addresses. Rewind when there is no `~` at all, so
                    // nothing else sees the blanks eaten here.
                    let save = self.pos;
                    self.skip_blank();
                    if self.eat(b'~') {
                        self.skip_blank();
                        let step = self.parse_number().unwrap_or(0);
                        // Step 0 degenerates to the plain line `first`, so `1~0`
                        // IS line 1 and `0~0` IS line 0, taking the address-0
                        // refusal. Normalising here rather than at the matcher
                        // is what makes it absolute everywhere: as a range END a
                        // line number behind the start closes at once, where a
                        // `Step` that only matched `first` never closed and
                        // `2,1~0d` ran away.
                        if step == 0 {
                            return Ok(Some(self.line_addr(n)));
                        }
                        return Ok(Some(AddrKind::Step { first: n, step }));
                    }
                    self.pos = save;
                }
                Ok(Some(self.line_addr(n)))
            }
            _ => Ok(None),
        }
    }

    /// A literal line number. `0` is its own kind because it alone may start a
    /// range whose end regex matches on line 1; every other number is `Line`.
    fn line_addr(&self, n: u64) -> AddrKind {
        match n {
            0 => AddrKind::Zero,
            _ => AddrKind::Line(n),
        }
    }

    fn parse_addr(&mut self) -> Result<Addr, String> {
        let mut addr = Addr { a1: self.parse_addr_kind()?, ..Addr::default() };
        if addr.a1.is_some() {
            self.skip_blank();
            if self.eat(b',') {
                self.skip_blank();
                // Same `in_integer(in_nonblank())` as the step: blanks after the
                // operator, and no digits at all reads as 0. `1,+p` is `+0`, a
                // range ending on its own start line.
                if !self.posix && self.eat(b'+') {
                    self.skip_blank();
                    addr.a2 = Some(Addr2::Plus(self.parse_number().unwrap_or(0)));
                } else if !self.posix && self.eat(b'~') {
                    self.skip_blank();
                    addr.a2 = Some(Addr2::Multiple(self.parse_number().unwrap_or(0)));
                } else {
                    let k = self
                        .parse_addr_kind()?
                        .ok_or_else(|| "unexpected `,'".to_string())?;
                    addr.a2 = Some(Addr2::Kind(k));
                }
            }
        }
        // `Zero` is distinct only as a range START (`0,/re/`); as an END it is
        // the line number 0, behind every start. Normalising to `Line(0)` here
        // is what stops the two range seams disagreeing: a second `Zero` arm in
        // `range_step` alone left `range_opens` holding the range open, so
        // `1,0d` looked right while `1,0c` never closed and swallowed its text.
        if matches!(addr.a2, Some(Addr2::Kind(AddrKind::Zero))) {
            addr.a2 = Some(Addr2::Kind(AddrKind::Line(0)));
        }
        self.skip_blank();
        // Checked BEFORE the `!`: GNU judges the address first, so `0!!p` is
        // `invalid usage of line address 0` and not `multiple `!'s`. TWO uses
        // survive. A range's end regex may match on line 1 if the range starts
        // at 0 -- `0,/b/` and `0,\%b%` are the whole of that, while `0,5`,
        // `0,$`, `0,+2` and `0,~2` are refused like a bare `0`. And `r` ALONE
        // takes 0 as a lone address, to prepend a file before line 1. Which is
        // why the command character has to be peeked at here rather than judged
        // in its own arm: GNU tests the char it has already read past the
        // address, so `!` is not `r` and `0!r` is this error, not a negated
        // prepend. Under `--posix` neither use is available.
        let ends_in_regex = matches!(addr.a2, Some(Addr2::Kind(AddrKind::Rx(_))));
        let prepends = addr.a2.is_none() && self.peek() == Some(b'r');
        if matches!(addr.a1, Some(AddrKind::Zero))
            && (self.posix || !(ends_in_regex || prepends))
        {
            return Err("invalid usage of line address 0".to_string());
        }
        // One `!` and no more: GNU refuses a second rather than toggling back.
        if self.eat(b'!') {
            addr.negate = true;
            self.skip_blank();
            if self.peek() == Some(b'!') {
                return Err("multiple `!'s".to_string());
            }
        }
        Ok(addr)
    }

    /// Text of an `a`/`i`/`c`: GNU's one-line form (`a text`) and the POSIX `a\`
    /// continuation form, where a trailing backslash keeps the text open.
    /// `a`/`i`/`c` text. GNU requires SOME text where the script (or the `-e` part)
    /// ENDS -- `sed a` and `sed 1a` are `expected \\ after `a', `c' or `i'`, and a
    /// blank does not count -- while a newline with more script after it is an EMPTY
    /// text, which appends an empty line. A script ending in the command's own
    /// backslash is accepted and writes NOTHING at all, which `None` is; an empty
    /// LINE (`sed 'a\\\\'`) is `Some("")`.
    fn parse_text(&mut self) -> Result<Option<Vec<u8>>, String> {
        self.skip_blank();
        let mut out = Vec::new();
        if self.eat(b'\\') {
            // The backslash asked for the next line, so it crosses a part boundary.
            // Crossing one is GNU's, and `--posix` wants the text in the SAME part:
            // `-e 'a\' -e 'text'` appends without the flag and is refused with it,
            // as is a script ending in the backslash, which otherwise appends
            // nothing. Asked BEFORE the newline is eaten, since eating it is what
            // makes the two parts look like one.
            if self.posix && (self.at_part_end() || self.peek().is_none()) {
                return Err("incomplete command".to_string());
            }
            if !self.eat(b'\n') && self.peek().is_none() {
                return Ok(None);
            }
        } else if self.posix || self.peek().is_none() || self.at_part_end() {
            // The one-line `a text` form is GNU's; `--posix` leaves only `a\`.
            return Err("expected \\ after `a', `c' or `i'".to_string());
        }
        // A text whose last character is an unpaired backslash is not decoded at
        // all: GNU emits `a\<newline>A\n\` as the three bytes `A\n`, where the
        // same text without that backslash appends a newline.
        let mut undecoded = false;
        loop {
            match self.bump() {
                None => break,
                // Carrying the text into the next `-e` part collapses the escape,
                // because the joiner's own newline is what lands in the buffer.
                // Inside ONE part both bytes reach the decoder instead, which is
                // why `a\<newline>A\c\<newline>B` is an error and not a `J`.
                Some(b'\\') if self.at_part_end() => {
                    // Same rule as the command's own backslash: crossing into the
                    // next `-e` is GNU's, and `--posix` will not have it.
                    if self.posix {
                        return Err("incomplete command".to_string());
                    }
                    self.pos += 1;
                    out.push(b'\n');
                }
                Some(b'\\') => match self.bump() {
                    None => {
                        if self.posix {
                            return Err("incomplete command".to_string());
                        }
                        undecoded = true;
                        break;
                    }
                    Some(c) => {
                        out.push(b'\\');
                        out.push(c);
                    }
                },
                Some(b'\n') => break,
                Some(c) => out.push(c),
            }
        }
        if undecoded {
            return Ok(Some(out));
        }
        // No terminator here: the writer appends the RECORD separator, which
        // under -z is NUL. Parsing cannot know it — the option is read first,
        // but the text belongs to the script.
        Ok(Some(normalize_buffer(&out)?))
    }

    /// A label ends at a BLANK as much as at `;`, `}`, `#` or a newline, so `sed -n
    /// ':a p'` is the label `a` and then `p` -- reading to the end of the command
    /// instead swallows commands that follow one on the same line. Leading blanks are
    /// skipped, which is why `b lbl` names `lbl`. Also `v`'s optional version.
    fn parse_label(&mut self) -> Vec<u8> {
        self.skip_blank();
        let start = self.pos;
        while !matches!(self.peek(), None | Some(b'\n' | b' ' | b'\t' | b';' | b'}' | b'#')) {
            self.pos += 1;
        }
        self.src.get(start..self.pos).unwrap_or_default().to_vec()
    }

    /// r/R/w/W and the `s///w` flag all take a file name to end of line. An
    /// EMPTY one is a script error, not a no-op: silently passing the input
    /// through is exactly the fail-open this crate refuses everywhere else.
    fn parse_filename(&mut self) -> Result<Vec<u8>, String> {
        self.skip_blank();
        let name = self.rest_of_line();
        if name.is_empty() {
            return Err("missing filename in r/R/w/W commands".to_string());
        }
        Ok(name)
    }

    /// `--sandbox` refuses the command that names a file, at the point the parse
    /// reaches it — so it outranks a LATER script error and loses to an EARLIER
    /// one, and the target is never opened. Exit 1: the script is what is wrong.
    ///
    /// GNU bans `e` here too. td-txt refuses `e` and the `s///e` flag outright, so
    /// the ban has nothing left to catch — but the DIAGNOSTIC differs under the
    /// flag, GNU naming sandbox mode where td-txt names the unsupported command.
    fn deny_in_sandbox(&self) -> Result<(), Fatal> {
        if self.sandbox {
            return Err(Fatal {
                msg: "e/r/w commands disabled in sandbox mode".to_string(),
                status: 1,
            });
        }
        Ok(())
    }

    /// Open a `w` target, which GNU does WHILE PARSING the command that names it.
    /// So the file exists and is truncated even if no cycle ever writes to it, and
    /// one that will not open is exit 4 from a command that never runs. Position in
    /// the script is therefore what orders this failure against a script error:
    /// everything parsed before it has already had its say.
    ///
    /// The two device names are not files and GNU creates nothing for them; they
    /// are registered here anyway so the write path never has to open anything.
    fn open_wfile(&mut self, path: &[u8]) -> Result<(), Fatal> {
        if self.wfiles.contains_key(path) {
            return Ok(());
        }
        let dest = match path {
            b"/dev/stdout" => WDest::Stdout,
            b"/dev/stderr" => WDest::Stderr,
            _ => WDest::File(
                std::fs::File::create(crate::util::path_from_bytes(path)).map_err(|e| {
                    Fatal::runtime(format!(
                        "couldn't open file {}: {}",
                        show(path),
                        errmsg(&e)
                    ))
                })?,
            ),
        };
        self.wfiles.insert(path.to_vec(), WFile { dest, owed: false });
        Ok(())
    }

    /// Returns `Fatal` because the `w` FLAG opens its target here, mid-command:
    /// GNU reaches that flag before it compiles the pattern, so an unopenable
    /// target beats a bad regex or a bad backreference in the same `s`.
    fn parse_subst(&mut self) -> Result<Subst, Fatal> {
        let delim = self.bump().ok_or_else(|| "unterminated `s' command".to_string())?;
        // A backslash CAN delimit (`s\a\b\` is `s/a/b/`); a newline cannot, and
        // GNU says so before reading anything else.
        if delim == b'\n' {
            return Err("unterminated `s' command".to_string().into());
        }
        let pattern = self.read_delimited(delim, "unterminated `s' command")?;
        let raw_repl = self.read_replacement(delim, "unterminated `s' command")?;
        let mut global = false;
        let mut print = false;
        let mut icase = false;
        let mut multiline = false;
        let mut occurrence: u64 = 0;
        let mut wfile = None;
        loop {
            match self.peek() {
                // GNU rejects a REPEAT of the three flags that carry a value, and
                // `i`/`I`/`m`/`M` are the ones it lets you say twice.
                Some(b'g') => {
                    if global {
                        return Err("multiple `g' options to `s' command".to_string().into());
                    }
                    global = true;
                    self.pos += 1;
                }
                Some(b'p') => {
                    if print {
                        return Err("multiple `p' options to `s' command".to_string().into());
                    }
                    print = true;
                    self.pos += 1;
                }
                Some(b'i' | b'I') if !self.posix => {
                    icase = true;
                    self.pos += 1;
                }
                Some(b'm' | b'M') if !self.posix => {
                    multiline = true;
                    self.pos += 1;
                }
                // Under the flag these are not flags at all, so the catch-all
                // below answers with GNU's `unknown option to `s''.
                Some(b'e') if !self.posix => {
                    return Err("the `e' flag is not supported".to_string().into())
                }
                Some(b'w') => {
                    self.pos += 1;
                    self.deny_in_sandbox()?;
                    let name = self.parse_filename()?;
                    self.open_wfile(&name)?;
                    wfile = Some(name);
                    break;
                }
                Some(b) if b.is_ascii_digit() => {
                    if occurrence != 0 {
                        return Err("multiple number options to `s' command".to_string().into());
                    }
                    occurrence = self.parse_number().unwrap_or(0);
                    if occurrence == 0 {
                        return Err("number option to `s' command may not be zero".to_string().into());
                    }
                }
                // A blank SEPARATES flags rather than ending them (`s/a/b/ g p` is
                // both), and anything that is not a flag or a command terminator is
                // an error rather than the next command: `s/a/b/x` is `unknown option
                // to `s'` in GNU, not a substitution followed by an exchange.
                Some(b' ' | b'\t') => self.pos += 1,
                None | Some(b'\n' | b';' | b'}' | b'#') => break,
                Some(_) => return Err("unknown option to `s'".to_string().into()),
            }
        }
        let re = self.add_regex(&pattern, icase, multiline)?;
        let replacement = compile_replacement(&raw_repl, self.posix)?;
        // A `\N` naming a group the pattern does not have is a script error, not
        // an empty expansion -- except under `--posix`, GNU's fourth rule for the
        // flag, where it is accepted and expands to nothing. Checked only when
        // the regex is known here; `s//.../` reuses the LAST one and goes
        // unchecked, which is a DIVERGENCE rather than a shared rule: GNU still
        // refuses when the command carries its own address regex, so
        // `/\(a\)/s//\2/` is an error there and empty here. See spec/README.
        if let Some(groups) = re
            .filter(|_| !self.posix)
            .and_then(|i| self.regexes.get(i))
            .map(Regex::group_count)
        {
            if let Some(n) = max_group(&replacement) {
                if n > groups {
                    return Err(format!("invalid reference \\{n} on `s' command's RHS").into());
                }
            }
        }
        Ok(Subst {
            re,
            replacement,
            global,
            print,
            occurrence: occurrence.max(1),
            wfile,
        })
    }

    /// The replacement half of `s///`, and either half of `y///`: `read_delimited`'s
    /// ordering over raw bytes, every other escape left for `compile_replacement`.
    /// `unterminated` is the caller's diagnostic, `y` naming itself rather than `s`.
    fn read_replacement(&mut self, delim: u8, unterminated: &str) -> Result<Vec<u8>, String> {
        let mut out = Vec::new();
        loop {
            let Some(b) = self.bump() else {
                return Err(unterminated.to_string());
            };
            if b == b'\n' {
                return Err(unterminated.to_string());
            }
            if b == delim {
                return Ok(out);
            }
            if b == b'\\' {
                let Some(n) = self.bump() else {
                    return Err(unterminated.to_string());
                };
                // One exception to the collapse: an `&` delimiter keeps its
                // backslash, `\&` being how a replacement says a literal ampersand.
                if n == delim && delim != b'&' {
                    out.push(n);
                } else if n == b'\n' {
                    // A continuation collapses before the decoder runs, which is
                    // the only way `\c` can control a newline: `s/X/\c\<nl>/` is `J`.
                    out.push(b'\n');
                } else {
                    out.push(b'\\');
                    out.push(n);
                }
                continue;
            }
            out.push(b);
        }
    }

    fn parse_transliterate(&mut self) -> Result<Box<[u8; 256]>, String> {
        const UNTERM_Y: &str = "unterminated `y' command";
        let delim = self.bump().ok_or_else(|| UNTERM_Y.to_string())?;
        let from = normalize_buffer(&self.read_replacement(delim, UNTERM_Y)?)?;
        let to = normalize_buffer(&self.read_replacement(delim, UNTERM_Y)?)?;
        if from.len() != to.len() {
            return Err("strings for `y' command are different lengths".to_string());
        }
        let mut table = Box::new([0u8; 256]);
        for (i, slot) in table.iter_mut().enumerate() {
            *slot = u8::try_from(i).unwrap_or(0);
        }
        for (f, t) in from.iter().zip(to.iter()) {
            if let Some(slot) = table.get_mut(usize::from(*f)) {
                *slot = *t;
            }
        }
        Ok(table)
    }
}

/// What GNU's escape decoder makes of one backslash pair.
enum Esc {
    /// A byte from GNU's vocabulary, whose operand is consumed.
    Byte(u8),
    /// Not GNU's (`\\`, `\w`, `\1`, `\U`, `\<newline>`): the escape stands, and
    /// what it means is the next parser's business.
    Other,
    /// Nothing left to escape -- the text ends in a backslash, or in a `\c` with
    /// no character to control. GNU leaves a bare backslash, which each caller
    /// resolves its own way: the regex compiler rejects it, a replacement keeps
    /// it, a text buffer drops it.
    Bare,
}

/// One backslash escape from GNU's vocabulary: `\n`, `\t`, `\r`, `\f`, `\v`, `\a`,
/// `\cX`, `\dNNN`, `\oNNN`, `\xHH` -- and NOT `\e`, which GNU has never taken.
/// `i` indexes the byte AFTER the backslash, and ends past the whole escape for
/// every answer but `Esc::Other`: there the caller decides what the escape means,
/// so it is left ON the character and the caller steps over it.
fn escape_byte(raw: &[u8], i: &mut usize) -> Result<Esc, String> {
    let Some(b) = raw.get(*i).copied() else {
        return Ok(Esc::Bare);
    };
    let byte = match b {
        b'n' => b'\n',
        b't' => b'\t',
        b'r' => b'\r',
        b'f' => 0x0c,
        b'v' => 0x0b,
        b'a' => 0x07,
        b'c' => {
            return match raw.get(*i + 1).copied() {
                None => {
                    *i += 1;
                    Ok(Esc::Bare)
                }
                // A backslash is the one character `\c` cannot take plainly:
                // `\c\\` is 0x1c and any other escape after it is refused.
                Some(b'\\') => match raw.get(*i + 2) {
                    Some(b'\\') => {
                        *i += 3;
                        Ok(Esc::Byte(0x1c))
                    }
                    _ => Err("recursive escaping after \\c not allowed".to_string()),
                },
                Some(c) => {
                    *i += 2;
                    Ok(Esc::Byte(c.to_ascii_uppercase() ^ 0x40))
                }
            };
        }
        b'd' | b'o' | b'x' => {
            let (radix, digits) = match b {
                b'd' => (10, 3),
                b'o' => (8, 3),
                _ => (16, 2),
            };
            let mut j = *i + 1;
            return match radix_escape(raw, &mut j, radix, digits) {
                Some(byte) => {
                    *i = j;
                    Ok(Esc::Byte(byte))
                }
                // With no digit the escape is still GNU's, and it sheds the
                // backslash rather than keeping it: `[\x]` is the set `x`, where
                // the unknown `[\w]` is a backslash AND a `w`.
                None => {
                    *i += 1;
                    Ok(Esc::Byte(b))
                }
            };
        }
        _ => return Ok(Esc::Other),
    };
    *i += 1;
    Ok(Esc::Byte(byte))
}

/// GNU decodes its escape vocabulary BEFORE compiling a pattern, and inserts the
/// decoded byte RAW -- so it arrives as regex SYNTAX, not as a literal. `\x2e` is
/// the metacharacter dot, `[a\x2dz]` is a range, `\x5b` is an unmatched bracket
/// and `\x5cw` is the word class. That is also why an escape works inside a
/// bracket expression, where the regex parser itself would take `\` literally.
fn normalize_regex(raw: &[u8], posix: bool) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(raw.len());
    let mut i = 0usize;
    while let Some(b) = raw.get(i).copied() {
        i += 1;
        // `--posix` drops the decoding INSIDE a bracket expression, so `[\x41]`
        // holds four ordinary members there rather than an `A`. GNU judges
        // "inside" on the pattern TEXT, not on what decoding produces: a `[`
        // that is itself an escape opens nothing, and `\x5b\x41]` decodes under
        // the flag exactly as it does without it.
        if posix && b == b'[' {
            out.push(b);
            copy_bracket(raw, &mut i, &mut out);
            continue;
        }
        if b != b'\\' {
            out.push(b);
            continue;
        }
        match escape_byte(raw, &mut i)? {
            Esc::Byte(c) => out.push(c),
            Esc::Other => {
                out.push(b'\\');
                if let Some(c) = raw.get(i).copied() {
                    out.push(c);
                    i += 1;
                }
            }
            Esc::Bare => out.push(b'\\'),
        }
    }
    Ok(out)
}

/// Copy the suppressed region through verbatim, leaving `i` past its `]`. This is
/// NOT a bracket parse and must not become one: GNU stops at the FIRST `]`, so
/// POSIX's rule that a leading `]` is an ordinary member does not hold here, and
/// the region can end before the bracket itself does. `[]\x41]` is the observable
/// case -- suppression covers only `[]`, the `\x41` after it decodes, and the
/// class is {], A}. The one thing that carries it PAST a `]` is a `[:`/`[.`/`[=`
/// sub-expression, and that runs to the NEXT `]` rather than to its own
/// `:]`/`.]`/`=]` -- so `[[.].]` ends at the second `]`, not the third character
/// from the end. The bracket's OWN `[` opens one like any other, which is why
/// `[.]\x41` suppresses the `\x41` where `[x]\x41` does not. A backslash means
/// nothing either way, POSIX giving it no role inside a bracket. Unterminated,
/// this runs to the end and the regex parser reports it, as without the flag.
/// One shape is NOT modelled: after a closed sub-expression, GNU keeps
/// suppressing a following bracket whose leading `]` would otherwise end it
/// (`[::][]\x41]`). See spec/README; it is an xfail, not an oversight.
fn copy_bracket(raw: &[u8], i: &mut usize, out: &mut Vec<u8>) {
    // The `[` the caller has already emitted, which pairs like any other.
    let mut after_open = true;
    while let Some(b) = raw.get(*i).copied() {
        *i += 1;
        out.push(b);
        if after_open && matches!(b, b':' | b'.' | b'=') {
            copy_past_close(raw, i, out);
            after_open = false;
            continue;
        }
        if b == b']' {
            return;
        }
        after_open = b == b'[';
    }
}

/// Consume through the next `]`, where a `[:`/`[.`/`[=` sub-expression ends.
fn copy_past_close(raw: &[u8], i: &mut usize, out: &mut Vec<u8>) {
    while let Some(c) = raw.get(*i).copied() {
        *i += 1;
        out.push(c);
        if c == b']' {
            return;
        }
    }
}

/// The same vocabulary for text that no other parser reads: `a`/`i`/`c` text and
/// the two halves of `y///`. Here an escape GNU does not know just loses its
/// backslash, as POSIX asks, and `y`'s operands are measured AFTER this runs.
fn normalize_buffer(raw: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(raw.len());
    let mut i = 0usize;
    while let Some(b) = raw.get(i).copied() {
        i += 1;
        if b != b'\\' {
            out.push(b);
            continue;
        }
        match escape_byte(raw, &mut i)? {
            Esc::Byte(c) => out.push(c),
            Esc::Other => {
                if let Some(c) = raw.get(i).copied() {
                    out.push(c);
                    i += 1;
                }
            }
            Esc::Bare => {}
        }
    }
    Ok(out)
}

/// Read up to `max_digits` of a `\d`/`\o`/`\x` escape's operand, advancing `i`.
fn radix_escape(raw: &[u8], i: &mut usize, radix: u32, max_digits: usize) -> Option<u8> {
    let mut value: u32 = 0;
    let mut digits = 0usize;
    while digits < max_digits {
        let Some(d) = raw.get(*i).and_then(|c| char::from(*c).to_digit(radix)) else {
            break;
        };
        value = value * radix + d;
        *i += 1;
        digits += 1;
    }
    if digits == 0 {
        return None;
    }
    u8::try_from(value & 0xff).ok()
}

/// Split a replacement into literals, group references and case operators.
/// `posix` drops the five case operators, GNU's third `--posix` rule after the
/// compiler's and `normalize_regex`'s (the fourth is at the caller, where an
/// out-of-range group reference stops being an error): each becomes the LITERAL letter,
/// so `s/a/\Ux/` yields `Ux`. Nothing else in a replacement moves -- `\1`, `&`,
/// `\&`, the `\x`/`\o`/`\d` bytes and `\n` are the same either way -- and the
/// five need no arm of their own here, an unknown escape ALREADY shedding its
/// backslash and keeping its letter, which is exactly the answer.
fn compile_replacement(raw: &[u8], posix: bool) -> Result<Vec<Repl>, String> {
    let mut out: Vec<Repl> = Vec::new();
    let mut lit: Vec<u8> = Vec::new();
    fn flush(lit: &mut Vec<u8>, out: &mut Vec<Repl>) {
        if !lit.is_empty() {
            out.push(Repl::Literal(std::mem::take(lit)));
        }
    }
    let mut i = 0usize;
    while let Some(b) = raw.get(i).copied() {
        i += 1;
        match b {
            b'&' => {
                flush(&mut lit, &mut out);
                out.push(Repl::Group(0));
            }
            b'\\' => {
                let Some(n) = raw.get(i).copied() else {
                    lit.push(b'\\');
                    break;
                };
                match n {
                    b'0'..=b'9' => {
                        i += 1;
                        flush(&mut lit, &mut out);
                        out.push(Repl::Group(usize::from(n - b'0')));
                    }
                    b'U' | b'L' | b'u' | b'l' | b'E' if !posix => {
                        i += 1;
                        flush(&mut lit, &mut out);
                        out.push(Repl::Case(match n {
                            b'U' => CaseOp::Upper,
                            b'L' => CaseOp::Lower,
                            b'u' => CaseOp::UpperOne,
                            b'l' => CaseOp::LowerOne,
                            _ => CaseOp::End,
                        }));
                    }
                    // Unlike a pattern, a decoded byte lands here as a LITERAL:
                    // `\x26` is an ampersand that does not stand for the match,
                    // which is how the corpus's amp-escape case writes one.
                    _ => match escape_byte(raw, &mut i)? {
                        Esc::Byte(c) => lit.push(c),
                        Esc::Other => {
                            lit.push(n); // `\&`, `\\` and `\<newline>` shed the backslash
                            i += 1;
                        }
                        Esc::Bare => lit.push(b'\\'),
                    },
                }
            }
            _ => lit.push(b),
        }
    }
    flush(&mut lit, &mut out);
    Ok(out)
}

/// The highest `\N` a replacement names, for validation against the pattern.
fn max_group(repl: &[Repl]) -> Option<usize> {
    repl.iter()
        .filter_map(|r| match r {
            Repl::Group(n) if *n > 0 => Some(*n),
            _ => None,
        })
        .max()
}

/// The record separator `-z` selects. `M` needs it at PARSE time (it decides what
/// `^`/`.` mean) and the executor needs it at run time, so the mapping lives once.
fn separator_for(null_data: bool) -> u8 {
    match null_data {
        true => 0,
        false => b'\n',
    }
}

/// Parse a whole script: a flat command list, its regex table, its labels, and the
/// `w` targets opened along the way. Returns `Fatal` rather than a bare message
/// because opening those targets means a PARSE can now fail the way the filesystem
/// does — exit 4, reported bare — and not only the way a bad script does.
fn parse_script(
    src: &[u8],
    ere: bool,
    null_data: bool,
    part_ends: Vec<usize>,
    posix: bool,
    sandbox: bool,
) -> Result<Script, Fatal> {
    let mut p = ScriptParser {
        src,
        pos: 0,
        ere,
        posix,
        sandbox,
        null_data,
        part_ends,
        regexes: Vec::new(),
        wfiles: BTreeMap::new(),
    };
    let mut cmds: Vec<Cmd> = Vec::new();
    let mut labels: BTreeMap<Vec<u8>, usize> = BTreeMap::new();
    let mut open_blocks: Vec<usize> = Vec::new();
    loop {
        p.skip_separators();
        if p.peek().is_none() {
            break;
        }
        let mut addr = p.parse_addr()?;
        let Some(c) = p.bump() else {
            return Err("missing command".to_string().into());
        };
        if p.posix_drops_command(c) {
            return Err(format!("unknown command: `{}'", char::from(c)).into());
        }
        if p.posix_limits_addresses(c) && addr.is_range() {
            return Err("command only uses one address".to_string().into());
        }
        let kind = match c {
            b'{' => {
                open_blocks.push(cmds.len());
                Kind::Block(0) // patched when the matching `}` is seen
            }
            b'}' => {
                let Some(open) = open_blocks.pop() else {
                    return Err("unexpected `}'".to_string().into());
                };
                // After the unmatched test, not before it: `sed '1}'` is GNU's
                // `unexpected `}'`, so the brace has to match its `{` first.
                if addr.a1.is_some() {
                    return Err("`}' doesn't want any addresses".to_string().into());
                }
                let after = cmds.len() + 1;
                if let Some(cmd) = cmds.get_mut(open) {
                    cmd.kind = Kind::Block(after);
                }
                Kind::BlockEnd
            }
            b'#' => {
                // GNU REFUSES an addressed comment rather than ignoring the
                // address. The `!` is not one, so `!#` is still a comment.
                if addr.a1.is_some() {
                    return Err("comments don't accept any addresses".to_string().into());
                }
                p.rest_of_line();
                Kind::Comment
            }
            b'=' => Kind::LineNumber,
            b'a' => Kind::Append(p.parse_text()?),
            b'i' => Kind::Insert(p.parse_text()?),
            b'c' => Kind::Change(p.parse_text()?),
            b'b' => Kind::Branch(Target::Name(p.parse_label())),
            b't' => Kind::BranchIfSub(Target::Name(p.parse_label())),
            b'T' => Kind::BranchIfNoSub(Target::Name(p.parse_label())),
            b':' => {
                // One of the FOUR rules GNU enforces at compile time for HOW
                // MANY addresses a command takes: `#`, `}` and `:` take none,
                // `q`/`Q` take one; a fifth holds under `--posix`, above. That
                // family is a subset of a larger one now closed: every
                // compile-time check GNU makes against a parsed ADDRESS --
                // these five, `unexpected `,'`, `multiple `!'s`, address 0 and
                // a leading `+N`/`~N` -- is implemented here.
                if addr.a1.is_some() {
                    return Err(": doesn't want any addresses".to_string().into());
                }
                let name = p.parse_label();
                if name.is_empty() {
                    return Err("\":\" lacks a label".to_string().into());
                }
                labels.insert(name, cmds.len());
                Kind::Label
            }
            b'd' => Kind::Delete,
            b'D' => Kind::DeleteFirstLine,
            b'g' => Kind::Get,
            b'G' => Kind::GetAppend,
            b'h' => Kind::Hold,
            b'H' => Kind::HoldAppend,
            b'x' => Kind::Exchange,
            b'l' => {
                p.skip_blank();
                // The width argument is GNU's, and an unread digit is what makes
                // `--posix 'l 5'` the `extra characters after command` GNU gives.
                let w = match p.posix {
                    true => None,
                    false => p.parse_number(),
                };
                Kind::List(list_width(w))
            }
            b'n' => Kind::Next,
            b'N' => Kind::NextAppend,
            b'p' => Kind::Print,
            b'P' => Kind::PrintFirstLine,
            b'q' | b'Q' => {
                if addr.is_range() {
                    return Err("command only uses one address".to_string().into());
                }
                p.skip_blank();
                // Likewise the exit code: `--posix 'q5'` leaves the `5` unread.
                let n = match p.posix {
                    true => None,
                    false => p.parse_number(),
                };
                // Only the low 8 bits survive `exit`, so the code is MASKED, not
                // narrowed: `i32::try_from` on one past `i32::MAX` fell back to 0,
                // a different STATUS rather than a clamped one. But GNU holds it
                // in an `int` whose -1 means "no code was given", so a code whose
                // 32-bit truncation IS -1 cannot be told from a bare `q` and falls
                // back to the normal status instead of exiting 255. Masking alone
                // got that one wrong in the other direction.
                let code = match n {
                    Some(v) if u32::try_from(v & 0xffff_ffff).unwrap_or(0) != u32::MAX => {
                        i32::try_from(v & 0xff).unwrap_or(0)
                    }
                    _ => 0,
                };
                Kind::Quit(code, c == b'q')
            }
            b'r' | b'R' => {
                p.deny_in_sandbox()?;
                let name = p.parse_filename()?;
                match c {
                    // GNU spells its address-0 exemption as a REWRITE of the
                    // command, not as a case in the executor: `0rFILE` becomes
                    // `1rFILE` in prepend mode. The address change is what makes
                    // an empty input print nothing (line 1 never arrives), and
                    // the mode change is what puts the file before line 1's own
                    // output instead of on the queue behind it. This condition
                    // must stay `parse_addr`'s `prepends`: a `Zero` that is
                    // admitted and NOT rewritten reaches `kind_matches`, which
                    // never matches it, so the drift is a silent no-op.
                    b'r' if matches!(addr.a1, Some(AddrKind::Zero)) && addr.a2.is_none() => {
                        addr.a1 = Some(AddrKind::Line(1));
                        Kind::PrependFile(name)
                    }
                    b'r' => Kind::ReadFile(name),
                    _ => Kind::ReadLine(name),
                }
            }
            b'w' | b'W' => {
                p.deny_in_sandbox()?;
                let name = p.parse_filename()?;
                p.open_wfile(&name)?;
                match c {
                    b'w' => Kind::Write(name),
                    _ => Kind::WriteFirstLine(name),
                }
            }
            b's' => Kind::Subst(p.parse_subst()?),
            b'y' => Kind::Transliterate(p.parse_transliterate()?),
            b'z' => Kind::Zap,
            b'F' => Kind::FileName,
            b'v' => {
                // `v` does nothing except REFUSE a script written for a newer sed, so
                // its argument ends where a label does -- `v;p` runs the `p`, which
                // swallowing the line lost. GNU compares against its own version, so
                // td-txt compares against the GNU level it implements: comparing
                // against its OWN 0.1.0 would refuse `v 4.2`, which real scripts use.
                if version_is_newer(&p.parse_label()) {
                    return Err("expected newer version of sed".to_string().into());
                }
                Kind::Comment
            }
            b'e' => return Err("the `e' command is not supported".to_string().into()),
            other => return Err(format!("unknown command: `{}'", char::from(other)).into()),
        };
        // The commands NOT listed here have already consumed everything that could
        // need separating, each in its own way: a filename, `a`/`i`/`c` text and a
        // comment run to the end of the line; a LABEL and `v`'s version stop at a
        // blank, and GNU asks for nothing after them (`:a p` is a label then a
        // command); and `s`'s flag loop enforces this same terminator set itself,
        // with its own `unknown option to `s'`. `{` needs no separator, but its
        // `}` does.
        match &kind {
            Kind::Comment
            | Kind::Label
            | Kind::Block(_)
            | Kind::Branch(_)
            | Kind::BranchIfSub(_)
            | Kind::BranchIfNoSub(_)
            | Kind::Append(_)
            | Kind::Insert(_)
            | Kind::Change(_)
            | Kind::ReadFile(_)
            | Kind::PrependFile(_)
            | Kind::ReadLine(_)
            | Kind::Write(_)
            | Kind::WriteFirstLine(_)
            | Kind::Subst(_) => {}
            _ => p.end_of_cmd()?,
        }
        cmds.push(Cmd { addr, kind });
    }
    if !open_blocks.is_empty() {
        return Err("unmatched `{'".to_string().into());
    }
    Ok(Script { cmds, regexes: p.regexes, labels, wfiles: p.wfiles })
}

/// Turn every branch's label name into a command index. Done after the whole
/// script is parsed because a branch may jump forward.
fn resolve_labels(cmds: &mut [Cmd], labels: &BTreeMap<Vec<u8>, usize>) -> Result<(), String> {
    for cmd in cmds.iter_mut() {
        let name = match &cmd.kind {
            Kind::Branch(Target::Name(n))
            | Kind::BranchIfSub(Target::Name(n))
            | Kind::BranchIfNoSub(Target::Name(n)) => n.clone(),
            _ => continue,
        };
        let target = if name.is_empty() {
            None
        } else {
            match labels.get(&name) {
                Some(t) => Some(*t),
                None => return Err(format!("can't operate on label `{}'", show(&name))),
            }
        };
        cmd.kind = match &cmd.kind {
            Kind::Branch(_) => Kind::Branch(Target::At(target)),
            Kind::BranchIfSub(_) => Kind::BranchIfSub(Target::At(target)),
            _ => Kind::BranchIfNoSub(Target::At(target)),
        };
    }
    Ok(())
}

// ---- execution -----------------------------------------------------------

struct Line {
    text: Vec<u8>,
    /// The line ended with the separator; the last line of a file may not.
    terminated: bool,
}

/// The input a run reads, opened ONE OPERAND AT A TIME. Without `-s` every
/// operand is concatenated into one stream, but they are not read up front: GNU
/// opens the next only when the current one runs out, so an operand a `q` never
/// reaches is never opened and never reported. That ordering is the whole point
/// of the type — it decides which read failures exist at all.
struct Stream {
    /// Operands not yet opened. Under `-s`/`-i` the caller opens one operand per
    /// `Stream`, so this is empty. An iterator rather than a slice and a cursor:
    /// nothing revisits an operand, so nothing needs to look back at one.
    pending: std::vec::IntoIter<Vec<u8>>,
    separator: u8,
    /// The file currently OPEN: `F` names it. The `$` lookahead below can open
    /// the NEXT one mid-cycle, which is exactly when `F` starts naming that.
    name: Vec<u8>,
    lines: Vec<Line>,
    pos: usize,
    /// A read failure already suffered. It outranks a quit code, and because
    /// operands open lazily it can only describe a file the run actually reached.
    bad: bool,
    /// An operand the READER opened and could not read, which ends the run at 4.
    /// The diagnostic is printed where it happens, ahead of the buffered output,
    /// so it lands beside the other operand diagnostics as GNU's does.
    fatal: bool,
}

/// Who is opening the next operand. It decides only what a READ failure means:
/// GNU's reader panics on one -- `read error on NAME`, exit 4, and nothing after
/// it runs -- while the `$` lookahead peeks with `getc`, which reports the error
/// as end-of-file, so that operand is stepped over without a word. `sed -n p A
/// DIR` is the first and `sed -n '$F' A DIR` the second. An OPEN failure is the
/// same warning to both.
#[derive(Clone, Copy)]
enum Opener {
    Reader,
    Lookahead,
}

impl Stream {
    /// One operand's worth of already-read data, for the `-s`/`-i` path.
    fn of(path: &[u8], data: &[u8], separator: u8) -> Self {
        Self {
            pending: Vec::new().into_iter(),
            separator,
            name: path.to_vec(),
            lines: to_lines(data, separator),
            pos: 0,
            bad: false,
            fatal: false,
        }
    }

    /// Every operand, none of them read yet. Taken BY VALUE: nothing revisits an
    /// operand, so nothing needs a copy of one.
    fn of_operands(paths: Vec<Vec<u8>>, separator: u8) -> Self {
        Self {
            pending: paths.into_iter(),
            separator,
            name: b"-".to_vec(),
            lines: Vec::new(),
            pos: 0,
            bad: false,
            fatal: false,
        }
    }

    /// Open operands until one HAS A LINE, reporting each that cannot be OPENED and
    /// stepping over each that is empty — GNU's `last_file_with_data_p`, which is
    /// also how its `$` test answers. `false` once nothing is left, and once a read
    /// failed under the reader, which ends the run.
    fn advance(&mut self, opener: Opener) -> bool {
        while self.pos >= self.lines.len() {
            let Some(path) = self.pending.next() else {
                return false;
            };
            // GNU sets the name before it tries to open, so a failed operand is
            // what `F` would report until the next open replaces it.
            self.name = path;
            match Input::open(&self.name, true) {
                Ok(mut input) => match input.read_all() {
                    Ok(data) => {
                        self.lines = to_lines(&data, self.separator);
                        self.pos = 0;
                    }
                    // The bytes a failed read DID deliver are dropped: GNU panics
                    // at the failure rather than processing them, and the reachable
                    // case — a DIRECTORY — delivers none, failing at the first read.
                    Err((e, _)) => match opener {
                        Opener::Reader => {
                            let name = input.error_name(&self.name);
                            eprintln!("sed: read error on {name}: {}", errmsg(&e));
                            self.fatal = true;
                            return false;
                        }
                        Opener::Lookahead => {}
                    },
                },
                Err(e) => {
                    eprintln!("sed: can't read {}: {}", show(&self.name), errmsg(&e));
                    self.bad = true;
                }
            }
        }
        true
    }

    /// The file `F` names: the one most recently OPENED, not the one the current
    /// line came from. Those differ exactly when a `$` has looked ahead.
    fn current_name(&self) -> &[u8] {
        &self.name
    }

    /// The next line, or `None` at end of input. WHO is asking decides what an
    /// unreadable operand means, which is why the caller says: the cycle reads,
    /// and `n`/`N` reach input through GNU's `test_eof` first, so a read failure
    /// there is the peek's silent end-of-file rather than the reader's panic.
    fn next_line(&mut self, opener: Opener) -> Option<Line> {
        if !self.advance(opener) {
            return None;
        }
        let line = self.lines.get(self.pos)?;
        let out = Line { text: line.text.clone(), terminated: line.terminated };
        self.pos += 1;
        Some(out)
    }

    /// No further input line, so the line just read was `$`. Takes `&mut` because
    /// ANSWERING opens operands: that is observable, through `F` and through which
    /// unreadable operand gets reported, so only where GNU asks may this — a `$`
    /// the evaluator reaches, and the `test_eof` inside `n` and `N`.
    fn at_last(&mut self) -> bool {
        !self.advance(Opener::Lookahead)
    }
}

enum Append {
    Text(Vec<u8>),
    /// `r`: the FILENAME, read when the queue is flushed.
    File(Vec<u8>),
    /// `R`: the bytes, already read — GNU takes its line when the command runs, so a
    /// source that cannot be READ fails before the cycle's own output.
    Line(Vec<u8>),
}

/// Where a cycle's output goes: stdout, or a buffer that `-i` renames into place.
enum Dest<'a> {
    Stdout(&'a mut Out),
    Buffer(Vec<u8>),
}

/// Which logical stream a write belongs to, for the purpose of the owed separator.
///
/// GNU keeps the debt per OUTPUT STRUCT, and `w /dev/stdout` is a different struct
/// from the auto-print stream even though both reach the same fd. The two debts are
/// therefore independent: `printf x | sed -n -e 'w /dev/stdout' -e p` writes `xx`,
/// because `p` does not pay what `w` owes, and neither does `q`'s settle.
#[derive(Clone, Copy)]
enum Chan {
    /// Auto-print, `p`/`P`, `=`, `l`, `i`/`c`, and the append queue.
    Main,
    /// `w`/`W` aimed at `/dev/stdout`.
    WFile,
}

/// The output stream, which OWES a separator rather than dropping one.
///
/// GNU sed omits the trailing separator only at the very END of its output: with
/// input `a\nb` (no final newline), `sed p` writes `a\na\nb\nb` — the first copy
/// of the unterminated line still gets its newline, because more output followed.
/// So an unterminated line sets `owed`, and the next write on the SAME channel
/// pays it.
struct Sink<'a> {
    dest: Dest<'a>,
    separator: u8,
    owed: bool,
    owed_wfile: bool,
}

impl<'a> Sink<'a> {
    fn stdout(out: &'a mut Out, separator: u8) -> Self {
        Self { dest: Dest::Stdout(out), separator, owed: false, owed_wfile: false }
    }

    fn buffer(separator: u8) -> Self {
        Self { dest: Dest::Buffer(Vec::new()), separator, owed: false, owed_wfile: false }
    }

    fn debt(&mut self, chan: Chan) -> &mut bool {
        match chan {
            Chan::Main => &mut self.owed,
            Chan::WFile => &mut self.owed_wfile,
        }
    }

    fn put(&mut self, bytes: &[u8]) -> Result<(), String> {
        match &mut self.dest {
            Dest::Stdout(out) => out.write(bytes).map_err(|e| format!("write error: {}", errmsg(&e))),
            Dest::Buffer(buf) => {
                buf.extend_from_slice(bytes);
                Ok(())
            }
        }
    }

    /// Pay a separator the channel's previous line left owed, if any.
    fn pay(&mut self, chan: Chan) -> Result<(), String> {
        if *self.debt(chan) {
            *self.debt(chan) = false;
            let sep = self.separator;
            return self.put(&[sep]);
        }
        Ok(())
    }

    /// Write, paying any separator the previous line left owed.
    fn write(&mut self, bytes: &[u8]) -> Result<(), String> {
        self.pay(Chan::Main)?;
        self.put(bytes)
    }

    /// Queued `a`/`r` text. GNU terminates it with a NEWLINE even under `-z`
    /// (the append queue dumps the text as parsed), where `i`/`c` follow the
    /// record separator.
    fn write_text(&mut self, bytes: &[u8]) -> Result<(), String> {
        self.write(bytes)?;
        self.put(b"\n")
    }

    /// Write one line; an unterminated one owes its separator to the next write on
    /// the same channel.
    fn write_line_on(&mut self, chan: Chan, bytes: &[u8], terminated: bool) -> Result<(), String> {
        self.pay(chan)?;
        self.put(bytes)?;
        if terminated {
            let sep = self.separator;
            return self.put(&[sep]);
        }
        *self.debt(chan) = true;
        Ok(())
    }

    fn write_line(&mut self, bytes: &[u8], terminated: bool) -> Result<(), String> {
        self.write_line_on(Chan::Main, bytes, terminated)
    }

    /// Pay a separator that an unterminated line left owed. Reaching end of input
    /// leaves it owed forever, which is how a file with no final newline keeps that
    /// shape; `q` settles the debt instead — the MAIN channel's only, since GNU's
    /// `q` leaves an unterminated `w /dev/stdout` write bare.
    fn settle(&mut self) -> Result<(), String> {
        self.pay(Chan::Main)
    }

    fn flush(&mut self) -> Result<(), String> {
        match &mut self.dest {
            Dest::Stdout(out) => out.flush().map_err(|e| format!("write error: {}", errmsg(&e))),
            Dest::Buffer(_) => Ok(()),
        }
    }

    fn into_buffer(self) -> Vec<u8> {
        match self.dest {
            Dest::Buffer(buf) => buf,
            Dest::Stdout(_) => Vec::new(),
        }
    }
}

/// One `w` target. GNU keeps an output per FILENAME, each with its OWN missing
/// separator debt, so `printf x | sed -n -e 'w f' -e 'w f'` writes `x\nx` while the
/// last write stays bare. `/dev/stderr` is an ordinary target to GNU too: raw
/// bytes, no forced newline, its own debt.
#[derive(Debug)]
struct WFile {
    dest: WDest,
    owed: bool,
}

#[derive(Debug)]
enum WDest {
    File(std::fs::File),
    Stderr,
    /// `/dev/stdout` under `-i` only; otherwise it rides the auto-print sink.
    Stdout,
}

impl WFile {
    fn put(&mut self, bytes: &[u8]) -> Result<(), String> {
        use std::io::Write as _;
        let wrote = match &mut self.dest {
            WDest::File(f) => f.write_all(bytes),
            WDest::Stderr => std::io::stderr().write_all(bytes),
            WDest::Stdout => std::io::stdout().write_all(bytes),
        };
        wrote.map_err(|e| format!("write error: {}", errmsg(&e)))
    }

    fn write_line(&mut self, bytes: &[u8], terminated: bool, separator: u8) -> Result<(), String> {
        if self.owed {
            self.owed = false;
            self.put(&[separator])?;
        }
        self.put(bytes)?;
        if terminated {
            return self.put(&[separator]);
        }
        self.owed = true;
        Ok(())
    }
}

/// What ended a cycle.
enum Flow {
    /// Run the end-of-cycle auto-print.
    Done,
    /// `d`/`c`/`n`-at-EOF: skip the auto-print.
    Deleted,
    /// `D` with an embedded newline: rerun the script without reading input.
    Restart,
    Quit,
}

struct Sed {
    script: Script,
    ranges: Vec<RangeState>,
    suppress: bool,
    separator: u8,
    posix: bool,
    /// The fallback width (`COLS`, then `-l`), read HERE rather than at compile
    /// time because GNU reads it when `l` runs: `sed -e l -l 12` folds at 12
    /// though the option came after.
    line_wrap: usize,
    pattern: Vec<u8>,
    hold: Vec<u8>,
    /// GNU tracks `chomped` per BUFFER: after `g`/`G`/`x` the pattern space's
    /// terminator is the hold space's, not the input line's. Starts terminated,
    /// so `G` on an unterminated last line still emits one.
    hold_terminated: bool,
    line_number: u64,
    /// `-i`. Only `w /dev/stdout` reads it: the auto-print stream is then the
    /// replacement buffer, and that write still has to reach standard output.
    in_place: bool,
    terminated: bool,
    appends: Vec<Append>,
    replaced: bool,
    quit: Option<i32>,
    /// Index in the regex table of the last regex applied — what `//` reuses.
    last_regex: Option<usize>,
    wfiles: BTreeMap<Vec<u8>, WFile>,
    /// `R` reads ONE line per invocation, so the file is parsed once and the
    /// cursor kept; re-reading per line would be quadratic. Keyed by NAME, not
    /// per command: two `R f` commands share one cursor in GNU, and advance it
    /// twice in a cycle.
    rfiles: BTreeMap<Vec<u8>, RFile>,
}

/// Does a single address match the current line? Free of `Sed` so the caller can
/// hold a borrow of the script while consulting the pattern space.
/// `0,/re/` is ACTIVE before the first line — that is the whole point of the
/// extension: the end regex may match on line 1. Seeding the state says so
/// directly, rather than special-casing line 1 in the start path (which a
/// leading `n`/`N` would then skip past).
fn seed_ranges(cmds: &[Cmd]) -> Vec<RangeState> {
    cmds.iter()
        .map(|c| match c.addr.a1 {
            Some(AddrKind::Zero) if c.addr.is_range() => RangeState::Active(0),
            _ => RangeState::Inactive,
        })
        .collect()
}

/// GNU's `addr,~N` end: the next multiple of N strictly greater than the start
/// (`line + N - line % N`), so `4,~2` runs through 6.
fn multiple_end(start: u64, n: u64) -> u64 {
    if n == 0 {
        return start;
    }
    // Saturating and wrapping cannot differ OBSERVABLY here, which is why `+N`'s
    // end moved to wrapping and this did not. Overflow needs `n` past the start
    // (or a start past 2^63, which no file reaches), and then `start % n` is
    // `start`, so the end is `n` wrapping and `u64::MAX - start` saturating --
    // both past any input. Changing it would be a rule no case could red.
    start.saturating_add(n).saturating_sub(start % n)
}

fn kind_matches(
    kind: &AddrKind,
    regexes: &[Regex],
    last_regex: Option<usize>,
    pattern: &[u8],
    line_number: u64,
    stream: &mut Stream,
) -> Result<(bool, Option<usize>), String> {
    match kind {
        AddrKind::Line(n) => Ok((line_number == *n, None)),
        AddrKind::Zero => Ok((false, None)),
        AddrKind::Always => Ok((true, None)),
        // Asked HERE and nowhere else, because asking OPENS the next operand:
        // a `$` that is never evaluated -- the end of a range that has not
        // started -- must not open one. GNU's `match_address_p` reaches its
        // `test_eof` on the same terms.
        AddrKind::Last => Ok((stream.at_last(), None)),
        // `step` is never 0 here: `first~0` is normalised to a plain line at
        // parse time, so this arm is only ever a real stride.
        AddrKind::Step { first, step } => {
            Ok((line_number >= *first && (line_number - *first).is_multiple_of(*step), None))
        }
        AddrKind::Rx(slot) => {
            let idx = match slot {
                Some(i) => *i,
                None => last_regex.ok_or_else(|| NO_PREVIOUS_REGEX.to_string())?,
            };
            let re = regexes.get(idx).ok_or_else(|| NO_PREVIOUS_REGEX.to_string())?;
            Ok((re.is_match(pattern).map_err(|e| e.msg)?, Some(idx)))
        }
    }
}

impl Sed {
    /// Does the command at `idx` select this line? Advances the range state.
    ///
    /// Ranges follow GNU's `match_address_p` (sed/execute.c) exactly, including
    /// its two asymmetries: a LINE-NUMBER start is absolute — it fires on the
    /// first line at or past it, so a start line consumed by `N`/`n`/`D` is not
    /// missed — and only such a range is subject to the line-number end test on
    /// its own first line. A range started by a regex always selects at least
    /// that line.
    fn addr_matches(&mut self, idx: usize, stream: &mut Stream) -> Result<bool, String> {
        // The script is only read through raw pieces here so each borrow ends
        // before the range state (or `last_regex`) is written.
        let Some(cmd) = self.script.cmds.get(idx) else {
            return Ok(false);
        };
        let negate = cmd.addr.negate;
        if cmd.addr.a1.is_none() {
            return Ok(!negate);
        }
        if !cmd.addr.is_range() {
            let hit = self.match_a1(idx, stream)?;
            return Ok(hit != negate);
        }
        let a1_line = match cmd.addr.a1 {
            Some(AddrKind::Line(n)) => Some(n),
            _ => None,
        };
        let state = self.ranges.get(idx).copied().unwrap_or_default();

        let hit = if let RangeState::Active(start) = state {
            let (ends, select) = self.range_step(idx, start, stream)?;
            if ends {
                self.set_range(idx, RangeState::Closed);
            }
            select
        } else {
            let starts = match a1_line {
                // Absolute, and never re-armed once closed.
                Some(n) => state != RangeState::Closed && self.line_number >= n,
                // `AddrKind::Zero` (`0,/re/`) never matches as a start: the
                // range is seeded ACTIVE before the first cycle instead.
                None => self.match_a1(idx, stream)?,
            };
            if !starts {
                false
            } else {
                let start = self.line_number;
                let (ends, select) = self.range_opens(idx, start, a1_line, stream)?;
                self.set_range(
                    idx,
                    if ends { RangeState::Closed } else { RangeState::Active(start) },
                );
                select
            }
        };
        Ok(hit != negate)
    }

    fn set_range(&mut self, idx: usize, state: RangeState) {
        if let Some(slot) = self.ranges.get_mut(idx) {
            *slot = state;
        }
    }

    /// The line a range STARTS on. A regex end is not tested here (GNU includes
    /// at least two lines); a line-number end is, but only bars the line when the
    /// start address was itself a line number.
    fn range_opens(
        &mut self,
        idx: usize,
        start: u64,
        a1_line: Option<u64>,
        stream: &mut Stream,
    ) -> Result<(bool, bool), String> {
        let line = self.line_number;
        match self.script.cmds.get(idx).map(|c| &c.addr.a2) {
            Some(Some(Addr2::Kind(AddrKind::Line(n)))) => {
                let n = *n;
                // An end already behind the start still yields ONE line when the
                // range starts on its own start line (`3,1d` deletes line 3).
                // It yields none when the start line was overshot — `N` jumping
                // from line 1 to 3 past a `1,2` range.
                let select = match a1_line {
                    Some(a1) => line == a1 || line <= n,
                    None => true,
                };
                Ok((line >= n, select))
            }
            // `$` is absolute like a line number: `1,$c\TEXT` on a one-line
            // file must CLOSE on that line, or `c` never prints. So is a
            // `first~step` end, which is why it is here and not below: GNU
            // tests every NUMERIC end on the start line, and only a REGEX end
            // gets its "at least two lines" rule. Untested, `1,1~0d` emptied
            // the file -- the same shape as the address-0 bug, in the last
            // numeric spelling.
            // `+N` counts FROM the start line, so on that line the range ends
            // only when N is 0. `line >= start + N` says the same thing for every
            // N that does not overflow, and something else for one that does: an
            // end that WRAPPED to before the start must still give the range its
            // second line, which is what GNU does -- `1,+(2^64-1)` ends at line 0
            // and selects lines 1 and 2, not line 1 alone.
            Some(Some(Addr2::Plus(n))) => Ok((*n == 0, true)),
            Some(Some(
                Addr2::Multiple(_) | Addr2::Kind(AddrKind::Last | AddrKind::Step { .. }),
            )) => {
                let (ends, _) = self.range_step(idx, start, stream)?;
                Ok((ends, true))
            }
            _ => Ok((false, true)),
        }
    }

    /// A line INSIDE a running range: does it end here, and is it selected?
    fn range_step(
        &mut self,
        idx: usize,
        start: u64,
        stream: &mut Stream,
    ) -> Result<(bool, bool), String> {
        let line = self.line_number;
        let kind = match self.script.cmds.get(idx).map(|c| &c.addr.a2) {
            // `start + N` WRAPS, as GNU's counter does: `1,+(2^64-1)` ends at
            // line 0, so the range closes on the line after it opened rather
            // than running to the end of input. Saturating made it run forever.
            Some(Some(Addr2::Plus(n))) => return Ok((line >= start.wrapping_add(*n), true)),
            Some(Some(Addr2::Multiple(n))) => return Ok((line >= multiple_end(start, *n), true)),
            // Past the end line the range closes WITHOUT selecting: reachable
            // when `b`/`t`/`N` jumped over the end.
            // A second address of `0` arrives here as `Line(0)`, normalised at
            // parse time: behind every start, so it closes at once and selects
            // nothing past the start line. Untreated it fell through to
            // `kind_matches`, never matched, never ended, and `1,0d` emptied
            // the file.
            Some(Some(Addr2::Kind(AddrKind::Line(n)))) => return Ok((line >= *n, line <= *n)),
            Some(Some(Addr2::Kind(k))) => k,
            _ => return Ok((true, true)),
        };
        let (hit, used) = kind_matches(
            kind,
            &self.script.regexes,
            self.last_regex,
            &self.pattern,
            line,
            stream,
        )?;
        if let Some(i) = used {
            self.last_regex = Some(i);
        }
        Ok((hit, true))
    }

    fn match_a1(&mut self, idx: usize, stream: &mut Stream) -> Result<bool, String> {
        let kind = match self.script.cmds.get(idx).map(|c| &c.addr.a1) {
            Some(Some(k)) => k,
            _ => return Ok(false),
        };
        // `kind` borrows the script; `kind_matches` is free of `self` for that
        // reason, and the one piece of state it can advance is returned.
        let (hit, used) = kind_matches(
            kind,
            &self.script.regexes,
            self.last_regex,
            &self.pattern,
            self.line_number,
            stream,
        )?;
        if let Some(i) = used {
            self.last_regex = Some(i);
        }
        Ok(hit)
    }

}

/// `l`'s WIDTH ARGUMENT, from the number the script wrote. GNU keeps it in the
/// same `int` as `q`'s exit code, so it TRUNCATES to 32 bits — `l4294967306`
/// folds at 10 — and a result that is not POSITIVE never wraps.
fn list_width(w: Option<u64>) -> Option<usize> {
    match w.map(|v| u32::try_from(v & 0xffff_ffff).unwrap_or(0)) {
        // -1 is the SAME sentinel `q` has, and it means "no width argument", not
        // "never wrap": `l4294967295` folds where a bare `l` folds, which is at
        // `-l`'s width and only therefore at 70. Reading it as non-positive got
        // that wrong; reading it as 70 got the other half wrong.
        None | Some(u32::MAX) => None,
        Some(low32) => Some(match i32::try_from(low32) {
            Ok(n) => usize::try_from(n).unwrap_or(0),
            // Any OTHER negative really is "never wrap". No case can red this
            // arm -- the alternative is a width of ~4.3e9, which no line reaches.
            Err(_) => 0,
        }),
    }
}

/// C `atoi`: leading whitespace, an optional sign, decimal digits, and whatever
/// follows them ignored rather than diagnosed. It cannot fail, so a word with no
/// digits at all is 0. Both inputs to the fallback width are read with this —
/// `-l`'s argument and `COLS` — and neither is the reader the SCRIPT's own
/// numbers use, in two ways. Overflow SATURATES, because `strtol` clamps at
/// `LONG_MAX` where `in_integer` wraps: same digits, so `-l 18446744073709551626`
/// never folds while `l18446744073709551626` folds at 10. And the result is a C
/// `int`, so it can be NEGATIVE — `-l -4294967286` is a positive 10.
fn atoi(arg: &[u8]) -> i32 {
    let mut i = 0usize;
    // `strtol`'s leading run is C `isspace`, which is wider than `skip_blank`'s
    // space-and-tab: `-l '\v12'` is a width of 12.
    while matches!(arg.get(i), Some(b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')) {
        i += 1;
    }
    let negative = match arg.get(i) {
        Some(b'-') => {
            i += 1;
            true
        }
        Some(b'+') => {
            i += 1;
            false
        }
        _ => false,
    };
    // The magnitude a `long` can hold: one more below zero than above it.
    let limit = if negative { 1u64 << 63 } else { (1u64 << 63) - 1 };
    let mut mag = 0u64;
    while let Some(d) = arg.get(i).filter(|b| b.is_ascii_digit()) {
        mag = mag
            .saturating_mul(10)
            .saturating_add(u64::from(*d - b'0'))
            .min(limit);
        i += 1;
    }
    // The low 32 bits of the two's-complement value, REINTERPRETED rather than
    // converted: the sign is part of the answer here, not a failure.
    let low32 = if negative {
        0u64.wrapping_sub(mag) & 0xffff_ffff
    } else {
        mag & 0xffff_ffff
    };
    let bits = u32::try_from(low32).unwrap_or(0);
    i32::from_ne_bytes(bits.to_ne_bytes())
}

/// `-l N`'s width. GNU keeps the width in an UNSIGNED counter, so a negative
/// `int` becomes a number no line reaches and folds nowhere — which is what 0,
/// the width `-l abc` gives, already means here.
fn line_length_arg(arg: &[u8]) -> usize {
    usize::try_from(atoi(arg)).unwrap_or(0)
}

/// `COLS`, the OTHER input to the fallback width, and it is not `-l` with a
/// different name. GNU takes `COLS - 1` -- one column kept back so a tty does not
/// wrap the line itself -- and only when the value exceeds 1, so `COLS=2` folds
/// at 1 while `COLS=1`, `COLS=0` and `COLS=abc` leave the default alone. The
/// comparison happens AFTER the `int` lands in that unsigned counter, so a
/// negative passes it as a huge number: `COLS=-1` folds nowhere rather than
/// leaving 70. `None` is "say nothing about the width".
fn cols_line_wrap(cols: &[u8]) -> Option<usize> {
    match atoi(cols) {
        n if n >= 2 => Some(usize::try_from(n - 1).unwrap_or(0)),
        n if n < 0 => Some(0),
        _ => None,
    }
}

fn escape_for_l(bytes: &[u8], width: usize, separator: u8, out: &mut Vec<u8>) {
    let mut col = 0usize;
    let push = |chunk: &[u8], col: &mut usize, out: &mut Vec<u8>| {
        // `l 0` never wraps; `l 1` wraps before EVERY chunk, which GNU shows as a
        // leading `\` on its own since the test already holds at column 0.
        if width > 0 && *col + chunk.len() >= width {
            out.push(b'\\');
            // The WRAP ends with the record separator, like the `$` below: under `-z`
            // GNU breaks a long `l` line with a backslash and a NUL, not a newline.
            out.push(separator);
            *col = 0;
        }
        out.extend_from_slice(chunk);
        *col += chunk.len();
    };
    for b in bytes {
        match *b {
            b'\\' => push(b"\\\\", &mut col, out),
            0x07 => push(b"\\a", &mut col, out),
            0x08 => push(b"\\b", &mut col, out),
            0x0c => push(b"\\f", &mut col, out),
            b'\n' => push(b"\\n", &mut col, out),
            b'\r' => push(b"\\r", &mut col, out),
            b'\t' => push(b"\\t", &mut col, out),
            0x0b => push(b"\\v", &mut col, out),
            c if (0x20..0x7f).contains(&c) => push(&[c], &mut col, out),
            c => push(
                &[b'\\', b'0' + (c >> 6), b'0' + ((c >> 3) & 7), b'0' + (c & 7)],
                &mut col,
                out,
            ),
        }
    }
    out.push(b'$');
    out.push(separator);
}

/// `\U`/`\L` set a run; `\u`/`\l` apply to the next byte only; `\E` clears both.
#[derive(Default)]
struct CaseState {
    run: Option<CaseOp>,
    once: Option<CaseOp>,
}

fn push_cased(bytes: &[u8], state: &mut CaseState, out: &mut Vec<u8>) {
    for b in bytes {
        let mut c = *b;
        if let Some(one) = state.once.take() {
            c = if one == CaseOp::UpperOne { c.to_ascii_uppercase() } else { c.to_ascii_lowercase() };
        } else if let Some(run) = state.run {
            c = if run == CaseOp::Upper { c.to_ascii_uppercase() } else { c.to_ascii_lowercase() };
        }
        out.push(c);
    }
}

/// Expand one `s///` replacement against a match.
fn expand(repl: &[Repl], hay: &[u8], caps: &Captures, out: &mut Vec<u8>) {
    let mut state = CaseState::default();
    for piece in repl {
        match piece {
            Repl::Literal(text) => push_cased(text, &mut state, out),
            Repl::Group(n) => {
                if let Some((s, e)) = caps.group(*n) {
                    let text = hay.get(s..e).unwrap_or_default().to_vec();
                    push_cased(&text, &mut state, out);
                }
            }
            Repl::Case(CaseOp::End) => state = CaseState::default(),
            Repl::Case(op @ (CaseOp::Upper | CaseOp::Lower)) => {
                state.run = Some(*op);
                state.once = None;
            }
            Repl::Case(op) => state.once = Some(*op),
        }
    }
}

#[derive(Debug)]
/// A fatal error and the status GNU sed exits with for it: 1 for a bad script
/// or usage, 2 for an unreadable input, 4 for a runtime failure — of which an
/// unresolvable branch label is one, because GNU resolves labels while running.
struct Fatal {
    msg: String,
    status: i32,
}

impl From<String> for Fatal {
    fn from(msg: String) -> Self {
        // GNU reports the `[:alpha:]`-for-`[[:alpha:]]` refusal bare and exits 4,
        // alone among pattern errors; classified here by text for the same reason
        // NO_PREVIOUS_REGEX is, so the raising site and this boundary cannot drift.
        let status = if msg == crate::regex::CLASS_SYNTAX { 4 } else { 1 };
        Self { msg, status }
    }
}

/// The one error raised WHILE RUNNING that GNU still classifies as a bad SCRIPT:
/// an empty `s//…/` or `//p` can only be checked once the previous regex is known,
/// but it is a mistake in the program, not a refusal by the filesystem — exit 1,
/// with the `-e expression #N' prefix. One constant, named at the sites that raise
/// it and at the boundary that classifies it, so the two cannot drift apart.
const NO_PREVIOUS_REGEX: &str = "no previous regular expression";

impl Fatal {
    /// A failure the FILESYSTEM raised rather than the script — a `w` file that will
    /// not open, a write that fails. Exit 4, and no `-e expression #N' prefix: the
    /// script was well formed and something outside it refused. Most of these
    /// happen while running, but a `w` target opens during the PARSE, so this is
    /// reachable from there too. `From<String>` stamps 1 for the malformed-script
    /// errors `parse_script` raises, so every such `String` has to come through
    /// here or it lands in the wrong bucket wearing the wrong prefix.
    fn runtime(msg: String) -> Self {
        let status = if msg == NO_PREVIOUS_REGEX { 1 } else { 4 };
        Self { msg, status }
    }
}

struct Conf {
    suppress: bool,
    sandbox: bool,
    ere: bool,
    separate: bool,
    null_data: bool,
    in_place: Option<Vec<u8>>,
    posix: bool,
    /// The width a bare `l` folds at: `COLS` seeds it, `-l N` overrides it, and
    /// with neither it is 70.
    line_wrap: usize,
}

pub fn main(args: &[Vec<u8>]) -> i32 {
    match run(args) {
        Ok(code) => code,
        Err(f) => {
            // The `-e expression #N' prefix belongs to a failure to COMPILE the
            // script (exit 1). A runtime failure — an unopenable `-f' file, an
            // unresolvable label (exit 4) — is not about an expression, and GNU
            // reports it bare; naming an expression there points at the wrong
            // thing.
            if f.status == 1 {
                eprintln!("sed: -e expression #1: {}", f.msg);
            } else {
                eprintln!("sed: {}", f.msg);
            }
            f.status
        }
    }
}

/// Parse a script and resolve its branches. Split from `parse_script` so the two
/// failures keep their own exit statuses (see `Fatal`).
fn compile_script(
    src: &[u8],
    ere: bool,
    sandbox: bool,
    null_data: bool,
    part_ends: Vec<usize>,
    posix: bool,
) -> Result<Script, Fatal> {
    let mut script = parse_script(src, ere, null_data, part_ends, posix, sandbox)?;
    let labels = std::mem::take(&mut script.labels);
    resolve_labels(&mut script.cmds, &labels).map_err(|msg| Fatal { msg, status: 4 })?;
    Ok(script)
}

/// Every long option sed knows, so an unambiguous PREFIX resolves the way GNU's
/// `getopt_long` accepts one (`sed --quie`, `sed --expr=2d`). An exact name
/// always wins over being a prefix of a longer one.
const LONG_OPTIONS: &[&[u8]] = &[
    b"debug",
    b"expression",
    b"file",
    b"follow-symlinks",
    b"help",
    b"in-place",
    b"line-length",
    b"null-data",
    b"posix",
    b"quiet",
    b"regexp-extended",
    b"sandbox",
    b"separate",
    b"silent",
    b"unbuffered",
    b"version",
    b"zero-terminated",
];

/// `Ok(full name)`, or `Err(msg)` where an EMPTY msg means "no such option" and
/// a non-empty one is GNU's ambiguity diagnostic.
fn resolve_long(name: &[u8]) -> Result<&'static [u8], String> {
    let mut hits: Vec<&'static [u8]> = Vec::new();
    for cand in LONG_OPTIONS {
        if *cand == name {
            return Ok(cand);
        }
        if cand.starts_with(name) {
            hits.push(cand);
        }
    }
    match hits.as_slice() {
        [one] => Ok(one),
        [] => Err(String::new()),
        many => {
            let list: Vec<String> = many.iter().map(|n| format!("'--{}'", show(n))).collect();
            Err(format!(
                "option '--{}' is ambiguous; possibilities: {}",
                show(name),
                list.join(" ")
            ))
        }
    }
}

/// A missing option ARGUMENT, reported the way GNU reports one: bare, with the
/// usage after it, at exit 1. Not through `Fatal`, whose status-1 path prefixes
/// `-e expression #1:` — true of a script that failed to compile and false of an
/// option that never got its value.
fn missing_short_argument(opt: u8) -> i32 {
    eprintln!("sed: option requires an argument -- '{}'", char::from(opt));
    eprintln!("{USAGE}");
    1
}

#[allow(clippy::too_many_lines)] // one option table; splitting it would only hide it
fn run(args: &[Vec<u8>]) -> Result<i32, Fatal> {
    let mut conf = Conf {
        suppress: false,
        sandbox: false,
        ere: false,
        separate: false,
        null_data: false,
        in_place: None,
        // NOT driven from POSIXLY_CORRECT: that variable and `--posix` are
        // different switches in GNU, and `conf.posix` is `--posix`'s. See the
        // POSIXLY_CORRECT gap in spec/README.
        posix: false,
        // Read BEFORE the options, as GNU reads it, so `-l` overrides it and not
        // the other way round.
        line_wrap: std::env::var_os("COLS")
            .and_then(|v| cols_line_wrap(v.as_bytes()))
            .unwrap_or(DEFAULT_LINE_WRAP),
    };
    let mut script_parts: Vec<Vec<u8>> = Vec::new();
    // Only the FIRST script part can carry `#n`, and a `-f` one carries it only
    // if its stream can seek. A literal or `-e` part always can.
    let mut hash_n_carries = true;
    let mut script_given = false;
    let mut operands: Vec<Vec<u8>> = Vec::new();
    let mut no_more_options = false;
    let posixly = posixly_correct();

    let mut i = 1usize;
    while let Some(arg) = args.get(i) {
        i += 1;
        if no_more_options || arg.first() != Some(&b'-') || arg.len() == 1 {
            operands.push(arg.clone());
            // Under POSIXLY_CORRECT the first operand ends options, so what
            // follows is a FILE, `--` included. (sed has no `-NUM` spelling.)
            no_more_options |= posixly;
            continue;
        }
        if arg.as_slice() == b"--" {
            no_more_options = true;
            continue;
        }
        if arg.starts_with(b"--") {
            let body = arg.get(2..).unwrap_or_default();
            let (name, inline) = match body.iter().position(|b| *b == b'=') {
                Some(eq) => (
                    body.get(..eq).unwrap_or_default(),
                    Some(body.get(eq + 1..).unwrap_or_default().to_vec()),
                ),
                None => (body, None),
            };
            let name = match resolve_long(name) {
                Ok(full) => full,
                Err(msg) => {
                    if msg.is_empty() {
                        eprintln!("sed: unrecognized option '--{}'", show(name));
                    } else {
                        eprintln!("sed: {msg}");
                    }
                    eprintln!("{USAGE}");
                    return Ok(1);
                }
            };
            match name {
                // Answered on stdout, exit 0, before any later option applies.
                // The text is td-txt's own: a GNU banner would be a lie a caller
                // could act on.
                // Answered on stdout, exit 0, before any later option applies —
                // but neither takes a value, so `--version=x` is the error GNU
                // makes it. The text is td-txt's own: a GNU banner would be a lie
                // a caller could act on.
                b"help" | b"version" => {
                    if inline.is_some() {
                        eprintln!("sed: option '--{}' doesn't allow an argument", show(name));
                        eprintln!("{USAGE}");
                        return Ok(1);
                    }
                    let line = if name == b"help" {
                        USAGE.to_string()
                    } else {
                        format!("sed (td-txt) {VERSION}")
                    };
                    return Ok(match print_line(&line) {
                        Ok(()) => 0,
                        Err(e) => {
                            eprintln!("sed: write error: {}", errmsg(&e));
                            4
                        }
                    });
                }
                b"quiet" | b"silent" => conf.suppress = true,
                b"regexp-extended" => conf.ere = true,
                b"separate" => conf.separate = true,
                b"null-data" | b"zero-terminated" => conf.null_data = true,
                b"posix" => conf.posix = true,
                b"sandbox" => conf.sandbox = true,
                b"unbuffered" => {} // output content does not depend on buffering
                // Accepting these silently would fail OPEN: --debug must print an
                // annotated program, and --follow-symlinks changes which file -i
                // rewrites. Refusing is the honest answer until they are built.
                b"debug" | b"follow-symlinks" => {
                    eprintln!("sed: unsupported option -- '{}'", String::from_utf8_lossy(name));
                    eprintln!("{USAGE}");
                    return Ok(1);
                }
                b"in-place" => {
                    conf.separate = true;
                    conf.in_place = Some(inline.unwrap_or_default());
                }
                b"expression" | b"file" | b"line-length" => {
                    let value = match inline {
                        Some(v) => v,
                        None => {
                            let v = args.get(i).cloned();
                            i += 1;
                            match v {
                                Some(v) => v,
                                // A missing option ARGUMENT is reported bare, with
                                // the usage after it: the failure is in argv, not in
                                // expression #1, which is what the `Fatal` path would
                                // have named.
                                None => {
                                    eprintln!(
                                        "sed: option '--{}' requires an argument",
                                        show(name)
                                    );
                                    eprintln!("{USAGE}");
                                    return Ok(1);
                                }
                            }
                        }
                    };
                    if name == b"line-length" {
                        conf.line_wrap = line_length_arg(&value);
                    } else {
                        if name == b"file" {
                            // Same failure as short `-f`, so the same status and the
                            // same wording: the script did not fail to COMPILE, the
                            // file failed to open.
                            let (text, seekable) = read_script(&value).map_err(|e| Fatal {
                                msg: format!("couldn't open file {}: {}", show(&value), errmsg(&e)),
                                status: 4,
                            })?;
                            if script_parts.is_empty() {
                                hash_n_carries = seekable;
                            }
                            script_parts.push(text);
                        } else {
                            script_parts.push(value);
                        }
                        script_given = true;
                    }
                }
                _ => {
                    eprintln!("sed: unrecognized option '{}'", show(arg));
                    eprintln!("{USAGE}");
                    return Ok(1);
                }
            }
            continue;
        }
        let mut j = 1usize;
        while let Some(opt) = arg.get(j).copied() {
            j += 1;
            let value_of = |j: &mut usize, i: &mut usize| -> Option<Vec<u8>> {
                if let Some(rest) = arg.get(*j..) {
                    if !rest.is_empty() {
                        *j = arg.len();
                        return Some(rest.to_vec());
                    }
                }
                let v = args.get(*i).cloned();
                if v.is_some() {
                    *i += 1;
                }
                v
            };
            match opt {
                b'n' => conf.suppress = true,
                b'r' | b'E' => conf.ere = true,
                b's' => conf.separate = true,
                b'z' => conf.null_data = true,
                b'u' => {}
                b'i' => {
                    // The backup suffix is ATTACHED, never a separate argument:
                    // `sed -i -e …` must not swallow `-e` as the suffix.
                    let suffix = arg.get(j..).unwrap_or_default().to_vec();
                    j = arg.len();
                    conf.separate = true;
                    conf.in_place = Some(suffix);
                }
                b'l' => {
                    let Some(v) = value_of(&mut j, &mut i) else {
                        return Ok(missing_short_argument(b'l'));
                    };
                    conf.line_wrap = line_length_arg(&v);
                }
                b'e' => {
                    let Some(v) = value_of(&mut j, &mut i) else {
                        return Ok(missing_short_argument(b'e'));
                    };
                    script_parts.push(v);
                    script_given = true;
                }
                b'f' => {
                    let Some(v) = value_of(&mut j, &mut i) else {
                        return Ok(missing_short_argument(b'f'));
                    };
                    // An unreadable SCRIPT file is exit 4, like any other
                    // runtime failure — not 1, which means a bad script.
                    let (text, seekable) = read_script(&v).map_err(|e| Fatal {
                        msg: format!("couldn't open file {}: {}", show(&v), errmsg(&e)),
                        status: 4,
                    })?;
                    if script_parts.is_empty() {
                        hash_n_carries = seekable;
                    }
                    script_parts.push(text);
                    script_given = true;
                }
                _ => {
                    eprintln!("sed: invalid option -- '{}'", char::from(opt));
                    eprintln!("{USAGE}");
                    return Ok(1);
                }
            }
        }
    }

    let mut operands = operands.into_iter();
    if !script_given {
        match operands.next() {
            Some(s) => script_parts.push(s),
            None => {
                eprintln!("{USAGE}");
                return Ok(1);
            }
        }
    }
    let files: Vec<Vec<u8>> = operands.collect();

    let mut source: Vec<u8> = Vec::new();
    let mut part_ends: Vec<usize> = Vec::new();
    for (n, part) in script_parts.iter().enumerate() {
        if n > 0 {
            part_ends.push(source.len());
            source.push(b'\n');
        }
        source.extend_from_slice(part);
    }
    // `#n` is POSIX's in-script spelling of -n, and the rule is about the first
    // two BYTES of the script, not the first line: `#nx` and `#n;p` suppress in
    // GNU as surely as `#n` does, the rest of the line being comment either way.
    // Requiring a newline after them made every such script print twice over.
    if hash_n_carries && source.starts_with(b"#n") {
        conf.suppress = true;
    }

    let mut script =
        compile_script(&source, conf.ere, conf.sandbox, conf.null_data, part_ends, conf.posix)?;
    let separator = separator_for(conf.null_data);
    let seed = seed_ranges(&script.cmds);
    let wfiles = std::mem::take(&mut script.wfiles);
    let mut sed = Sed {
        script,
        ranges: seed,
        suppress: conf.suppress,
        separator,
        posix: conf.posix,
        line_wrap: conf.line_wrap,
        pattern: Vec::new(),
        hold: Vec::new(),
        hold_terminated: true,
        line_number: 0,
        in_place: conf.in_place.is_some(),
        terminated: true,
        appends: Vec::new(),
        replaced: false,
        quit: None,
        last_regex: None,
        wfiles,
        rfiles: BTreeMap::new(),
    };

    // `-i` rewrites its OPERANDS, so with none there is nothing to edit and
    // falling back to stdin would silently send the edit to stdout — the file the
    // caller meant to change left untouched, exit 0. GNU refuses; so does this.
    // Checked after `compile_script` because a bad script still reports itself
    // first (exit 1), which is the order GNU resolves the two in.
    if conf.in_place.is_some() && files.is_empty() {
        eprintln!("sed: no input files");
        return Ok(4);
    }
    let inputs: Vec<Vec<u8>> = if files.is_empty() { vec![b"-".to_vec()] } else { files };
    let mut out = Out::new();
    let mut status = 0;

    if conf.separate || conf.in_place.is_some() {
        // ONE stdout sink for every operand: `-s` restarts line numbers and range
        // state per file, but stdout is still one stream, so a separator the last
        // record of one file left owed is paid by the first write of the next.
        // (`-i` writes each file's own buffer, where the debt IS per file.)
        let mut sink = Sink::stdout(&mut out, separator);
        for path in &inputs {
            // Under `-i` an operand names the file to REWRITE, so `-` is an
            // ordinary name here rather than stdin — there is no rewriting a
            // pipe. GNU reports it as the missing file it is.
            let mut input = match Input::open(path, conf.in_place.is_none()) {
                Ok(input) => input,
                Err(e) => {
                    eprintln!("sed: can't read {}: {}", show(path), errmsg(&e));
                    status = 2;
                    continue;
                }
            };
            // `-i` REWRITES what it reads, so an operand it cannot rewrite is
            // refused BEFORE the read rather than by it — which is why the message
            // is about editing rather than about reading, and why a TERMINAL is
            // refused rather than read until someone types. GNU gives up on the
            // whole run there, leaving every later operand unedited.
            if conf.in_place.is_some() {
                if let Some(why) = input.in_place_refusal() {
                    eprintln!("sed: couldn't edit {}: {why}", show(path));
                    sink.flush().map_err(Fatal::runtime)?;
                    return Ok(4);
                }
            }
            let data = match input.read_all() {
                Ok(data) => data,
                // A file that opened and would not read ends the run at 4, as it
                // does on the other path. Its partial bytes are dropped for the
                // reason `advance` drops them, and under `-i` keeping them would
                // rewrite the operand TRUNCATED where GNU's own failure path
                // unlinks its temp file and leaves the original alone.
                Err((e, _)) => {
                    let name = input.error_name(path);
                    eprintln!("sed: read error on {name}: {}", errmsg(&e));
                    sink.flush().map_err(Fatal::runtime)?;
                    return Ok(4);
                }
            };
            let mut stream = to_stream(path, &data, separator);
            // Line numbers, range state, the HOLD SPACE and every REWINDABLE `R`
            // read position restart per file under -s / -i. The output streams and
            // their owed separators do not: those belong to the whole run. GNU
            // clears the hold's TEXT but not its terminator flag, which is visible
            // only after an unterminated file: `sed -s -n 'x;p'` over an
            // unterminated `a` then `b\n` writes ONE separator, because the emptied
            // hold still counts as unterminated and the second `p` leaves its
            // separator owed.
            sed.line_number = 0;
            sed.ranges = seed_ranges(&sed.script.cmds);
            sed.hold.clear();
            for rfile in sed.rfiles.values_mut() {
                if rfile.rewindable {
                    rfile.pos = 0;
                }
            }
            let quit = match &conf.in_place {
                Some(suffix) => {
                    let mut buf = Sink::buffer(separator);
                    let quit = sed.run_stream(&mut stream, &mut buf).map_err(Fatal::runtime)?;
                    write_in_place(path, suffix, &buf.into_buffer())?;
                    quit
                }
                _ => sed.run_stream(&mut stream, &mut sink).map_err(Fatal::runtime)?,
            };
            if let Some(code) = quit {
                sink.flush().map_err(Fatal::runtime)?;
                // A read failure that has ALREADY happened outranks the quit code,
                // whatever it is: `sed -s -n Q7 /nosuch A` is 2 in GNU, not 7, while
                // `sed -s -n Q7 A /nosuch` is 7 because the quit fires before the
                // second operand is ever opened. Sound on both paths now that each
                // opens one operand at a time — a `status` set here can only
                // describe a file the run actually reached.
                return Ok(match status {
                    0 => code,
                    _ => status,
                });
            }
        }
    } else {
        // One logical stream: line numbers and `$` span every input. The operands
        // are opened one at a time, so a failure is only suffered where GNU
        // suffers it — reading every one up front reported files a `q` had
        // already quit before, and reported them as SUCCESS.
        let mut stream = Stream::of_operands(inputs, separator);
        let mut sink = Sink::stdout(&mut out, separator);
        let quit = sed.run_stream(&mut stream, &mut sink).map_err(Fatal::runtime)?;
        // A read that failed outranks everything else, including a `bad` operand
        // opened earlier: GNU's reader panics there, so nothing after it happened.
        // The flush is for its ERROR — what was written survives either way, since
        // dropping the `BufWriter` writes it, but silently.
        if stream.fatal {
            out.flush().map_err(|e| Fatal::runtime(format!("write error: {}", errmsg(&e))))?;
            return Ok(4);
        }
        if stream.bad {
            status = 2;
        }
        if let Some(code) = quit {
            out.flush().map_err(|e| Fatal::runtime(format!("write error: {}", errmsg(&e))))?;
            // Same rule the `-s` path has always applied, and now sound here too:
            // a read failure ALREADY suffered outranks the quit code.
            return Ok(match status {
                0 => code,
                _ => status,
            });
        }
    }
    out.flush().map_err(|e| Fatal::runtime(format!("write error: {}", errmsg(&e))))?;
    Ok(status)
}

/// One `R` source: the lines, the cursor, and whether the per-operand reset may
/// REWIND it. GNU rewinds with `rewind(3)`, which fails silently on a stream that
/// cannot seek, so a pipe or fifo keeps its place across an `-s` boundary while a
/// regular file restarts.
struct RFile {
    lines: Vec<Line>,
    pos: usize,
    rewindable: bool,
}

/// Read a `-f` script, and say whether `#n` may come from it. GNU's test for a file
/// script is `prog.file && !prog.base && 2 == ftell(prog.file)` -- an ABSOLUTE
/// offset, so the stream must both SEEK (`ftell` is -1 on a pipe, which is why
/// `printf '#n\np' | sed -f -` auto-prints there) and have STARTED at its own
/// beginning (a descriptor handed over already part-read does not carry the rule).
/// A named file is opened here, so the second half holds by construction and only
/// the seek is asked. Stdin can be neither seeked nor reopened, so it gets a PROXY
/// -- see the comment below for where the proxy and GNU part company.
fn read_script(path: &[u8]) -> std::io::Result<(Vec<u8>, bool)> {
    let mut data = Vec::new();
    if path == b"-" {
        std::io::stdin().lock().read_to_end(&mut data)?;
        // `Stdin` cannot seek and must NOT be reopened to ask: opening fd 0 again
        // waits for a writer for ever when it is a fifo. `stat` does not take part
        // in that handshake, and it answers both halves at once for the case that
        // reaches here -- a regular file is the seekable one, and a size equal to
        // what was just read means the descriptor began at offset 0.
        //
        // `is_file` is defence in depth rather than the deciding test: `st_size`
        // is 0 for a pipe, fifo, socket, tty and block device alike, so the
        // length comparison already declines every one of them.
        //
        // It is a PROXY, not `ftell`, and parts company both ways -- spec/README
        // enumerates. It DECLINES what GNU takes for a block device, a virtual
        // file sized 0 (procfs) or a fixed 4096 (sysfs) against a shorter read,
        // and a file RESIZED between the read and the stat; those auto-print
        // rather than swallow output, as an absent or unreadable /proc does. A
        // CHARACTER device is not among them: through GNU's own sequence
        // (`fopen`, `fread` of two, `ftell`) none reaches 2, so it declines them
        // too. See spec/README -- the mechanism is not "offset 0", which is what
        // raw `lseek` reports rather than what `ftell` does.
        // It can wrongly ACCEPT only where `S_IFREG` is not in fact seekable
        // (FUSE may serve such a file), or under a concurrent truncation to
        // exactly the bytes read -- never from a pre-positioned descriptor
        // alone, since what is readable from offset k is `size - k`, and that
        // equals `size` only at k = 0. Being exact would need `lseek` on fd 0,
        // which safe `std` cannot reach.
        let seekable = std::fs::metadata("/proc/self/fd/0")
            .map(|m| m.is_file() && m.len() == data.len() as u64)
            .unwrap_or(false);
        return Ok((data, seekable));
    }
    let mut file = std::fs::File::open(crate::util::path_from_bytes(path))?;
    file.read_to_end(&mut data)?;
    Ok((data, file.seek(std::io::SeekFrom::Start(0)).is_ok()))
}

/// Read a file named by `r`/`R`. Unlike an operand or `-f`, `-` is NOT stdin here:
/// GNU opens the name literally, so `R -` reads a file called `-` and reads nothing
/// when there is none (`w -` already wrote one). The bool is whether the source can
/// be rewound — `rewind(3)`'s own test, applied to the handle just read.
///
/// An OPEN failure is not an error, which is what makes `r /nonexistent` silent; a
/// READ failure IS one, exit 4, and a DIRECTORY is how you get one.
fn read_source(path: &[u8]) -> Result<(Vec<u8>, bool), String> {
    let mut data = Vec::new();
    let Ok(mut file) = std::fs::File::open(crate::util::path_from_bytes(path)) else {
        return Ok((data, false));
    };
    if let Err(e) = file.read_to_end(&mut data) {
        return Err(format!("read error on {}: {}", show(path), errmsg(&e)));
    }
    // GNU aliases the literal `/dev/stdin` to its own stdin stream, which is not
    // among the streams it rewinds, however seekable that stream happens to be.
    let rewindable = path != b"/dev/stdin" && file.seek(std::io::SeekFrom::Start(0)).is_ok();
    Ok((data, rewindable))
}

/// Is `want` newer than the GNU sed level td-txt implements? GNU's `v` runs glibc's
/// `strverscmp` against its own version, where runs of digits compare NUMERICALLY --
/// which is what makes `4.10` newer than `4.9` -- and everything else compares
/// bytewise: `v 4a` is refused because `a` > `.`, `v 4-9` is not because `-` < `.`,
/// and a leading zero is an ordinary byte below every other digit rather than the
/// start of a number. An empty argument is GNU's own 4.0, i.e. older.
fn version_is_newer(want: &[u8]) -> bool {
    // The GNU sed this program is written against, which is the corpus's oracle
    // (spec/README names the store path). Moving that pin moves this.
    const LEVEL: &[u8] = b"4.9";
    let byte = |s: &[u8], i: usize| s.get(i).copied().unwrap_or(0);
    // Both sides share everything up to the first difference, so one index serves.
    let mut i = 0usize;
    while byte(want, i) == byte(LEVEL, i) && byte(want, i) != 0 {
        i += 1;
    }
    let (x, y) = (byte(want, i), byte(LEVEL, i));
    if x == y {
        return false;
    }
    // The digit run that still has digits where the other stops is the longer, and so
    // the greater, number; runs of equal length are decided by the byte that differs.
    let longer_run = || {
        let count = |s: &[u8]| {
            let mut n = i;
            while s.get(n).is_some_and(u8::is_ascii_digit) {
                n += 1;
            }
            n - i
        };
        match count(want).cmp(&count(LEVEL)) {
            std::cmp::Ordering::Equal => x > y,
            other => other == std::cmp::Ordering::Greater,
        }
    };
    if i > 0 && byte(LEVEL, i - 1).is_ascii_digit() {
        // INSIDE a run: whichever side keeps going has the longer number, and a side
        // that has left it compares as the byte it left with.
        return match (x.is_ascii_digit(), y.is_ascii_digit()) {
            (true, true) => longer_run(),
            (xd, yd) if xd != yd => xd,
            _ => x > y,
        };
    }
    // Between runs: two numbers BEGIN here only if neither begins with a zero.
    match (x, y) {
        (b'1'..=b'9', b'1'..=b'9') => longer_run(),
        _ => x > y,
    }
}

fn to_lines(data: &[u8], separator: u8) -> Vec<Line> {
    records(data, separator)
        .into_iter()
        .map(|(text, terminated)| Line { text: text.to_vec(), terminated })
        .collect()
}

fn to_stream(path: &[u8], data: &[u8], separator: u8) -> Stream {
    Stream::of(path, data, separator)
}

/// The backup name for `-i SUFFIX`: the suffix appended, or — GNU's other form —
/// the suffix with `*` standing for the OPERAND AS WRITTEN, path and all. So
/// `-i'bak_*'` on `sub/f` backs up to `bak_sub/f`, not `sub/bak_f`, and fails if
/// that directory does not exist.
fn backup_name(target: &Path, suffix: &[u8]) -> PathBuf {
    let base = crate::util::path_bytes(target);
    if !suffix.contains(&b'*') {
        let mut name = base;
        name.extend_from_slice(suffix);
        return crate::util::path_from_bytes(&name);
    }
    let mut name = Vec::new();
    for b in suffix {
        if *b == b'*' {
            name.extend_from_slice(&base);
        } else {
            name.push(*b);
        }
    }
    crate::util::path_from_bytes(&name)
}

/// Rewrite `path` with `data`, GNU's way: a NEW file beside it, then a rename
/// over the name.
///
/// Writing THROUGH the name instead — which is what this used to do — silently
/// made `-i` follow symlinks and share hard links: the edit landed on whatever
/// the name resolved to, and every other name for that inode saw it. That is
/// exactly the `--follow-symlinks` behavior this applet REFUSES to be asked for,
/// so it must not be the default. A rename also makes the replacement atomic (no
/// truncated file if the write dies partway) and lets a read-only file be
/// rewritten, both of which GNU callers rely on.
///
/// NOT copied across the rename: owner and group. GNU `fchown`s the temp file when
/// it is privileged enough to; td-txt does not, so a root `sed -i` over a file
/// owned by someone else leaves the rewrite owned by root. Nothing on the image
/// runs sed at all, let alone as root over another user's file, so this is a
/// recorded gap rather than a fix.
fn write_in_place(path: &[u8], suffix: &[u8], data: &[u8]) -> Result<(), Fatal> {
    // Every failure here is the filesystem refusing, so they all go through
    // `Fatal::runtime` rather than the `String` conversion, which would blame the
    // script for a full disk.
    let target = crate::util::path_from_bytes(path);
    let dir = match target.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => PathBuf::from("."),
    };
    // The new file carries the original's mode; a rename would otherwise silently
    // reset it to the create default.
    let mode = std::fs::metadata(&target).ok().map(|m| m.permissions());

    let (temp, mut file) = create_temp(&dir, path).map_err(Fatal::runtime)?;
    let write = file
        .write_all(data)
        .and_then(|()| file.flush())
        .map_err(|e| format!("couldn't write {}: {}", show(path), errmsg(&e)));
    drop(file);
    let finish = write.and_then(|()| {
        if let Some(mode) = mode {
            std::fs::set_permissions(&temp, mode)
                .map_err(|e| format!("couldn't write {}: {}", show(path), errmsg(&e)))?;
        }
        let mut moved_aside = None;
        if !suffix.is_empty() {
            // The ORIGINAL is renamed aside, so the backup keeps its inode and
            // the new content lands at the original name — GNU's order.
            let backup = backup_name(&target, suffix);
            std::fs::rename(&target, &backup)
                .map_err(|e| format!("cannot rename {}: {}", show(path), errmsg(&e)))?;
            moved_aside = Some(backup);
        }
        std::fs::rename(&temp, &target).map_err(|e| {
            // The original is already at the backup name; without this the run
            // would fail with the edited path GONE, which is worse than either
            // outcome the caller asked for.
            if let Some(backup) = moved_aside {
                let _ = std::fs::rename(&backup, &target);
            }
            format!("cannot rename {}: {}", show(path), errmsg(&e))
        })
    });
    if finish.is_err() {
        // Leaving the scratch file behind would litter the directory the caller
        // asked to edit.
        let _ = std::fs::remove_file(&temp);
    }
    finish.map_err(Fatal::runtime)
}

/// Create a fresh scratch file beside the target. Exclusive create, so an
/// existing name (or a planted symlink) is never adopted; the pid plus a counter
/// keeps concurrent seds apart without a random source.
///
/// Created 0600, NOT at the umask default: the caller's content is written before
/// the original's mode is restored, so a 0600 secret edited in a traversable
/// directory would otherwise be world-readable for the length of the write under
/// a predictable name. Widening to the original mode afterwards is safe; starting
/// wide is not.
fn create_temp(dir: &Path, path: &[u8]) -> Result<(PathBuf, std::fs::File), String> {
    use std::os::unix::fs::OpenOptionsExt;
    let pid = std::process::id();
    for n in 0..u32::MAX {
        let candidate = dir.join(format!("sed{pid}{n}.tmp"));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&candidate)
        {
            Ok(f) => return Ok((candidate, f)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => {
                return Err(format!(
                    "couldn't open temporary file {}: {}",
                    show(&crate::util::path_bytes(&candidate)),
                    errmsg(&e)
                ))
            }
        }
    }
    Err(format!("couldn't open temporary file for {}", show(path)))
}

impl Sed {
    /// Run every cycle of one input stream. `Ok(Some(code))` means `q`/`Q` asked
    /// to exit with that status.
    fn run_stream(&mut self, stream: &mut Stream, sink: &mut Sink) -> Result<Option<i32>, String> {
        loop {
            let Some(line) = stream.next_line(Opener::Reader) else {
                return Ok(self.quit);
            };
            self.line_number += 1;
            self.pattern = line.text;
            self.terminated = line.terminated;
            self.replaced = false;
            loop {
                match self.run_cycle(stream, sink)? {
                    Flow::Done => {
                        if !self.suppress {
                            self.emit_pattern(sink)?;
                        }
                        self.flush_appends(sink)?;
                        break;
                    }
                    Flow::Deleted => {
                        self.flush_appends(sink)?;
                        break;
                    }
                    Flow::Restart => continue,
                    Flow::Quit => {
                        self.flush_appends(sink)?;
                        return Ok(self.quit);
                    }
                }
            }
        }
    }

    fn emit_pattern(&mut self, sink: &mut Sink) -> Result<(), String> {
        let pattern = std::mem::take(&mut self.pattern);
        let res = self.emit(sink, &pattern, self.terminated);
        self.pattern = pattern;
        res
    }

    fn emit(&self, sink: &mut Sink, bytes: &[u8], terminated: bool) -> Result<(), String> {
        sink.write_line(bytes, terminated)
    }

    /// Take `R`'s next line from `path` and queue the bytes. GNU reads it when the
    /// command runs, not when the queue is flushed, so `R` over a directory fails
    /// BEFORE the cycle prints while `r` over one fails after.
    fn queue_line(&mut self, path: &[u8]) -> Result<(), String> {
        let separator = self.separator;
        let entry = match self.rfiles.get_mut(path) {
            Some(e) => e,
            None => {
                // A missing file is not an error for `R`: cache it as exhausted so it
                // is opened at most once.
                let (data, rewindable) = read_source(path)?;
                let lines = to_lines(&data, separator);
                self.rfiles.entry(path.to_vec()).or_insert(RFile { lines, pos: 0, rewindable })
            }
        };
        let RFile { lines, pos, .. } = entry;
        let Some(line) = lines.get(*pos) else {
            return Ok(());
        };
        // `R` writes the line as it found it and owes NOTHING of its own: over a
        // source with no final newline, `sed -n -e '1R f' -e '2p'` runs the two
        // together in GNU. It still PAYS a debt the pattern space left, which the
        // ordinary write does.
        let mut bytes = line.text.clone();
        if line.terminated {
            bytes.push(separator);
        }
        *pos += 1;
        self.appends.push(Append::Line(bytes));
        Ok(())
    }

    fn flush_appends(&mut self, sink: &mut Sink) -> Result<(), String> {
        let appends = std::mem::take(&mut self.appends);
        for a in appends {
            match a {
                Append::Text(text) => sink.write_text(&text)?,
                // A missing file is not an error for `r`/`R`, as in GNU sed — but
                // GNU pays the owed separator BEFORE it finds out, so `printf x |
                // sed 'r /nonexistent'` still ends with one. Writing the empty
                // content pays it and adds nothing, which is what an existing but
                // empty file already did.
                Append::File(path) => {
                    // The owed separator is paid BEFORE the file's fate is known, so
                    // `printf x | sed 'r /nonexistent'` still ends with one -- and so
                    // does `r` over a directory, which then fails with exit 4.
                    sink.write(&[])?;
                    let (data, _) = read_source(&path)?;
                    sink.write(&data)?;
                }
                // `R` resolved its line when the command RAN — see `queue_line`.
                Append::Line(bytes) => sink.write(&bytes)?,
            }
        }
        Ok(())
    }

    fn write_to_file(
        &mut self,
        path: &[u8],
        bytes: &[u8],
        terminated: bool,
        sink: &mut Sink,
    ) -> Result<(), String> {
        // `/dev/stdout` must share the auto-print stream's sink, or a `w` and a `p`
        // in one script interleave wrongly. Under `-i` that sink is the replacement
        // buffer, and GNU still writes to the real standard output — so there it is
        // an ordinary target with a debt of its own, like every other one.
        if path == b"/dev/stdout" && !self.in_place {
            return sink.write_line_on(Chan::WFile, bytes, terminated);
        }
        let separator = self.separator;
        // Every target was opened by the parser, so a miss is a command whose name
        // never reached `open_wfile` — not a filesystem failure, and saying so
        // would send the reader to the one place that is working.
        let Some(w) = self.wfiles.get_mut(path) else {
            return Err(format!("no output was opened for {}", show(path)));
        };
        w.write_line(bytes, terminated, separator)
    }

    /// One pass over the script for the current pattern space.
    #[allow(clippy::too_many_lines)] // the command dispatch: one arm per sed command
    fn run_cycle(&mut self, stream: &mut Stream, sink: &mut Sink) -> Result<Flow, String> {
        let mut pc = 0usize;
        while pc < self.script.cmds.len() {
            if !self.addr_matches(pc, stream)? {
                // A block whose address does not match is skipped whole.
                if let Some(Cmd { kind: Kind::Block(end), .. }) = self.script.cmds.get(pc) {
                    pc = *end;
                    continue;
                }
                pc += 1;
                continue;
            }
            let mut next = pc + 1;
            // Each arm copies what it needs out of the script so the borrow ends
            // before `self` is mutated.
            match self.script.cmds.get(pc).map(|c| &c.kind) {
                None => break,
                Some(Kind::Block(_) | Kind::BlockEnd | Kind::Label | Kind::Comment) => {}
                Some(Kind::Branch(Target::At(target))) => match target {
                    Some(t) => next = *t,
                    None => return Ok(Flow::Done),
                },
                Some(Kind::BranchIfSub(Target::At(target))) => {
                    if self.replaced {
                        self.replaced = false;
                        match target {
                            Some(t) => next = *t,
                            None => return Ok(Flow::Done),
                        }
                    }
                }
                Some(Kind::BranchIfNoSub(Target::At(target))) => {
                    if self.replaced {
                        self.replaced = false;
                    } else {
                        match target {
                            Some(t) => next = *t,
                            None => return Ok(Flow::Done),
                        }
                    }
                }
                // `resolve_labels` rewrote every branch, so a name here is a bug
                // in that pass rather than a script error.
                Some(Kind::Branch(Target::Name(n)) | Kind::BranchIfSub(Target::Name(n)) | Kind::BranchIfNoSub(Target::Name(n))) => {
                    return Err(format!("unresolved branch to `{}'", show(n)))
                }
                Some(Kind::Append(text)) => {
                    if let Some(text) = text.clone() {
                        self.appends.push(Append::Text(text));
                    }
                }
                Some(Kind::Insert(text)) => {
                    if let Some(text) = text.clone() {
                        sink.write_line(&text, true)?;
                    }
                }
                Some(Kind::Change(text)) => {
                    let text = text.clone();
                    // Over a range, `c` prints once, at the range's last line.
                    let ends = self
                        .ranges
                        .get(pc)
                        .is_none_or(|r| !matches!(r, RangeState::Active(_)));
                    // A `c` with no text still deletes; it just writes no line.
                    if let (true, Some(text)) = (ends, text) {
                        sink.write_line(&text, true)?;
                    }
                    self.pattern.clear();
                    return Ok(Flow::Deleted);
                }
                Some(Kind::Delete) => {
                    self.pattern.clear();
                    return Ok(Flow::Deleted);
                }
                Some(Kind::DeleteFirstLine) => {
                    let sep = self.separator;
                    match self.pattern.iter().position(|b| *b == sep) {
                        Some(nl) => {
                            self.pattern = self.pattern.get(nl + 1..).unwrap_or_default().to_vec();
                            return Ok(Flow::Restart);
                        }
                        None => {
                            self.pattern.clear();
                            return Ok(Flow::Deleted);
                        }
                    }
                }
                Some(Kind::Get) => {
                    self.pattern = self.hold.clone();
                    self.terminated = self.hold_terminated;
                }
                Some(Kind::GetAppend) => {
                    self.pattern.push(self.separator);
                    let hold = self.hold.clone();
                    self.pattern.extend_from_slice(&hold);
                    self.terminated = self.hold_terminated;
                }
                Some(Kind::Hold) => {
                    self.hold = self.pattern.clone();
                    self.hold_terminated = self.terminated;
                }
                Some(Kind::HoldAppend) => {
                    self.hold.push(self.separator);
                    let pattern = self.pattern.clone();
                    self.hold.extend_from_slice(&pattern);
                    self.hold_terminated = self.terminated;
                }
                Some(Kind::Exchange) => {
                    std::mem::swap(&mut self.pattern, &mut self.hold);
                    std::mem::swap(&mut self.terminated, &mut self.hold_terminated);
                }
                Some(Kind::List(width)) => {
                    let width = width.unwrap_or(self.line_wrap);
                    let mut buf = Vec::new();
                    escape_for_l(&self.pattern, width, self.separator, &mut buf);
                    sink.write(&buf)?;
                }
                Some(Kind::Next) => {
                    if !self.suppress {
                        self.emit_pattern(sink)?;
                    }
                    self.flush_appends(sink)?;
                    let Some(line) = stream.next_line(Opener::Lookahead) else {
                        self.pattern.clear();
                        return Ok(Flow::Deleted);
                    };
                    self.line_number += 1;
                    self.pattern = line.text;
                    self.terminated = line.terminated;
                }
                Some(Kind::NextAppend) => {
                    // At END of input GNU prints the pattern space and only THEN
                    // flushes the append queue, so the flush waits until a line is
                    // known to exist: `sed -e 'r f' -e N` over a one-line input
                    // writes the line before f's text. Both flows the early returns
                    // take already flush, in that order. (`--posix` drops the
                    // pattern space instead, and `n` never had the problem.)
                    let Some(line) = stream.next_line(Opener::Lookahead) else {
                        if self.posix {
                            self.pattern.clear();
                            return Ok(Flow::Deleted);
                        }
                        return Ok(Flow::Done);
                    };
                    self.flush_appends(sink)?;
                    self.line_number += 1;
                    self.pattern.push(self.separator);
                    self.pattern.extend_from_slice(&line.text);
                    self.terminated = line.terminated;
                }
                Some(Kind::Print) => {
                    let (pattern, terminated) = (self.pattern.clone(), self.terminated);
                    self.emit(sink, &pattern, terminated)?;
                }
                Some(Kind::PrintFirstLine) => {
                    let sep = self.separator;
                    let upto = self
                        .pattern
                        .iter()
                        .position(|b| *b == sep)
                        .unwrap_or(self.pattern.len());
                    let head = self.pattern.get(..upto).unwrap_or_default().to_vec();
                    // `P` prints THROUGH the first newline, so it is terminated
                    // even when the pattern space is not.
                    let terminated = upto < self.pattern.len() || self.terminated;
                    self.emit(sink, &head, terminated)?;
                }
                Some(Kind::Quit(code, autoprint)) => {
                    let (code, autoprint) = (*code, *autoprint);
                    self.quit = Some(code);
                    if autoprint {
                        if !self.suppress {
                            self.emit_pattern(sink)?;
                        }
                        // `q` pays the separator a missing one is OWED, which reaching
                        // end of input does not: `printf x | sed q` writes `x\n` and
                        // `printf x | sed -n 'p;q'` does too, while `printf x | sed 2q`
                        // and `-n 'p;Q'` write a bare `x`. So it is quitting through `q`
                        // that settles it, not the auto-print — `-n 'p;q'` settles
                        // although the `q` itself printed nothing.
                        sink.settle()?;
                    } else {
                        // `Q` quits WITHOUT flushing the append queue, so a pending
                        // `a` text is discarded: GNU prints nothing for `a\ntext` then
                        // `Q`, while `q` prints the text. `Q` is "quit now", and the
                        // queue is part of finishing the cycle it never finishes.
                        self.appends.clear();
                    }
                    return Ok(Flow::Quit);
                }
                Some(Kind::ReadFile(path)) => {
                    let path = path.clone();
                    self.appends.push(Append::File(path));
                }
                Some(Kind::PrependFile(path)) => {
                    // `put`, not `write`: GNU's immediate read goes straight at the
                    // output and does NOT pay an owed separator, so an unterminated
                    // previous line still owes one AFTER the prepended bytes.
                    let (data, _) = read_source(path)?;
                    sink.put(&data)?;
                }
                Some(Kind::ReadLine(path)) => {
                    let path = path.clone();
                    self.queue_line(&path)?;
                }
                Some(Kind::Write(path)) => {
                    let path = path.clone();
                    let pattern = self.pattern.clone();
                    let terminated = self.terminated;
                    self.write_to_file(&path, &pattern, terminated, sink)?;
                }
                Some(Kind::WriteFirstLine(path)) => {
                    let path = path.clone();
                    let sep = self.separator;
                    let upto = self
                        .pattern
                        .iter()
                        .position(|b| *b == sep)
                        .unwrap_or(self.pattern.len());
                    let head = self.pattern.get(..upto).unwrap_or_default().to_vec();
                    // The extracted record ends at a separator whenever one was
                    // found; `self.terminated` describes the whole space.
                    let terminated = upto < self.pattern.len() || self.terminated;
                    self.write_to_file(&path, &head, terminated, sink)?;
                }
                Some(Kind::LineNumber) => {
                    let mut buf = number(self.line_number);
                    buf.push(self.separator);
                    sink.write(&buf)?;
                }
                Some(Kind::FileName) => {
                    // The name is read HERE, not snapshotted at the cycle's start:
                    // GNU prints whatever operand is open, and a `$` earlier in the
                    // same cycle has already opened the NEXT one.
                    let mut buf = stream.current_name().to_vec();
                    buf.push(self.separator);
                    sink.write(&buf)?;
                }
                Some(Kind::Zap) => self.pattern.clear(),
                Some(Kind::Transliterate(table)) => {
                    let table = table.clone();
                    for b in self.pattern.iter_mut() {
                        if let Some(mapped) = table.get(usize::from(*b)) {
                            *b = *mapped;
                        }
                    }
                }
                Some(Kind::Subst(_)) => {
                    let (changed, print, wfile) = self.substitute(pc)?;
                    if changed {
                        self.replaced = true;
                        if print {
                            let (pattern, terminated) = (self.pattern.clone(), self.terminated);
                            self.emit(sink, &pattern, terminated)?;
                        }
                        if let Some(path) = wfile {
                            let pattern = self.pattern.clone();
                            let terminated = self.terminated;
                            self.write_to_file(&path, &pattern, terminated, sink)?;
                        }
                    }
                }
            }
            pc = next;
        }
        Ok(Flow::Done)
    }

    /// Apply the `s///` at `idx` to the pattern space.
    fn substitute(&mut self, idx: usize) -> Result<(bool, bool, Option<Vec<u8>>), String> {
        let (global, print, occurrence, wfile, own_re) = {
            let Some(Cmd { kind: Kind::Subst(s), .. }) = self.script.cmds.get(idx) else {
                return Ok((false, false, None));
            };
            (s.global, s.print, s.occurrence, s.wfile.clone(), s.re)
        };
        let re_idx = match own_re.or(self.last_regex) {
            Some(i) => i,
            None => return Err(NO_PREVIOUS_REGEX.to_string()),
        };
        self.last_regex = Some(re_idx);
        let Some(re) = self.script.regexes.get(re_idx) else {
            return Err(NO_PREVIOUS_REGEX.to_string());
        };
        let Some(Cmd { kind: Kind::Subst(sub), .. }) = self.script.cmds.get(idx) else {
            return Ok((false, false, None));
        };

        let hay = std::mem::take(&mut self.pattern);
        let mut out: Vec<u8> = Vec::with_capacity(hay.len());
        let mut pos = 0usize;
        let mut count: u64 = 0;
        let mut changed = false;
        let mut last_end: Option<usize> = None;
        loop {
            // A SUBSTITUTION is confined to one segment under `M` with a
            // non-newline separator; an address is not. See `search_subst`.
            let caps = match re.search_subst(&hay, pos) {
                Ok(Some(c)) => c,
                Ok(None) => break,
                Err(e) => {
                    self.pattern = hay;
                    return Err(e.msg);
                }
            };
            let (s, e) = (caps.start(), caps.end());
            // An empty match abutting the previous one is not a new occurrence:
            // `s/a*/X/g` on `baac` gives `XbXcX`, and `s/a*/x/3` on `bac` counts
            // the empty match at end-of-line as the third — which is why this
            // must not depend on whether the previous match was REPLACED.
            if e == s && last_end == Some(s) {
                out.extend_from_slice(hay.get(pos..s).unwrap_or_default());
                if let Some(b) = hay.get(s) {
                    out.push(*b);
                }
                pos = s + 1;
                if pos > hay.len() {
                    break;
                }
                continue;
            }
            count += 1;
            let replace_this = count >= occurrence && (global || count == occurrence);
            out.extend_from_slice(hay.get(pos..s).unwrap_or_default());
            if replace_this {
                expand(&sub.replacement, &hay, &caps, &mut out);
                changed = true;
            } else {
                out.extend_from_slice(hay.get(s..e).unwrap_or_default());
            }
            last_end = Some(e);
            if e == s {
                if let Some(b) = hay.get(e) {
                    out.push(*b);
                }
                pos = e + 1;
            } else {
                pos = e;
            }
            if pos > hay.len() {
                break;
            }
            if !global && count >= occurrence {
                break;
            }
        }
        out.extend_from_slice(hay.get(pos.min(hay.len())..).unwrap_or_default());
        self.pattern = if changed { out } else { hay };
        Ok((changed, print, wfile))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Run a script over `input` and return what it wrote.
    fn sed(script: &str, input: &str, opts: &[&str]) -> Vec<u8> {
        let ere = opts.contains(&"-E") || opts.contains(&"-r");
        let null_data = opts.contains(&"-z");
        let sep = separator_for(null_data);
        let mut script =
            compile_script(script.as_bytes(), ere, false, null_data, Vec::new(), false).unwrap();
        let nranges = script.cmds.len();
        let seed = seed_ranges(&script.cmds);
        let wfiles = std::mem::take(&mut script.wfiles);
        let mut sed = Sed {
            script,
            ranges: seed,
            suppress: opts.contains(&"-n"),
            separator: sep,
            posix: false,
            line_wrap: DEFAULT_LINE_WRAP,
            pattern: Vec::new(),
            hold: Vec::new(),
            hold_terminated: true,
            line_number: 0,
            in_place: false,
            terminated: true,
            appends: Vec::new(),
            replaced: false,
            quit: None,
            last_regex: None,
            wfiles,
            rfiles: BTreeMap::new(),
            };
        let mut stream = to_stream(b"-", input.as_bytes(), sep);
        let mut sink = Sink::buffer(sep);
        sed.run_stream(&mut stream, &mut sink).unwrap();
        sink.into_buffer()
    }

    /// `l` wraps with a backslash and the RECORD separator, and its guard is `> 0`:
    /// width 1 wraps before every chunk (so the line opens with a bare backslash),
    /// width 0 never wraps. Both were wrong: the wrap was hardcoded to a newline and
    /// the guard was `> 1`, so `l 1` behaved like `l 0`.
    #[test]
    fn list_wraps_on_the_record_separator_at_every_width() {
        let mut out = Vec::new();
        escape_for_l(b"abcdef", 1, b'\n', &mut out);
        assert_eq!(out, b"\\\na\\\nb\\\nc\\\nd\\\ne\\\nf$\n");
        out.clear();
        escape_for_l(b"abcdef", 0, b'\n', &mut out);
        assert_eq!(out, b"abcdef$\n");
        out.clear();
        escape_for_l(b"abcdef", 3, b'\n', &mut out);
        assert_eq!(out, b"ab\\\ncd\\\nef$\n");
        // Under `-z` both the wrap and the terminator are NUL.
        out.clear();
        escape_for_l(b"abcd", 3, 0, &mut out);
        assert_eq!(out, b"ab\\\0cd$\0");
    }

    /// The `atoi` grammar is pinned here rather than in the corpus because three
    /// of the six bytes `strtol` skips (VT, FF, CR) can only reach a `## argv:`
    /// line as raw bytes, invisible in a file people edit.
    #[test]
    fn atoi_reads_what_c_reads() {
        for pre in [" ", "\t", "\n", "\x0b", "\x0c", "\r", " \t\n\x0b\x0c\r"] {
            assert_eq!(atoi(format!("{pre}12").as_bytes()), 12, "{pre:?}");
        }
        assert_eq!(atoi(b"+12"), 12);
        assert_eq!(atoi(b"-12"), -12);
        assert_eq!(atoi(b"012"), 12);
        // Base 10 only, and the rest of the word is ignored rather than diagnosed.
        assert_eq!(atoi(b"12x"), 12);
        assert_eq!(atoi(b"0x10"), 0);
        // Nothing to read is 0.
        assert_eq!(atoi(b""), 0);
        assert_eq!(atoi(b"abc"), 0);
        assert_eq!(atoi(b"-"), 0);
        assert_eq!(atoi(b"+"), 0);
        // The whitespace run comes FIRST: a sign it follows is not a sign.
        assert_eq!(atoi(b"1 2"), 1);
        assert_eq!(atoi(b"- 12"), 0);
    }

    /// The two rules this reader does NOT share with the script's own numbers: it
    /// truncates to an `int`, and it saturates where they wrap.
    #[test]
    fn atoi_truncates_to_an_int_and_saturates_rather_than_wrapping() {
        assert_eq!(atoi(b"4294967306"), 10);
        assert_eq!(atoi(b"-4294967286"), 10);
        assert_eq!(atoi(b"4294967295"), -1);
        assert_eq!(atoi(b"2147483648"), i32::MIN);
        // Past a `long` the value clamps, so the low 32 bits stop tracking the
        // digits: wrapping would make this 10, as the command's width is.
        assert_eq!(atoi(b"18446744073709551626"), -1);
        assert_eq!(atoi(b"99999999999999999999999"), -1);
        assert_eq!(atoi(b"-99999999999999999999999"), 0);
    }

    /// `-l`'s width and `COLS` read the same number and then do DIFFERENT things
    /// with it: GNU keeps both in an unsigned counter, so every negative folds
    /// nowhere, but `COLS` also subtracts one and only acts above 1.
    #[test]
    fn the_two_fallback_inputs_differ_in_what_they_do_with_the_number() {
        assert_eq!(line_length_arg(b"10"), 10);
        assert_eq!(line_length_arg(b"4294967306"), 10);
        assert_eq!(line_length_arg(b"-4294967286"), 10);
        assert_eq!(line_length_arg(b"-5"), 0);
        assert_eq!(line_length_arg(b"abc"), 0);
        assert_eq!(line_length_arg(b"18446744073709551626"), 0);

        assert_eq!(cols_line_wrap(b"12"), Some(11));
        assert_eq!(cols_line_wrap(b"2"), Some(1));
        // Not greater than 1: the default stands, which is what `None` says.
        assert_eq!(cols_line_wrap(b"1"), None);
        assert_eq!(cols_line_wrap(b"0"), None);
        assert_eq!(cols_line_wrap(b"abc"), None);
        // The comparison is unsigned, so a negative is a huge count, not a small
        // one -- it folds nowhere rather than leaving the default alone.
        assert_eq!(cols_line_wrap(b"-1"), Some(0));
        assert_eq!(cols_line_wrap(b"-4294967286"), Some(9));
        assert_eq!(cols_line_wrap(b"4294967307"), Some(10));
    }

    /// -1 is the command width's "no width argument", and nothing else is.
    #[test]
    fn a_command_width_of_minus_one_defers_where_its_neighbours_do_not() {
        assert_eq!(list_width(None), None);
        assert_eq!(list_width(Some(4_294_967_295)), None);
        assert_eq!(list_width(Some(4_294_967_294)), Some(0));
        assert_eq!(list_width(Some(4_294_967_306)), Some(10));
        assert_eq!(list_width(Some(0)), Some(0));
        assert_eq!(list_width(Some(70)), Some(70));
    }

    /// `q` pays a separator the record left OWED; `Q` does not, and `Q` also throws
    /// away the append queue that `q` flushes.
    #[test]
    fn quit_settles_the_owed_separator_but_quit_silent_does_not() {
        assert_eq!(sed("q", "x", &[]), b"x\n");
        assert_eq!(sed("2q", "x", &[]), b"x");
        assert_eq!(sed("p;q", "x", &["-n"]), b"x\n");
        assert_eq!(sed("p;Q", "x", &["-n"]), b"x");
        assert_eq!(sed("a text\nQ", "x\n", &[]), b"");
        assert_eq!(sed("a text\nq", "x\n", &[]), b"x\ntext\n");
    }

    /// `M` is relative to the RECORD separator, and `N` is how one gets inside the
    /// pattern space: under `-z` it joins with a NUL, and both halves move there.
    #[test]
    fn the_m_flag_follows_the_null_separator_that_next_append_inserted() {
        assert_eq!(sed("N;s/^b/X/M", "a\0b\0", &["-z"]), b"a\0X\0");
        assert_eq!(sed("N;s/a$/X/M", "a\0b\0", &["-z"]), b"X\0b\0");
        // The exclusion half keeps `.` and a non-matching list off that NUL...
        assert_eq!(sed("N;s/a.b/X/M", "a\0b\0", &["-z"]), b"a\0b\0");
        assert_eq!(sed("N;s/[^a]/Y/M", "a\0b\0", &["-z"]), b"a\0Y\0");
        // ...and off a newline too, which is not the separator here.
        assert_eq!(sed("s/c.d/Z/M", "abc\ndef\0", &["-z"]), b"abc\ndef\0");
        // Without `M` both bytes are ordinary.
        assert_eq!(sed("N;s/a.b/X/", "a\0b\0", &["-z"]), b"X\0");
        // And the anchor half does NOT fire at a newline under `-z`.
        assert_eq!(sed("s/^d/X/M", "abc\ndef\0", &["-z"]), b"abc\ndef\0");
    }

    /// `w /dev/stdout` shares the fd with auto-print but not the debt, so neither
    /// stream pays the other's and `q` settles only the auto-print stream's.
    #[test]
    fn the_owed_separator_is_per_output_stream() {
        assert_eq!(sed("w /dev/stdout\np", "x", &["-n"]), b"xx");
        assert_eq!(sed("p\nw /dev/stdout", "x", &["-n"]), b"xx");
        assert_eq!(sed("w /dev/stdout\nw /dev/stdout", "x", &["-n"]), b"x\nx");
        assert_eq!(sed("p\nw /dev/stdout\nq", "x", &["-n"]), b"xx\n");
        assert_eq!(sed("w /dev/stdout\nq", "x", &["-n"]), b"x");
        assert_eq!(sed("w /dev/stdout\nw /dev/stdout\nq", "x", &["-n"]), b"x\nx");
        // A terminated record leaves no debt on either stream.
        assert_eq!(sed("w /dev/stdout\np", "x\n", &["-n"]), b"x\nx\n");
    }

    #[test]
    fn substitute_replaces_the_first_occurrence_by_default() {
        assert_eq!(sed("s/a/X/", "aaa\n", &[]), b"Xaa\n");
        assert_eq!(sed("s/a/X/g", "aaa\n", &[]), b"XXX\n");
        assert_eq!(sed("s/a/X/2", "aaa\n", &[]), b"aXa\n");
        assert_eq!(sed("s/a/X/2g", "aaaa\n", &[]), b"aXXX\n");
    }

    #[test]
    fn replacement_honors_ampersand_groups_and_case_operators() {
        assert_eq!(sed("s/b*/[&]/", "bbc\n", &[]), b"[bb]c\n");
        assert_eq!(sed(r"s/\(a\)\(b\)/\2\1/", "ab\n", &[]), b"ba\n");
        assert_eq!(sed(r"s/.*/\U&/", "ab\n", &[]), b"AB\n");
        assert_eq!(sed(r"s/.*/\u&/", "ab\n", &[]), b"Ab\n");
        assert_eq!(sed(r"s/b/\n/", "ab\n", &[]), b"a\n\n");
    }

    #[test]
    fn an_empty_match_after_a_replacement_does_not_repeat() {
        assert_eq!(sed("s/a*/X/g", "baac\n", &[]), b"XbXcX\n");
    }

    #[test]
    fn addresses_select_lines_and_ranges() {
        assert_eq!(sed("2d", "a\nb\nc\n", &[]), b"a\nc\n");
        assert_eq!(sed("2,3d", "a\nb\nc\nd\n", &[]), b"a\nd\n");
        assert_eq!(sed("$d", "a\nb\n", &[]), b"a\n");
        assert_eq!(sed("/b/,/c/d", "a\nb\nc\nd\n", &[]), b"a\nd\n");
        assert_eq!(sed("1~2d", "a\nb\nc\nd\n", &[]), b"b\nd\n");
        assert_eq!(sed("2,+1d", "a\nb\nc\nd\n", &[]), b"a\nd\n");
        assert_eq!(sed("0,/b/d", "b\na\nb\n", &[]), b"a\nb\n");
        // `1,/b/`: the end regex is tested from line 2, so the range runs to the
        // second `b` — GNU sed 4.9 deletes every line here.
        assert_eq!(sed("1,/b/d", "b\na\nb\n", &[]), b"");
    }

    #[test]
    fn negation_inverts_an_address() {
        assert_eq!(sed("2!d", "a\nb\nc\n", &[]), b"b\n");
    }

    #[test]
    fn hold_space_commands_move_text() {
        assert_eq!(sed("1h;2G", "a\nb\n", &[]), b"a\nb\na\n");
        assert_eq!(sed("1{h;d};2{x;G}", "a\nb\n", &[]), b"a\nb\n");
    }

    #[test]
    fn branching_loops_until_no_substitution_is_left() {
        assert_eq!(sed(":a;s/aa/a/;ta", "aaaa\n", &[]), b"a\n");
    }

    #[test]
    fn n_and_capital_n_pull_the_next_line() {
        assert_eq!(sed("N;s/\\n/-/", "a\nb\n", &[]), b"a-b\n");
        assert_eq!(sed("n;d", "a\nb\nc\nd\n", &[]), b"a\nc\n");
    }

    #[test]
    fn suppress_prints_only_what_p_selects() {
        assert_eq!(sed("2p", "a\nb\n", &["-n"]), b"b\n");
    }

    #[test]
    fn transliterate_maps_bytes() {
        assert_eq!(sed("y/abc/xyz/", "cab\n", &[]), b"zxy\n");
    }

    #[test]
    fn a_missing_final_newline_is_preserved() {
        assert_eq!(sed("s/a/b/", "a", &[]), b"b");
    }

    #[test]
    fn empty_regex_reuses_the_last_one() {
        assert_eq!(sed("s/ab/X/;s//Y/", "abab\n", &[]), b"XY\n".to_vec());
        assert_eq!(sed("/b/s//X/", "abc\n", &[]), b"aXc\n".to_vec());
    }

    #[test]
    fn list_escapes_nonprinting_bytes() {
        assert_eq!(sed("l", "a\tb\n", &["-n"]), b"a\\tb$\n");
    }

    #[test]
    fn a_i_and_c_place_text_around_the_line() {
        assert_eq!(sed("1a X", "a\nb\n", &[]), b"a\nX\nb\n");
        assert_eq!(sed("2i X", "a\nb\n", &[]), b"a\nX\nb\n");
        assert_eq!(sed("1c X", "a\nb\n", &[]), b"X\nb\n");
    }

    #[test]
    fn q_stops_after_printing() {
        assert_eq!(sed("2q", "a\nb\nc\n", &[]), b"a\nb\n");
    }

    #[test]
    fn a_delimiter_inside_a_bracket_expression_is_literal() {
        assert_eq!(sed("s/[/]/:/", "a/b\n", &[]), b"a:b\n");
    }

    #[test]
    fn d_capital_restarts_the_cycle_on_the_remainder() {
        assert_eq!(sed("N;P;D", "a\nb\nc\n", &[]), b"a\nb\nc\n");
    }

    /// The scratch file `-i` writes through must be created 0600, not at the
    /// umask default. The content lands in it BEFORE the original's mode is
    /// restored, so a 0600 secret edited in a traversable directory would be
    /// world-readable for the length of the write, under a predictable name.
    /// Asserted on `create_temp` directly: the finished file carries the
    /// original's mode either way, so no end-to-end test can see this window.
    #[test]
    fn the_in_place_scratch_file_is_created_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("td-txt-temp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (path, file) = create_temp(&dir, b"f").unwrap();
        let mode = file.metadata().unwrap().permissions().mode() & 0o777;
        drop(file);
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(mode, 0o600, "the scratch file was created world-readable");
        assert!(
            path.starts_with(&dir),
            "the scratch file must sit beside the target so the rename stays within one \
             filesystem"
        );
    }

    /// The runtime/compile split `Fatal::runtime` draws, both ways. A filesystem
    /// refusal is exit 4 and blames no expression; the one runtime error that IS a
    /// script mistake keeps GNU's exit 1 and its prefix.
    #[test]
    fn a_runtime_failure_is_exit_4_unless_it_is_a_script_mistake() {
        assert_eq!(Fatal::runtime("couldn't open file x: nope".into()).status, 4);
        assert_eq!(Fatal::runtime("write error: nope".into()).status, 4);
        assert_eq!(Fatal::runtime(NO_PREVIOUS_REGEX.into()).status, 1);
    }

    /// The delimiter is tested before the backslash, so `\<delim>` is that
    /// delimiter as a literal whatever it is -- and a `\` may itself delimit.
    /// GNU's `match_slash` is one rule; deciding from the character class is not.
    #[test]
    fn any_delimiter_collapses_its_own_escape_and_a_backslash_is_one() {
        assert_eq!(sed(r"sxa\xbxXx", "axb\n", &[]), b"X\n");
        assert_eq!(sed(r"\xa\xbxd", "axb\n", &[]), b"");
        // `\n` is a newline only where `n` is not the delimiter.
        assert_eq!(sed(r"sna\nbnXn", "anb\n", &[]), b"X\n");
        assert_eq!(sed(r"sxa\nbxXx", "anb\n", &[]), b"anb\n");
        assert_eq!(sed(r"s\a\b\", "ab\n", &[]), b"bb\n");
        assert_eq!(sed(r"y\ab\ba\", "ab\n", &[]), b"ba\n");
        // A replacement keeps the backslash for `&` alone, where it means the
        // character rather than the match.
        assert_eq!(sed(r"s&x&\&&", "x\n", &[]), b"&\n");
        assert_eq!(sed(r"s&a\&b&X&", "a&b\n", &[]), b"X\n");
    }

    /// A bare newline ends a delimited half ahead of the delimiter AND of bracket
    /// state, which is why no newline can delimit and `s/[<nl>]/X/` is refused.
    #[test]
    fn a_bare_newline_is_unterminated_before_it_is_anything_else() {
        for (script, msg) in [
            (&b"s\na\nb\n"[..], "unterminated `s' command"),
            (b"s/[\n]/X/", "unterminated `s' command"),
            (b"s/a/x\ny/", "unterminated `s' command"),
            (b"y/a\n/xy/", "unterminated `y' command"),
            (b"/a\nb/p", "unterminated address regex"),
        ] {
            let err = compile_script(script, false, false, false, Vec::new(), false).err();
            assert_eq!(
                err.map(|f| f.msg),
                Some(msg.to_string()),
                "{:?} must be {msg}",
                String::from_utf8_lossy(script)
            );
        }
        // The backslash that makes it a continuation is what makes it legal.
        assert_eq!(sed("s/a/x\\\ny/", "ab\n", &[]), b"x\nyb\n");
    }

    /// A `]` inside a POSIX sub-expression is a member, not the end of the set,
    /// so a delimiter after one is still a member too.
    #[test]
    fn a_posix_sub_expression_does_not_end_the_set_the_delimiter_reader_is_in() {
        assert_eq!(sed("s/[[:alpha:]/]/X/g", "a/b\n", &[]), b"XXX\n");
        assert_eq!(sed("s,[[:alpha:],],X,g", "a,b\n", &[]), b"XXX\n");
        // With `]` for a delimiter the set ends right after the class, so the
        // `]` in the subject is not a member -- but reaching the set's end at all
        // is what needed fixing.
        assert_eq!(sed("s][[:alpha:]]]X]g", "a]b\n", &[]), b"X]X\n");
        assert_eq!(sed("s/[[.a.]/]/X/g", "a/b\n", &[]), b"XXb\n");
        assert_eq!(sed("s/[[=a=]/]/X/g", "a/b\n", &[]), b"XXb\n");
        // The reader CONSUMES the byte it tests after the kind character, so a
        // closer overlapping it is missed and the parity shows.
        // An even name-length CLOSES, and is then rejected for its name.
        assert_eq!(
            compile_script(b"s/[[....]]/X/", false, false, false, Vec::new(), false)
                .err()
                .map(|f| f.msg),
            Some("Invalid collation character".to_string())
        );
        for script in [&b"s/[[...]]/X/"[..], b"s/[[.....]]/X/", b"s/[[:::]]/X/", b"s/[[===]]/X/"] {
            let err = compile_script(script, false, false, false, Vec::new(), false).err();
            assert_eq!(
                err.map(|f| f.msg),
                Some("unterminated `s' command".to_string()),
                "{:?}",
                String::from_utf8_lossy(script)
            );
        }
        // The closer must be the character that opened it, so these run off the
        // end of the script rather than closing a set.
        for script in [&b"s/[[:alpha:]/X/"[..], b"s/[[:alpha.]]/X/", b"s/[[:]]/X/"] {
            let err = compile_script(script, false, false, false, Vec::new(), false).err();
            assert_eq!(
                err.map(|f| f.msg),
                Some("unterminated `s' command".to_string()),
                "{:?}",
                String::from_utf8_lossy(script)
            );
        }
    }

    /// GNU reports the bare-class-syntax refusal without an expression prefix and
    /// exits 4, alone among pattern errors.
    #[test]
    fn the_bare_class_syntax_refusal_is_exit_4_where_other_pattern_errors_are_1() {
        let f = compile_script(b"s@[:alpha:]@X@", false, false, false, Vec::new(), false).err();
        assert_eq!(f.as_ref().map(|f| f.status), Some(4));
        assert_eq!(f.map(|f| f.msg), Some(crate::regex::CLASS_SYNTAX.to_string()));
        let f = compile_script(b"s@[[:a:]]@X@", false, false, false, Vec::new(), false).err();
        assert_eq!(f.map(|f| f.status), Some(1));
    }

    #[test]
    fn an_unknown_command_is_a_diagnosed_error() {
        // A bad script is status 1; an unresolvable branch is a RUNTIME error,
        // which GNU reports as 4.
        for bad in [&b"k"[..], b"s/a/b", b"{p"] {
            let err = compile_script(bad, false, false, false, Vec::new(), false).err().map(|f| f.status);
            assert_eq!(err, Some(1), "{:?} must be a status-1 script error", bad);
        }
        assert_eq!(
            compile_script(b"bnowhere", false, false, false, Vec::new(), false).err().map(|f| f.status),
            Some(4)
        );
    }

    /// `a`/`i`/`c` need text where the script ENDS, and an `-e` part ends there for
    /// that question only -- the same bytes as one argument are legal.
    #[test]
    fn a_text_command_needs_text_only_where_its_part_ends() {
        let compile = |src: &[u8], parts: Vec<usize>| {
            compile_script(src, false, false, false, parts, false).err().map(|f| f.status)
        };
        // `sed a` and `sed -e a -e p`: nothing after the command in its own part.
        assert_eq!(compile(b"a", Vec::new()), Some(1));
        assert_eq!(compile(b"a\np", vec![1]), Some(1));
        // The same bytes as ONE argument are an empty text and a following command.
        assert_eq!(compile(b"a\np", Vec::new()), None);
        // A backslash asks for the next line, so it crosses the boundary.
        assert_eq!(compile(b"a\\\ntext", vec![2]), None);
    }

    /// The vocabulary is decoded before any parser reads the text, and the byte it
    /// leaves means different things to different readers — syntax to the regex
    /// compiler, a plain byte to a text buffer. One rule the two REALLY share is
    /// the bare backslash `\c` leaves behind, whose three outcomes (a rejected
    /// pattern, a kept backslash, a dropped one) no single value can show.
    #[test]
    fn the_escape_vocabulary_is_decoded_before_anything_else_reads_the_text() {
        let rx = |p: &[u8]| normalize_regex(p, false).unwrap();
        assert_eq!(rx(b"\\x41\\d066\\o103\\t"), b"ABC\t");
        assert_eq!(rx(b"[a\\x2dz]"), b"[a-z]"); // a decoded dash RANGES
        assert_eq!(rx(b"\\x5cw"), b"\\w"); // ... and a decoded backslash escapes
        assert_eq!(rx(b"\\\\n"), b"\\\\n"); // while an ESCAPED one just stands
        assert_eq!(rx(b"\\w\\1\\e"), b"\\w\\1\\e"); // none of these are GNU's
        assert_eq!(rx(b"[\\x]"), b"[x]"); // ... but a digitless `\x` IS, and sheds it
        assert_eq!(rx(b"\\c\\\\"), b"\x1c");
        assert_eq!(rx(b"\\c"), b"\\"); // bare: `Trailing backslash` from the compiler
        assert!(normalize_regex(b"\\c\\q", false).is_err());

        let buf = |p: &[u8]| normalize_buffer(p).unwrap();
        assert_eq!(buf(b"\\c"), b""); // ... which a text drops instead
        assert_eq!(buf(b"A\\"), b"A");
        assert_eq!(buf(b"\\x5cx41"), b"\\x41"); // a decoded byte is not read again
        assert_eq!(buf(b"\\\\x41"), b"\\x41");
        assert_eq!(buf(b"A\\qB\\e"), b"AqBe"); // an unknown escape sheds its backslash
        // A text ending in an unpaired backslash never reaches the decoder at all
        // (`parse_text` returns it raw), which is why this shape is an error here.
        assert!(normalize_buffer(b"X\\c\\").is_err());
    }
}
